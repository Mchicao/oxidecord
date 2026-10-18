use std::mem::ManuallyDrop;
use std::sync::mpsc::{SyncSender, sync_channel};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::*;
use windows::Win32::System::Com::{
    BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{AgileReference, HRESULT, Interface, Ref, implement};

use super::{AudioEvent, AudioPacket, AudioShare, pcm};
use crate::platform::capture::{clock::MediaClock, qpc_hns};

pub(super) fn applications() -> Result<Vec<super::AudioApplication>, String> {
    let owns_com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let result = (|| unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut applications = std::collections::BTreeMap::new();
        for index in 0..devices.GetCount()?.min(32) {
            let endpoint = devices.Item(index)?;
            let manager: IAudioSessionManager2 = endpoint.Activate(CLSCTX_ALL, None)?;
            let sessions = manager.GetSessionEnumerator()?;
            for index in 0..sessions.GetCount()?.min(512) {
                let session = sessions.GetSession(index)?;
                let extended: IAudioSessionControl2 = session.cast()?;
                let pid = extended.GetProcessId()?;
                if pid == 0 || pid == std::process::id() || applications.contains_key(&pid) {
                    continue;
                }
                let display = session
                    .GetDisplayName()
                    .ok()
                    .map(|text| {
                        let value = text.to_string().unwrap_or_default();
                        windows::Win32::System::Com::CoTaskMemFree(Some(text.0.cast()));
                        value
                    })
                    .filter(|name| !name.is_empty() && !name.starts_with('@'));
                let name = display
                    .or_else(|| process_name(pid))
                    .unwrap_or_else(|| format!("Application {pid}"));
                applications.insert(pid, name);
            }
        }
        windows::core::Result::Ok(
            applications
                .into_iter()
                .map(|(process_id, name)| super::AudioApplication { process_id, name })
                .collect(),
        )
    })()
    .map_err(|error| format!("Couldn't list applications with audio sessions: {error}"));
    if owns_com {
        unsafe { CoUninitialize() };
    }
    result
}

fn process_name(pid: u32) -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut path = [0u16; 32768];
    let mut length = path.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    };
    let _ = unsafe { CloseHandle(process) };
    result.ok()?;
    let path = String::from_utf16_lossy(&path[..length as usize]);
    std::path::Path::new(&path)
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
}

pub(super) struct Event(HANDLE);
// Kernel events may be signalled and waited on from any thread.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    pub(super) fn new(manual: bool) -> Result<Self, String> {
        unsafe { CreateEventW(None, manual, false, None) }
            .map(Self)
            .map_err(|e| e.to_string())
    }
    pub(super) fn signal(&self) {
        let _ = unsafe { SetEvent(self.0) };
    }
    fn stopped(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activation {
    result: SyncSender<Result<AgileReference<IAudioClient>, String>>,
    completed: std::sync::Arc<Event>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activation_Impl {
    fn ActivateCompleted(
        &self,
        operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let result = (|| {
            let operation = operation.as_ref().ok_or("No audio activation result")?;
            let mut status = HRESULT(0);
            let mut interface = None;
            unsafe { operation.GetActivateResult(&mut status, &mut interface) }
                .map_err(|e| e.to_string())?;
            status.ok().map_err(|e| e.to_string())?;
            let client = interface
                .ok_or("Audio activation returned no interface")?
                .cast::<IAudioClient>()
                .map_err(|e| e.to_string())?;
            AgileReference::new(&client).map_err(|e| e.to_string())
        })();
        let _ = self.result.try_send(result);
        self.completed.signal();
        Ok(())
    }
}

fn wait_activation(stop: &Event, completed: &Event) -> Result<bool, String> {
    let outcome = unsafe { WaitForMultipleObjects(&[stop.0, completed.0], false, 10_000) };
    if outcome == WAIT_OBJECT_0 {
        return Ok(false);
    }
    if outcome.0 == WAIT_OBJECT_0.0 + 1 {
        return Ok(true);
    }
    Err("Audio activation did not complete within its deadline".into())
}

fn activate(target: AudioShare, stop: &Event) -> Result<Option<IAudioClient>, String> {
    if stop.stopped() {
        return Ok(None);
    }
    if target == AudioShare::SystemOutput {
        return (|| unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)?
                .Activate(CLSCTX_ALL, None)
                .map(Some)
        })()
        .map_err(|e: windows::core::Error| e.to_string());
    }
    let AudioShare::Process(pid) = target else {
        return Err("Audio sharing is off".into());
    };
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let variant = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&mut params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast(),
                    },
                },
                ..Default::default()
            }),
        },
    };
    let (tx, rx) = sync_channel(1);
    let completed = std::sync::Arc::new(Event::new(true)?);
    let callback: IActivateAudioInterfaceCompletionHandler = Activation {
        result: tx,
        completed: completed.clone(),
    }
    .into();
    let _operation = unsafe {
        ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&variant), &callback)
    }.map_err(|e| format!("Process audio capture requires Windows build 20348 or later; no other apps were captured: {e}"))?;
    if !wait_activation(stop, &completed)? {
        return Ok(None);
    }
    rx.recv()
        .map_err(|e| format!("Audio activation did not complete: {e}"))?
        .map_err(|e| {
            format!("Couldn't capture the selected process; no system-wide fallback was used: {e}")
        })?
        .resolve()
        .map(Some)
        .map_err(|e| e.to_string())
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
struct Running(IAudioClient);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = unsafe { self.0.Stop() };
    }
}

pub(super) fn capture(
    target: AudioShare,
    clock: MediaClock,
    stop: &Event,
    events: &tokio::sync::mpsc::Sender<AudioEvent>,
) -> Result<(), String> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(|e| e.to_string())?;
    let _apartment = Apartment;
    let Some(client) = activate(target, stop)? else {
        return Ok(());
    };
    if stop.stopped() {
        return Ok(());
    }
    let ready = Event::new(false)?;
    let format = WAVEFORMATEX {
        wFormatTag: 1,
        nChannels: 2,
        nSamplesPerSec: pcm::RATE,
        nAvgBytesPerSec: pcm::RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    (|| unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            0,
            0,
            &format,
            None,
        )?;
        client.SetEventHandle(ready.0)
    })()
    .map_err(|e: windows::core::Error| format!("Couldn't initialize screen audio: {e}"))?;
    let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|e| e.to_string())?;
    let mut encoder = opus2::Encoder::new(
        pcm::RATE,
        opus2::Channels::Stereo,
        opus2::Application::Audio,
    )
    .map_err(|e| e.to_string())?;
    encoder
        .set_bitrate(opus2::Bitrate::Bits(128_000))
        .map_err(|e| e.to_string())?;
    let mut packetizer = pcm::Packetizer::default();
    let mut compressed = [0u8; 1275];
    unsafe { client.Start() }.map_err(|e| e.to_string())?;
    let _running = Running(client);
    if events.blocking_send(AudioEvent::Started).is_err() {
        return Ok(());
    }
    loop {
        let wait = unsafe { WaitForMultipleObjects(&[stop.0, ready.0], false, INFINITE) };
        if wait == WAIT_OBJECT_0 {
            return Ok(());
        }
        if wait.0 != WAIT_OBJECT_0.0 + 1 {
            return Err("Screen audio wait failed".into());
        }
        while unsafe { capture.GetNextPacketSize() }.map_err(|e| e.to_string())? != 0 {
            let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0, 0, 0);
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)) }
                .map_err(|e| e.to_string())?;
            let timestamp = if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 {
                clock.captured_at(qpc, qpc_hns())
            } else {
                clock
                    .elapsed()
                    .saturating_sub(pcm::duration(frames as usize))
            };
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let mut error = None;
            let mut process = |samples: &[i16], time| {
                packetizer.push(samples, time, |pcm, timestamp| {
                    match encoder.encode(pcm, &mut compressed) {
                        Ok(length) => {
                            let _ = events.try_send(AudioEvent::Packet(AudioPacket {
                                generation: 0,
                                capture_epoch: 0,
                                opus: compressed[..length].to_vec(),
                                timestamp,
                            }));
                        }
                        Err(e) => error = Some(e.to_string()),
                    }
                })
            };
            if silent {
                let zeros = [0; pcm::SAMPLES];
                let mut offset = 0;
                while offset < frames as usize {
                    let count = (frames as usize - offset).min(pcm::FRAMES);
                    process(&zeros[..count * 2], timestamp + pcm::duration(offset));
                    offset += count;
                }
            } else if !data.is_null() {
                process(
                    unsafe { std::slice::from_raw_parts(data.cast::<i16>(), frames as usize * 2) },
                    timestamp,
                );
            }
            unsafe { capture.ReleaseBuffer(frames) }.map_err(|e| e.to_string())?;
            if let Some(error) = error {
                return Err(format!("Screen audio encoding failed: {error}"));
            }
            if events.is_closed() || stop.stopped() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_process_activation_does_not_wait_for_a_device_callback() {
        let stop = Event::new(true).unwrap();
        let completed = Event::new(true).unwrap();
        stop.signal();
        assert!(!wait_activation(&stop, &completed).unwrap());
        completed.signal();
        assert!(!wait_activation(&stop, &completed).unwrap());
    }

    #[test]
    fn completed_process_activation_is_observed_without_polling() {
        let stop = Event::new(true).unwrap();
        let completed = Event::new(true).unwrap();
        completed.signal();
        assert!(wait_activation(&stop, &completed).unwrap());
    }
}
