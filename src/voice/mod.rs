//! Voice calls: the connection to Discord's voice servers and the audio that
//! travels over it.
//!
//! [`stream::CallMedia`] owns the primary websocket, UDP transport and DAVE
//! session. [`audio`] owns devices, processing, Opus coding and playback.
//!
//! Everything here runs on the shared Tokio runtime. [`VoiceEngine`] is the
//! only handle the rest of the app holds: commands go down a channel, events
//! come back up another, so media and device types stay off gpui's thread.
//!
//! Camera uses the primary call connection; screen sharing uses its own
//! stream server and media connection in [`stream`].

mod audio;
pub mod cues;
pub mod peer_settings;
pub mod stream;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, UserMarker},
};

use crate::platform::runtime;

use audio::AudioIo;

pub use audio::processing::LICENSE as SONORA_LICENSE;
pub use audio::processing::Options as ProcessingOptions;
pub(crate) use audio::settings::AudioControl;
pub use audio::settings::{AudioPreferences, AudioSettings, InputMode};
pub use audio::{AudioDevice, input_devices, output_devices};

/// Everything needed to open a voice connection, gathered from the pair of
/// gateway dispatches that follow a join.
pub struct VoiceConnection {
    /// Identifies this local attempt, so late events cannot confirm a newer call.
    pub attempt: u64,
    pub channel_id: Id<ChannelMarker>,
    /// The guild the channel is in. A DM call has none, and Discord expects
    /// the channel's own id in its place.
    pub guild_id: Option<Id<GuildMarker>>,
    pub user_id: Id<UserMarker>,
    pub session_id: String,
    pub token: String,
    pub endpoint: String,
}

pub enum VoiceEvent {
    AudioSettingsChanged,
    Connected {
        attempt: u64,
    },
    /// The call ended on its own: the connection dropped, or the user was
    /// disconnected from the far end.
    Disconnected {
        attempt: u64,
    },
    /// Who is transmitting right now, sent whenever the set changes.
    Speaking(HashSet<Id<UserMarker>>),
    Failed {
        attempt: u64,
        error: String,
    },
}

/// Drives one call at a time.
///
/// Dropping the engine ends its task and tears down whatever call was live.
pub struct VoiceEngine {
    commands: UnboundedSender<Command>,
    settings: AudioSettings,
    transport: Arc<Mutex<Option<Arc<stream::CallMedia>>>>,
}

enum Command {
    Connect(Box<VoiceConnection>, u64),
    Disconnect,
    Gate,
    Processing,
    Shutdown,
    Deafen(bool),
    /// Switch to another microphone, by the id [`input_devices`] reported.
    InputDevice(Option<String>),
    /// Switch to another speaker, by the id [`output_devices`] reported.
    OutputDevice(Option<String>),
    Camera(Option<stream::VideoDescription>),
    CameraEvents(tokio::sync::mpsc::Sender<CameraVoiceEvent>),
    StreamAudio(tokio::sync::mpsc::Receiver<stream::RemoteAudio>),
}

pub enum CameraVoiceEvent {
    Frame {
        user_id: u64,
        frame: crate::platform::capture::EncodedFrame,
    },
    State {
        user_id: u64,
        active: bool,
    },
    KeyframeRequested,
    Ended,
}

impl VoiceEngine {
    /// Starts the engine. Events are delivered on `events` until the receiver
    /// is dropped.
    pub fn start(events: UnboundedSender<VoiceEvent>) -> Self {
        let (commands, rx) = unbounded();
        let settings = AudioSettings::with_events(
            crate::platform::prefs::load().audio,
            Some(events.clone()),
            Some(commands.clone()),
        );
        let transport = Arc::new(Mutex::new(None));
        runtime::handle().spawn(run(rx, events, settings.control.clone(), transport.clone()));
        Self {
            commands,
            settings,
            transport,
        }
    }

    pub fn connect(&self, connection: VoiceConnection) {
        let session = self.settings.begin_session();
        self.send(Command::Connect(Box::new(connection), session));
    }

    pub fn disconnect(&self) {
        if let Ok(transport) = self.transport.lock()
            && let Some(media) = transport.as_ref()
        {
            media.set_flags(stream::CallMediaFlags::default());
        }
        self.settings.control.begin_session();
        self.send(Command::Disconnect);
    }

    pub fn set_voice_flags(&self, muted: bool, deafened: bool, restricted: bool) {
        self.settings
            .control
            .set_voice_flags(muted, deafened, restricted);
        if (muted || deafened || restricted)
            && let Ok(transport) = self.transport.lock()
            && let Some(media) = transport.as_ref()
        {
            media.invalidate_audio();
        }
        self.send(Command::Deafen(deafened));
        self.send(Command::Gate);
    }

    /// Picks the microphone to send, taking effect immediately if a call is
    /// already up. `None` follows the system default.
    pub fn set_input_device(&self, device: Option<String>) {
        if let Ok(transport) = self.transport.lock()
            && let Some(media) = transport.as_ref()
        {
            media.invalidate_audio();
        }
        self.send(Command::InputDevice(device));
    }

    /// Picks the speaker to play through, taking effect immediately if a call
    /// is already up. `None` follows the system default.
    ///
    /// Like a microphone switch, this reopens the call's streams: playback is
    /// interrupted locally until they restart, and every sender's decoder
    /// starts from scratch — whatever it had buffered or already decoded is
    /// dropped, so someone mid-sentence is picked up again from their next
    /// packet. Mute, deafen and the connection itself are untouched.
    pub fn set_output_device(&self, device: Option<String>) {
        self.send(Command::OutputDevice(device));
    }

    fn send(&self, command: Command) {
        let _ = self.commands.unbounded_send(command);
    }

    pub fn audio_settings(&self) -> AudioSettings {
        self.settings.clone()
    }

    pub fn set_server_restricted(&self, restricted: bool) {
        self.settings.control.set_server_restricted(restricted);
        self.send(Command::Gate);
    }

    pub fn stream_audio(&self) -> tokio::sync::mpsc::Sender<stream::RemoteAudio> {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        self.send(Command::StreamAudio(rx));
        tx
    }

    pub fn set_camera_events(&self, events: tokio::sync::mpsc::Sender<CameraVoiceEvent>) {
        self.send(Command::CameraEvents(events));
    }
    pub fn set_camera(&self, description: Option<stream::VideoDescription>) {
        self.send(Command::Camera(description));
    }
    pub fn camera_clock(&self) -> Option<crate::platform::capture::clock::MediaClock> {
        self.transport
            .lock()
            .ok()?
            .as_ref()
            .map(|media| media.clock())
    }
    pub fn send_camera_frame(
        &self,
        frame: crate::platform::capture::EncodedFrame,
    ) -> Result<(), crate::platform::capture::EncodedFrame> {
        match self.transport.lock() {
            Ok(transport) => match transport.as_ref() {
                Some(media) => media.send_frame(frame),
                None => Err(frame),
            },
            Err(_) => Err(frame),
        }
    }
    pub fn request_video_keyframe(&self, user_id: u64) {
        if let Ok(transport) = self.transport.lock()
            && let Some(media) = transport.as_ref()
        {
            media.request_keyframe(user_id);
        }
    }
}

impl Drop for VoiceEngine {
    fn drop(&mut self) {
        self.settings.stop();
        self.send(Command::Shutdown);
    }
}

async fn run(
    mut commands: UnboundedReceiver<Command>,
    events: UnboundedSender<VoiceEvent>,
    control: Arc<audio::settings::AudioControl>,
    transport: Arc<Mutex<Option<Arc<stream::CallMedia>>>>,
) {
    use futures::StreamExt as _;

    let mut call: Option<NativeCall> = None;
    let mut deafened = false;
    let mut input_device: Option<String> = None;
    let mut output_device: Option<String> = None;

    let mut camera_events: Option<tokio::sync::mpsc::Sender<CameraVoiceEvent>> = None;
    let mut screen_audio: Option<tokio::sync::mpsc::Receiver<stream::RemoteAudio>> = None;
    loop {
        enum Next {
            Command(Option<Command>),
            Media(Option<stream::CallMediaEvent>),
            Mic(Option<audio::transmit::TransmitEvent>),
            Screen(Option<stream::RemoteAudio>),
        }
        let next = tokio::select! {
            biased;
            command = commands.next() => Next::Command(command),
            event = async { match call.as_mut() {
                Some(call) => tokio::select! {
                    event = call.events.recv() => Next::Media(event),
                    event = call.mic_events.recv() => Next::Mic(event),
                },
                None => std::future::pending().await,
            } } => event,
            packet = async { match screen_audio.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => Next::Screen(packet),
        };
        let command = match next {
            Next::Command(Some(command)) => command,
            Next::Command(None) => break,
            Next::Screen(packet) => {
                if let Some(call) = &mut call {
                    if let Some(packet) = packet {
                        call.receive(packet, true);
                    } else {
                        call.forget_screen();
                        screen_audio = None;
                    }
                } else {
                    screen_audio = None;
                }
                continue;
            }
            Next::Mic(event) => {
                let ended = matches!(
                    event,
                    None | Some(audio::transmit::TransmitEvent::Failed(_))
                );
                if let Some(call) = &mut call {
                    match event {
                        Some(audio::transmit::TransmitEvent::Speaking(speaking)) => {
                            call.update_transmission(deafened);
                            report_speaking(&mut call.speaking, call.user_id, speaking, &events);
                        }
                        Some(audio::transmit::TransmitEvent::Failed(error)) => {
                            let _ = events.unbounded_send(VoiceEvent::Failed {
                                attempt: call.attempt,
                                error,
                            });
                            if let Ok(mut current) = transport.lock() {
                                *current = None;
                            }
                            call.media.set_flags(stream::CallMediaFlags::default());
                            call.audio.stop();
                        }
                        None => {}
                    }
                }
                if ended {
                    if let Ok(mut current) = transport.lock() {
                        *current = None;
                    }
                    call = None;
                    screen_audio = None;
                    if let Some(tx) = &camera_events {
                        let _ = tx.try_send(CameraVoiceEvent::Ended);
                    }
                }
                continue;
            }
            Next::Media(event) => {
                let Some(current) = &mut call else {
                    continue;
                };
                let mut ended = false;
                match event {
                    Some(stream::CallMediaEvent::Connected) => {
                        control.activate_session(current.session);
                        current.update_transmission(deafened);
                        let _ = events.unbounded_send(VoiceEvent::Connected {
                            attempt: current.attempt,
                        });
                    }
                    Some(stream::CallMediaEvent::RemoteAudio(packet)) => {
                        current.receive(packet, false)
                    }
                    Some(stream::CallMediaEvent::Speaking { user_id, speaking }) => {
                        report_speaking(&mut current.speaking, user_id, speaking, &events)
                    }
                    Some(stream::CallMediaEvent::UserLeft { user_id }) => {
                        report_speaking(&mut current.speaking, user_id, false, &events);
                        current.forget_user(user_id);
                        if let Some(tx) = &camera_events {
                            let _ = tx.try_send(CameraVoiceEvent::State {
                                user_id,
                                active: false,
                            });
                        }
                    }
                    Some(stream::CallMediaEvent::RemoteVideo { user_id, frame }) => {
                        if current.video_dropped.contains(&user_id) && !frame.keyframe {
                            continue;
                        }
                        if let Some(tx) = &camera_events {
                            if tx
                                .try_send(CameraVoiceEvent::Frame { user_id, frame })
                                .is_err()
                            {
                                if current.video_dropped.insert(user_id) {
                                    current.media.request_keyframe(user_id);
                                }
                            } else {
                                current.video_dropped.remove(&user_id);
                            }
                        }
                    }
                    Some(stream::CallMediaEvent::VideoState { user_id, active }) => {
                        if let Some(tx) = &camera_events {
                            let _ = tx.try_send(CameraVoiceEvent::State { user_id, active });
                        }
                    }
                    Some(stream::CallMediaEvent::KeyframeRequested) => {
                        if let Some(tx) = &camera_events {
                            let _ = tx.try_send(CameraVoiceEvent::KeyframeRequested);
                        }
                    }
                    Some(stream::CallMediaEvent::Reconnecting) => {
                        current.transmitter.set_open(false);
                        current.media.invalidate_audio();
                        current.audio.forget_all();
                    }
                    Some(stream::CallMediaEvent::Disconnected { error }) => {
                        if let Some(error) = error {
                            let _ = events.unbounded_send(VoiceEvent::Failed {
                                attempt: current.attempt,
                                error,
                            });
                        } else {
                            let _ = events.unbounded_send(VoiceEvent::Disconnected {
                                attempt: current.attempt,
                            });
                        }
                        ended = true;
                    }
                    None => {
                        ended = true;
                    }
                }
                if ended {
                    if let Ok(mut current) = transport.lock() {
                        *current = None;
                    }
                    call = None;
                    screen_audio = None;
                    if let Some(tx) = &camera_events {
                        let _ = tx.try_send(CameraVoiceEvent::Ended);
                    }
                }
                continue;
            }
        };
        match command {
            Command::Connect(connection, session) => {
                // Only one call at a time: leaving the old one first stops its
                // devices before the new one opens them.
                drop(call.take());
                screen_audio = None;
                let new = NativeCall::start(
                    *connection,
                    control.clone(),
                    session,
                    input_device.as_deref(),
                    output_device.as_deref(),
                    deafened,
                );
                if let Ok(mut current) = transport.lock() {
                    *current = Some(new.media.clone());
                }
                call = Some(new);
            }
            Command::Disconnect => {
                if let Ok(mut current) = transport.lock() {
                    *current = None;
                }
                call = None;
                screen_audio = None;
                if let Some(tx) = &camera_events {
                    let _ = tx.try_send(CameraVoiceEvent::Ended);
                }
            }
            Command::Gate => {
                if let Some(call) = &mut call {
                    call.update_transmission(deafened);
                }
            }
            Command::Processing => {
                if let Some(call) = &mut call {
                    call.media.invalidate_audio();
                    call.audio.configure_processing();
                    call.update_transmission(deafened);
                }
            }
            Command::Shutdown => break,
            Command::Deafen(value) => {
                deafened = value;
                if let Some(call) = &mut call {
                    call.audio.set_playback_deafened(value);
                    call.update_transmission(deafened);
                }
            }
            Command::InputDevice(device) => {
                // The same device again — the picker re-sent what's already
                // live — is nothing to reopen the streams for.
                if input_device != device {
                    input_device = device;
                    if let Some(call) = &mut call {
                        call.swap_devices(
                            input_device.as_deref(),
                            output_device.as_deref(),
                            deafened,
                        );
                    }
                }
            }
            Command::OutputDevice(device) => {
                if output_device != device {
                    output_device = device;
                    if let Some(call) = &mut call {
                        call.swap_devices(
                            input_device.as_deref(),
                            output_device.as_deref(),
                            deafened,
                        );
                    }
                }
            }
            Command::Camera(video) => {
                if let Some(call) = &mut call {
                    call.video = video;
                    call.update_transmission(deafened);
                }
            }
            Command::CameraEvents(events) => camera_events = Some(events),
            Command::StreamAudio(receiver) => {
                if let Some(call) = &mut call {
                    call.forget_screen();
                }
                screen_audio = Some(receiver);
            }
        }
    }
    if let Ok(mut current) = transport.lock() {
        *current = None;
    }
}

struct NativeCall {
    attempt: u64,
    session: u64,
    user_id: u64,
    audio: Arc<AudioIo>,
    media: Arc<stream::CallMedia>,
    events: tokio::sync::mpsc::Receiver<stream::CallMediaEvent>,
    mic_events: tokio::sync::mpsc::Receiver<audio::transmit::TransmitEvent>,
    transmitter: audio::transmit::Transmitter,
    video: Option<stream::VideoDescription>,
    video_dropped: HashSet<u64>,
    streams: HashMap<(bool, u32), (u32, u64)>,
    next_stream: u32,
    speaking: HashSet<Id<UserMarker>>,
}

impl NativeCall {
    fn start(
        connection: VoiceConnection,
        control: Arc<AudioControl>,
        session: u64,
        input: Option<&str>,
        output: Option<&str>,
        deafened: bool,
    ) -> Self {
        let attempt = connection.attempt;
        let user_id = connection.user_id.get();
        let audio = Arc::new(AudioIo::start(input, output, control, session));
        audio.set_playback_deafened(deafened);
        let (tx, events) = tokio::sync::mpsc::channel(64);
        let media = Arc::new(stream::CallMedia::start(connection, tx));
        let captured = audio.clone();
        media.set_audio_validator(Arc::new(move |epoch| captured.accepts_capture_epoch(epoch)));
        let (tx, mic_events) = tokio::sync::mpsc::channel(4);
        let transmitter = audio::transmit::Transmitter::start(audio.clone(), media.clone(), tx);
        Self {
            attempt,
            session,
            user_id,
            audio,
            media,
            events,
            mic_events,
            transmitter,
            video: None,
            video_dropped: HashSet::new(),
            streams: HashMap::new(),
            next_stream: 1,
            speaking: HashSet::new(),
        }
    }

    fn update_transmission(&mut self, deafened: bool) {
        let open = self.audio.can_transmit();
        self.media.set_flags(stream::CallMediaFlags {
            microphone: open,
            speaking: self.audio.is_speaking(),
            deafened,
            video: self.video,
        });
        self.transmitter.set_open(open && !deafened);
    }

    fn swap_devices(&mut self, input: Option<&str>, output: Option<&str>, deafened: bool) {
        self.media.invalidate_audio();
        self.transmitter.set_open(false);
        self.audio.capture.control.invalidate_capture();
        self.audio.stop();
        self.audio = Arc::new(AudioIo::start(
            input,
            output,
            self.audio.capture.control.clone(),
            self.session,
        ));
        self.audio.set_playback_deafened(deafened);
        let captured = self.audio.clone();
        self.media
            .set_audio_validator(Arc::new(move |epoch| captured.accepts_capture_epoch(epoch)));
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        self.transmitter =
            audio::transmit::Transmitter::start(self.audio.clone(), self.media.clone(), tx);
        self.mic_events = rx;
        self.streams.clear();
        self.update_transmission(deafened);
    }

    fn receive(&mut self, packet: stream::RemoteAudio, screen: bool) {
        let key = (screen, packet.ssrc);
        if self
            .streams
            .get(&key)
            .is_some_and(|(_, user)| *user != packet.user_id)
            && let Some((id, _)) = self.streams.remove(&key)
        {
            self.audio.forget(id);
        }
        if !self.streams.contains_key(&key) {
            if self.streams.len() >= 512 {
                return;
            }
            self.next_stream = self.next_stream.wrapping_add(1).max(1);
            self.streams.insert(key, (self.next_stream, packet.user_id));
        }
        if let Some(&(id, user)) = self.streams.get(&key) {
            self.audio
                .receive_from(user, id, packet.sequence, &packet.opus);
        }
    }

    fn forget_screen(&mut self) {
        self.streams.retain(|&(screen, _), value| {
            let (id, _) = *value;
            if screen {
                self.audio.forget(id);
            }
            !screen
        });
    }

    fn forget_user(&mut self, user_id: u64) {
        self.streams.retain(|_, value| {
            let (id, user) = *value;
            if user == user_id {
                self.audio.forget(id);
            }
            user != user_id
        });
    }
}

fn report_speaking(
    users: &mut HashSet<Id<UserMarker>>,
    user_id: u64,
    speaking: bool,
    events: &UnboundedSender<VoiceEvent>,
) {
    let Some(id) = Id::new_checked(user_id) else {
        return;
    };
    let changed = if speaking {
        users.insert(id)
    } else {
        users.remove(&id)
    };
    if changed {
        let _ = events.unbounded_send(VoiceEvent::Speaking(users.clone()));
    }
}

impl Drop for NativeCall {
    fn drop(&mut self) {
        self.transmitter.set_open(false);
        self.media.set_flags(stream::CallMediaFlags::default());
        self.audio.end_call();
    }
}
