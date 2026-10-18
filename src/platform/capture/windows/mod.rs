//! Windows.Graphics.Capture into Media Foundation's H.264 encoder.
//!
//! Frames stay on the GPU from capture to encoder where the machine allows
//! it: the captured BGRA texture is copied into one of ours, the Direct3D
//! video processor scales and converts it to NV12 in a single pass, and a
//! hardware encoder takes the NV12 texture as it is. Only the software
//! encoder, the fallback for a machine without one, reads the frame back to
//! system memory.
//!
//! Frame notifications and stop requests wake the worker; a static source
//! does not keep encoding or waking a timer.

mod convert;
mod encode;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::Sender;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCapturePicker,
    GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::UI::Shell::IInitializeWithWindow;
use windows::core::Interface as _;

use super::{CaptureEvent, CaptureSettings, Control};
use convert::Converter;
use encode::Encoder;

/// A window or display the user picked.
pub struct CaptureSource {
    item: GraphicsCaptureItem,
}

impl CaptureSource {
    /// What the system calls the source — the window title or display name.
    pub fn name(&self) -> String {
        self.item
            .DisplayName()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default()
    }
}

pub(super) fn is_supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

pub(super) fn pick(
    hwnd: isize,
) -> Result<impl Future<Output = Option<CaptureSource>> + use<>, String> {
    let picker = GraphicsCapturePicker::new().context("Couldn't open the capture picker")?;
    // A desktop app has no CoreWindow for the picker to attach to, so it's
    // told which window to be modal over instead.
    let init: IInitializeWithWindow = picker.cast().context("Couldn't open the capture picker")?;
    unsafe { init.Initialize(HWND(hwnd as *mut _)) }.context("Couldn't open the capture picker")?;
    let pick = picker
        .PickSingleItemAsync()
        .context("Couldn't open the capture picker")?;

    Ok(async move {
        // Kept alive until the pick resolves.
        let _picker = picker;
        // A cancelled pick resolves to a null item, which surfaces as an
        // error; either way there's nothing to share.
        pick.await.ok().map(|item| CaptureSource { item })
    })
}

pub(super) fn capture(
    source: CaptureSource,
    settings: CaptureSettings,
    control: &Control,
    events: &Sender<CaptureEvent>,
    awakened: std::sync::mpsc::Receiver<()>,
    clock: super::clock::MediaClock,
) -> Result<(), String> {
    let _platform = MediaFoundation::startup()?;

    let (device, context) = create_device()?;
    let rt_device = winrt_device(&device)?;

    let item = source.item;
    let mut pool_size = item.Size().context("Couldn't read the source's size")?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &rt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        pool_size,
    )
    .context("Couldn't start capturing")?;
    let session = pool
        .CreateCaptureSession(&item)
        .context("Couldn't start capturing")?;
    let _capture_session = ActiveCapture {
        session: session.clone(),
        pool: pool.clone(),
    };
    // Both are newer than capture itself; on a build without them, the yellow
    // border stays and the cursor is captured anyway.
    let _ = session.SetIsBorderRequired(false);
    let _ = session.SetIsCursorCaptureEnabled(true);

    let closed = Arc::new(AtomicBool::new(false));
    let closed_flag = closed.clone();
    let closed_wake = control.wake.clone();
    item.Closed(&TypedEventHandler::new(move |_, _| {
        closed_flag.store(true, Ordering::Release);
        let _ = closed_wake.try_send(());
        Ok(())
    }))
    .context("Couldn't start capturing")?;
    let frame_wake = control.wake.clone();
    pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
        let _ = frame_wake.try_send(());
        Ok(())
    }))
    .context("Couldn't subscribe to captured frames")?;
    session.StartCapture().context("Couldn't start capturing")?;

    let out_size = output_size(pool_size, &settings);
    let mut converter = Converter::new(&device, &context, out_size, settings.fps)?;
    let mut encoder = Encoder::new(&device, &context, out_size, &settings)?;

    if events
        .blocking_send(CaptureEvent::Started {
            width: out_size.0,
            height: out_size.1,
        })
        .is_err()
    {
        return Ok(());
    }

    let interval = Duration::from_secs(1) / settings.fps;
    let mut next_tick = Instant::now();
    let mut have_frame = false;
    let mut taken = Duration::ZERO;

    loop {
        if awakened.recv().is_err() {
            break;
        }
        if control.is_stopped() || closed.load(Ordering::Acquire) {
            break;
        }

        // Several frames may have queued since the last tick; only the newest
        // is worth sending.
        let mut newest: Option<Direct3D11CaptureFrame> = None;
        while let Ok(frame) = pool.TryGetNextFrame() {
            newest = Some(frame);
        }
        let refreshed = newest.is_some();
        if let Some(frame) = newest {
            taken = frame
                .SystemRelativeTime()
                .ok()
                .map(|time| clock.captured_at(time.Duration.max(0) as u64, qpc_hns()))
                .unwrap_or_else(|| clock.elapsed());
            let size = frame
                .ContentSize()
                .context("Couldn't read a captured frame")?;
            converter.load(&frame, size)?;
            // A resized window keeps arriving in the old buffers, cropped or
            // padded, until the pool is rebuilt at the new size.
            if size.Width != pool_size.Width || size.Height != pool_size.Height {
                pool.Recreate(
                    &rt_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    size,
                )
                .context("Couldn't follow the source's new size")?;
                pool_size = size;
            }
            have_frame = true;
        }

        let keyframe = control.take_keyframe();
        if Instant::now() + Duration::from_millis(1) < next_tick && !keyframe {
            continue;
        }
        if keyframe && !refreshed {
            taken = clock.elapsed();
        }
        next_tick = Instant::now() + interval;

        if have_frame {
            let texture = converter.convert()?;
            // Stamped with when it was taken rather than counted, so a tick
            // the loop fell behind on doesn't put the stream's clock out.
            for frame in encoder.encode(&texture, taken, keyframe, next_tick)? {
                match events.try_send(CaptureEvent::Frame(frame)) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        control.keyframe.store(true, Ordering::Release);
                    }
                }
            }
        }
    }

    Ok(())
}

struct ActiveCapture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
}
impl Drop for ActiveCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

pub(crate) fn qpc_hns() -> u64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut ticks, mut frequency) = (0, 0);
    unsafe {
        let _ = QueryPerformanceCounter(&mut ticks);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    if frequency <= 0 {
        return 0;
    }
    (ticks.max(0) as u128 * 10_000_000 / frequency as u128) as u64
}

/// The stream's frame size: the source fitted inside the configured maximum,
/// rounded down to even dimensions, which NV12's half-size chroma needs.
fn output_size(source: SizeInt32, settings: &CaptureSettings) -> (u32, u32) {
    let width = source.Width.max(2) as f64;
    let height = source.Height.max(2) as f64;
    let scale = (f64::from(settings.max_width) / width)
        .min(f64::from(settings.max_height) / height)
        .min(1.);
    let even = |value: f64| ((value * scale) as u32 & !1).max(2);
    (even(width), even(height))
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("Couldn't create a Direct3D device")?;
    let (Some(device), Some(context)) = (device, context) else {
        return Err("Couldn't create a Direct3D device".into());
    };

    // The encoder's own threads use the device alongside this one.
    if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
    }
    Ok((device, context))
}

/// The same device, as the WinRT capture API takes it.
fn winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice, String> {
    let dxgi: IDXGIDevice = device
        .cast()
        .context("Couldn't share the Direct3D device")?;
    unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .and_then(|inspectable| inspectable.cast())
        .context("Couldn't share the Direct3D device")
}

fn texture(
    device: &ID3D11Device,
    size: (u32, u32),
    format: DXGI_FORMAT,
    bind: i32,
    usage: D3D11_USAGE,
    cpu_access: i32,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind as u32,
        CPUAccessFlags: cpu_access as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("Couldn't allocate a video frame")?;
    texture.ok_or_else(|| "Couldn't allocate a video frame".into())
}

/// COM and Media Foundation, for the life of the capture thread.
pub(crate) struct MediaFoundation {
    /// Whether this thread's COM apartment is ours to close.
    owns_com: bool,
}

impl MediaFoundation {
    pub(crate) fn startup() -> Result<Self, String> {
        let owns_com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        match unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            Ok(()) => Ok(Self { owns_com }),
            Err(err) => {
                if owns_com {
                    unsafe { CoUninitialize() };
                }
                Err(format!("Couldn't start Media Foundation: {err}"))
            }
        }
    }
}

/// Camera frames use the same GPU device and hardware encoder as screen frames.
pub(crate) struct Nv12Encoder {
    encoder: Encoder,
    device_context: ID3D11DeviceContext,
    input: Vec<ID3D11Texture2D>,
    next: usize,
    size: (u32, u32),
}

impl Nv12Encoder {
    pub(crate) fn new(size: (u32, u32), settings: &CaptureSettings) -> Result<Self, String> {
        let (device, context) = create_device()?;
        let encoder = Encoder::new(&device, &context, size, settings)?;
        let input = (0..4)
            .map(|_| texture(&device, size, DXGI_FORMAT_NV12, 0, D3D11_USAGE_DEFAULT, 0))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            encoder,
            device_context: context,
            input,
            next: 0,
            size,
        })
    }

    pub(crate) fn encode(
        &mut self,
        data: &[u8],
        timestamp: Duration,
        keyframe: bool,
        deadline: Instant,
    ) -> Result<Vec<super::EncodedFrame>, String> {
        let length = self.size.0 as usize * self.size.1 as usize * 3 / 2;
        if data.len() < length {
            return Err("Camera returned an incomplete NV12 frame".into());
        }
        let input = &self.input[self.next];
        self.next = (self.next + 1) % self.input.len();
        unsafe {
            self.device_context.UpdateSubresource(
                input,
                0,
                None,
                data.as_ptr().cast(),
                self.size.0,
                0,
            )
        };
        self.encoder.encode(input, timestamp, keyframe, deadline)
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            if self.owns_com {
                CoUninitialize();
            }
        }
    }
}

/// Attaches a human-readable cause to a Windows error.
trait Context<T> {
    fn context(self, what: &str) -> Result<T, String>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn context(self, what: &str) -> Result<T, String> {
        self.map_err(|err| format!("{what}: {}", err.message()))
    }
}
