//! The primary call's single voice websocket, UDP transport and MLS group.
//! AudioIo remains responsible for devices, processing, gating and playback.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};

use crate::platform::capture::{EncodedFrame, clock::MediaClock};
use crate::platform::loopback::AudioPacket;
use crate::platform::runtime;
use crate::voice::VoiceConnection;

use super::fence::AudioFence;
use super::media::{H264_PAYLOAD, Media, OPUS_PAYLOAD, RTX_PAYLOAD, RtpPacket};
use super::receive::Assembler;
use super::signalling::{Change, Signal, Signalling};
use super::{CallMediaFlags, StreamConnection};

pub struct RemoteAudio {
    pub user_id: u64,
    pub ssrc: u32,
    pub sequence: u16,
    pub opus: Vec<u8>,
}

pub enum CallMediaEvent {
    Connected,
    RemoteAudio(RemoteAudio),
    RemoteVideo { user_id: u64, frame: EncodedFrame },
    VideoState { user_id: u64, active: bool },
    Speaking { user_id: u64, speaking: bool },
    UserLeft { user_id: u64 },
    KeyframeRequested,
    Reconnecting,
    Disconnected { error: Option<String> },
}

pub struct CallMedia {
    audio: mpsc::Sender<AudioPacket>,
    video: mpsc::Sender<EncodedFrame>,
    flags: watch::Sender<CallMediaFlags>,
    stop: Option<oneshot::Sender<()>>,
    clock: MediaClock,
    audio_fence: Arc<AudioFence>,
    requests: mpsc::Sender<u64>,
    validator: Arc<Mutex<Option<AudioValidator>>>,
}

type AudioValidator = Arc<dyn Fn(u64) -> bool + Send + Sync>;

struct MediaInput {
    audio: mpsc::Receiver<AudioPacket>,
    video: mpsc::Receiver<EncodedFrame>,
    flags: watch::Receiver<CallMediaFlags>,
    requests: mpsc::Receiver<u64>,
}

fn capture_valid(validator: &Mutex<Option<AudioValidator>>, epoch: u64) -> bool {
    validator
        .lock()
        .is_ok_and(|validator| validator.as_ref().is_none_or(|valid| valid(epoch)))
}

impl CallMedia {
    /// Replaces Songbird's connection; never start both for the same session.
    pub fn start(connection: VoiceConnection, events: mpsc::Sender<CallMediaEvent>) -> Self {
        let (audio, audio_rx) = mpsc::channel(8);
        let (video, video_rx) = mpsc::channel(2);
        let (flags, flags_rx) = watch::channel(CallMediaFlags::default());
        let (stop, stopped) = oneshot::channel();
        let clock = MediaClock::default();
        let audio_fence = Arc::new(AudioFence::default());
        let worker_fence = audio_fence.clone();
        let (requests, request_rx) = mpsc::channel(16);
        let validator = Arc::new(Mutex::new(None));
        let worker_validator = validator.clone();
        let input = MediaInput {
            audio: audio_rx,
            video: video_rx,
            flags: flags_rx,
            requests: request_rx,
        };
        runtime::handle().spawn(async move {
            let result = tokio::select! {
                _ = stopped => return,
                result = run(connection, events.clone(), input, clock, worker_fence, worker_validator) => result,
            };
            let _ = events.send(CallMediaEvent::Disconnected { error: result.err() }).await;
        });
        Self {
            audio,
            video,
            flags,
            stop: Some(stop),
            clock,
            audio_fence,
            requests,
            validator,
        }
    }

    pub fn clock(&self) -> MediaClock {
        self.clock
    }

    pub fn send_opus(&self, packet: AudioPacket) -> Result<(), AudioPacket> {
        if !self.audio_fence.accepts(packet.generation)
            || !capture_valid(&self.validator, packet.capture_epoch)
            || packet.opus.is_empty()
            || packet.opus.len() > 1275
        {
            return Err(packet);
        }
        self.audio
            .try_send(packet)
            .map_err(mpsc::error::TrySendError::into_inner)
    }

    pub fn send_frame(&self, frame: EncodedFrame) -> Result<(), EncodedFrame> {
        if frame.data.is_empty() || frame.data.len() > 8 * 1024 * 1024 {
            return Err(frame);
        }
        self.video
            .try_send(frame)
            .map_err(mpsc::error::TrySendError::into_inner)
    }

    pub fn set_flags(&self, flags: CallMediaFlags) {
        self.audio_fence.set_open(flags.sending());
        self.flags.send_if_modified(|current| {
            if *current == flags {
                return false;
            }
            *current = flags;
            true
        });
    }

    pub fn audio_generation(&self) -> u64 {
        self.audio_fence.generation()
    }

    pub(crate) fn set_audio_validator(&self, validator: AudioValidator) {
        if let Ok(mut current) = self.validator.lock() {
            *current = Some(validator);
        }
        self.invalidate_audio();
    }

    pub fn request_keyframe(&self, user_id: u64) {
        let _ = self.requests.try_send(user_id);
    }

    pub fn invalidate_audio(&self) {
        self.audio_fence.invalidate();
        self.flags.send_modify(|_| {});
    }

    pub fn stop(&mut self) {
        self.audio_fence.set_open(false);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for CallMedia {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn run(
    connection: VoiceConnection,
    events: mpsc::Sender<CallMediaEvent>,
    input: MediaInput,
    clock: MediaClock,
    audio_fence: Arc<AudioFence>,
    validator: Arc<Mutex<Option<AudioValidator>>>,
) -> Result<(), String> {
    let MediaInput {
        mut audio,
        mut video,
        flags: mut flags_rx,
        mut requests,
    } = input;
    let connection = StreamConnection {
        stream_key: String::new(),
        server_id: connection
            .guild_id
            .map_or(connection.channel_id.get(), |id| id.get())
            .to_string(),
        dave_channel_id: connection.channel_id.get(),
        user_id: connection.user_id,
        session_id: connection.session_id,
        token: connection.token,
        endpoint: connection.endpoint,
    };
    let (mut signalling, ready) = tokio::time::timeout(
        Duration::from_secs(15),
        Signalling::connect_kind(&connection, "video"),
    )
    .await
    .map_err(|_| "Voice handshake timed out")??;
    let (mut media, external) = Media::connect(ready.ip, ready.port, ready.ssrcs).await?;
    let (mode, key) = tokio::time::timeout(
        Duration::from_secs(15),
        signalling.select_protocol(external, ready.mode),
    )
    .await
    .map_err(|_| "Voice protocol selection timed out")??;
    media.set_key(&mode, &key)?;
    media.set_clock(clock);
    signalling.announce_watching(ready.ssrcs).await?;
    signalling.set_speaking(ready.ssrcs.audio, 0).await?;
    events
        .send(CallMediaEvent::Connected)
        .await
        .map_err(|_| "Voice event receiver closed")?;
    let mut flags = CallMediaFlags::default();
    let mut heartbeat = tokio::time::interval(signalling.heartbeat);
    let mut reports = tokio::time::interval(Duration::from_secs(1));
    let mut buffer = [0u8; 8192];
    let mut viewers: HashMap<u64, RemoteCamera> = HashMap::new();
    let mut needs_local_keyframe = true;
    let mut ready_before = false;
    loop {
        for change in signalling.changes().collect::<Vec<_>>() {
            let event = match change {
                Change::Speaking { user_id, speaking } => {
                    CallMediaEvent::Speaking { user_id, speaking }
                }
                Change::Video { user_id, active } => {
                    viewers.remove(&user_id);
                    CallMediaEvent::VideoState { user_id, active }
                }
                Change::Left { user_id } => {
                    viewers.remove(&user_id);
                    CallMediaEvent::UserLeft { user_id }
                }
            };
            events
                .send(event)
                .await
                .map_err(|_| "Voice event receiver closed")?;
        }
        let ready_now = signalling.dave.is_ready();
        if ready_now && !ready_before {
            needs_local_keyframe = true;
            let _ = events.try_send(CallMediaEvent::KeyframeRequested);
        }
        ready_before = ready_now;
        // A deadline exists only while video packets are held behind a gap.
        let deadline = viewers.values().filter_map(|viewer| viewer.deadline).min();
        let expiry = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(expiry);
        tokio::select! {
            biased;
            user_id = requests.recv() => {
                if let Some(user_id) = user_id && let Some(remote) = signalling.videos.get(&user_id).copied() {
                    let viewer = viewers.entry(user_id).or_default();
                    viewer.needs_keyframe = true;
                    if viewer.last_request.is_none_or(|last| last.elapsed() >= Duration::from_millis(500)) {
                        viewer.last_request = Some(Instant::now());
                        media.send_picture_loss(remote.ssrc).await;
                    }
                }
            },
            changed = flags_rx.changed() => {
                if changed.is_err() { break; }
                let next = *flags_rx.borrow_and_update();
                if next.video != flags.video {
                    let description = next.video.map(|video| ((video.width, video.height), video.fps, video.bitrate));
                    if next.video.is_some_and(|video| video.width < 2 || video.height < 2 || video.fps == 0 || video.fps > 60 || video.bitrate == 0) { return Err("Invalid camera stream description".into()); }
                    signalling.set_video(ready.ssrcs, description).await?;
                    needs_local_keyframe = true;
                    let _ = events.try_send(CallMediaEvent::KeyframeRequested);
                }
                if let Some(speaking) = next.speaking_change(flags) {
                    if !speaking {
                        for offset in 0..5 {
                            if let Some(protected) = signalling.dave.protect_audio(&[0xF8, 0xFF, 0xFE]) {
                                media.send_audio(&protected, clock.elapsed() + Duration::from_millis(offset * 20)).await?;
                            }
                        }
                    }
                    signalling.set_speaking(ready.ssrcs.audio, u8::from(speaking)).await?;
                }
                while audio.try_recv().is_ok() {}
                flags = next;
            },
            _ = heartbeat.tick() => {
                if signalling.send_heartbeat().await.is_err() {
                    resume(&mut signalling, &connection, &events).await?;
                    heartbeat = tokio::time::interval(signalling.heartbeat);
                    needs_local_keyframe = true;
                }
            },
            _ = reports.tick() => { media.send_report().await; media.send_receiver_report().await; },
            signal = signalling.next() => match signal {
                Ok(Signal::Nothing) => {},
                Ok(Signal::Closed(None)) => break,
                Ok(Signal::Closed(Some(error))) | Err(error) => {
                    if !signalling.resumable { return Err(error); }
                    resume(&mut signalling, &connection, &events).await?;
                    heartbeat = tokio::time::interval(signalling.heartbeat);
                    needs_local_keyframe = true;
                },
            },
            packet = audio.recv() => {
                let Some(packet) = packet else { break; };
                if flags.sending() && audio_fence.accepts(packet.generation) && capture_valid(&validator, packet.capture_epoch) && fresh(packet.timestamp, clock.elapsed())
                    && let Some(protected) = signalling.dave.protect_audio(&packet.opus)
                    && audio_fence.accepts(packet.generation) && capture_valid(&validator, packet.capture_epoch)
                {
                    media.send_audio(&protected, packet.timestamp).await?;
                }
            },
            frame = video.recv() => {
                let Some(frame) = frame else { break; };
                if flags.video.is_some() && fresh(frame.timestamp, clock.elapsed()) && (!needs_local_keyframe || frame.keyframe) {
                    let rewritten = super::sps::rewrite_frame(&frame.data);
                    if let Some(protected) = signalling.dave.protect(&rewritten) {
                        media.send_frame(&protected, frame.timestamp).await?;
                        needs_local_keyframe = false;
                    }
                }
            },
            received = media.recv(&mut buffer) => {
                let length = received.map_err(|error| format!("Voice UDP receive failed: {error}"))?;
                if let Some(packet) = media.open_rtp(&buffer[..length]) {
                    if packet.payload_type == OPUS_PAYLOAD {
                        if !flags.deafened && let Some(&user_id) = signalling.audio.get(&packet.ssrc)
                            && let Some(opus) = signalling.dave.unprotect_audio(user_id, packet.payload)
                        {
                            let _ = events.try_send(CallMediaEvent::RemoteAudio(RemoteAudio { user_id, ssrc: packet.ssrc, sequence: packet.sequence, opus }));
                        }
                    } else if matches!(packet.payload_type, H264_PAYLOAD | RTX_PAYLOAD)
                        && let Some(remote) = signalling.videos.values().find(|remote| remote.ssrc == packet.ssrc || remote.rtx_ssrc == Some(packet.ssrc)).copied() {
                            // Bound active camera assembly independently of gateway metadata.
                            if viewers.len() < 8 || viewers.contains_key(&remote.user_id) {
                                let viewer = viewers.entry(remote.user_id).or_default();
                                viewer.receive(packet, remote.ssrc);
                                viewer.output(remote.user_id, remote.ssrc, &mut signalling, &mut media, &events).await;
                            }
                    }
                } else {
                    let feedback = media.feedback(&buffer[..length]);
                    if feedback.keyframe { needs_local_keyframe = true; let _ = events.try_send(CallMediaEvent::KeyframeRequested); }
                    if !feedback.lost.is_empty() { media.retransmit(&feedback.lost).await; }
                }
            },
            _ = &mut expiry => {
                for (&user_id, viewer) in &mut viewers {
                    if let Some(remote) = signalling.videos.get(&user_id).copied() {
                        viewer.output(user_id, remote.ssrc, &mut signalling, &mut media, &events).await;
                    }
                }
            },
        }
    }
    signalling.close().await;
    Ok(())
}

fn fresh(timestamp: Duration, now: Duration) -> bool {
    timestamp <= now + Duration::from_millis(50)
        && now.saturating_sub(timestamp) <= Duration::from_millis(200)
}

async fn resume(
    signalling: &mut Signalling,
    connection: &StreamConnection,
    events: &mpsc::Sender<CallMediaEvent>,
) -> Result<(), String> {
    let _ = events.try_send(CallMediaEvent::Reconnecting);
    let mut error = "Voice resume failed".to_owned();
    for delay in [250, 500, 1000] {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        match tokio::time::timeout(Duration::from_secs(5), signalling.resume(connection)).await {
            Ok(Ok(())) => {
                let _ = events.try_send(CallMediaEvent::Connected);
                return Ok(());
            }
            Ok(Err(reason)) => error = reason,
            Err(_) => error = "Voice resume timed out".into(),
        }
    }
    Err(error)
}

struct RemoteCamera {
    assembler: Assembler,
    needs_keyframe: bool,
    last_request: Option<Instant>,
    deadline: Option<Instant>,
}

impl Default for RemoteCamera {
    fn default() -> Self {
        Self {
            assembler: Assembler::default(),
            needs_keyframe: true,
            last_request: None,
            deadline: None,
        }
    }
}

impl RemoteCamera {
    fn receive(&mut self, packet: RtpPacket, video_ssrc: u32) {
        let (sequence, payload) = if packet.ssrc == video_ssrc {
            (packet.sequence, packet.payload)
        } else {
            let [high, low, tail @ ..] = packet.payload.as_slice() else {
                return;
            };
            (u16::from_be_bytes([*high, *low]), tail.to_vec())
        };
        self.assembler.insert(
            sequence,
            packet.timestamp,
            packet.marker,
            payload,
            Instant::now(),
        );
        self.deadline
            .get_or_insert(Instant::now() + Duration::from_millis(300));
    }

    async fn output(
        &mut self,
        user_id: u64,
        ssrc: u32,
        signalling: &mut Signalling,
        media: &mut Media,
        events: &mpsc::Sender<CallMediaEvent>,
    ) {
        let now = Instant::now();
        let assembled = self.assembler.assemble(now);
        self.deadline = self.assembler.deadline();
        if assembled.lost {
            self.needs_keyframe = true;
        }
        if !assembled.nack.is_empty() {
            media.send_nack(ssrc, &assembled.nack).await;
        }
        for frame in assembled.frames {
            if self.needs_keyframe && !frame.keyframe {
                continue;
            }
            let Some(data) = signalling.dave.unprotect(user_id, frame.data) else {
                self.needs_keyframe = true;
                continue;
            };
            self.needs_keyframe = false;
            let event = CallMediaEvent::RemoteVideo {
                user_id,
                frame: EncodedFrame {
                    data,
                    keyframe: frame.keyframe,
                    timestamp: Duration::from_nanos(
                        u64::from(frame.timestamp) * 1_000_000_000 / 90_000,
                    ),
                },
            };
            if events.try_send(event).is_err() {
                self.needs_keyframe = true;
            }
        }
        if self.needs_keyframe
            && self
                .last_request
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(500))
        {
            self.last_request = Some(now);
            media.send_picture_loss(ssrc).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pausing_activity_invalidates_queued_opus_before_a_new_speech_burst() {
        let quiet = CallMediaFlags {
            microphone: true,
            ..CallMediaFlags::default()
        };
        let active = CallMediaFlags {
            speaking: true,
            ..quiet
        };
        let fence = AudioFence::default();
        fence.set_open(active.sending());
        let captured = fence.generation();
        assert!(fence.accepts(captured));
        fence.set_open(quiet.sending());
        assert!(!fence.accepts(captured));
        fence.set_open(active.sending());
        assert!(!fence.accepts(captured));
        assert!(fence.accepts(fence.generation()));
    }

    #[test]
    fn stale_or_future_media_cannot_replay_after_reconnection() {
        assert!(fresh(Duration::from_millis(980), Duration::from_secs(1)));
        assert!(!fresh(Duration::from_millis(400), Duration::from_secs(1)));
        assert!(!fresh(Duration::from_secs(2), Duration::from_secs(1)));
    }

    #[test]
    fn video_queue_backpressure_returns_the_original_frame() {
        let (tx, mut rx) = mpsc::channel(2);
        for n in 0..2 {
            tx.try_send(n).unwrap();
        }
        assert_eq!(tx.try_send(2).unwrap_err().into_inner(), 2);
        drop(tx);
        assert_eq!(rx.try_recv().unwrap(), 0);
        assert_eq!(rx.try_recv().unwrap(), 1);
        assert!(rx.try_recv().is_err());
    }
}
