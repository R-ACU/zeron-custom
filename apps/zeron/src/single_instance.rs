//! Windows single-instance guard and `zeron://` deep-link forwarding.
//!
//! Only the headed default command uses this (see `main.rs`): `zeron
//! headless`, `zeron status`, `zeron daemon ...`, etc. never call
//! [`guard`], so a background `zeron headless` engine and a headed instance
//! (or several headless engines) can coexist without tripping the guard.
//!
//! A second headed launch acquires the same named mutex as the first,
//! notices it already exists, finds the first instance's main window, hands
//! it the deep-link URL (if any) over `WM_COPYDATA`, brings it to the
//! foreground, and exits. The receiving side lives in
//! `vendor/zui/crates/gpui_windows/src/events.rs` (implemented alongside
//! this change) and reacts to a `WM_COPYDATA` message whose `dwData` is
//! `ZERON_OPEN_URL_MAGIC`.

use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, FALSE, HANDLE, HWND, LPARAM, TRUE, WPARAM,
};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, EnumWindows, GetClassNameW, GetWindowThreadProcessId,
    SendMessageW, SetForegroundWindow, WM_COPYDATA,
};
use windows_sys::core::BOOL;

/// `Local\` scope keeps the mutex in the current user session (Windows
/// convention for "one instance per logged-in user"; Terminal Services would
/// otherwise share it across sessions under `Global\`).
const MUTEX_NAME: &str = "Local\\Zeron.SingleInstance";

/// GPUI's top-level window class on Windows
/// (`vendor/zui/crates/gpui_windows/src/window.rs`, `WINDOW_CLASS_NAME`).
/// Kept as a literal here since `zeron` depends on `gpui`, not
/// `gpui_windows`, so the constant itself is not importable.
const MAIN_WINDOW_CLASS_NAME: &str = "Zed::Window";

/// The exe name of the `zeron` binary (`apps/zeron/Cargo.toml`'s `[[bin]]
/// name`), used to make sure a matching window class actually belongs to a
/// `zeron` process and not some unrelated window that happens to reuse the
/// class name.
const PROCESS_IMAGE_NAME: &str = "zeron.exe";

/// Marks a `WM_COPYDATA` payload as a Zeron deep-link URL. Mirrors
/// `gpui_windows::events::ZERON_OPEN_URL_MAGIC`
/// (`vendor/zui/crates/gpui_windows/src/events.rs`) — duplicated here because
/// `zeron` depends on `gpui`, not `gpui_windows` directly, so the constant
/// cannot be imported. Keep both definitions in sync if either changes.
const ZERON_OPEN_URL_MAGIC: u32 = 0x5A4F_5055;

/// Outcome of [`guard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SingleInstance {
    /// No other headed instance is running (or its window could not be
    /// found, e.g. a daemon-only mutex holder, or one still starting up):
    /// the caller should start the headed app normally.
    StartNormally,
    /// A running instance's window was found and, if a URL was given,
    /// handed the deep link; the caller must exit immediately without
    /// starting a second UI.
    ForwardedToRunningInstance,
}

/// Acquires the process-wide single-instance mutex. If this is the first
/// headed instance, keeps holding the mutex (leaking the handle is
/// intentional: Windows closes it automatically on process exit) and
/// returns [`SingleInstance::StartNormally`]. If another instance already
/// holds it, tries to forward `url` (if any) to that instance's main window
/// and bring it to the foreground.
pub fn guard(url: Option<&str>) -> SingleInstance {
    let name = to_wide_null(MUTEX_NAME);
    // SAFETY: `name` is a valid, null-terminated wide string that outlives
    // the call; no security attributes are requested (null).
    let handle: HANDLE = unsafe { CreateMutexW(ptr::null(), TRUE, name.as_ptr()) };
    if handle.is_null() {
        // CreateMutexW itself failed; never block a normal launch on it.
        return SingleInstance::StartNormally;
    }
    // Per MSDN: even a successful call can return a handle to a
    // pre-existing mutex, distinguishable only via GetLastError.
    let already_running = unsafe { windows_sys::Win32::Foundation::GetLastError() }
        == ERROR_ALREADY_EXISTS;
    if !already_running {
        // We are the first instance. Do not close `handle` — it must stay
        // open for the lifetime of the process so the next launch's
        // CreateMutexW observes ERROR_ALREADY_EXISTS.
        return SingleInstance::StartNormally;
    }
    // Another instance already holds the mutex; we never owned it, so this
    // handle is just an extra reference to the same kernel object. Close it,
    // we do not need to hold it open.
    unsafe { CloseHandle(handle) };

    match find_main_window() {
        Some(hwnd) => {
            if let Some(url) = url {
                forward_url(hwnd, url);
            }
            let mut pid = 0u32;
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut pid);
                AllowSetForegroundWindow(pid);
                SetForegroundWindow(hwnd);
            }
            SingleInstance::ForwardedToRunningInstance
        }
        // Mutex exists but no matching window: daemon-only mutex holder, or
        // the other instance is still starting up. Fall back to a normal
        // launch rather than silently doing nothing.
        None => SingleInstance::StartNormally,
    }
}

/// Sends `url` to `hwnd` via `WM_COPYDATA`. The payload is the URL encoded
/// as raw UTF-16 code units (see [`encode_url_utf16`]), matching what
/// `gpui_windows::events` is expected to decode on the receiving side.
fn forward_url(hwnd: HWND, url: &str) {
    let payload = encode_url_utf16(url);
    let mut data = COPYDATASTRUCT {
        dwData: ZERON_OPEN_URL_MAGIC as usize,
        cbData: payload.len() as u32,
        lpData: payload.as_ptr() as *mut core::ffi::c_void,
    };
    // SAFETY: `data` (and the `payload` buffer it points at) lives until
    // SendMessageW returns; WM_COPYDATA is delivered synchronously, so the
    // receiving window has already copied out of `lpData` by then. `wparam`
    // (conventionally the sending window) is 0: this process has no window
    // of its own, and the message is a one-way notification.
    unsafe {
        SendMessageW(
            hwnd,
            WM_COPYDATA,
            0 as WPARAM,
            &mut data as *mut COPYDATASTRUCT as LPARAM,
        );
    }
}

/// Encodes `url` as raw UTF-16 code units, little-endian, two bytes per
/// unit, with no null terminator — the byte count is therefore always even.
/// This is exactly the payload `WM_COPYDATA`'s `cbData`/`lpData` carry.
fn encode_url_utf16(url: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(url.len() * 2);
    for unit in url.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// UTF-16LE bytes (as produced by [`encode_url_utf16`]) back to a `String`.
/// Only used by tests here; the real decoder lives on the receiving side in
/// `gpui_windows::events`.
#[cfg(test)]
fn decode_url_utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// `s` as a null-terminated UTF-16 buffer, for APIs taking `PCWSTR`.
fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Finds the first top-level window that both uses GPUI's window class and
/// belongs to a `zeron.exe` process.
fn find_main_window() -> Option<HWND> {
    let mut ctx = FindContext { found: None };
    // SAFETY: `enum_windows_proc` only touches `ctx` through the `LPARAM`
    // pointer for the duration of this call, and `ctx` outlives it.
    unsafe {
        EnumWindows(Some(enum_windows_proc), &mut ctx as *mut FindContext as LPARAM);
    }
    ctx.found
}

struct FindContext {
    found: Option<HWND>,
}

/// `EnumWindows` callback: records the first matching window and stops
/// enumeration (returns `FALSE`); continues (`TRUE`) otherwise.
unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = unsafe { &mut *(lparam as *mut FindContext) };
    if is_zeron_main_window(hwnd) {
        ctx.found = Some(hwnd);
        FALSE
    } else {
        TRUE
    }
}

fn is_zeron_main_window(hwnd: HWND) -> bool {
    class_name_is(hwnd, MAIN_WINDOW_CLASS_NAME) && process_image_name_is(hwnd, PROCESS_IMAGE_NAME)
}

fn class_name_is(hwnd: HWND, expected: &str) -> bool {
    let mut buf = [0u16; 256];
    // SAFETY: `buf` is valid for `buf.len()` wide chars for the call.
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 0 {
        return false;
    }
    String::from_utf16_lossy(&buf[..len as usize]) == expected
}

fn process_image_name_is(hwnd: HWND, expected: &str) -> bool {
    let mut pid = 0u32;
    // SAFETY: `hwnd` came from EnumWindows and `pid` is valid for the write.
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return false;
    }
    // SAFETY: no resources are borrowed across this call besides `pid`.
    let process: HANDLE =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if process.is_null() {
        return false;
    }
    let mut buf = [0u16; 260]; // MAX_PATH
    let mut size = buf.len() as u32;
    // SAFETY: `process` is a valid handle just opened above; `buf`/`size`
    // describe a valid, correctly sized output buffer.
    let ok =
        unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut size) };
    unsafe { CloseHandle(process) };
    if ok == 0 {
        return false;
    }
    let path = String::from_utf16_lossy(&buf[..size as usize]);
    path.rsplit(['\\', '/'])
        .next()
        .is_some_and(|file| file.eq_ignore_ascii_case(expected))
}

#[cfg(test)]
mod tests {
    use super::{decode_url_utf16, encode_url_utf16};

    #[test]
    fn encodes_ascii_url_as_even_length_utf16le() {
        let bytes = encode_url_utf16("zeron://open/chat/abc");
        assert_eq!(bytes.len() % 2, 0);
        assert_eq!(bytes.len(), "zeron://open/chat/abc".len() * 2);
    }

    #[test]
    fn round_trips_ascii_and_unicode_and_surrogate_pairs() {
        for url in [
            "zeron://open/chat/abc",
            "zeron://open/chat/äöü",
            "zeron://open/chat/🚀",
            "",
        ] {
            let bytes = encode_url_utf16(url);
            assert_eq!(bytes.len() % 2, 0, "payload for {url:?} must be even-length");
            assert_eq!(decode_url_utf16(&bytes), url);
        }
    }
}
