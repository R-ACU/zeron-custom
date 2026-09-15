//! Windows Appshots: `PrintWindow` capture of the frontmost top-level window,
//! plus UI Automation derived application text.
//!
//! The global shortcut lives on its own thread with a message-only window,
//! because `RegisterHotKey` delivers `WM_HOTKEY` to the registering thread's
//! queue and GPUI owns the main message loop. Capture runs on a dedicated
//! thread so UI Automation gets a clean single-threaded apartment.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};
use std::{mem, thread};

use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, HGDIOBJ, RGBQUAD, ROP_CODE,
    ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
    TreeScope_Descendants, UIA_AppBarControlTypeId, UIA_ButtonControlTypeId, UIA_CONTROLTYPE_ID,
    UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId, UIA_DataGridControlTypeId,
    UIA_DataItemControlTypeId, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
    UIA_GroupControlTypeId, UIA_HeaderControlTypeId, UIA_HeaderItemControlTypeId,
    UIA_HyperlinkControlTypeId, UIA_ImageControlTypeId, UIA_ListControlTypeId,
    UIA_ListItemControlTypeId, UIA_MenuBarControlTypeId, UIA_MenuControlTypeId,
    UIA_MenuItemControlTypeId, UIA_PaneControlTypeId, UIA_ProgressBarControlTypeId,
    UIA_RadioButtonControlTypeId, UIA_ScrollBarControlTypeId, UIA_SeparatorControlTypeId,
    UIA_SliderControlTypeId, UIA_SpinnerControlTypeId, UIA_SplitButtonControlTypeId,
    UIA_StatusBarControlTypeId, UIA_TabControlTypeId, UIA_TabItemControlTypeId,
    UIA_TableControlTypeId, UIA_TextControlTypeId, UIA_TextPatternId, UIA_TitleBarControlTypeId,
    UIA_ToolBarControlTypeId, UIA_ToolTipControlTypeId, UIA_TreeControlTypeId,
    UIA_TreeItemControlTypeId, UIA_WindowControlTypeId,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey,
};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DI_NORMAL, DefWindowProcW, DestroyIcon, DestroyWindow, DispatchMessageW,
    DrawIconEx, GW_HWNDNEXT, GWL_EXSTYLE, GetForegroundWindow, GetMessageW, GetWindow,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, HICON,
    HWND_MESSAGE, IsWindowVisible, MSG, PW_RENDERFULLCONTENT, PostMessageW, PostQuitMessage,
    RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_DESTROY, WM_HOTKEY,
    WNDCLASSW, WS_EX_TOOLWINDOW,
};
use windows::core::{PCWSTR, PWSTR};

use super::{
    AccessibilitySnapshot, AppshotBackend, AppshotCapabilities, AppshotPlatform, CapabilityState,
    CaptureError, CaptureTarget, CapturedAppshot, validate_capture_dimensions,
};

/// One process-wide hotkey identity. Registration is replaced in place when
/// the user changes the combination, so a second id is never needed.
const HOTKEY_ID: i32 = 0x5A41;
/// Private message asking the hotkey thread to re-read the user preference.
const WM_APPSHOT_REFRESH: u32 = WM_APP + 0x51;
const HOTKEY_WINDOW_CLASS: &str = "ZeronAppshotHotkey";
/// Walk at most this far down the z-order before giving up. A desktop with
/// more cloaked or tool windows than this in front has nothing to capture.
const MAX_ZORDER_WALK: usize = 64;

/// Mirror the macOS accessibility budget so both platforms truncate the same
/// way and the serialized context stays a bounded, predictable size.
const MAX_UIA_NODES: usize = 1_500;
const MAX_UIA_BYTES: usize = 96 * 1024;
const MAX_UIA_VALUE_CHARS: usize = 4_096;
const UIA_DEADLINE: Duration = Duration::from_millis(900);

/// Longest captured edge. A 4K or 5K window stays inside the PNG attachment
/// budget after encoding, matching the macOS backend's cap.
const CAPTURE_EDGE_CAP: u32 = 4_096;

static SHORTCUT_READY: AtomicBool = AtomicBool::new(true);
/// The hotkey window handle, published for the preference watcher. `HWND` is
/// not `Send`, so only the raw address crosses threads and it is used solely
/// as a `PostMessageW` target.
static HOTKEY_WINDOW: AtomicIsize = AtomicIsize::new(0);

pub struct WindowsBackend;

#[async_trait::async_trait]
impl AppshotBackend for WindowsBackend {
    fn capabilities(&self) -> AppshotCapabilities {
        AppshotCapabilities {
            platform: AppshotPlatform::Windows,
            global_shortcut: if SHORTCUT_READY.load(Ordering::Relaxed) {
                CapabilityState::Ready
            } else {
                CapabilityState::Unavailable
            },
            // Windows needs no capture consent for ordinary desktop windows,
            // and UI Automation is always available to a desktop process.
            window_capture: CapabilityState::Ready,
            application_text: CapabilityState::Ready,
            target: CaptureTarget::ActiveWindow,
        }
    }

    fn start_global_shortcut(
        &self,
        _activation_dir: &std::path::Path,
    ) -> mpsc::UnboundedReceiver<()> {
        start_global_shortcut()
    }

    async fn capture_active_window(&self) -> Result<CapturedAppshot, CaptureError> {
        let (tx, rx) = oneshot::channel();
        thread::Builder::new()
            .name("appshot-windows-capture".into())
            .spawn(move || {
                // UI Automation is COM work: give it a private apartment that
                // no other subsystem in this process shares.
                let _apartment = Apartment::enter();
                let _ = tx.send(capture_frontmost_window());
            })
            .map_err(|error| {
                CaptureError::CaptureFailed(format!("Could not start Appshot capture: {error}"))
            })?;
        rx.await.unwrap_or_else(|_| {
            Err(CaptureError::CaptureFailed(
                "The Appshot capture thread stopped unexpectedly.".into(),
            ))
        })
    }
}

/// Own the COM apartment for the lifetime of one capture thread.
struct Apartment {
    initialized: bool,
}

impl Apartment {
    fn enter() -> Self {
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        Self {
            initialized: result.is_ok(),
        }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

// ---------------------------------------------------------------------------
// Global shortcut
// ---------------------------------------------------------------------------

fn start_global_shortcut() -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded();
    thread::Builder::new()
        .name("appshot-windows-hotkey".into())
        .spawn(move || {
            if let Err(error) = run_hotkey_window(tx) {
                SHORTCUT_READY.store(false, Ordering::Relaxed);
                tracing::warn!(%error, "Windows Appshot shortcut thread stopped");
            }
        })
        .ok();
    rx
}

fn run_hotkey_window(tx: mpsc::UnboundedSender<()>) -> windows::core::Result<()> {
    let class = wide(HOTKEY_WINDOW_CLASS);
    let instance = unsafe { GetModuleHandleW(None) }?;
    let descriptor = WNDCLASSW {
        lpfnWndProc: Some(hotkey_wndproc),
        hInstance: instance.into(),
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    // A non-zero atom means this registration won; a zero one means the class
    // already exists from an earlier service start, which is equally usable.
    unsafe { RegisterClassW(&descriptor) };
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )
    }?;
    HOTKEY_SENDER.with(|sender| *sender.borrow_mut() = Some(tx));
    HOTKEY_WINDOW.store(window.0 as isize, Ordering::Release);
    apply_shortcut(window);
    start_preference_watch();
    let mut message = MSG::default();
    // `GetMessageW` returns 0 on WM_QUIT and -1 on error; both end the loop.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    HOTKEY_WINDOW.store(0, Ordering::Release);
    unsafe { DestroyWindow(window) }?;
    Ok(())
}

thread_local! {
    /// Read only from the hotkey thread's own window procedure.
    static HOTKEY_SENDER: std::cell::RefCell<Option<mpsc::UnboundedSender<()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Shortcut changes arrive on the shared preference channel, but registration
/// must happen on the thread owning the message loop. Hand the change over as
/// a window message instead of touching `RegisterHotKey` from here.
fn start_preference_watch() {
    thread::Builder::new()
        .name("appshot-windows-hotkey-watch".into())
        .spawn(|| {
            let mut updates = super::shortcut::subscribe();
            futures::executor::block_on(async move {
                while updates.next().await.is_some() {
                    let window = HOTKEY_WINDOW.load(Ordering::Acquire);
                    if window == 0 {
                        break;
                    }
                    let _ = unsafe {
                        PostMessageW(
                            Some(HWND(window as *mut c_void)),
                            WM_APPSHOT_REFRESH,
                            WPARAM(0),
                            LPARAM(0),
                        )
                    };
                }
            });
        })
        .ok();
}

unsafe extern "system" fn hotkey_wndproc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID => {
            // Recording a new combination, or a disabled feature, must not
            // deliver a capture even if the old registration still fires.
            if super::capture_allowed() {
                let closed = HOTKEY_SENDER.with(|sender| {
                    sender
                        .borrow()
                        .as_ref()
                        .map(|tx| tx.unbounded_send(()).is_err())
                        .unwrap_or(true)
                });
                if closed {
                    unsafe { PostQuitMessage(0) };
                }
            }
            LRESULT(0)
        }
        WM_APPSHOT_REFRESH => {
            apply_shortcut(window);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = unsafe { UnregisterHotKey(Some(window), HOTKEY_ID) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

fn apply_shortcut(window: HWND) {
    // Always drop the previous binding first: a failed re-registration must
    // not leave the old chord consuming the user's keystrokes.
    let _ = unsafe { UnregisterHotKey(Some(window), HOTKEY_ID) };
    let Some(shortcut) = super::shortcut::current() else {
        // No active shortcut is not a failure. Appshots may be off, or the
        // settings recorder may be holding the keyboard.
        SHORTCUT_READY.store(true, Ordering::Relaxed);
        return;
    };
    let Some((modifiers, key)) = hotkey_binding(&shortcut) else {
        SHORTCUT_READY.store(false, Ordering::Relaxed);
        tracing::warn!(
            key = %shortcut.key,
            "Windows Appshot shortcut uses a key without a virtual-key mapping"
        );
        return;
    };
    match unsafe {
        RegisterHotKey(
            Some(window),
            HOTKEY_ID,
            HOT_KEY_MODIFIERS(modifiers | MOD_NOREPEAT.0),
            u32::from(key),
        )
    } {
        Ok(()) => {
            SHORTCUT_READY.store(true, Ordering::Relaxed);
            tracing::debug!(
                key = %shortcut.key,
                modifiers,
                "Windows Appshot shortcut registered"
            );
        }
        Err(error) => {
            SHORTCUT_READY.store(false, Ordering::Relaxed);
            tracing::warn!(
                %error,
                "Windows Appshot shortcut is already taken by another application"
            );
        }
    }
}

/// Translate a gpui-style combination into `RegisterHotKey` arguments.
/// Returns `None` for keys Windows cannot bind as a hotkey.
fn hotkey_binding(shortcut: &super::shortcut::Shortcut) -> Option<(u32, u16)> {
    let mut modifiers = 0;
    if shortcut.control {
        modifiers |= MOD_CONTROL.0;
    }
    if shortcut.alt {
        modifiers |= MOD_ALT.0;
    }
    if shortcut.shift {
        modifiers |= MOD_SHIFT.0;
    }
    if shortcut.platform {
        modifiers |= MOD_WIN.0;
    }
    // Windows refuses an unmodified hotkey, and a Shift-only chord would eat
    // ordinary typing. `Shortcut::parse` already enforces this; keep the
    // invariant local so a future parser change cannot register junk.
    if modifiers & (MOD_CONTROL.0 | MOD_ALT.0 | MOD_WIN.0) == 0 {
        return None;
    }
    Some((modifiers, virtual_key(&shortcut.key)?))
}

/// Virtual-key codes are written out rather than imported so the whole
/// mapping reads as one table. Values are from WinUser.h.
fn virtual_key(key: &str) -> Option<u16> {
    Some(match key {
        "space" => 0x20,
        "tab" => 0x09,
        "enter" => 0x0d,
        "backspace" => 0x08,
        "delete" => 0x2e,
        "insert" => 0x2d,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        key if key.len() == 1 && key.as_bytes()[0].is_ascii_digit() => u16::from(key.as_bytes()[0]),
        key if key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic() => {
            u16::from(key.as_bytes()[0].to_ascii_uppercase())
        }
        key => {
            let number = key.strip_prefix('f')?.parse::<u16>().ok()?;
            if !(1..=24).contains(&number) {
                return None;
            }
            0x70 + number - 1
        }
    })
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

fn capture_frontmost_window() -> Result<CapturedAppshot, CaptureError> {
    let started = Instant::now();
    let window = eligible_window()?;
    let bounds = window_bounds(window).ok_or(CaptureError::NoEligibleWindow)?;
    let width = bounds.right.saturating_sub(bounds.left).max(0) as u32;
    let height = bounds.bottom.saturating_sub(bounds.top).max(0) as u32;
    if width == 0 || height == 0 {
        return Err(CaptureError::NoEligibleWindow);
    }
    // Reject an impossible surface before allocating a device-independent
    // bitmap for it, exactly like the other backends bound their acquisition.
    validate_capture_dimensions(width, height)?;
    let dpi = unsafe { GetDpiForWindow(window) };
    let bgra = window_pixels(window, bounds, width, height)?;
    let rgba = bgra_to_rgba(&bgra);
    // Frame bounds are already physical pixels, so a high-DPI window arrives
    // at its true resolution and only the shared edge cap scales it down.
    let (rgba, width, height) = downscale_rgba(rgba, width, height, CAPTURE_EDGE_CAP);
    let png = super::encode_rgba_png(width, height, &rgba, "Windows")?;
    let pid = window_process_id(window);
    let image = pid.and_then(process_image_path);
    let app_name = image
        .as_deref()
        .and_then(file_description)
        .or_else(|| {
            image
                .as_deref()
                .and_then(|path| path.file_stem())
                .and_then(|stem| stem.to_str())
                .map(capitalize)
        })
        .unwrap_or_else(|| "Windows application".into());
    let title = window_text(window);
    let pixels_ready = Instant::now();
    let (screenshot, screenshot_dimensions) = super::stage_appshot_png(&app_name, png)?;
    super::capture_ready();
    tracing::debug!(
        dpi,
        width,
        height,
        capture_ms = pixels_ready.duration_since(started).as_millis(),
        total_ms = started.elapsed().as_millis(),
        "Appshot capture feedback requested"
    );
    let accessibility = application_text(window);
    tracing::debug!(
        context_bytes = accessibility.content.len(),
        truncated = accessibility.truncated,
        "Appshot application context collected"
    );
    Ok(CapturedAppshot {
        id: uuid::Uuid::new_v4().to_string(),
        app_name,
        bundle_identifier: image
            .as_deref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .map(|name| format!("windows:{}", name.to_ascii_lowercase())),
        window_title: title,
        accessibility,
        screenshot,
        screenshot_dimensions: Some(screenshot_dimensions),
        app_icon: image.as_deref().and_then(icon_png).map(|bytes| {
            std::sync::Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes))
        }),
        captured_at: chrono::Utc::now(),
    })
}

/// Resolve the window the user meant. The foreground window can still belong
/// to Zeron (the hotkey can land during a focus change), so walk the z-order
/// past our own, cloaked, hidden and tool windows.
fn eligible_window() -> Result<HWND, CaptureError> {
    let own = unsafe { GetCurrentProcessId() };
    let mut window = unsafe { GetForegroundWindow() };
    if window.0.is_null() {
        return Err(CaptureError::NoEligibleWindow);
    }
    let mut skipped_self = false;
    for _ in 0..MAX_ZORDER_WALK {
        if window_process_id(window) == Some(own) {
            skipped_self = true;
        } else if is_capturable(window) {
            return Ok(window);
        }
        let next = unsafe { GetWindow(window, GW_HWNDNEXT) };
        let Ok(next) = next else {
            break;
        };
        if next.0.is_null() {
            break;
        }
        window = next;
    }
    Err(if skipped_self {
        CaptureError::SelfCapture
    } else {
        CaptureError::NoEligibleWindow
    })
}

fn is_capturable(window: HWND) -> bool {
    if !unsafe { IsWindowVisible(window) }.as_bool() {
        return false;
    }
    // A tool window is a palette, not an application the user would capture.
    let extended = unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) } as u32;
    if extended & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    // Suspended store applications stay in the z-order but are not on screen.
    !is_cloaked(window)
}

fn is_cloaked(window: HWND) -> bool {
    let mut cloaked = 0_u32;
    let queried = unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            mem::size_of::<u32>() as u32,
        )
    };
    queried.is_ok() && cloaked != 0
}

/// Prefer the DWM frame rectangle: `GetWindowRect` includes the invisible
/// resize border, which would frame every capture in desktop wallpaper.
fn window_bounds(window: HWND) -> Option<RECT> {
    let mut bounds = RECT::default();
    let queried = unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut bounds).cast(),
            mem::size_of::<RECT>() as u32,
        )
    };
    if queried.is_ok() && bounds.right > bounds.left && bounds.bottom > bounds.top {
        return Some(bounds);
    }
    let mut fallback = RECT::default();
    unsafe { GetWindowRect(window, &raw mut fallback) }.ok()?;
    (fallback.right > fallback.left && fallback.bottom > fallback.top).then_some(fallback)
}

/// `PW_RENDERFULLCONTENT` covers Chromium, Electron and most DirectX windows.
/// When it refuses, or paints nothing, fall back to reading the screen.
fn window_pixels(
    window: HWND,
    bounds: RECT,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, CaptureError> {
    let surface = Surface::new(width, height).ok_or_else(|| {
        CaptureError::CaptureFailed("Could not allocate a Windows capture surface.".into())
    })?;
    let printed = unsafe {
        PrintWindow(
            window,
            surface.device,
            PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT),
        )
    }
    .as_bool();
    if printed {
        let pixels = surface.pixels();
        if !is_blank(pixels) {
            return Ok(pixels.to_vec());
        }
    }
    tracing::debug!(
        printed,
        "PrintWindow produced no usable Appshot pixels; reading the screen instead"
    );
    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        return Err(CaptureError::CaptureFailed(
            "Windows did not provide a screen device context.".into(),
        ));
    }
    let blitted = unsafe {
        BitBlt(
            surface.device,
            0,
            0,
            width as i32,
            height as i32,
            Some(screen),
            bounds.left,
            bounds.top,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        )
    };
    unsafe { ReleaseDC(None, screen) };
    blitted.map_err(|error| {
        CaptureError::CaptureFailed(format!("Windows Appshot capture failed: {error}"))
    })?;
    Ok(surface.pixels().to_vec())
}

/// A top-down 32-bit device-independent bitmap plus the memory device context
/// drawing into it. Dropping the surface releases both GDI objects.
struct Surface {
    device: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    length: usize,
}

impl Surface {
    fn new(width: u32, height: u32) -> Option<Self> {
        let length = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))?;
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Negative height requests top-down rows, so the buffer is
                // already in the order the PNG encoder expects.
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default(); 1],
        };
        let device = unsafe { CreateCompatibleDC(None) };
        if device.is_invalid() {
            return None;
        }
        let mut bits = std::ptr::null_mut();
        let bitmap = unsafe {
            CreateDIBSection(
                Some(device),
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut bits,
                None,
                0,
            )
        };
        let Ok(bitmap) = bitmap else {
            unsafe {
                let _ = DeleteDC(device);
            };
            return None;
        };
        if bits.is_null() {
            unsafe {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(device);
            }
            return None;
        }
        let previous = unsafe { SelectObject(device, bitmap.into()) };
        Some(Self {
            device,
            bitmap,
            previous,
            bits: bits.cast(),
            length,
        })
    }

    fn pixels(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.bits, self.length) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.device, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.device);
        }
    }
}

/// `PrintWindow` can report success and still leave the surface untouched for
/// hardware-composited windows. Treat a fully black frame as a failure.
fn is_blank(bgra: &[u8]) -> bool {
    !bgra
        .chunks_exact(4)
        .any(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
}

/// GDI hands back BGRA with an alpha channel that `PrintWindow` frequently
/// leaves at zero. Force it opaque so the shared padding trim does not treat
/// the whole capture as empty.
fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for pixel in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
    }
    rgba
}

/// Box-average the capture down until its longest edge fits `cap`. Returns the
/// input untouched when it already fits.
fn downscale_rgba(rgba: Vec<u8>, width: u32, height: u32, cap: u32) -> (Vec<u8>, u32, u32) {
    let longest = width.max(height);
    if longest <= cap || cap == 0 || width == 0 || height == 0 {
        return (rgba, width, height);
    }
    let scale = f64::from(cap) / f64::from(longest);
    let target_width = ((f64::from(width) * scale).round() as u32).clamp(1, width);
    let target_height = ((f64::from(height) * scale).round() as u32).clamp(1, height);
    let mut output = Vec::with_capacity(target_width as usize * target_height as usize * 4);
    for y in 0..target_height {
        let y0 = (y as u64 * height as u64 / target_height as u64) as u32;
        let y1 = (((y + 1) as u64 * height as u64 / target_height as u64) as u32).max(y0 + 1);
        for x in 0..target_width {
            let x0 = (x as u64 * width as u64 / target_width as u64) as u32;
            let x1 = (((x + 1) as u64 * width as u64 / target_width as u64) as u32).max(x0 + 1);
            let mut sums = [0_u64; 4];
            let mut count = 0_u64;
            for row in y0..y1.min(height) {
                for column in x0..x1.min(width) {
                    let start = (row as usize * width as usize + column as usize) * 4;
                    for (sum, value) in sums.iter_mut().zip(&rgba[start..start + 4]) {
                        *sum += u64::from(*value);
                    }
                    count += 1;
                }
            }
            let count = count.max(1);
            output.extend(sums.map(|sum| (sum / count) as u8));
        }
    }
    (output, target_width, target_height)
}

// ---------------------------------------------------------------------------
// Application identity
// ---------------------------------------------------------------------------

fn window_process_id(window: HWND) -> Option<u32> {
    let mut pid = 0_u32;
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
    (pid != 0).then_some(pid)
}

fn window_text(window: HWND) -> Option<String> {
    let mut buffer = [0_u16; 512];
    let length = unsafe { GetWindowTextW(window, &mut buffer) };
    if length <= 0 {
        return None;
    }
    let title = String::from_utf16_lossy(&buffer[..length as usize]);
    (!title.trim().is_empty()).then_some(title)
}

fn process_image_path(pid: u32) -> Option<PathBuf> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let handle = OwnedHandle(process);
    let mut buffer = [0_u16; 1024];
    let mut length = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut length,
        )
    }
    .ok()?;
    (length > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])))
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        };
    }
}

/// The version resource carries the name users know ("Notepad", "Google
/// Chrome"); the executable stem is only a fallback for unsigned binaries.
fn file_description(path: &std::path::Path) -> Option<String> {
    let file = wide(path.to_str()?);
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(file.as_ptr()), None) };
    if size == 0 {
        return None;
    }
    let mut block = vec![0_u8; size as usize];
    unsafe { GetFileVersionInfoW(PCWSTR(file.as_ptr()), None, size, block.as_mut_ptr().cast()) }
        .ok()?;
    let mut translation = std::ptr::null_mut::<c_void>();
    let mut translation_length = 0_u32;
    let query = wide("\\VarFileInfo\\Translation");
    let found = unsafe {
        VerQueryValueW(
            block.as_ptr().cast(),
            PCWSTR(query.as_ptr()),
            &raw mut translation,
            &raw mut translation_length,
        )
    }
    .as_bool();
    if !found || translation.is_null() || translation_length < 4 {
        return None;
    }
    // The first translation is the binary's own language; other entries are
    // localizations we have no reason to prefer.
    let language = unsafe { translation.cast::<u16>().read_unaligned() };
    let codepage = unsafe { translation.cast::<u16>().add(1).read_unaligned() };
    let path = wide(&format!(
        "\\StringFileInfo\\{language:04x}{codepage:04x}\\FileDescription"
    ));
    let mut value = std::ptr::null_mut::<c_void>();
    let mut characters = 0_u32;
    let found = unsafe {
        VerQueryValueW(
            block.as_ptr().cast(),
            PCWSTR(path.as_ptr()),
            &raw mut value,
            &raw mut characters,
        )
    }
    .as_bool();
    if !found || value.is_null() || characters == 0 {
        return None;
    }
    let text = unsafe { std::slice::from_raw_parts(value.cast::<u16>(), characters as usize) };
    let text = String::from_utf16_lossy(text);
    let text = text.trim_end_matches('\0').trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Presentation only. The icon is never uploaded or serialized.
fn icon_png(path: &std::path::Path) -> Option<Vec<u8>> {
    const EDGE: u32 = 64;
    let file = wide(path.to_str()?);
    let mut icon = HICON::default();
    let extracted =
        unsafe { ExtractIconExW(PCWSTR(file.as_ptr()), 0, Some(&raw mut icon), None, 1) };
    if extracted == 0 || extracted == u32::MAX || icon.is_invalid() {
        return None;
    }
    let owned = OwnedIcon(icon);
    let surface = Surface::new(EDGE, EDGE)?;
    unsafe {
        DrawIconEx(
            surface.device,
            0,
            0,
            owned.0,
            EDGE as i32,
            EDGE as i32,
            0,
            None,
            DI_NORMAL,
        )
    }
    .ok()?;
    let bgra = surface.pixels();
    // Keep the icon's own alpha: a square opaque badge would look wrong in the
    // composer, unlike the window capture which must stay opaque.
    let mut rgba = Vec::with_capacity(bgra.len());
    for pixel in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    if rgba.chunks_exact(4).all(|pixel| pixel[3] == 0) {
        return None;
    }
    super::encode_rgba_png(EDGE, EDGE, &rgba, "Windows").ok()
}

struct OwnedIcon(HICON);

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(self.0);
        };
    }
}

fn capitalize(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// UI Automation text
// ---------------------------------------------------------------------------

/// Serialize the window's automation tree in the same shape the macOS
/// accessibility path produces: one indented `role field=value` line per
/// element, bounded by node, byte and time budgets.
fn application_text(window: HWND) -> AccessibilitySnapshot {
    match collect_application_text(window) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::debug!(%error, "UI Automation Appshot context unavailable");
            AccessibilitySnapshot::unavailable()
        }
    }
}

fn collect_application_text(window: HWND) -> windows::core::Result<AccessibilitySnapshot> {
    let deadline = Instant::now() + UIA_DEADLINE;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?;
    let root = unsafe { automation.ElementFromHandle(window) }?;
    let mut output = String::new();
    let mut truncated = false;
    append_element(&root, 0, &mut output, &mut truncated, deadline);
    // A flat descendant query is bounded and fast; a full tree walk would
    // multiply the cross-process calls a hung application can block on.
    if !truncated && Instant::now() < deadline {
        match unsafe { automation.CreateTrueCondition() }
            .and_then(|condition| unsafe { root.FindAll(TreeScope_Descendants, &condition) })
        {
            Ok(elements) => {
                let count = unsafe { elements.Length() }.unwrap_or(0);
                for index in 0..count {
                    if index as usize >= MAX_UIA_NODES
                        || output.len() >= MAX_UIA_BYTES
                        || Instant::now() >= deadline
                    {
                        truncated = true;
                        break;
                    }
                    let element = unsafe { elements.GetElement(index) };
                    let Ok(element) = element else {
                        continue;
                    };
                    append_element(&element, 1, &mut output, &mut truncated, deadline);
                    if truncated {
                        break;
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%error, "UI Automation descendant query failed");
                truncated = true;
            }
        }
    }
    Ok(AccessibilitySnapshot {
        format_version: 1,
        content: output,
        truncated,
    })
}

fn append_element(
    element: &IUIAutomationElement,
    depth: usize,
    output: &mut String,
    truncated: &mut bool,
    deadline: Instant,
) {
    let role = unsafe { element.CurrentControlType() }
        .map(control_type_name)
        .unwrap_or("UIAElement");
    let secure = unsafe { element.CurrentIsPassword() }
        .map(|value| value.as_bool())
        .unwrap_or(false);
    let title = bstr(unsafe { element.CurrentName() }.ok());
    let description = bstr(unsafe { element.CurrentHelpText() }.ok());
    let value = if secure {
        None
    } else {
        document_text(element, deadline)
    };
    let mut fields = Vec::new();
    if let Some(title) = title {
        fields.push(format!("title={}", compact(&title)));
    }
    if let Some(description) = description {
        fields.push(format!("description={}", compact(&description)));
    }
    if let Some(value) = value {
        fields.push(format!("value={}", compact(&value)));
    }
    if fields.is_empty() && depth > 0 {
        // A descendant with no text of its own says nothing about the window.
        return;
    }
    let line = if fields.is_empty() {
        format!("{}{role}\n", "  ".repeat(depth))
    } else {
        format!("{}{role} {}\n", "  ".repeat(depth), fields.join(" "))
    };
    let remaining = MAX_UIA_BYTES.saturating_sub(output.len());
    if line.len() > remaining {
        let end = line
            .char_indices()
            .take_while(|(index, _)| *index <= remaining)
            .map(|(index, _)| index)
            .last()
            .unwrap_or(0);
        output.push_str(&line[..end]);
        *truncated = true;
        return;
    }
    output.push_str(&line);
}

/// Text patterns hold the off-screen document content a screenshot cannot
/// show. Everything else contributes its name only.
fn document_text(element: &IUIAutomationElement, deadline: Instant) -> Option<String> {
    if Instant::now() >= deadline {
        return None;
    }
    let pattern =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) }
            .ok()?;
    let range = unsafe { pattern.DocumentRange() }.ok()?;
    let text = unsafe { range.GetText(MAX_UIA_VALUE_CHARS as i32) }.ok()?;
    bstr(Some(text))
}

fn bstr(value: Option<windows::core::BSTR>) -> Option<String> {
    let value = value?.to_string();
    (!value.trim().is_empty()).then_some(value)
}

fn compact(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_UIA_VALUE_CHARS {
        compact
    } else {
        let end = compact
            .char_indices()
            .nth(MAX_UIA_VALUE_CHARS)
            .map(|(index, _)| index)
            .unwrap_or(compact.len());
        format!("{}\u{2026}", &compact[..end])
    }
}

// The Windows SDK spells these constants in PascalCase, and matching against
// the generated bindings is clearer than repeating their numeric values here.
#[allow(non_upper_case_globals)]
fn control_type_name(control: UIA_CONTROLTYPE_ID) -> &'static str {
    match control {
        UIA_AppBarControlTypeId => "AppBar",
        UIA_ButtonControlTypeId => "Button",
        UIA_CheckBoxControlTypeId => "CheckBox",
        UIA_ComboBoxControlTypeId => "ComboBox",
        UIA_DataGridControlTypeId => "DataGrid",
        UIA_DataItemControlTypeId => "DataItem",
        UIA_DocumentControlTypeId => "Document",
        UIA_EditControlTypeId => "Edit",
        UIA_GroupControlTypeId => "Group",
        UIA_HeaderControlTypeId => "Header",
        UIA_HeaderItemControlTypeId => "HeaderItem",
        UIA_HyperlinkControlTypeId => "Hyperlink",
        UIA_ImageControlTypeId => "Image",
        UIA_ListControlTypeId => "List",
        UIA_ListItemControlTypeId => "ListItem",
        UIA_MenuBarControlTypeId => "MenuBar",
        UIA_MenuControlTypeId => "Menu",
        UIA_MenuItemControlTypeId => "MenuItem",
        UIA_PaneControlTypeId => "Pane",
        UIA_ProgressBarControlTypeId => "ProgressBar",
        UIA_RadioButtonControlTypeId => "RadioButton",
        UIA_ScrollBarControlTypeId => "ScrollBar",
        UIA_SeparatorControlTypeId => "Separator",
        UIA_SliderControlTypeId => "Slider",
        UIA_SpinnerControlTypeId => "Spinner",
        UIA_SplitButtonControlTypeId => "SplitButton",
        UIA_StatusBarControlTypeId => "StatusBar",
        UIA_TabControlTypeId => "Tab",
        UIA_TabItemControlTypeId => "TabItem",
        UIA_TableControlTypeId => "Table",
        UIA_TextControlTypeId => "Text",
        UIA_TitleBarControlTypeId => "TitleBar",
        UIA_ToolBarControlTypeId => "ToolBar",
        UIA_ToolTipControlTypeId => "ToolTip",
        UIA_TreeControlTypeId => "Tree",
        UIA_TreeItemControlTypeId => "TreeItem",
        UIA_WindowControlTypeId => "Window",
        _ => "Custom",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(combo: &str) -> Option<(u32, u16)> {
        hotkey_binding(&super::super::shortcut::Shortcut::parse(combo).unwrap())
    }

    #[test]
    fn combos_map_to_windows_modifiers_and_virtual_keys() {
        assert_eq!(
            binding("ctrl-alt-space"),
            Some((MOD_CONTROL.0 | MOD_ALT.0, 0x20))
        );
        assert_eq!(
            binding("ctrl-shift-k"),
            Some((MOD_CONTROL.0 | MOD_SHIFT.0, 0x4b))
        );
        // "mod" is Ctrl off macOS, so the stored default resolves the same way.
        assert_eq!(binding("mod-alt-space"), binding("ctrl-alt-space"));
        assert_eq!(
            binding("alt-win-pageup"),
            Some((MOD_ALT.0 | MOD_WIN.0, 0x21))
        );
        assert_eq!(
            binding("ctrl-alt-7"),
            Some((MOD_CONTROL.0 | MOD_ALT.0, 0x37))
        );
    }

    #[test]
    fn unbindable_keys_and_bare_shift_are_rejected() {
        assert_eq!(virtual_key("f25"), None);
        assert_eq!(virtual_key("mystery"), None);
        assert_eq!(virtual_key("f12"), Some(0x7b));
        // Windows needs at least one of Ctrl, Alt or Win in a hotkey.
        let shift_only = super::super::shortcut::Shortcut {
            key: "space".into(),
            control: false,
            alt: false,
            shift: true,
            platform: false,
        };
        assert_eq!(hotkey_binding(&shift_only), None);
    }

    #[test]
    fn bgra_becomes_opaque_rgba() {
        // One blue and one red pixel, both with the zero alpha GDI returns.
        let bgra = [255, 0, 0, 0, 0, 0, 255, 0];
        assert_eq!(bgra_to_rgba(&bgra), vec![0, 0, 255, 255, 255, 0, 0, 255]);
    }

    #[test]
    fn blank_surfaces_are_detected_by_colour_only() {
        assert!(is_blank(&[0, 0, 0, 255, 0, 0, 0, 0]));
        assert!(!is_blank(&[0, 0, 1, 0]));
    }

    #[test]
    fn downscale_keeps_small_captures_and_bounds_large_ones() {
        let small = vec![7_u8; 2 * 2 * 4];
        let (pixels, width, height) = downscale_rgba(small.clone(), 2, 2, 4_096);
        assert_eq!((width, height), (2, 2));
        assert_eq!(pixels, small);

        // A 4x2 surface capped at 2 averages each 2x1 block.
        let wide = vec![
            0, 0, 0, 255, 100, 100, 100, 255, 200, 200, 200, 255, 255, 255, 255, 255, //
            0, 0, 0, 255, 100, 100, 100, 255, 200, 200, 200, 255, 255, 255, 255, 255,
        ];
        let (pixels, width, height) = downscale_rgba(wide, 4, 2, 2);
        assert_eq!((width, height), (2, 1));
        assert_eq!(pixels, vec![50, 50, 50, 255, 227, 227, 227, 255]);

        let large = vec![9_u8; 10 * 4 * 4];
        let (pixels, width, height) = downscale_rgba(large, 10, 4, 5);
        assert_eq!((width, height), (5, 2));
        assert_eq!(pixels.len(), 5 * 2 * 4);
        assert!(width.max(height) <= 5);
        assert!(pixels.iter().all(|value| *value == 9));
    }

    #[test]
    fn context_lines_match_the_macos_field_shape() {
        assert_eq!(compact("  a   b  "), "a b");
        let long = "x".repeat(MAX_UIA_VALUE_CHARS + 10);
        let compacted = compact(&long);
        assert_eq!(compacted.chars().count(), MAX_UIA_VALUE_CHARS + 1);
        assert!(compacted.ends_with('\u{2026}'));
    }
}
