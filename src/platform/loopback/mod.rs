//! Explicitly opted-in application audio. There is no automatic system-wide fallback.

pub mod pcm;
#[cfg(windows)]
mod windows;

use crate::platform::capture::clock::MediaClock;
use std::time::Duration;
use tokio::sync::mpsc::Sender;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioShare {
    #[default]
    Off,
    Process(u32),
    /// Captures every app on the default output. The UI must ask explicitly.
    SystemOutput,
}

#[derive(Clone, Debug)]
pub struct AudioApplication {
    pub process_id: u32,
    pub name: String,
}

/// Called when the user opens the application-audio picker; never polled.
pub fn applications() -> Result<Vec<AudioApplication>, String> {
    #[cfg(windows)]
    {
        windows::applications()
    }
    #[cfg(not(windows))]
    {
        Err("Application audio sharing requires Windows".into())
    }
}

pub struct AudioPacket {
    /// Primary-call capture epoch. Screen loopback uses zero on its own transport.
    pub generation: u64,
    pub capture_epoch: u64,
    pub opus: Vec<u8>,
    pub timestamp: Duration,
}

pub enum AudioEvent {
    Started,
    Packet(AudioPacket),
    Failed(String),
}

pub struct Loopback {
    #[cfg(windows)]
    stop: std::sync::Arc<windows::Event>,
}

impl Loopback {
    pub fn start(
        target: AudioShare,
        clock: MediaClock,
        events: Sender<AudioEvent>,
    ) -> Result<Self, String> {
        if target == AudioShare::Off {
            return Err("Audio sharing is off".into());
        }
        if let AudioShare::Process(pid) = target
            && (pid == 0 || pid == std::process::id())
        {
            return Err(
                "Choose another application's process to avoid capturing the call itself".into(),
            );
        }
        #[cfg(windows)]
        {
            let stop = std::sync::Arc::new(windows::Event::new(true)?);
            let worker_stop = stop.clone();
            std::thread::Builder::new()
                .name("screen-audio".into())
                .spawn(move || {
                    if let Err(error) = windows::capture(target, clock, &worker_stop, &events) {
                        let _ = events.blocking_send(AudioEvent::Failed(error));
                    }
                })
                .map_err(|error| format!("Couldn't start screen audio: {error}"))?;
            Ok(Self { stop })
        }
        #[cfg(not(windows))]
        {
            let _ = (clock, events);
            Err("Application audio sharing requires Windows".into())
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        #[cfg(windows)]
        self.stop.signal();
    }
}
