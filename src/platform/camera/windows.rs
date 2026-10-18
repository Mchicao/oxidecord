use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::{Duration, Instant};

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{AgileReference, HRESULT, Interface, PCWSTR, PWSTR, Ref, implement};

use super::CameraDevice;
use crate::platform::capture::{
    CaptureEvent, CaptureSettings, MediaFoundation, Nv12Encoder, clock::MediaClock,
};

fn attributes() -> windows::core::Result<IMFAttributes> {
    let mut attributes = None;
    unsafe {
        MFCreateAttributes(&mut attributes, 3)?;
    }
    let attributes = attributes.ok_or_else(windows::core::Error::from_thread)?;
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
    }
    Ok(attributes)
}

pub(super) fn devices() -> Result<Vec<CameraDevice>, String> {
    let _platform = MediaFoundation::startup()?;
    (|| unsafe {
        let attributes = attributes()?;
        let (mut list, mut count) = (std::ptr::null_mut(), 0);
        MFEnumDeviceSources(&attributes, &mut list, &mut count)?;
        if list.is_null() {
            return Ok(Vec::new());
        }
        let activates = std::slice::from_raw_parts_mut(list, count as usize);
        let devices = activates
            .iter_mut()
            .filter_map(Option::take)
            .filter_map(|activate| {
                Some(CameraDevice {
                    id: string_attribute(
                        &activate,
                        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                    )
                    .ok()?,
                    name: string_attribute(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                        .ok()?,
                })
            })
            .collect();
        CoTaskMemFree(Some(list.cast()));
        windows::core::Result::Ok(devices)
    })()
    .map_err(|error| format!("Couldn't list cameras: {error}"))
}

fn string_attribute(
    attributes: &IMFAttributes,
    key: &windows::core::GUID,
) -> windows::core::Result<String> {
    let mut text = PWSTR::null();
    let mut length = 0;
    unsafe {
        attributes.GetAllocatedString(key, &mut text, &mut length)?;
        let result = String::from_utf16_lossy(std::slice::from_raw_parts(text.0, length as usize));
        CoTaskMemFree(Some(text.0.cast()));
        Ok(result)
    }
}

pub(super) enum Message {
    Stop,
    Sample {
        status: HRESULT,
        flags: u32,
        sample: Option<AgileReference<IMFSample>>,
    },
}

#[implement(IMFSourceReaderCallback)]
struct Callback {
    messages: SyncSender<Message>,
}

impl IMFSourceReaderCallback_Impl for Callback_Impl {
    fn OnReadSample(
        &self,
        status: HRESULT,
        _: u32,
        flags: u32,
        _: i64,
        sample: Ref<IMFSample>,
    ) -> windows::core::Result<()> {
        let (status, sample) = match sample.as_ref().map(AgileReference::new).transpose() {
            Ok(sample) => (status, sample),
            Err(error) => (error.code(), None),
        };
        let _ = self.messages.try_send(Message::Sample {
            status,
            flags,
            sample,
        });
        Ok(())
    }
    fn OnFlush(&self, _: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnEvent(&self, _: u32, event: Ref<IMFMediaEvent>) -> windows::core::Result<()> {
        if let Some(event) = event.as_ref()
            && let Ok(status) = unsafe { event.GetStatus() }
            && status.is_err()
        {
            let _ = self.messages.try_send(Message::Sample {
                status,
                flags: 0,
                sample: None,
            });
        }
        Ok(())
    }
}

struct Source(IMFMediaSource);
impl Drop for Source {
    fn drop(&mut self) {
        let _ = unsafe { self.0.Shutdown() };
    }
}

pub(super) fn capture(
    device_id: String,
    settings: CaptureSettings,
    events: &tokio::sync::mpsc::Sender<CaptureEvent>,
    messages: Receiver<Message>,
    callback_tx: SyncSender<Message>,
    keyframe: &AtomicBool,
    clock: MediaClock,
) -> Result<(), String> {
    if matches!(messages.try_recv(), Ok(Message::Stop)) {
        return Ok(());
    }
    let _platform = MediaFoundation::startup()?;
    let source = (|| unsafe {
        let attributes = attributes()?;
        let device: Vec<u16> = device_id.encode_utf16().chain(Some(0)).collect();
        attributes.SetString(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            PCWSTR(device.as_ptr()),
        )?;
        MFCreateDeviceSource(&attributes)
    })()
    .map(Source)
    .map_err(|error| {
        format!(
            "Couldn't open the selected camera. Check Windows camera privacy permissions: {error}"
        )
    })?;
    let reader = (|| unsafe {
        let mut options = None;
        MFCreateAttributes(&mut options, 3)?;
        let options = options.ok_or_else(windows::core::Error::from_thread)?;
        let callback: IMFSourceReaderCallback = Callback {
            messages: callback_tx,
        }
        .into();
        options.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback)?;
        options.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        options.SetUINT32(&MF_LOW_LATENCY, 1)?;
        MFCreateSourceReaderFromMediaSource(&source.0, &options)
    })()
    .map_err(|error| format!("Couldn't read the selected camera: {error}"))?;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut selected = None;
    for index in 0..512 {
        let Ok(format) = (unsafe { reader.GetNativeMediaType(stream, index) }) else {
            break;
        };
        let Ok(size) = (unsafe { format.GetUINT64(&MF_MT_FRAME_SIZE) }) else {
            continue;
        };
        let size = ((size >> 32) as u32, size as u32);
        if size.0 < 2
            || size.1 < 2
            || size.0 > settings.max_width
            || size.1 > settings.max_height
            || !size.0.is_multiple_of(2)
            || !size.1.is_multiple_of(2)
        {
            continue;
        }
        let rate = unsafe { format.GetUINT64(&MF_MT_FRAME_RATE) }.unwrap_or(30u64 << 32 | 1);
        let fps = (rate >> 32) as u32 / (rate as u32).max(1);
        if fps == 0 || fps > settings.fps {
            continue;
        }
        let score = u64::from(size.0) * u64::from(size.1) * u64::from(fps);
        if selected
            .as_ref()
            .is_none_or(|(best, _, _, _)| score > *best)
        {
            selected = Some((score, size, fps, format));
        }
    }
    let Some((_, size, fps, native)) = selected else {
        return Err(
            "The camera has no supported format within the selected resolution/frame-rate limit"
                .into(),
        );
    };
    (|| unsafe {
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(stream, true)?;
        reader.SetCurrentMediaType(stream, None, &native)?;
        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        output.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(size.0) << 32) | u64::from(size.1),
        )?;
        reader.SetCurrentMediaType(stream, None, &output)
    })()
    .map_err(|error| format!("The camera couldn't provide NV12 video: {error}"))?;
    let settings = CaptureSettings { fps, ..settings };
    let mut encoder = Nv12Encoder::new(size, &settings)?;
    let mut pixels = vec![0; size.0 as usize * size.1 as usize * 3 / 2];
    let mut sample_origin: Option<(i64, Duration)> = None;
    if events
        .blocking_send(CaptureEvent::Started {
            width: size.0,
            height: size.1,
        })
        .is_err()
    {
        return Ok(());
    }
    unsafe { reader.ReadSample(stream, 0, None, None, None, None) }.map_err(|e| e.to_string())?;
    while let Ok(message) = messages.recv() {
        let Message::Sample {
            status,
            flags,
            sample,
        } = message
        else {
            break;
        };
        status
            .ok()
            .map_err(|error| format!("Camera capture failed: {error}"))?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            let _ = events.blocking_send(CaptureEvent::SourceClosed);
            break;
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            return Err("The camera changed video format; select the camera again".into());
        }
        if let Some(sample) = sample {
            let sample = sample.resolve().map_err(|error| error.to_string())?;
            let timestamp = match unsafe { sample.GetSampleTime() } {
                Ok(time) => {
                    let (origin, at) = *sample_origin.get_or_insert((time, clock.elapsed()));
                    if time < origin {
                        sample_origin = Some((time, clock.elapsed()));
                        clock.elapsed()
                    } else {
                        at + Duration::from_nanos((time - origin) as u64 * 100)
                    }
                }
                Err(_) => clock.elapsed(),
            };
            let buffer =
                unsafe { sample.ConvertToContiguousBuffer() }.map_err(|e| e.to_string())?;
            if let Ok(planar) = buffer.cast::<IMF2DBuffer>() {
                unsafe { planar.ContiguousCopyTo(&mut pixels) }.map_err(|e| e.to_string())?;
            } else {
                let (mut pointer, mut length) = (std::ptr::null_mut(), 0);
                unsafe { buffer.Lock(&mut pointer, None, Some(&mut length)) }
                    .map_err(|e| e.to_string())?;
                let valid = length as usize >= pixels.len() && !pointer.is_null();
                if valid {
                    unsafe {
                        std::ptr::copy_nonoverlapping(pointer, pixels.as_mut_ptr(), pixels.len())
                    };
                }
                let unlocked = unsafe { buffer.Unlock() };
                unlocked.map_err(|e| e.to_string())?;
                if !valid {
                    return Err("The camera returned an incomplete video frame".into());
                }
            }
            for frame in encoder.encode(
                &pixels,
                timestamp,
                keyframe.swap(false, Ordering::AcqRel),
                Instant::now() + Duration::from_secs(1) / fps,
            )? {
                match events.try_send(CaptureEvent::Frame(frame)) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        keyframe.store(true, Ordering::Release);
                    }
                }
            }
        }
        if events.is_closed() {
            break;
        }
        unsafe { reader.ReadSample(stream, 0, None, None, None, None) }
            .map_err(|e| e.to_string())?;
    }
    let _ = unsafe { reader.Flush(stream) };
    Ok(())
}
