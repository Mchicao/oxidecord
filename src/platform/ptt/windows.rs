use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::Input::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use super::AudioControl;

const DESKTOP_CHANGED: u32 = WM_APP + 1;
const REARM: u32 = WM_APP + 2;

pub struct Listener {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
    control: Arc<AudioControl>,
}

impl Listener {
    pub fn rearm(&self) {
        self.control.ptt_failed();
        unsafe {
            if PostThreadMessageW(self.thread_id, REARM, WPARAM(0), LPARAM(0)).is_err() {
                self.control.ptt_failed();
            }
        }
    }
    pub fn start(control: Arc<AudioControl>, key: u16) -> Result<Self, String> {
        let (ready, result) = mpsc::sync_channel(1);
        let state = control.clone();
        let thread = std::thread::Builder::new()
            .name("voice-ptt".into())
            .spawn(move || {
                let outcome = unsafe { listen(&state, key, &ready) };
                state.ptt_failed();
                if let Err(error) = outcome {
                    let _ = ready.send(Err(format!(
                        "Windows background key registration failed: {error}"
                    )));
                }
            })
            .map_err(|error| error.to_string())?;
        match result.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread: Some(thread),
                thread_id,
                control,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                let _ = thread.join();
                Err(error.to_string())
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.control.ptt_failed();
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

unsafe extern "system" fn desktop_changed(
    _: HWINEVENTHOOK,
    _: u32,
    _: windows::Win32::Foundation::HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    unsafe {
        let _ = PostThreadMessageW(GetCurrentThreadId(), DESKTOP_CHANGED, WPARAM(0), LPARAM(0));
    }
}

unsafe fn listen(
    control: &AudioControl,
    key: u16,
    ready: &mpsc::SyncSender<Result<u32, String>>,
) -> windows::core::Result<()> {
    unsafe {
        let window = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("Oxidecord PTT"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )?;
        let result = (|| {
            WTSRegisterSessionNotification(window, 0)?;
            let hook = SetWinEventHook(
                EVENT_SYSTEM_DESKTOPSWITCH,
                EVENT_SYSTEM_DESKTOPSWITCH,
                None,
                Some(desktop_changed),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            );
            if hook.is_invalid() {
                return Err(windows::core::Error::from_thread());
            }
            let mut registered = false;
            let result = (|| {
                RegisterRawInputDevices(
                    &[RAWINPUTDEVICE {
                        usUsagePage: 1,
                        usUsage: 6,
                        dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
                        hwndTarget: window,
                    }],
                    size_of::<RAWINPUTDEVICE>() as u32,
                )?;
                registered = true;
                control.ptt_ready(GetAsyncKeyState(i32::from(key)) < 0);
                let mut input_since = GetTickCount64();
                let mut healthy = true;
                let _ = ready.send(Ok(GetCurrentThreadId()));
                let mut message = MSG::default();
                loop {
                    match GetMessageW(&mut message, None, 0, 0).0 {
                        0 => break,
                        -1 => return Err(windows::core::Error::from_thread()),
                        _ => {}
                    }
                    match message.message {
                        WM_INPUT => {
                            let mut input = RAWINPUT::default();
                            let mut size = size_of::<RAWINPUT>() as u32;
                            let read = GetRawInputData(
                                HRAWINPUT(message.lParam.0 as *mut _),
                                RID_INPUT,
                                Some((&mut input as *mut RAWINPUT).cast()),
                                &mut size,
                                size_of::<RAWINPUTHEADER>() as u32,
                            );
                            if read == u32::MAX
                                || read < size_of::<RAWINPUTHEADER>() as u32
                                || (input.header.dwType == RIM_TYPEKEYBOARD.0
                                    && read
                                        < (size_of::<RAWINPUTHEADER>() + size_of::<RAWKEYBOARD>())
                                            as u32)
                            {
                                healthy = false;
                                control.ptt_failed();
                            } else if input.header.dwType == RIM_TYPEKEYBOARD.0
                                && healthy
                                && let Some(down) = key_transition(input.data.keyboard, key)
                                && after_rearm(message.time, GetTickCount64(), input_since)
                            {
                                control.key_event(key, down);
                            }
                            // Foreground WM_INPUT requires DefWindowProc cleanup. No legacy key is suppressed.
                            DefWindowProcW(
                                message.hwnd,
                                message.message,
                                message.wParam,
                                message.lParam,
                            );
                        }
                        WM_INPUT_DEVICE_CHANGE | WM_WTSSESSION_CHANGE | DESKTOP_CHANGED => {
                            healthy = false;
                            control.ptt_failed()
                        }
                        REARM => {
                            if healthy {
                                input_since = GetTickCount64();
                                control.ptt_ready(GetAsyncKeyState(i32::from(key)) < 0);
                            }
                        }
                        _ => {
                            DispatchMessageW(&message);
                        }
                    }
                }
                Ok(())
            })();
            if registered {
                let _ = RegisterRawInputDevices(
                    &[RAWINPUTDEVICE {
                        usUsagePage: 1,
                        usUsage: 6,
                        dwFlags: RIDEV_REMOVE,
                        hwndTarget: Default::default(),
                    }],
                    size_of::<RAWINPUTDEVICE>() as u32,
                );
            }
            let _ = UnhookWinEvent(hook);
            result
        })();
        let _ = WTSUnRegisterSessionNotification(window);
        let _ = DestroyWindow(window);
        result
    }
}

fn key_transition(keyboard: RAWKEYBOARD, chosen: u16) -> Option<bool> {
    let extended = u32::from(keyboard.Flags) & RI_KEY_E0 != 0;
    let key = match keyboard.VKey {
        0x10 => {
            if keyboard.MakeCode == 0x36 {
                0xa1
            } else {
                0xa0
            }
        }
        0x11 => {
            if extended {
                0xa3
            } else {
                0xa2
            }
        }
        0x12 => {
            if extended {
                0xa5
            } else {
                0xa4
            }
        }
        key => key,
    };
    (key == chosen).then_some(u32::from(keyboard.Flags) & RI_KEY_BREAK == 0)
}

fn after_rearm(message_time: u32, now: u64, since: u64) -> bool {
    let age = u64::from((now as u32).wrapping_sub(message_time));
    now.saturating_sub(age) > since
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_key_events_before_rearming_are_ignored_across_tick_wrap() {
        let wrap = 1u64 << 32;
        let since = wrap - 5;
        assert!(!after_rearm((wrap - 6) as u32, wrap + 10, since));
        assert!(!after_rearm((wrap - 5) as u32, wrap + 10, since));
        assert!(after_rearm(3, wrap + 10, since));
        assert!(after_rearm(99, wrap * 2 + 100, since));
    }

    #[test]
    fn raw_key_transitions_distinguish_modifiers_and_ignore_other_keys() {
        for (virtual_key, scan, flags, chosen) in [
            (0x77, 0, 0, 0x77),
            (0x10, 0x2a, 0, 0xa0),
            (0x10, 0x36, 0, 0xa1),
            (0x11, 0, 0, 0xa2),
            (0x11, 0, RI_KEY_E0, 0xa3),
            (0x12, 0, 0, 0xa4),
            (0x12, 0, RI_KEY_E0, 0xa5),
        ] {
            let mut keyboard = RAWKEYBOARD {
                VKey: virtual_key,
                MakeCode: scan,
                Flags: flags as u16,
                ..RAWKEYBOARD::default()
            };
            assert_eq!(key_transition(keyboard, chosen), Some(true));
            assert_eq!(key_transition(keyboard, chosen + 1), None);
            keyboard.Flags |= RI_KEY_BREAK as u16;
            assert_eq!(key_transition(keyboard, chosen), Some(false));
        }
    }
}
