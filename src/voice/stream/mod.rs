//! Go Live: streaming the screen into a call.
//!
//! A stream is a second voice connection, separate from the call's. The main
//! gateway is asked to create the stream, answers with a server and token of
//! its own, and the screen goes to that server as H.264 over RTP, end-to-end
//! encrypted with DAVE like the call's audio. Everyone watching connects to
//! the same server; the streamer never hears from them directly, only
//! through the RTCP feedback the server relays.
//!
//! Watching someone else's stream is the same connection the other way
//! round, in [`watch`]: [`WatchStream`] receives the streamer's RTP and hands
//! the frames to the platform's decoder.
//!
//! The whole connection — websocket, UDP socket, DAVE session — lives in one
//! task on the shared runtime, so none of it needs a lock. [`GoLive`] is the
//! only handle the app holds, and dropping it ends the stream.

mod call;
mod fence;
mod flags;
mod media;
mod receive;
mod recovery;
mod signalling;
mod sps;
mod sync;
mod watch;

pub use crate::platform::loopback::{AudioPacket, AudioShare};
pub use call::{CallMedia, CallMediaEvent, RemoteAudio};
pub use flags::{CallMediaFlags, VideoDescription};
pub use watch::WatchStream;

use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use futures::channel::oneshot;
use twilight_model::id::{Id, marker::UserMarker};

use crate::platform::capture::clock::MediaClock;
use crate::platform::capture::{
    CaptureEvent, CaptureSettings, CaptureSource, EncodedFrame, ScreenCapture,
};
use crate::platform::loopback::{AudioEvent, Loopback};
use crate::platform::runtime;

use media::Media;
use signalling::{Signal, Signalling};

/// How often a sender report goes out, so viewers can keep the stream's
/// clock in step with theirs.
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Everything needed to open a stream connection, gathered from the main
/// gateway's `STREAM_CREATE` and `STREAM_SERVER_UPDATE`.
pub struct StreamConnection {
    pub stream_key: String,
    /// The stream server's id, which the connection identifies to.
    pub server_id: String,
    /// The DAVE group's channel id, derived from [`Self::server_id`].
    pub dave_channel_id: u64,
    pub user_id: Id<UserMarker>,
    /// The call's voice session: a stream belongs to the session that
    /// created it, and is watched from the session that's in the call.
    pub session_id: String,
    pub token: String,
    pub endpoint: String,
}

impl StreamConnection {
    /// DAVE's channel id for a stream: the stream server's id, less one.
    pub fn dave_channel_for(server_id: &str) -> Option<u64> {
        server_id.parse::<u64>().ok()?.checked_sub(1)
    }
}

pub enum StreamEvent {
    /// The screen is going out.
    Live { stream_key: String },
    /// The stream is over. `None` when it ended normally — the shared window
    /// closed, or the server ended it — and the reason when it failed.
    Ended {
        stream_key: String,
        error: Option<String>,
    },
}

/// One running stream. Dropping it stops the capture and closes the
/// connection.
pub struct GoLive {
    _stop: oneshot::Sender<()>,
}

impl GoLive {
    pub fn start_with_audio(
        connection: StreamConnection,
        source: CaptureSource,
        settings: CaptureSettings,
        audio: AudioShare,
        events: UnboundedSender<StreamEvent>,
    ) -> Self {
        let (stop, stopped) = oneshot::channel();
        runtime::handle().spawn(async move {
            let stream_key = connection.stream_key.clone();
            let Some(outcome) =
                until_stopped(stopped, run(connection, source, settings, audio, &events)).await
            else {
                return;
            };
            let error = outcome.err();
            let _ = events.unbounded_send(StreamEvent::Ended { stream_key, error });
        });
        Self { _stop: stop }
    }
}

async fn until_stopped<T>(
    stopped: oneshot::Receiver<()>,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = stopped => None,
        result = work => Some(result),
    }
}

async fn run(
    connection: StreamConnection,
    source: CaptureSource,
    settings: CaptureSettings,
    audio: AudioShare,
    events: &UnboundedSender<StreamEvent>,
) -> Result<(), String> {
    let (mut signalling, ready) = Signalling::connect(&connection).await?;
    let (mut media, external) = Media::connect(ready.ip, ready.port, ready.ssrcs).await?;
    let (mode, key) = tokio::time::timeout(
        Duration::from_secs(15),
        signalling.select_protocol(external, ready.mode),
    )
    .await
    .map_err(|_| "Stream protocol selection timed out")??;
    media.set_key(&mode, &key)?;
    let clock = MediaClock::default();
    media.set_clock(clock);

    let (capture_events, mut captured) = tokio::sync::mpsc::channel(2);
    let capture = ScreenCapture::start_with_clock(source, settings, capture_events, clock);
    let (audio_events, mut audio_rx) = tokio::sync::mpsc::channel(8);
    let _audio = if audio == AudioShare::Off {
        None
    } else {
        Some(Loopback::start(audio, clock, audio_events)?)
    };

    let mut heartbeat = tokio::time::interval(signalling.heartbeat);
    let mut reports = tokio::time::interval(REPORT_INTERVAL);
    let mut buffer = vec![0u8; 1500];
    // Frames can't go out until DAVE's group has formed; the first one that
    // does has to be a keyframe, or there's nothing for viewers to start on.
    let mut readiness = recovery::CaptureReadiness::default();
    let mut needs_keyframe = true;
    let mut live = false;

    loop {
        readiness.update(signalling.dave.is_ready(), || {
            needs_keyframe = true;
            capture.request_keyframe();
        });
        tokio::select! {
            _ = heartbeat.tick() => signalling.send_heartbeat().await?,
            _ = reports.tick() => media.send_report().await,
            signal = signalling.next() => match signal? {
                Signal::Nothing => {}
                Signal::Closed(None) => break,
                Signal::Closed(Some(err)) => return Err(err),
            },
            event = captured.recv() => match event {
                Some(CaptureEvent::Started { width, height }) => {
                    eprintln!("screen share: capture ready at {width}x{height}, {} fps", settings.fps);
                    signalling
                        .announce_video(ready.ssrcs, (width, height), settings.fps, settings.bitrate, audio != AudioShare::Off)
                        .await?;
                }
                Some(CaptureEvent::Frame(frame)) => {
                    let ready_now = signalling.dave.is_ready();
                    if ready_now && (!needs_keyframe || frame.keyframe)
                        && send(&mut signalling, &mut media, frame).await? {
                            needs_keyframe = false;
                            if !live {
                                live = true;
                                let _ = events.unbounded_send(StreamEvent::Live { stream_key: connection.stream_key.clone() });
                            }
                    }
                }
                Some(CaptureEvent::SourceClosed) | None => break,
                Some(CaptureEvent::Failed(err)) => return Err(err),
            },
            event = audio_rx.recv(), if _audio.is_some() => match event {
                Some(AudioEvent::Started) => signalling.set_speaking(ready.ssrcs.audio, 2).await?,
                Some(AudioEvent::Packet(packet)) => {
                    if let Some(protected) = signalling.dave.protect_audio(&packet.opus) {
                        media.send_audio(&protected, packet.timestamp).await?;
                    }
                },
                Some(AudioEvent::Failed(error)) => return Err(error),
                None => return Err("Screen audio ended unexpectedly".into()),
            },
            received = media.recv(&mut buffer) => {
                let Ok(length) = received else { continue };
                let feedback = media.feedback(&buffer[..length]);
                if feedback.keyframe {
                    capture.request_keyframe();
                    needs_keyframe = true;
                }
                if !feedback.lost.is_empty() {
                    media.retransmit(&feedback.lost).await;
                }
            },
        }
    }

    signalling.close().await;
    Ok(())
}

async fn send(
    signalling: &mut Signalling,
    media: &mut Media,
    frame: EncodedFrame,
) -> Result<bool, String> {
    let frame_data = sps::rewrite_frame(&frame.data);
    let Some(protected) = signalling.dave.protect(&frame_data) else {
        return Ok(false);
    };
    media.send_frame(&protected, frame.timestamp).await?;
    Ok(true)
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[tokio::test]
    async fn cancelled_share_never_starts_a_late_handshake() {
        let (stop, stopped) = oneshot::channel();
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = started.clone();
        drop(stop);
        let outcome = until_stopped(stopped, async move {
            worker_started.store(true, Ordering::Release);
        })
        .await;
        assert!(outcome.is_none());
        assert!(!started.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn stopping_during_handshake_releases_watched_audio_and_pending_media() {
        struct Released(Arc<AtomicBool>);
        impl Drop for Released {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let (stop, stopped) = oneshot::channel();
        let (entered, started) = oneshot::channel();
        let (complete, handshake) = oneshot::channel::<()>();
        let (audio, mut received) = tokio::sync::mpsc::channel::<RemoteAudio>(8);
        let released = Arc::new(AtomicBool::new(false));
        let finished = released.clone();
        let worker = tokio::spawn(until_stopped(stopped, async move {
            let _released = Released(finished);
            let _audio = audio;
            let _ = entered.send(());
            let _ = handshake.await;
            std::future::pending::<()>().await;
        }));
        started.await.unwrap();
        drop(stop);
        assert!(worker.await.unwrap().is_none());
        assert!(released.load(Ordering::Acquire));
        assert!(complete.send(()).is_err());
        assert!(
            tokio::time::timeout(Duration::from_secs(1), received.recv())
                .await
                .unwrap()
                .is_none()
        );
    }
}
