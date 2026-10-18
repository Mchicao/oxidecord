//! Camera selection is inert; only an explicit start activates a Media Foundation source.

#[cfg(windows)]
mod windows;

use crate::platform::capture::{CaptureEvent, CaptureSettings, clock::MediaClock};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::Sender;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraDevice {
    pub id: String,
    pub name: String,
}

pub fn devices() -> Result<Vec<CameraDevice>, String> {
    #[cfg(windows)]
    {
        windows::devices()
    }
    #[cfg(not(windows))]
    {
        Err("Camera capture requires Windows".into())
    }
}

pub struct CameraCapture {
    keyframe: Arc<AtomicBool>,
    #[cfg(windows)]
    stop: std::sync::mpsc::SyncSender<windows::Message>,
}

impl CameraCapture {
    pub fn start_with_clock(
        device_id: String,
        settings: CaptureSettings,
        events: Sender<CaptureEvent>,
        clock: MediaClock,
    ) -> Result<Self, String> {
        if device_id.is_empty()
            || settings.fps == 0
            || settings.fps > 60
            || settings.max_width < 2
            || settings.max_height < 2
            || settings.bitrate == 0
        {
            return Err("Choose a camera and valid video settings".into());
        }
        #[cfg(windows)]
        {
            // Only one ReadSample is outstanding; the second slot is reserved for stop.
            let (stop, messages) = std::sync::mpsc::sync_channel(2);
            let callback = stop.clone();
            let keyframe = Arc::new(AtomicBool::new(true));
            let requested = keyframe.clone();
            std::thread::Builder::new()
                .name("camera-capture".into())
                .spawn(move || {
                    let outcome = windows::capture(
                        device_id, settings, &events, messages, callback, &requested, clock,
                    );
                    if let Err(error) = outcome {
                        let _ = events.blocking_send(CaptureEvent::Failed(error));
                    }
                })
                .map_err(|error| format!("Couldn't start the camera worker: {error}"))?;
            Ok(Self { keyframe, stop })
        }
        #[cfg(not(windows))]
        {
            let _ = (events, settings, clock);
            Err("Camera capture requires Windows".into())
        }
    }

    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Release);
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        #[cfg(windows)]
        let _ = self.stop.try_send(windows::Message::Stop);
    }
}
