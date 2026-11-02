//! Window capture over Windows.Graphics.Capture (WGC).
//!
//! Same contract as [`crate::dxgi`]: `Recorder::start` builds the pipeline on
//! the calling thread so creation failures report synchronously, then a
//! background thread paces `TryGetNextFrame` polls. WGC only emits a frame when
//! the window actually changes, so the loop polls instead of waiting on a
//! callback and resends the last frame on resume — the `first` flag that in
//! DXGI means "the acquire reports a stale picture" here means "nothing new
//! arrived, send what we have".

use std::sync::mpsc::{channel, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::core::{Interface, BOOL};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};

use crate::capture::{Frame, WindowInfo};
use crate::dxgi::{due_at, readback, Control, Staging};

/// How long the recorder idles while the session is paused or no new frame
/// exists — WGC produces nothing while the window is static.
const IDLE_SLEEP: Duration = Duration::from_millis(20);
const NO_FRAME_SLEEP: Duration = Duration::from_millis(2);

/// A live window capture: the frame pool, session and poll thread.
pub struct Recorder {
    join: Option<JoinHandle<bool>>,
}

impl Recorder {
    /// Creates the WGC pipeline for `hwnd` and starts the capture thread.
    ///
    /// All the fallible setup runs on the calling thread — like the DXGI
    /// recorder — so a dead handle or an unsupported runtime is a synchronous
    /// error the session can route through its normal recovery path.
    pub fn start(hwnd: isize, tx: SyncSender<Frame>, control: Control) -> Result<Self, String> {
        // WinRT objects are not `Send`, so all the fallible setup runs inside
        // the capture thread and reports back over this channel — `start`
        // still fails synchronously, like the DXGI recorder.
        let (init_tx, init_rx) = channel();
        let join = thread::Builder::new()
            .name("argos-wgc".to_string())
            .spawn(move || match setup(hwnd) {
                Ok((device, context, direct3d, pool, session)) => {
                    let _ = init_tx.send(Ok(()));
                    run(device, context, direct3d, pool, session, hwnd, tx, control)
                }
                Err(error) => {
                    let _ = init_tx.send(Err(error));
                    false
                }
            })
            .map_err(|error| error.to_string())?;
        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self { join: Some(join) }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("capture thread died during setup".to_string()),
        }
    }

    /// Same contract as [`crate::dxgi::Recorder::wait`]: `true` when the thread
    /// ended on its own (window gone, session broken), `false` for a clean stop.
    pub fn wait(&mut self) -> bool {
        self.join
            .take()
            .map(|join| join.join().unwrap_or(false))
            .unwrap_or(false)
    }
}

type Device = (ID3D11Device, ID3D11DeviceContext, IDirect3DDevice);

/// All the WinRT setup for a capture, on the thread that will poll it —
/// `IDirect3DDevice` and friends are apartment-bound.
type Pipeline = (
    ID3D11Device,
    ID3D11DeviceContext,
    IDirect3DDevice,
    Direct3D11CaptureFramePool,
    GraphicsCaptureSession,
);

fn setup(hwnd: isize) -> Result<Pipeline, String> {
    unsafe {
        // Already-initialized is fine; WinRT needs some apartment on every
        // thread that activates it.
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let (device, context, direct3d) = create_device()?;
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(|error| format!("IGraphicsCaptureItemInterop: {error}"))?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(HWND(hwnd as _)) }
        .map_err(|error| format!("CreateForWindow: {error}"))?;
    let size = item.Size().map_err(|error| format!("item size: {error}"))?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &direct3d,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(|error| format!("frame pool: {error}"))?;
    let session = pool
        .CreateCaptureSession(&item)
        .map_err(|error| format!("capture session: {error}"))?;
    // Best-effort and old-build-tolerant: the cursor already travels on the
    // share machine's own channel, so WGC drawing it too would mean two
    // pointers; the border is cosmetic. Missing APIs must not fail.
    let _ = session.SetIsCursorCaptureEnabled(false);
    let _ = session.SetIsBorderRequired(false);
    session
        .StartCapture()
        .map_err(|error| format!("StartCapture: {error}"))?;
    Ok((device, context, direct3d, pool, session))
}

fn create_device() -> Result<Device, String> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|error| format!("D3D11CreateDevice: {error}"))?;
        let device = device.ok_or("D3D11CreateDevice returned no device")?;
        let context = context.ok_or("D3D11CreateDevice returned no context")?;
        let dxgi: IDXGIDevice = device
            .cast()
            .map_err(|error| format!("IDXGIDevice: {error}"))?;
        let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)
            .map_err(|error| format!("CreateDirect3D11DeviceFromDXGIDevice: {error}"))?;
        let direct3d: IDirect3DDevice = inspectable
            .cast()
            .map_err(|error| format!("IDirect3DDevice: {error}"))?;
        Ok((device, context, direct3d))
    }
}

/// The window's content rect as WGC sees it, for the cursor overlay.
///
/// `DWMWA_EXTENDED_FRAME_BOUNDS` matches the frame pool's content (no shadow
/// borders); `GetWindowRect` is the fallback when DWM declines.
fn window_rect(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut rect = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_ok()
        {
            return Some(rect);
        }
        GetWindowRect(hwnd, &mut rect).ok()?;
        Some(rect)
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    direct3d: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    _session: GraphicsCaptureSession,
    hwnd: isize,
    tx: SyncSender<Frame>,
    control: Control,
) -> bool {
    let Control {
        stop,
        active,
        interval,
        metrics,
        cursor,
    } = control;
    let hwnd = HWND(hwnd as _);
    let mut shadow = crate::cursor::CursorShadow::new();
    let mut staging: Option<Staging> = None;
    let mut first = true;
    let mut next_due = Instant::now();
    let mut last_timestamp: Option<i64> = None;
    // WGC emits nothing while the window is static, so the last frame is kept
    // for the resume case — a new viewer or a preview coming back would
    // otherwise stare at nothing until the window moves.
    let mut last_frame: Option<Frame> = None;
    let mut content_size: Option<(u32, u32)> = None;
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        if !active.load(std::sync::atomic::Ordering::Relaxed) {
            thread::sleep(IDLE_SLEEP);
            first = true;
            continue;
        }
        // A closed window is the one thing the session's retry cannot fix;
        // dying here lets `pump` report it immediately.
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return true;
        }
        if let Some(rect) = window_rect(hwnd) {
            if let Some(update) = shadow.poll(&rect) {
                if let Ok(mut slot) = cursor.lock() {
                    *slot = Some(update);
                }
            }
        }
        let interval_micros = interval.load(std::sync::atomic::Ordering::Relaxed).max(1);
        metrics.cadence_target.set(interval_micros);
        let target = Duration::from_micros(interval_micros);

        match pool.TryGetNextFrame() {
            Ok(frame) => {
                let now = Instant::now();
                let timestamp = frame.SystemRelativeTime().map(|t| t.Duration).unwrap_or(0);
                let fresh = Some(timestamp) != last_timestamp || first;
                if fresh {
                    last_timestamp = Some(timestamp);
                    metrics.fresh.incr();
                }
                // A resize means the pool's textures are the wrong shape.
                if let Ok(size) = frame.ContentSize() {
                    let dims = (size.Width.max(0) as u32, size.Height.max(0) as u32);
                    if content_size != Some(dims) {
                        content_size = Some(dims);
                        let _ = pool.Recreate(
                            &direct3d,
                            DirectXPixelFormat::B8G8R8A8UIntNormalized,
                            2,
                            size,
                        );
                    }
                }
                if fresh && due_at(&mut next_due, now, target) {
                    let copying = Instant::now();
                    match frame_rgba(&device, &context, &frame, &mut staging) {
                        Ok(out) => {
                            metrics.readback.record(copying.elapsed());
                            first = false;
                            last_frame = Some(Frame {
                                width: out.width,
                                height: out.height,
                                rgba: out.rgba.clone(),
                            });
                            match tx.try_send(out) {
                                Ok(()) => metrics.captured.record(),
                                Err(TrySendError::Full(_)) => metrics.captured.drop_frame(),
                                Err(TrySendError::Disconnected(_)) => {
                                    let _ = frame.Close();
                                    return false;
                                }
                            }
                        }
                        Err(_) => metrics.readback.record(copying.elapsed()),
                    }
                } else if fresh {
                    metrics.paced.incr();
                }
                let _ = frame.Close();
            }
            Err(_) => {
                // No new frame: the window is static (or minimised). If the
                // loop is in `first` — a resume — resend the remembered frame
                // instead of nothing.
                if first {
                    if let Some(frame) = &last_frame {
                        if due_at(&mut next_due, Instant::now(), target) {
                            match tx.try_send(Frame {
                                width: frame.width,
                                height: frame.height,
                                rgba: frame.rgba.clone(),
                            }) {
                                Ok(()) => {
                                    metrics.captured.record();
                                    first = false;
                                }
                                Err(TrySendError::Disconnected(_)) => return false,
                                Err(TrySendError::Full(_)) => {}
                            }
                        }
                    }
                }
                thread::sleep(NO_FRAME_SLEEP.min(target));
            }
        }
    }
}

/// Turns one WGC frame into RGBA, cropping the pooled texture to the window's
/// content size — the pool can allocate larger than the picture it holds.
fn frame_rgba(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &Direct3D11CaptureFrame,
    staging: &mut Option<Staging>,
) -> Result<Frame, String> {
    let surface = frame
        .Surface()
        .map_err(|error| format!("surface: {error}"))?;
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .map_err(|error| format!("interface access: {error}"))?;
    let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
        .map_err(|error| format!("GetInterface<ID3D11Texture2D>: {error}"))?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let size = frame
        .ContentSize()
        .map_err(|error| format!("content size: {error}"))?;
    let crop = even_crop(
        size.Width.max(0) as u32,
        size.Height.max(0) as u32,
        desc.Width,
        desc.Height,
    )
    .ok_or_else(|| "window content is smaller than two pixels".to_string())?;
    readback(device, context, &texture, staging, Some(crop))
}

/// The region of a WGC texture that actually holds window content: the smaller
/// of content and texture on each axis, rounded down to even — the encoder
/// rejects odd dimensions. `None` when there is no usable picture at all.
pub(crate) fn even_crop(
    content_w: u32,
    content_h: u32,
    tex_w: u32,
    tex_h: u32,
) -> Option<(u32, u32)> {
    let w = content_w.min(tex_w) & !1;
    let h = content_h.min(tex_h) & !1;
    (w >= 2 && h >= 2).then_some((w, h))
}

/// Whether WGC can run on this machine at all (false on old builds or error).
pub fn window_capture_supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// Whether `hwnd` still names a live window — the check `pump` runs before
/// attempting to (re)create a capture for it.
pub fn window_exists(hwnd: isize) -> bool {
    unsafe { IsWindow(Some(HWND(hwnd as _))) }.as_bool()
}

/// The attributes a window is filtered on, gathered once per hwnd.
#[derive(Default)]
struct WindowAttrs {
    visible: bool,
    cloaked: bool,
    titled: bool,
    tool_window: bool,
    own_process: bool,
    has_area: bool,
}

/// The keep rule, pure so it can be tested without a desktop: a sharable
/// window is a visible, real, titled top-level window belonging to someone
/// else and bigger than a point.
fn keep_window(a: &WindowAttrs) -> bool {
    a.visible && !a.cloaked && a.titled && !a.tool_window && !a.own_process && a.has_area
}

/// Every top-level window worth offering in the source picker.
pub fn list_windows() -> Vec<WindowInfo> {
    let mut found: Vec<(HWND, WindowAttrs)> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_window_proc),
            LPARAM(&mut found as *mut _ as isize),
        );
    }
    found
        .into_iter()
        .filter(|(_, attrs)| keep_window(attrs))
        .map(|(hwnd, _)| {
            let title = window_title(hwnd);
            WindowInfo {
                hwnd: hwnd.0 as isize,
                title,
                process: process_name(hwnd).unwrap_or_default(),
            }
        })
        .collect()
}

unsafe extern "system" fn enum_window_proc(hwnd: HWND, data: LPARAM) -> BOOL {
    let found = &mut *(data.0 as *mut Vec<(HWND, WindowAttrs)>);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let mut cloaked = 0u32;
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut _ as *mut _,
        std::mem::size_of::<u32>() as u32,
    );
    let has_area =
        window_rect(hwnd).is_some_and(|rect| rect.right > rect.left && rect.bottom > rect.top);
    found.push((
        hwnd,
        WindowAttrs {
            visible: IsWindowVisible(hwnd).as_bool(),
            cloaked: cloaked != 0,
            titled: GetWindowTextLengthW(hwnd) > 0,
            tool_window: GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & (WS_EX_TOOLWINDOW.0 as isize) != 0,
            own_process: pid == std::process::id(),
            has_area,
        },
    ));
    BOOL(1)
}

fn window_title(hwnd: HWND) -> String {
    let mut buffer = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) } as usize;
    String::from_utf16_lossy(&buffer[..len.min(buffer.len())])
}

/// The owning process's exe file name, e.g. `notepad.exe`. Empty on failure —
/// a window whose process cannot be opened is still worth listing.
fn process_name(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut len,
        );
        let _ = windows::Win32::Foundation::CloseHandle(process);
        result.ok()?;
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        path.rsplit(['\\', '/']).next().map(|s| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crop is where a rounded-up pooled texture meets the encoder's
    /// even-dimension rule: the window is what it is, the encoder takes what
    /// it takes, and the crop is the honest intersection.
    #[test]
    fn the_crop_is_min_and_even() {
        assert_eq!(even_crop(100, 100, 100, 100), Some((100, 100)));
        // Texture bigger than content: crop to content.
        assert_eq!(even_crop(101, 100, 128, 128), Some((100, 100)));
        // Content bigger than texture: crop to texture.
        assert_eq!(even_crop(200, 200, 129, 63), Some((128, 62)));
        // Below two pixels there is nothing the encoder can take.
        assert_eq!(even_crop(1, 100, 128, 128), None);
        assert_eq!(even_crop(0, 0, 0, 0), None);
    }

    /// The picker's keep rule, since it decides which hwnds a user can share:
    /// one wrong bit either hides a real window or offers a phantom.
    #[test]
    fn the_window_filter_keeps_real_windows() {
        let good = WindowAttrs {
            visible: true,
            cloaked: false,
            titled: true,
            tool_window: false,
            own_process: false,
            has_area: true,
        };
        assert!(keep_window(&good));
        for (field, value) in [
            ("visible", false),
            ("cloaked", true),
            ("titled", false),
            ("tool_window", true),
            ("own_process", true),
            ("has_area", false),
        ] {
            let mut a = WindowAttrs {
                visible: true,
                cloaked: false,
                titled: true,
                tool_window: false,
                own_process: false,
                has_area: true,
            };
            match field {
                "visible" => a.visible = value,
                "cloaked" => a.cloaked = value,
                "titled" => a.titled = value,
                "tool_window" => a.tool_window = value,
                "own_process" => a.own_process = value,
                _ => a.has_area = value,
            }
            assert!(!keep_window(&a), "{field} did not exclude the window");
        }
    }

    /// Same shape as `captures_real_frames` for the monitor path: enumerate,
    /// start, expect a frame. Needs a visible window, so it is ignored by
    /// default.
    #[test]
    #[ignore = "requires an interactive desktop with a visible window"]
    fn captures_window_frames() {
        let windows = list_windows();
        let window = windows.first().expect("no capturable window");
        let mut session = crate::capture::CaptureSession::new();
        session
            .start(&crate::capture::CaptureSource::Window(window.clone()))
            .expect("window capture should start");
        let deadline = Instant::now() + Duration::from_secs(5);
        let frame = loop {
            if let Some(frame) = session.latest() {
                break frame;
            }
            assert!(Instant::now() < deadline, "no frame arrived within 5s");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(
            frame.rgba.len(),
            (frame.width as usize) * (frame.height as usize) * 4,
        );
    }
}
