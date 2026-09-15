//! Win32 boundary for the browser. A clip window owns geometry, hit testing
//! and visibility; the WebView2 controller renders straight into it. Creating
//! the environment and the controller is asynchronous on purpose: every
//! blocking WebView2 helper pumps the message loop, and a nested pump inside a
//! GPUI entity update re-enters GPUI and panics on its borrowed App. So
//! `NativePage::new` returns a pending host immediately and the completion
//! handlers finish the setup later, off any update. Every callback only
//! touches this module's own state and enqueues an event, never GPUI. No
//! page-to-engine IPC.
use super::model::{PageState, Presentation, allowed_navigation};
use gpui::{Bounds, Pixels, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_FAVICON_IMAGE_FORMAT_PNG, COREWEBVIEW2_KEY_EVENT_KIND,
    COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN, COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN,
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL, COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC,
    COREWEBVIEW2_PERMISSION_STATE_DENY, COREWEBVIEW2_WEB_ERROR_STATUS,
    COREWEBVIEW2_WEB_ERROR_STATUS_CANNOT_CONNECT,
    COREWEBVIEW2_WEB_ERROR_STATUS_HOST_NAME_NOT_RESOLVED,
    COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED,
    COREWEBVIEW2_WEB_ERROR_STATUS_SERVER_UNREACHABLE, COREWEBVIEW2_WEB_ERROR_STATUS_TIMEOUT,
    CreateCoreWebView2EnvironmentWithOptions, GetAvailableCoreWebView2BrowserVersionString,
    ICoreWebView2, ICoreWebView2_4, ICoreWebView2_15, ICoreWebView2_19, ICoreWebView2Controller,
    ICoreWebView2Environment, ICoreWebView2Environment10, ICoreWebView2EnvironmentOptions,
    ICoreWebView2Settings3,
};
use webview2_com::{
    AcceleratorKeyPressedEventHandler, CoreWebView2EnvironmentOptions,
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    DocumentTitleChangedEventHandler, DownloadStartingEventHandler, FaviconChangedEventHandler,
    GetFaviconCompletedHandler, HistoryChangedEventHandler, NavigationCompletedEventHandler,
    NavigationStartingEventHandler, NewWindowRequestedEventHandler,
    PermissionRequestedEventHandler, ProcessFailedEventHandler, SourceChangedEventHandler,
    take_pwstr,
};
use windows::Win32::Foundation::{E_POINTER, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::IStream;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetActiveWindow, GetFocus, GetKeyState, MAPVK_VK_TO_CHAR, MapVirtualKeyW,
    SetFocus, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1,
    VK_F2, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_F10, VK_F11, VK_F12, VK_HOME,
    VK_INSERT, VK_LEFT, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE,
    VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW,
    HTTRANSPARENT, IsChild, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SWP_NOCOPYBITS, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, ShowWindow, WINDOW_EX_STYLE,
    WM_ERASEBKGND, WM_NCHITTEST, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN,
};
use windows::core::w;
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR};

type Sender = tokio::sync::mpsc::Sender<NativeEvent>;

/// Mirrors the flags Wry passes: the mini menu and SmartScreen both open
/// browser UI a sidebar page must never show.
const BROWSER_ARGUMENTS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

pub(super) enum NativeEvent {
    Changed,
    Finished,
    NewTab(String),
    Key(gpui::Keystroke),
    Favicon { page: String, url: String },
}

/// A window/profile's website data. WebView2 keys its browser process off the
/// environment, so every tab of one profile shares a single environment and
/// waits on the same in-flight creation.
#[derive(Default)]
struct BrowserStore {
    environment: Option<ICoreWebView2Environment>,
    creating: bool,
    waiting: Vec<Weak<RefCell<Host>>>,
    #[allow(dead_code, reason = "recorded until WebView2 gets the preview proxy")]
    preview_hosts: std::collections::BTreeSet<String>,
}

#[derive(Clone, Default)]
pub(super) struct BrowserData(Rc<RefCell<BrowserStore>>);

impl BrowserData {
    /// Automatic preview hostnames need a per-profile HTTP proxy, which
    /// WebView2 does not offer per environment (only a process-wide
    /// `--proxy-server` browser argument). Record the host so the proxy can be
    /// wired up later; preview tabs keep working through their explicit
    /// `127.0.0.1:PORT` address in the meantime.
    pub(super) fn register_preview(&self, address: &str) {
        let Ok(url) = url::Url::parse(address) else {
            return;
        };
        let Some(host) = url.host_str() else {
            return;
        };
        if url.scheme() != "http"
            || url.port() != Some(zeron_proto::PREVIEW_PROXY_PORT)
            || !host.ends_with(".localhost")
        {
            return;
        }
        self.0.borrow_mut().preview_hosts.insert(host.into());
    }

    /// Start (or join) the asynchronous creation that ends in
    /// [`Host::attach`]. Returns as soon as the request is queued.
    fn attach(&self, host: &Rc<RefCell<Host>>) -> Result<(), String> {
        let environment = {
            let mut store = self.0.borrow_mut();
            match store.environment.clone() {
                Some(environment) => Some(environment),
                None => {
                    store.waiting.push(Rc::downgrade(host));
                    if store.creating {
                        return Ok(());
                    }
                    store.creating = true;
                    None
                }
            }
        };
        match environment {
            Some(environment) => {
                start_controller(Rc::downgrade(host), environment);
                Ok(())
            }
            None => self.create_environment().inspect_err(|_| {
                let mut store = self.0.borrow_mut();
                store.creating = false;
                store.waiting.clear();
            }),
        }
    }

    fn create_environment(&self) -> Result<(), String> {
        let data = self.clone();
        let options = CoreWebView2EnvironmentOptions::default();
        unsafe {
            options.set_additional_browser_arguments(BROWSER_ARGUMENTS.into());
        }
        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
            move |result, created| {
                let environment = result
                    .and_then(|()| created.ok_or_else(|| windows::core::Error::from(E_POINTER)));
                let waiting = {
                    let mut store = data.0.borrow_mut();
                    store.creating = false;
                    if let Ok(environment) = &environment {
                        store.environment = Some(environment.clone());
                    }
                    std::mem::take(&mut store.waiting)
                };
                for host in waiting {
                    match &environment {
                        Ok(environment) => start_controller(host, environment.clone()),
                        Err(error) => {
                            if let Some(host) = host.upgrade() {
                                let shared = host.borrow().shared.clone();
                                shared.fail(&start_failed(error));
                                host.borrow_mut().attaching = false;
                            }
                        }
                    }
                }
                Ok(())
            },
        ));
        let folder = HSTRING::from(user_data_folder().as_path());
        unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                PCWSTR::null(),
                &folder,
                &ICoreWebView2EnvironmentOptions::from(options),
                &handler,
            )
        }
        .map_err(|error| error.to_string())
    }
}

/// Ask WebView2 for a controller that renders into `host`'s clip window. The
/// completion handler runs on the UI thread from the ordinary message loop.
fn start_controller(host: Weak<RefCell<Host>>, environment: ICoreWebView2Environment) {
    let Some(strong) = host.upgrade() else {
        return;
    };
    let (clip, shared) = {
        let borrowed = strong.borrow();
        (borrowed.clip, borrowed.shared.clone())
    };
    drop(strong);
    let failed = shared.clone();
    let handler =
        CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, created| {
            let controller =
                result.and_then(|()| created.ok_or_else(|| windows::core::Error::from(E_POINTER)));
            match controller {
                Ok(controller) => match host.upgrade() {
                    // The tab is gone. Closing is mandatory, otherwise this
                    // controller keeps a browser process alive forever.
                    None => unsafe {
                        let _ = controller.Close();
                    },
                    Some(host) => {
                        let mut host = host.borrow_mut();
                        host.attaching = false;
                        if let Err(error) = host.attach(controller) {
                            host.shared.fail(&error);
                        }
                    }
                },
                Err(error) => {
                    shared.fail(&start_failed(&error));
                    if let Some(host) = host.upgrade() {
                        host.borrow_mut().attaching = false;
                    }
                }
            }
            Ok(())
        }));
    let started = unsafe {
        match environment.cast::<ICoreWebView2Environment10>() {
            Ok(environment) => {
                environment
                    .CreateCoreWebView2ControllerOptions()
                    .and_then(|options| {
                        options.SetIsInPrivateModeEnabled(true)?;
                        environment
                            .CreateCoreWebView2ControllerWithOptions(clip, &options, &handler)
                    })
            }
            Err(_) => environment.CreateCoreWebView2Controller(clip, &handler),
        }
    };
    if let Err(error) = started {
        failed.fail(&start_failed(&error));
    }
}

/// Ephemeral profiles still need a folder for the runtime's own crash and
/// cache scratch space; in-private mode keeps website data out of it.
fn user_data_folder() -> std::path::PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Zeron")
        .join("WebView2")
}

fn runtime_missing() -> String {
    "Microsoft Edge WebView2 Runtime is not installed. Install it from \
     https://go.microsoft.com/fwlink/p/?LinkId=2124703 and try again."
        .into()
}

fn start_failed(error: &windows::core::Error) -> String {
    tracing::warn!(%error, "browser runtime could not start");
    "The browser engine could not start. Try again.".into()
}

/// Page state the COM callbacks write and `state()` reads. Everything that
/// WebView2 can be asked for directly is read live instead of mirrored here.
#[derive(Default)]
struct PageStore {
    loading: Cell<bool>,
    error: RefCell<Option<String>>,
    requested_url: RefCell<Option<String>>,
    favicon: RefCell<Option<(String, Vec<u8>)>>,
    pending: Cell<bool>,
}

struct Shared {
    tx: Sender,
    page: PageStore,
}

impl Shared {
    fn changed(&self) {
        if !self.page.pending.replace(true) && self.tx.try_send(NativeEvent::Changed).is_err() {
            self.page.pending.set(false);
        }
    }
    fn fail(&self, message: &str) {
        *self.page.error.borrow_mut() = Some(message.into());
        self.page.loading.set(false);
        self.changed();
    }
}

/// Clips native content to GPUI's current paint mask. During app drags the
/// whole native subtree passes pointer events back to GPUI, without hiding it.
struct ClipState {
    dragging: Cell<bool>,
}

const CLIP_CLASS: PCWSTR = w!("ZeronBrowserClip");

fn register_clip_class() -> Result<(), String> {
    static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let ok = *REGISTERED.get_or_init(|| unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(clip_proc),
            hInstance: GetModuleHandleW(PCWSTR::null())
                .map(Into::into)
                .unwrap_or_default(),
            lpszClassName: CLIP_CLASS,
            ..Default::default()
        };
        RegisterClassW(&class) != 0
    });
    if ok {
        Ok(())
    } else {
        Err("Could not register the browser host window".into())
    }
}

unsafe extern "system" fn clip_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCHITTEST => {
            let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const ClipState;
            if !state.is_null() && unsafe { (*state).dragging.get() } {
                return LRESULT(HTTRANSPARENT as isize);
            }
        }
        // The page paints every pixel. Erasing first would flash the class
        // background through a resize.
        WM_ERASEBKGND => return LRESULT(1),
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn parent_hwnd(window: &Window) -> Result<HWND, String> {
    // GPUI has its own inherent `window_handle`, so go through the trait.
    let handle = HasWindowHandle::window_handle(window).map_err(|error| error.to_string())?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as *mut _)),
        _ => Err("The browser needs a Win32 window".into()),
    }
}

fn pressed(key: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(key.0 as i32) < 0 }
}

/// Name a virtual key the way GPUI names its own, so `set_shortcuts` strings
/// compare equal to what the accelerator hook produces.
fn key_name(vkey: u32) -> Option<String> {
    let named = match VIRTUAL_KEY(vkey as u16) {
        VK_SPACE => "space",
        VK_BACK => "backspace",
        VK_RETURN => "enter",
        VK_TAB => "tab",
        VK_UP => "up",
        VK_DOWN => "down",
        VK_RIGHT => "right",
        VK_LEFT => "left",
        VK_HOME => "home",
        VK_END => "end",
        VK_PRIOR => "pageup",
        VK_NEXT => "pagedown",
        VK_ESCAPE => "escape",
        VK_INSERT => "insert",
        VK_DELETE => "delete",
        VK_F1 => "f1",
        VK_F2 => "f2",
        VK_F3 => "f3",
        VK_F4 => "f4",
        VK_F5 => "f5",
        VK_F6 => "f6",
        VK_F7 => "f7",
        VK_F8 => "f8",
        VK_F9 => "f9",
        VK_F10 => "f10",
        VK_F11 => "f11",
        VK_F12 => "f12",
        _ => {
            let data = unsafe { MapVirtualKeyW(vkey, MAPVK_VK_TO_CHAR) };
            let character = char::from_u32(data & 0xFFFF)?;
            if character.is_control() {
                return None;
            }
            return Some(character.to_ascii_lowercase().to_string());
        }
    };
    Some(named.to_owned())
}

fn error_message(status: COREWEBVIEW2_WEB_ERROR_STATUS) -> Option<String> {
    match status {
        COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED => None,
        COREWEBVIEW2_WEB_ERROR_STATUS_CANNOT_CONNECT
        | COREWEBVIEW2_WEB_ERROR_STATUS_HOST_NAME_NOT_RESOLVED
        | COREWEBVIEW2_WEB_ERROR_STATUS_SERVER_UNREACHABLE
        | COREWEBVIEW2_WEB_ERROR_STATUS_TIMEOUT => {
            Some("Check the address and make sure your server is running, then try again.".into())
        }
        _ => Some("The connection was interrupted. Try loading this page again.".into()),
    }
}

fn string_from(read: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> Option<String> {
    let mut raw = PWSTR::null();
    read(&mut raw).ok()?;
    if raw.is_null() {
        return None;
    }
    Some(take_pwstr(raw))
}

fn read_stream(stream: &IStream) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let mut read = 0u32;
        let status = unsafe {
            stream.Read(
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                Some(&mut read),
            )
        };
        if status.is_err() || read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read as usize]);
        if bytes.len() > 1024 * 1024 {
            return None;
        }
    }
    (!bytes.is_empty()).then_some(bytes)
}

/// Ask WebView2 for the current favicon as PNG. The bytes are cached so the
/// GPUI side never has to re-fetch an icon it already has in memory.
fn request_favicon(core: &ICoreWebView2, shared: &Rc<Shared>, page: String) {
    let Ok(core) = core.cast::<ICoreWebView2_15>() else {
        return;
    };
    let Some(icon) = string_from(|out| unsafe { core.FaviconUri(out) }).filter(|s| !s.is_empty())
    else {
        return;
    };
    let shared = shared.clone();
    let url = icon.clone();
    let completed = GetFaviconCompletedHandler::create(Box::new(move |result, stream| {
        result?;
        let Some(bytes) = stream.as_ref().and_then(read_stream) else {
            return Ok(());
        };
        *shared.page.favicon.borrow_mut() = Some((url.clone(), bytes));
        let _ = shared.tx.try_send(NativeEvent::Favicon {
            page: page.clone(),
            url: url.clone(),
        });
        Ok(())
    }));
    let _ = unsafe { core.GetFavicon(COREWEBVIEW2_FAVICON_IMAGE_FORMAT_PNG, &completed) };
}

pub(super) struct NativePage(Rc<RefCell<Host>>);

/// The controller exists only after WebView2 answers. Until then the tab is a
/// sized, hidden clip window that remembers what it was asked to open.
enum Backend {
    Pending { queued_url: Option<String> },
    Ready(Ready),
}

struct Ready {
    controller: ICoreWebView2Controller,
    core: ICoreWebView2,
    tokens: Vec<(Registration, i64)>,
}

pub(super) struct Host {
    backend: Backend,
    data: BrowserData,
    attaching: bool,
    clip: HWND,
    clip_state: *mut ClipState,
    parent: HWND,
    shared: Rc<Shared>,
    shortcuts: Rc<RefCell<Vec<String>>>,
    bounds: Option<Bounds<Pixels>>,
    clip_rect: Option<RECT>,
    page_rect: Option<RECT>,
    parent_origin: Option<POINT>,
    presentation: Presentation,
    visible: bool,
    enabled: bool,
}

/// Which object a registration token belongs to, so Drop can hand it back.
enum Registration {
    NavigationStarting,
    SourceChanged,
    DocumentTitleChanged,
    HistoryChanged,
    NavigationCompleted,
    ProcessFailed,
    PermissionRequested,
    NewWindowRequested,
    DownloadStarting,
    FaviconChanged,
    AcceleratorKeyPressed,
}

impl NativePage {
    pub fn new(window: &Window, data: &BrowserData, tx: Sender) -> Result<Self, String> {
        // Without DirectComposition GPUI presents through a plain swap chain,
        // which cannot punch the hole a native child window shows through.
        if std::env::var_os("GPUI_DISABLE_DIRECT_COMPOSITION").is_some() {
            return Err("Built-in browser needs DirectComposition".into());
        }
        let mut version = PWSTR::null();
        unsafe { GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version) }
            .map_err(|_| runtime_missing())?;
        if version.is_null() {
            return Err(runtime_missing());
        }
        drop(take_pwstr(version));

        let parent = parent_hwnd(window)?;
        register_clip_class()?;
        let clip_state = Box::into_raw(Box::new(ClipState {
            dragging: Cell::new(false),
        }));
        let clip = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                CLIP_CLASS,
                PCWSTR::null(),
                WS_CHILD | WS_CLIPCHILDREN,
                0,
                0,
                0,
                0,
                Some(parent),
                None,
                GetModuleHandleW(PCWSTR::null()).map(Into::into).ok(),
                None,
            )
        };
        let clip = match clip {
            Ok(clip) => clip,
            Err(error) => {
                drop(unsafe { Box::from_raw(clip_state) });
                return Err(error.to_string());
            }
        };
        unsafe { SetWindowLongPtrW(clip, GWLP_USERDATA, clip_state as isize) };

        let host = Rc::new(RefCell::new(Host {
            backend: Backend::Pending { queued_url: None },
            data: data.clone(),
            attaching: true,
            clip,
            clip_state,
            parent,
            shared: Rc::new(Shared {
                tx,
                page: PageStore::default(),
            }),
            shortcuts: Rc::new(RefCell::new(Vec::new())),
            bounds: None,
            clip_rect: None,
            page_rect: None,
            parent_origin: None,
            presentation: Presentation::Hidden,
            visible: false,
            enabled: false,
        }));
        if let Err(error) = data.attach(&host) {
            host.borrow_mut().attaching = false;
            return Err(error);
        }
        Ok(Self(host))
    }

    pub fn handle(&self) -> Rc<RefCell<Host>> {
        self.0.clone()
    }
    /// False until the controller exists. The GPUI side must not punch its
    /// scene open before something is there to show through the hole.
    pub fn is_ready(&self) -> bool {
        self.0.borrow().is_ready()
    }
    pub fn focus_chrome(&self) {
        self.0.borrow().focus_parent();
    }
    pub fn focus_page(&self) {
        self.0.borrow().focus_page();
    }
    pub fn set_shortcuts(&self, shortcuts: Vec<String>) {
        *self.0.borrow().shortcuts.borrow_mut() = shortcuts;
    }
    pub fn present(&mut self, presentation: Presentation) {
        self.0.borrow_mut().present(presentation);
    }
    pub fn load(&self, url: &str) -> Result<(), String> {
        let retry = {
            let mut host = self.0.borrow_mut();
            host.shared.page.error.borrow_mut().take();
            host.shared.page.favicon.borrow_mut().take();
            *host.shared.page.requested_url.borrow_mut() = Some(url.into());
            host.shared.page.loading.set(true);
            match &mut host.backend {
                Backend::Ready(ready) => {
                    unsafe { ready.core.Navigate(&HSTRING::from(url)) }
                        .map_err(|error| error.to_string())?;
                    false
                }
                Backend::Pending { queued_url } => {
                    *queued_url = Some(url.into());
                    // A failed start leaves the tab pending; the toolbar's
                    // Try again lands here and starts the next attempt.
                    let retry = !host.attaching;
                    host.attaching |= retry;
                    retry
                }
            }
        };
        if retry {
            let data = self.0.borrow().data.clone();
            if let Err(error) = data.attach(&self.0) {
                self.0.borrow_mut().attaching = false;
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn reload(&self) {
        let host = self.0.borrow();
        host.shared.page.error.borrow_mut().take();
        if let Backend::Ready(ready) = &host.backend {
            let _ = unsafe { ready.core.Reload() };
        }
        host.shared.changed();
    }
    pub fn history(&self, forward: bool) {
        let host = self.0.borrow();
        if let Backend::Ready(ready) = &host.backend {
            let _ = unsafe {
                if forward {
                    ready.core.GoForward()
                } else {
                    ready.core.GoBack()
                }
            };
        }
    }
    pub fn state(&self) -> PageState {
        let host = self.0.borrow();
        host.shared.page.pending.set(false);
        let requested = host.shared.page.requested_url.borrow().clone();
        let error = host.shared.page.error.borrow().clone();
        let Backend::Ready(ready) = &host.backend else {
            return PageState {
                url: requested,
                title: String::new(),
                loading: error.is_none(),
                can_back: false,
                can_forward: false,
                error,
            };
        };
        let source = string_from(|out| unsafe { ready.core.Source(out) })
            .filter(|url| !url.is_empty() && url != "about:blank");
        let mut can_back = windows::core::BOOL(0);
        let mut can_forward = windows::core::BOOL(0);
        unsafe {
            let _ = ready.core.CanGoBack(&mut can_back);
            let _ = ready.core.CanGoForward(&mut can_forward);
        }
        PageState {
            url: requested.or(source),
            title: string_from(|out| unsafe { ready.core.DocumentTitle(out) }).unwrap_or_default(),
            loading: host.shared.page.loading.get(),
            can_back: can_back.as_bool(),
            can_forward: can_forward.as_bool(),
            error,
        }
    }
    /// WebView2 reports favicons through its own event, so this only re-asks
    /// for the current icon in case the page committed one before we listened.
    pub fn discover_favicon(&self, page: String) {
        let host = self.0.borrow();
        if host.shared.page.favicon.borrow().is_some() {
            return;
        }
        if let Backend::Ready(ready) = &host.backend {
            request_favicon(&ready.core, &host.shared, page);
        }
    }
    /// PNG bytes WebView2 already handed us for `url`, so the GPUI side never
    /// downloads an icon the runtime has in memory.
    pub fn favicon_bytes(&self, url: &str) -> Option<Vec<u8>> {
        let host = self.0.borrow();
        let favicon = host.shared.page.favicon.borrow();
        favicon
            .as_ref()
            .filter(|(cached, _)| cached == url)
            .map(|(_, bytes)| bytes.clone())
    }
}

#[cfg(feature = "browser-fixture")]
impl NativePage {
    pub fn fixture_visible(&self) -> bool {
        self.0.borrow().visible
    }
    pub fn fixture_eval(&self, script: &str) {
        let host = self.0.borrow();
        if let Backend::Ready(ready) = &host.backend {
            let completed =
                webview2_com::ExecuteScriptCompletedHandler::create(Box::new(|_, _| Ok(())));
            let _ = unsafe { ready.core.ExecuteScript(&HSTRING::from(script), &completed) };
        }
    }
}

fn register_events(
    core: &ICoreWebView2,
    controller: &ICoreWebView2Controller,
    shared: &Rc<Shared>,
    shortcuts: &Rc<RefCell<Vec<String>>>,
) -> Vec<(Registration, i64)> {
    let mut tokens = Vec::new();
    let mut token = 0i64;

    let state = shared.clone();
    let handler = NavigationStartingEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = &args {
            let uri = string_from(|out| unsafe { args.Uri(out) }).unwrap_or_default();
            if !allowed_navigation(&uri) {
                let _ = unsafe { args.SetCancel(true) };
                return Ok(());
            }
            *state.page.requested_url.borrow_mut() = Some(uri);
        }
        state.page.loading.set(true);
        state.page.error.borrow_mut().take();
        state.changed();
        Ok(())
    }));
    if unsafe { core.add_NavigationStarting(&handler, &mut token) }.is_ok() {
        tokens.push((Registration::NavigationStarting, token));
    }

    let state = shared.clone();
    let handler = SourceChangedEventHandler::create(Box::new(move |_, _| {
        // The committed URL is authoritative from here on.
        state.page.requested_url.borrow_mut().take();
        state.changed();
        Ok(())
    }));
    if unsafe { core.add_SourceChanged(&handler, &mut token) }.is_ok() {
        tokens.push((Registration::SourceChanged, token));
    }

    let state = shared.clone();
    let handler = DocumentTitleChangedEventHandler::create(Box::new(move |_, _| {
        state.changed();
        Ok(())
    }));
    if unsafe { core.add_DocumentTitleChanged(&handler, &mut token) }.is_ok() {
        tokens.push((Registration::DocumentTitleChanged, token));
    }

    let state = shared.clone();
    let handler = HistoryChangedEventHandler::create(Box::new(move |_, _| {
        state.changed();
        Ok(())
    }));
    if unsafe { core.add_HistoryChanged(&handler, &mut token) }.is_ok() {
        tokens.push((Registration::HistoryChanged, token));
    }

    let state = shared.clone();
    let handler = NavigationCompletedEventHandler::create(Box::new(move |_, args| {
        let mut success = windows::core::BOOL(0);
        let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
        if let Some(args) = &args {
            let _ = unsafe { args.IsSuccess(&mut success) };
            let _ = unsafe { args.WebErrorStatus(&mut status) };
        }
        state.page.loading.set(false);
        if success.as_bool() {
            state.changed();
            let _ = state.tx.try_send(NativeEvent::Finished);
        } else if let Some(message) = error_message(status) {
            tracing::warn!(status = status.0, "browser navigation failed");
            state.fail(&message);
        } else {
            state.changed();
        }
        Ok(())
    }));
    if unsafe { core.add_NavigationCompleted(&handler, &mut token) }.is_ok() {
        tokens.push((Registration::NavigationCompleted, token));
    }

    let state = shared.clone();
    let handler = ProcessFailedEventHandler::create(Box::new(move |_, _| {
        state.fail("The page stopped responding. Reload to continue.");
        Ok(())
    }));
    if unsafe { core.add_ProcessFailed(&handler, &mut token) }.is_ok() {
        tokens.push((Registration::ProcessFailed, token));
    }

    // A sidebar page never gets camera, microphone, location or
    // notifications. Nothing here can raise a prompt.
    let handler = PermissionRequestedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = &args {
            let _ = unsafe { args.SetState(COREWEBVIEW2_PERMISSION_STATE_DENY) };
        }
        Ok(())
    }));
    if unsafe { core.add_PermissionRequested(&handler, &mut token) }.is_ok() {
        tokens.push((Registration::PermissionRequested, token));
    }

    // Popups become app tabs. Handling the request without supplying a window
    // is what denies the popup itself.
    let state = shared.clone();
    let handler = NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = &args {
            if let Some(uri) = string_from(|out| unsafe { args.Uri(out) }) {
                if allowed_navigation(&uri) {
                    let _ = state.tx.try_send(NativeEvent::NewTab(uri));
                }
            }
            let _ = unsafe { args.SetHandled(true) };
        }
        Ok(())
    }));
    if unsafe { core.add_NewWindowRequested(&handler, &mut token) }.is_ok() {
        tokens.push((Registration::NewWindowRequested, token));
    }

    if let Ok(downloads) = core.cast::<ICoreWebView2_4>() {
        let handler = DownloadStartingEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = &args {
                let _ = unsafe { args.SetCancel(true) };
            }
            Ok(())
        }));
        if unsafe { downloads.add_DownloadStarting(&handler, &mut token) }.is_ok() {
            tokens.push((Registration::DownloadStarting, token));
        }
    }

    if let Ok(favicons) = core.cast::<ICoreWebView2_15>() {
        let state = shared.clone();
        let handler = FaviconChangedEventHandler::create(Box::new(move |sender, _| {
            let Some(core) = sender else {
                return Ok(());
            };
            let Some(page) = string_from(|out| unsafe { core.Source(out) }) else {
                return Ok(());
            };
            request_favicon(&core, &state, page);
            Ok(())
        }));
        if unsafe { favicons.add_FaviconChanged(&handler, &mut token) }.is_ok() {
            tokens.push((Registration::FaviconChanged, token));
        }
    }

    let state = shared.clone();
    let chords = shortcuts.clone();
    let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else {
            return Ok(());
        };
        let mut kind = COREWEBVIEW2_KEY_EVENT_KIND::default();
        if unsafe { args.KeyEventKind(&mut kind) }.is_err()
            || (kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
                && kind != COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN)
        {
            return Ok(());
        }
        let mut vkey = 0u32;
        if unsafe { args.VirtualKey(&mut vkey) }.is_err() {
            return Ok(());
        }
        let Some(key) = key_name(vkey) else {
            return Ok(());
        };
        let mut combo = String::new();
        if pressed(VK_CONTROL) {
            combo.push_str("ctrl-");
        }
        if pressed(VK_MENU) {
            combo.push_str("alt-");
        }
        if pressed(VK_SHIFT) {
            combo.push_str("shift-");
        }
        combo.push_str(&key);
        let Ok(keystroke) = gpui::Keystroke::parse(&combo) else {
            return Ok(());
        };
        let browser_key = matches!(
            combo.as_str(),
            "ctrl-l"
                | "ctrl-t"
                | "ctrl-w"
                | "ctrl-["
                | "ctrl-]"
                | "ctrl-shift-r"
                | "ctrl-k"
                | "ctrl-,"
        );
        let app_key = chords
            .borrow()
            .iter()
            .any(|chord| gpui::Keystroke::parse(chord).is_ok_and(|chord| chord == keystroke));
        if (browser_key || app_key) && state.tx.try_send(NativeEvent::Key(keystroke)).is_ok() {
            let _ = unsafe { args.SetHandled(true) };
        }
        Ok(())
    }));
    if unsafe { controller.add_AcceleratorKeyPressed(&handler, &mut token) }.is_ok() {
        tokens.push((Registration::AcceleratorKeyPressed, token));
    }
    tokens
}

impl Ready {
    fn close(&mut self) {
        unsafe {
            let _ = self.core.Stop();
            for (registration, token) in self.tokens.drain(..) {
                let _ = match registration {
                    Registration::NavigationStarting => self.core.remove_NavigationStarting(token),
                    Registration::SourceChanged => self.core.remove_SourceChanged(token),
                    Registration::DocumentTitleChanged => {
                        self.core.remove_DocumentTitleChanged(token)
                    }
                    Registration::HistoryChanged => self.core.remove_HistoryChanged(token),
                    Registration::NavigationCompleted => {
                        self.core.remove_NavigationCompleted(token)
                    }
                    Registration::ProcessFailed => self.core.remove_ProcessFailed(token),
                    Registration::PermissionRequested => {
                        self.core.remove_PermissionRequested(token)
                    }
                    Registration::NewWindowRequested => self.core.remove_NewWindowRequested(token),
                    Registration::DownloadStarting => self
                        .core
                        .cast::<ICoreWebView2_4>()
                        .and_then(|core| core.remove_DownloadStarting(token)),
                    Registration::FaviconChanged => self
                        .core
                        .cast::<ICoreWebView2_15>()
                        .and_then(|core| core.remove_FaviconChanged(token)),
                    Registration::AcceleratorKeyPressed => {
                        self.controller.remove_AcceleratorKeyPressed(token)
                    }
                };
            }
            // Mandatory: an open controller keeps a browser process alive.
            let _ = self.controller.Close();
        }
    }
}

impl Host {
    fn is_ready(&self) -> bool {
        matches!(self.backend, Backend::Ready(_))
    }

    /// Finish setup once WebView2 hands over the controller. Runs on the UI
    /// thread from a completion handler, never inside a GPUI update.
    fn attach(&mut self, controller: ICoreWebView2Controller) -> Result<(), String> {
        let core = unsafe { controller.CoreWebView2() }.map_err(|error| error.to_string())?;
        unsafe {
            let _ = controller.SetIsVisible(false);
            // GPUI owns every browser chord; WebView2 must not swallow them.
            if let Ok(settings) = core
                .Settings()
                .and_then(|settings| settings.cast::<ICoreWebView2Settings3>())
            {
                let _ = settings.SetAreBrowserAcceleratorKeysEnabled(false);
            }
        }
        let tokens = register_events(&core, &controller, &self.shared, &self.shortcuts);
        let previous = std::mem::replace(
            &mut self.backend,
            Backend::Ready(Ready {
                controller,
                core,
                tokens,
            }),
        );
        let queued = match previous {
            Backend::Pending { queued_url } => queued_url,
            Backend::Ready(mut ready) => {
                ready.close();
                None
            }
        };
        self.apply_bounds();
        self.update_visibility();
        if let Some(url) = queued {
            if let Backend::Ready(ready) = &self.backend {
                let _ = unsafe { ready.core.Navigate(&HSTRING::from(url.as_str())) };
            }
        }
        self.shared.changed();
        Ok(())
    }

    pub fn sync(
        &mut self,
        bounds: Bounds<Pixels>,
        mask: Bounds<Pixels>,
        dragging: bool,
        resize_inset: Pixels,
        scale: f32,
    ) {
        if !self.clip_state.is_null() {
            unsafe { (*self.clip_state).dragging.set(dragging) };
        }
        let visible = bounds.intersect(&mask);
        let inset = f32::from(resize_inset).max(0.0);
        let physical = |value: f32| (value * scale).round() as i32;
        let left = physical(f32::from(visible.origin.x) + inset);
        let top = physical(f32::from(visible.origin.y));
        let clip_rect = RECT {
            left,
            top,
            right: physical(f32::from(visible.origin.x) + f32::from(visible.size.width)).max(left),
            bottom: physical(f32::from(visible.origin.y) + f32::from(visible.size.height)).max(top),
        };
        let mut moved = false;
        if self.clip_rect != Some(clip_rect) {
            self.clip_rect = Some(clip_rect);
            moved = true;
            unsafe {
                let _ = SetWindowPos(
                    self.clip,
                    None,
                    clip_rect.left,
                    clip_rect.top,
                    clip_rect.right - clip_rect.left,
                    clip_rect.bottom - clip_rect.top,
                    SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOCOPYBITS,
                );
            }
        }
        // The page frame follows `bounds`, not the mask: it only ever moves
        // inside the clip window when the mask scrolls, so the viewport size
        // stays put and the page does not reflow.
        let page_left = physical(f32::from(bounds.origin.x)) - clip_rect.left;
        let page_top = physical(f32::from(bounds.origin.y)) - clip_rect.top;
        let page_rect = RECT {
            left: page_left,
            top: page_top,
            right: page_left + physical(f32::from(bounds.size.width)).max(0),
            bottom: page_top + physical(f32::from(bounds.size.height)).max(0),
        };
        if self.page_rect != Some(page_rect) {
            self.bounds = Some(bounds);
            self.page_rect = Some(page_rect);
            moved = true;
            self.apply_bounds();
        }
        // Nothing else tells WebView2 that its window moved on screen: menus,
        // IME candidates and drag targets would land at the old position.
        let mut origin = POINT { x: 0, y: 0 };
        let tracked = unsafe { ClientToScreen(self.parent, &mut origin) }.as_bool();
        if moved || (tracked && self.parent_origin != Some(origin)) {
            if tracked {
                self.parent_origin = Some(origin);
            }
            if let Backend::Ready(ready) = &self.backend {
                let _ = unsafe { ready.controller.NotifyParentWindowPositionChanged() };
            }
        }
        self.update_visibility();
    }

    fn apply_bounds(&self) {
        let (Backend::Ready(ready), Some(rect)) = (&self.backend, self.page_rect) else {
            return;
        };
        let _ = unsafe { ready.controller.SetBounds(rect) };
    }

    fn has_focus(&self) -> bool {
        let focus = unsafe { GetFocus() };
        !focus.is_invalid()
            && (focus == self.clip || unsafe { IsChild(self.clip, focus) }.as_bool())
    }

    fn focus_parent(&self) {
        unsafe {
            let _ = SetFocus(Some(self.parent));
        }
    }

    /// Give the page keyboard focus once GPUI decided it owns the pointer.
    pub fn focus_page(&self) {
        if self.presentation != Presentation::Live || !self.visible {
            return;
        }
        if let Backend::Ready(ready) = &self.backend {
            let _ = unsafe {
                ready
                    .controller
                    .MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC)
            };
        }
    }

    fn set_memory_level(&self, level: COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL) {
        if let Backend::Ready(ready) = &self.backend
            && let Ok(core) = ready.core.cast::<ICoreWebView2_19>()
        {
            let _ = unsafe { core.SetMemoryUsageTargetLevel(level) };
        }
    }

    fn update_visibility(&mut self) {
        let visible = self.is_ready()
            && self.presentation != Presentation::Hidden
            && self.shared.page.error.borrow().is_none()
            && self
                .clip_rect
                .is_some_and(|rect| rect.right > rect.left && rect.bottom > rect.top);
        // A disabled window hands its mouse messages to the parent, so GPUI
        // keeps pointer ownership for the whole drag without a hidden page.
        let enabled = visible && self.presentation == Presentation::Live;
        // Hiding or disabling the window that owns the keyboard focus drops
        // the thread's focus to nothing, and nothing puts it back. Hand it to
        // GPUI first, while the page still officially has it.
        if (!visible || !enabled) && self.has_focus() {
            self.focus_parent();
        }
        if self.visible != visible {
            self.visible = visible;
            unsafe {
                let _ = ShowWindow(self.clip, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
            if let Backend::Ready(ready) = &self.backend {
                let _ = unsafe { ready.controller.SetIsVisible(visible) };
            }
            self.set_memory_level(if visible {
                COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
            } else {
                COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
            });
        }
        if self.enabled != enabled {
            self.enabled = enabled;
            unsafe {
                let _ = EnableWindow(self.clip, enabled);
            }
        }
        // Windows clears the focus without telling anyone, so a keystroke
        // after a tab switch would reach no window at all.
        if !enabled
            && unsafe { GetFocus() }.is_invalid()
            && unsafe { GetActiveWindow() } == self.parent
        {
            self.focus_parent();
        }
    }

    fn present(&mut self, presentation: Presentation) {
        self.presentation = presentation;
        if !self.clip_state.is_null() {
            unsafe {
                (*self.clip_state)
                    .dragging
                    .set(presentation == Presentation::Passthrough)
            };
        }
        self.update_visibility();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if self.has_focus() {
            self.focus_parent();
        }
        if let Backend::Ready(ready) = &mut self.backend {
            ready.close();
        }
        unsafe {
            SetWindowLongPtrW(self.clip, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.clip);
        }
        if !self.clip_state.is_null() {
            drop(unsafe { Box::from_raw(self.clip_state) });
            self.clip_state = std::ptr::null_mut();
        }
    }
}
