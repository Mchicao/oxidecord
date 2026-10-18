use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::*;
use image::{Frame, ImageBuffer};
use tokio::sync::mpsc;

use super::HomeScreen;
use super::voice::VoiceStatus;
use crate::platform::camera::{self, CameraCapture, CameraDevice};
use crate::platform::capture::{CaptureEvent, CaptureSettings, EncodedFrame};
use crate::platform::h264::{self, DecodeStatus, DecoderEvent, DecoderInput, LiveDecoder};
use crate::voice::{CameraVoiceEvent, stream::VideoDescription};

const SETTINGS: CaptureSettings = CaptureSettings {
    max_width: 1280,
    max_height: 720,
    fps: 30,
    bitrate: 1_500_000,
};

pub(super) struct CameraState {
    pub devices: Vec<CameraDevice>,
    pub selected: Option<String>,
    pub enumerating: bool,
    pub error: Option<String>,
    pub events: mpsc::Sender<CameraVoiceEvent>,
    pub active: bool,
    capture: Option<CameraCapture>,
    local: Option<Picture>,
    remote: HashMap<u64, Picture>,
    retired: Vec<Arc<RenderImage>>,
    next_epoch: u64,
}

struct Picture {
    epoch: u64,
    decoder: LiveDecoder,
    input: DecoderInput,
    image: Option<Arc<RenderImage>>,
    needs_keyframe: bool,
    last_request: Option<Instant>,
}

impl CameraState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<HomeScreen>) -> Self {
        let (events, mut received) = mpsc::channel(4);
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = received.recv().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        this.handle_camera_voice(event, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            devices: Vec::new(),
            selected: None,
            enumerating: false,
            error: None,
            events,
            active: false,
            capture: None,
            local: None,
            remote: HashMap::new(),
            retired: Vec::new(),
            next_epoch: 0,
        }
    }
}

impl HomeScreen {
    pub(in crate::screens::home) fn attach_camera_engine(&self) {
        if let Some(engine) = &self.voice_engine {
            engine.set_camera_events(self.camera.events.clone());
        }
    }

    /// Enumeration is triggered by the device picker, never by startup/render.
    pub(in crate::screens::home) fn refresh_cameras(&mut self, cx: &mut Context<Self>) {
        if self.camera.enumerating {
            return;
        }
        self.camera.enumerating = true;
        self.camera.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::platform::runtime::run(async {
                match tokio::task::spawn_blocking(camera::devices).await {
                    Ok(result) => result,
                    Err(error) => Err(error.to_string()),
                }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.camera.enumerating = false;
                match result {
                    Ok(Ok(devices)) => {
                        if !devices
                            .iter()
                            .any(|device| Some(&device.id) == this.camera.selected.as_ref())
                        {
                            this.camera.selected = devices.first().map(|device| device.id.clone());
                        }
                        this.camera.devices = devices;
                    }
                    Ok(Err(error)) => this.camera.error = Some(error),
                    Err(error) => {
                        this.camera.error = Some(format!("Camera enumeration failed: {error}"))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::screens::home) fn select_camera(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.camera.devices.iter().any(|device| device.id == id)
            || self.camera.selected.as_ref() == Some(&id)
        {
            return;
        }
        let was_running = self.camera.capture.is_some();
        let publishing = self.camera.active;
        self.stop_camera(cx);
        self.camera.selected = Some(id);
        if was_running {
            self.open_camera(publishing, window, cx);
        }
        cx.notify();
    }

    pub(in crate::screens::home) fn can_use_camera(&self) -> bool {
        cfg!(windows)
            && self
                .voice
                .as_ref()
                .is_some_and(|call| call.status == VoiceStatus::Connected)
    }

    pub(in crate::screens::home) fn toggle_camera(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.camera.capture.is_some() {
            self.stop_camera(cx);
        } else if self.can_use_camera() {
            self.open_camera(true, window, cx);
        }
    }

    pub(in crate::screens::home) fn preview_camera(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.camera.capture.is_some() {
            self.stop_camera(cx);
        } else {
            self.open_camera(false, window, cx);
        }
    }

    fn open_camera(&mut self, publish: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(device) = self.camera.selected.clone() else {
            self.camera.error = Some("Choose a camera before starting video".into());
            self.refresh_cameras(cx);
            return;
        };
        if publish && !self.can_use_camera() {
            return;
        }
        self.attach_camera_engine();
        let clock = self
            .voice_engine
            .as_ref()
            .and_then(|engine| engine.camera_clock())
            .unwrap_or_default();
        let (events, mut received) = mpsc::channel(2);
        let capture = match CameraCapture::start_with_clock(device, SETTINGS, events, clock) {
            Ok(capture) => capture,
            Err(error) => {
                self.camera.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.camera.next_epoch = self.camera.next_epoch.wrapping_add(1);
        let epoch = self.camera.next_epoch;
        self.camera.local = Some(self.camera_picture(None, epoch, window, cx));
        self.camera.capture = Some(capture);
        self.camera.active = publish;
        self.camera.error = None;
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = received.recv().await {
                let handled = this.update_in(cx, |this, window, cx| {
                    if this
                        .camera
                        .local
                        .as_ref()
                        .is_none_or(|picture| picture.epoch != epoch)
                    {
                        return false;
                    }
                    match event {
                        CaptureEvent::Started { width, height } => {
                            if this.camera.active {
                                if let Some(engine) = &this.voice_engine {
                                    engine.set_camera(Some(VideoDescription {
                                        width,
                                        height,
                                        fps: SETTINGS.fps,
                                        bitrate: SETTINGS.bitrate,
                                    }));
                                }
                                this.announce_camera(true);
                            }
                        }
                        CaptureEvent::Frame(frame) => {
                            let preview = EncodedFrame {
                                data: frame.data.clone(),
                                keyframe: frame.keyframe,
                                timestamp: frame.timestamp,
                            };
                            this.decode_camera(None, preview);
                            if this.camera.active
                                && let Some(engine) = &this.voice_engine
                                && engine.send_camera_frame(frame).is_err()
                                && let Some(capture) = &this.camera.capture
                            {
                                capture.request_keyframe();
                            }
                        }
                        CaptureEvent::Failed(error) => {
                            this.stop_camera(cx);
                            this.camera.error = Some(error);
                        }
                        CaptureEvent::SourceClosed => this.stop_camera(cx),
                    }
                    this.release_camera_frames(window);
                    cx.notify();
                    true
                });
                if !matches!(handled, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn announce_camera(&self, active: bool) {
        if let (Some(call), Some(gateway)) = (&self.voice, &self.gateway) {
            gateway.update_voice_state_with_video(
                call.guild_id,
                Some(call.channel_id),
                self.voice_muted,
                self.voice_deafened,
                active,
            );
        }
    }

    pub(in crate::screens::home) fn stop_camera(&mut self, cx: &mut Context<Self>) {
        self.camera.capture = None;
        if let Some(picture) = self.camera.local.take()
            && let Some(image) = picture.image
        {
            self.camera.retired.push(image);
        }
        if self.camera.active {
            if let Some(engine) = &self.voice_engine {
                engine.set_camera(None);
            }
            self.announce_camera(false);
        }
        self.camera.active = false;
        cx.notify();
    }

    pub(in crate::screens::home) fn end_camera_call(&mut self, cx: &mut Context<Self>) {
        self.stop_camera(cx);
        for (_, picture) in self.camera.remote.drain() {
            if let Some(image) = picture.image {
                self.camera.retired.push(image);
            }
        }
    }

    pub(in crate::screens::home) fn camera_frame(&self, user_id: u64) -> Option<Arc<RenderImage>> {
        let picture = if self.self_user_id.is_some_and(|id| id.get() == user_id) {
            self.camera.local.as_ref()
        } else {
            self.camera.remote.get(&user_id)
        };
        picture.and_then(|picture| picture.image.clone())
    }

    pub(in crate::screens::home) fn release_camera_frames(&mut self, window: &mut Window) {
        for image in self.camera.retired.drain(..) {
            let _ = window.drop_image(image);
        }
    }

    fn camera_picture(
        &mut self,
        user_id: Option<u64>,
        epoch: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Picture {
        let (events, mut received) = futures::channel::mpsc::unbounded();
        let (decoder, input) = h264::start((640, 360), events);
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = received.next().await {
                let handled = this.update_in(cx, |this, window, cx| {
                    let picture = match user_id {
                        Some(user) => this.camera.remote.get_mut(&user),
                        None => this.camera.local.as_mut(),
                    };
                    let Some(picture) = picture.filter(|picture| picture.epoch == epoch) else {
                        return false;
                    };
                    match event {
                        DecoderEvent::Frame(frame) => {
                            picture.decoder.frame_consumed();
                            if let Some(pixels) =
                                ImageBuffer::from_raw(frame.width, frame.height, frame.bgra)
                            {
                                let image = Arc::new(RenderImage::new(vec![Frame::new(pixels)]));
                                if let Some(previous) = picture.image.replace(image) {
                                    let _ = window.drop_image(previous);
                                }
                            }
                        }
                        DecoderEvent::Failed(error) => {
                            this.camera.error = Some(error);
                            if user_id.is_none() {
                                this.stop_camera(cx);
                            } else if let Some(user) = user_id {
                                this.remove_remote_camera(user);
                            }
                        }
                    }
                    this.release_camera_frames(window);
                    cx.notify();
                    true
                });
                if !matches!(handled, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
        Picture {
            epoch,
            decoder,
            input,
            image: None,
            needs_keyframe: true,
            last_request: None,
        }
    }

    fn handle_camera_voice(
        &mut self,
        event: CameraVoiceEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            CameraVoiceEvent::Frame { user_id, frame } => {
                if !self.camera.remote.contains_key(&user_id) {
                    if self.camera.remote.len() >= 8 {
                        return;
                    }
                    self.camera.next_epoch = self.camera.next_epoch.wrapping_add(1);
                    let picture =
                        self.camera_picture(Some(user_id), self.camera.next_epoch, window, cx);
                    self.camera.remote.insert(user_id, picture);
                }
                self.decode_camera(Some(user_id), frame);
            }
            CameraVoiceEvent::State { user_id, active } => {
                if !active {
                    self.remove_remote_camera(user_id);
                }
            }
            CameraVoiceEvent::KeyframeRequested => {
                if let Some(capture) = &self.camera.capture {
                    capture.request_keyframe();
                }
            }
            CameraVoiceEvent::Ended => self.end_camera_call(cx),
        }
        self.release_camera_frames(window);
        cx.notify();
    }

    fn remove_remote_camera(&mut self, user_id: u64) {
        if let Some(picture) = self.camera.remote.remove(&user_id)
            && let Some(image) = picture.image
        {
            self.camera.retired.push(image);
        }
    }

    fn decode_camera(&mut self, user_id: Option<u64>, frame: EncodedFrame) {
        let picture = match user_id {
            Some(user) => self.camera.remote.get_mut(&user),
            None => self.camera.local.as_mut(),
        };
        let Some(picture) = picture else {
            return;
        };
        if picture.needs_keyframe && !frame.keyframe {
            return;
        }
        match picture.input.try_decode(frame.data) {
            DecodeStatus::Queued => picture.needs_keyframe = false,
            DecodeStatus::Busy | DecodeStatus::Closed => {
                picture.needs_keyframe = true;
                if picture
                    .last_request
                    .is_none_or(|last| last.elapsed() >= Duration::from_millis(500))
                {
                    picture.last_request = Some(Instant::now());
                    match user_id {
                        Some(user) => {
                            if let Some(engine) = &self.voice_engine {
                                engine.request_video_keyframe(user);
                            }
                        }
                        None => {
                            if let Some(capture) = &self.camera.capture {
                                capture.request_keyframe();
                            }
                        }
                    }
                }
            }
        }
    }
}
