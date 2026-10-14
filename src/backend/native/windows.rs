//! Windows: a WebView2 child HWND inside a clip-container HWND under GPUI's
//! window (EMBEDS.md §10.2).
//!
//! The container's rect is the visible clip (Windows clips child windows to
//! their parent) and the webview keeps the block's full size, sliding inside
//! it with a negative offset, so scrolling never relayouts the page. This is
//! the macOS container pattern with HWNDs.
//!
//! GPUI must present through a flip-model swapchain (`prepare_process`): its
//! default DirectComposition target is created topmost and draws above every
//! child HWND (T44), so without the switch the surface would be created,
//! receive input, and never be visible.
//!
//! Overlays freeze the surface: the container is hidden so GPUI's poster and
//! menus are never occluded, as on macOS.

use std::sync::{Arc, Mutex};

use gpui_kit::Window;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND, COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND_SEPARATOR,
    ICoreWebView2, ICoreWebView2_11, ICoreWebView2ContextMenuItemCollection,
    ICoreWebView2ContextMenuRequestedEventArgs,
};
use webview2_com::{ContextMenuRequestedEventHandler, ProcessFailedEventHandler, take_pwstr};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM,
};
#[cfg(feature = "test-support")]
use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, GetForegroundWindow,
    HCURSOR, HICON, HWND_TOP, IsChild, RegisterClassExW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SetWindowPos, ShowWindow, WINDOW_EX_STYLE, WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN,
    WS_CLIPSIBLINGS,
};
use windows::core::{BOOL, Interface as _, PCWSTR, PWSTR, w};
use wry::raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle,
};

use super::NativeSurface;
use crate::debug::debug_log;
use crate::spec::{ContextMenu, Error, Placement, Result, SurfaceId, SurfaceSpec, WebViewEvent};

/// GPUI's window HWND; every surface's clip container is created under it.
#[derive(Clone)]
pub(super) struct ParentWindow {
    hwnd: HWND,
}

impl ParentWindow {
    pub(super) fn from_window(window: &Window) -> Result<Self> {
        // `Window` has an inherent `window_handle` returning GPUI's own
        // handle type, so the raw-window-handle trait is called explicitly.
        let raw = HasWindowHandle::window_handle(window)
            .map_err(|error| Error::Platform(format!("window handle: {error}")))?;
        let RawWindowHandle::Win32(handle) = raw.as_raw() else {
            return Err(Error::Platform("not a Win32 window".into()));
        };
        Ok(Self {
            hwnd: HWND(handle.hwnd.get() as *mut _),
        })
    }

    /// The window's current DPI scale, read live at every `place` so dragging
    /// the window to a monitor with a different DPI re-maps the surfaces.
    pub(super) fn scale(&self) -> f64 {
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        if dpi == 0 { 1.0 } else { dpi as f64 / 96.0 }
    }

    /// Makes GPUI's window the Win32 focus window again.
    ///
    /// Call through [`return_focus`], not directly: `SetFocus` activates a
    /// background window, so an unconditional call would pop GPUI to the front
    /// when a surface in a background window is torn down.
    pub(super) fn focus(&self) {
        // Fails while the window is minimized; nothing to do then.
        let _ = unsafe { SetFocus(Some(self.hwnd)) };
    }
}

/// Returns keyboard focus to GPUI only when this surface plausibly owns it.
///
/// `SetFocus` activates a window, so calling it for a background window's
/// surface (scroll-out, eviction, teardown) would steal activation from
/// whatever the user is really using. Two gates:
///
/// * GPUI's window must be the foreground window.
/// * When the calling thread can see a focused window (`GetFocus`), it must be
///   this surface's container or inside it. It is `NULL` while the WebView2
///   browser thread owns the keyboard — that is this surface too, so the
///   foreground gate is enough there.
pub(super) fn return_focus(parent: &ParentWindow, container: Option<HWND>, id: SurfaceId) {
    let foreground_matches = unsafe { GetForegroundWindow() } == parent.hwnd;
    let focus = unsafe { GetFocus() };
    let focus_ours = container.is_none_or(|container| {
        focus.0.is_null() || focus == container || unsafe { IsChild(container, focus) }.as_bool()
    });
    let hand_back = foreground_matches && focus_ours;
    debug_log!(
        "return_focus {id}: foreground_matches={foreground_matches} focus_ours={focus_ours} hand_back={hand_back}"
    );
    if hand_back {
        parent.focus();
    }
}

/// A handle wry can build a child into: the clip container HWND we create.
struct ContainerHandle(HWND);

impl HasWindowHandle for ContainerHandle {
    fn window_handle(&self) -> std::result::Result<WindowHandle<'_>, HandleError> {
        let hwnd =
            std::num::NonZeroIsize::new(self.0.0 as isize).ok_or(HandleError::Unavailable)?;
        let handle = Win32WindowHandle::new(hwnd);
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(handle)) })
    }
}

/// Creates a live Windows surface: clip container + WebView2 child, policy
/// applied.
pub(super) fn create_surface<'a>(
    id: SurfaceId,
    spec: &SurfaceSpec,
    parent: &ParentWindow,
    events: &Arc<Mutex<Vec<WebViewEvent>>>,
    context: &'a mut wry::WebContext,
) -> Result<NativeSurface> {
    ensure_clip_class()?;
    // No WS_VISIBLE: `place` shows the container when the surface is placed.
    let container = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("gpui_webview_clip"),
            PCWSTR::null(),
            WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            Some(parent.hwnd),
            None,
            Some(module_handle()),
            None,
        )
    }
    .map_err(|error| Error::Platform(format!("create clip container: {error}")))?;

    let builder = wry::WebViewBuilder::new_with_web_context(context);
    let result = build_builder(builder, id, spec, events).and_then(|builder| {
        builder
            .build_as_child(&ContainerHandle(container))
            .map_err(|error| Error::Platform(format!("wry: {error}")))
    });
    let webview = match result {
        Ok(webview) => webview,
        Err(error) => {
            // A failed build may leave wry's own child window behind; the
            // container's destruction takes it down.
            unsafe {
                let _ = DestroyWindow(container);
            }
            return Err(error);
        }
    };
    attach_handlers(id, events, &webview);
    debug_log!("create {id}: container={container:?}");
    Ok(NativeSurface {
        webview,
        container: Some(container),
        lowered: false,
    })
}

/// Applies geometry/visibility through the container + webview.
///
/// The container is the visible clip and lives in GPUI's coordinates; the
/// webview keeps the block's full size and is offset inside the container, so
/// the page never relayouts while scrolling. Both rectangles come from
/// [`super::geometry::physical_placement`], so their edges stay aligned at
/// fractional scales.
pub(super) fn place(
    surface: &mut NativeSurface,
    id: SurfaceId,
    at: Placement,
    parent: &ParentWindow,
) {
    // A lowered surface needs the transparent hole; until that lands, hide it
    // so GPUI overlays are never occluded.
    let visible = at.visible && !at.overlay;
    let Some(container) = surface.container else {
        let _ = surface.webview.set_visible(visible);
        return;
    };
    if !visible {
        let _ = surface.webview.set_visible(false);
        unsafe {
            let _ = ShowWindow(container, SW_HIDE);
        }
        surface.lowered = at.overlay;
        debug_log!(
            "place {id}: hidden (visible={} overlay={})",
            at.visible,
            at.overlay
        );
        return;
    }
    let scale = parent.scale();
    let physical = super::geometry::physical_placement(
        (
            at.rect.origin.x.as_f32() as f64,
            at.rect.origin.y.as_f32() as f64,
            at.rect.size.width.as_f32() as f64,
            at.rect.size.height.as_f32() as f64,
        ),
        (
            at.clip.origin.x.as_f32() as f64,
            at.clip.origin.y.as_f32() as f64,
            at.clip.size.width.as_f32() as f64,
            at.clip.size.height.as_f32() as f64,
        ),
        scale,
    );
    let (container_x, container_y, container_width, container_height) = physical.container;
    unsafe {
        // Synchronous (the caller runs on GPUI's thread), so the clip moves
        // with the same frame that painted the rest of the UI.
        let _ = SetWindowPos(
            container,
            Some(HWND_TOP),
            container_x,
            container_y,
            container_width,
            container_height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
    let (webview_x, webview_y, webview_width, webview_height) = physical.webview;
    let _ = surface.webview.set_bounds(wry::Rect {
        position: wry::dpi::PhysicalPosition::new(webview_x as f64, webview_y as f64).into(),
        size: wry::dpi::PhysicalSize::new(webview_width as f64, webview_height as f64).into(),
    });
    let _ = surface.webview.set_visible(true);
    surface.lowered = false;
    debug_log!(
        "place {id}: scale={scale} container=({container_x},{container_y},{container_width}x{container_height}) \
         webview=({webview_x},{webview_y},{webview_width}x{webview_height})"
    );
}

/// Tears the surface down: give focus back to GPUI first (if this surface has
/// it), drop the webview, then destroy the container.
pub(super) fn destroy(surface: NativeSurface, id: SurfaceId, parent: Option<&ParentWindow>) {
    let container = surface.container;
    debug_log!("destroy {id}: container={container:?}");
    if let Some(parent) = parent {
        return_focus(parent, container, id);
    }
    drop(surface.webview);
    if let Some(container) = container {
        unsafe {
            let _ = DestroyWindow(container);
        }
    }
}

/// Bounded [`webview2_com::wait_with_pump`]: pumps this thread's messages until
/// `rx` yields or `timeout` passes. `wait_with_pump` blocks in `GetMessageA`
/// forever when a completion never arrives (a wedged WebView2 renderer), which
/// would hang the app instead of degrading to the poster.
#[cfg(feature = "test-support")]
fn wait_with_pump_timeout<T>(
    rx: &std::sync::mpsc::Receiver<T>,
    timeout: std::time::Duration,
) -> Option<T> {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    let deadline = std::time::Instant::now() + timeout;
    let mut message = MSG::default();
    loop {
        if let Ok(result) = rx.try_recv() {
            return Some(result);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        // Non-blocking pump; the completion is posted to this thread's queue.
        let pending = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool();
        if pending {
            let _ = unsafe { TranslateMessage(&message) };
            let _ = unsafe { DispatchMessageW(&message) };
        } else {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// A PNG snapshot of the page, rendered by WebView2 itself (`CapturePreview`):
/// no desktop compositing is involved, so it works in any session. The crate's
/// Windows E2E uses it to prove the engine paints where GDI readback is blind
/// to the desktop's GPU composition.
#[cfg(feature = "test-support")]
pub(super) fn page_snapshot(webview: &wry::WebView) -> Option<Vec<u8>> {
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG;
    use webview2_com::CapturePreviewCompletedHandler;
    use wry::WebViewExtWindows as _;
    use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
    use windows::Win32::System::Com::{STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET};

    let core = webview.webview();
    let stream = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) }.ok()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let handler = CapturePreviewCompletedHandler::create(Box::new(move |error_code| {
        let _ = tx.send(error_code);
        Ok(())
    }));
    unsafe {
        core.CapturePreview(
            COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
            &stream,
            &handler,
        )
    }
    .ok()?;
    // The completion arrives on this thread's message loop; pump it with a
    // bound, or a wedged renderer would hang the caller forever (T45: never
    // from a paint).
    let error_code = wait_with_pump_timeout(&rx, std::time::Duration::from_millis(1_000))?;
    error_code.ok()?;

    let mut stat = STATSTG::default();
    unsafe { stream.Stat(&mut stat, STATFLAG_NONAME) }.ok()?;
    let mut bytes = vec![0_u8; stat.cbSize as usize];
    let mut read = 0_u32;
    unsafe { stream.Seek(0, STREAM_SEEK_SET, None) }.ok()?;
    // `IStream::Read` returns a bare HRESULT in windows 0.62.
    let read_result = unsafe {
        stream.Read(
            bytes.as_mut_ptr() as *mut _,
            stat.cbSize as u32,
            Some(&mut read),
        )
    };
    read_result.ok().ok()?;
    bytes.truncate(read as usize);
    Some(bytes)
}

/// Re-applies the parts of a policy that can change on navigation. WebView2
/// reads these settings at load time, so toggling them here affects the
/// *next* load, as on macOS.
pub(super) fn apply_policy(webview: &wry::WebView, spec: &SurfaceSpec) {
    use wry::WebViewExtWindows as _;
    let Ok(settings) = (unsafe { webview.webview().Settings() }) else {
        return;
    };
    unsafe {
        let _ = settings.SetIsScriptEnabled(spec.policy.scripts);
        let _ = settings
            .SetAreDefaultContextMenusEnabled(spec.policy.engine.context_menu != ContextMenu::Off);
        let _ = settings.SetAreDevToolsEnabled(spec.policy.engine.devtools);
    }
}

/// Builds the wry builder for Windows: the shared pieces from
/// [`super::base_builder`] plus the WebView2 settings and handlers.
fn build_builder<'a>(
    builder: wry::WebViewBuilder<'a>,
    id: SurfaceId,
    spec: &SurfaceSpec,
    events: &Arc<Mutex<Vec<WebViewEvent>>>,
) -> Result<wry::WebViewBuilder<'a>> {
    use wry::WebViewBuilderExtWindows as _;
    let mut builder = super::base_builder(builder, id, spec, events, true)?;
    if !spec.policy.scripts {
        builder = builder.with_javascript_disabled();
    }
    if spec.policy.engine.context_menu == ContextMenu::Off {
        builder = builder.with_default_context_menus(false);
    }
    if !spec.policy.engine.media_stream {
        // Camera and microphone are the media-capture permissions; everything
        // else keeps WebView2's default, a prompt the embed never uses.
        builder = builder.with_permission_handler(|kind| match kind {
            wry::PermissionKind::Camera | wry::PermissionKind::Microphone => {
                wry::PermissionResponse::Deny
            }
            _ => wry::PermissionResponse::Default,
        });
    }
    if !spec.policy.engine.popups {
        let popup_events = events.clone();
        builder = builder.with_new_window_req_handler(move |url, _features| {
            // The same rule as the navigation handler: a denied window that
            // points somewhere real opens externally.
            if url.starts_with("http://") || url.starts_with("https://") {
                if let Ok(mut events) = popup_events.lock() {
                    events.push(WebViewEvent::OpenExternal { id, url });
                }
            }
            wry::NewWindowResponse::Deny
        });
    }
    Ok(builder)
}

/// Attaches the handlers wry does not install for this crate: `ProcessFailed`
/// (wry's terminate handler is Darwin-only) and the context-menu trim.
fn attach_handlers(id: SurfaceId, events: &Arc<Mutex<Vec<WebViewEvent>>>, webview: &wry::WebView) {
    use wry::WebViewExtWindows as _;
    let core = webview.webview();
    let mut token = 0_i64;
    {
        let events = events.clone();
        let _ = unsafe {
            core.add_ProcessFailed(
                &ProcessFailedEventHandler::create(Box::new(move |_, _| {
                    if let Ok(mut events) = events.lock() {
                        events.push(WebViewEvent::Crashed { id });
                    }
                    Ok(())
                })),
                &mut token,
            )
        };
    }
    // `Trimmed` (the default) keeps the engine's menu minus what this crate
    // cannot honour; `Off` disables menus at the builder, so the event never
    // fires there.
    if let Ok(core11) = core.cast::<ICoreWebView2_11>() {
        let _ = unsafe {
            core11.add_ContextMenuRequested(
                &ContextMenuRequestedEventHandler::create(Box::new(move |sender, args| {
                    if let Some(args) = args {
                        trim_context_menu(&args, devtools_enabled(sender.as_ref()));
                    }
                    Ok(())
                })),
                &mut token,
            )
        };
    }
}

/// The live devtools setting; `apply_policy` keeps it in step with the spec
/// on navigation, so a reused engine trims correctly.
fn devtools_enabled(core: Option<&ICoreWebView2>) -> bool {
    let Some(core) = core else {
        return false;
    };
    let Ok(settings) = (unsafe { core.Settings() }) else {
        return false;
    };
    let mut enabled = BOOL::default();
    unsafe { settings.AreDevToolsEnabled(&mut enabled) }.is_ok() && enabled.as_bool()
}

/// Removes what this crate cannot honour (new windows, downloads, engine
/// fullscreen, "Inspect Element" without devtools) and tidies the separators;
/// navigation stays, as on WebKit (T23).
fn trim_context_menu(args: &ICoreWebView2ContextMenuRequestedEventArgs, devtools: bool) {
    let Ok(items) = (unsafe { args.MenuItems() }) else {
        return;
    };
    let mut count = 0_u32;
    if unsafe { items.Count(&mut count) }.is_err() {
        return;
    }
    let mut doomed = Vec::new();
    for index in 0..count {
        let Ok(item) = (unsafe { items.GetValueAtIndex(index) }) else {
            continue;
        };
        let mut kind = COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND::default();
        if unsafe { item.Kind(&mut kind) }.is_err()
            || kind == COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND_SEPARATOR
        {
            continue;
        }
        let mut name = PWSTR::null();
        if unsafe { item.Name(&mut name) }.is_err() {
            continue;
        }
        if dooms_item(&take_pwstr(name), devtools) {
            doomed.push(index);
        }
    }
    for index in doomed.into_iter().rev() {
        let _ = unsafe { items.RemoveValueAtIndex(index) };
    }
    tidy_separators(&items);
}

/// The unlocalized WebView2 item names this crate removes, mirroring
/// `nested/policy.rs::trim_context_menu`'s WebKit actions.
fn dooms_item(name: &str, devtools: bool) -> bool {
    match name {
        // No engine windows and no download UI (EMBEDS.md §13).
        "openLinkInNewWindow"
        | "openLinkInNewTab"
        | "openImageInNewWindow"
        | "openImageInNewTab"
        | "openFrameInNewWindow"
        | "openVideoInNewWindow"
        | "openAudioInNewWindow"
        | "saveAs"
        | "saveImageAs"
        | "saveVideoAs"
        | "saveAudioAs"
        | "saveLinkAs" => true,
        // The engine never fullscreens (the host only sees the request).
        "enterFullscreen" | "exitFullscreen" | "toggleFullscreen" => true,
        "inspectElement" => !devtools,
        _ => false,
    }
}

/// Removes the separators that no longer separate: leading, doubled or
/// trailing ones. One pass over a snapshot, like the WebKit backend (T23).
fn tidy_separators(items: &ICoreWebView2ContextMenuItemCollection) {
    let mut count = 0_u32;
    if unsafe { items.Count(&mut count) }.is_err() {
        return;
    }
    let mut kinds = Vec::with_capacity(count as usize);
    for index in 0..count {
        let Ok(item) = (unsafe { items.GetValueAtIndex(index) }) else {
            continue;
        };
        let mut kind = COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND::default();
        if unsafe { item.Kind(&mut kind) }.is_ok() {
            kinds.push(kind);
        }
    }
    let is_separator = |kind: &COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND| {
        *kind == COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND_SEPARATOR
    };
    let mut doomed = Vec::new();
    for (index, kind) in kinds.iter().enumerate() {
        if !is_separator(kind) {
            continue;
        }
        let doubled = index > 0 && is_separator(&kinds[index - 1]);
        let trailing = kinds.get(index + 1).is_none_or(is_separator);
        if index == 0 || doubled || trailing {
            doomed.push(index as u32);
        }
    }
    for index in doomed.into_iter().rev() {
        let _ = unsafe { items.RemoveValueAtIndex(index) };
    }
}

/// Registers the clip-container class once.
///
/// `DefWindowProcW` with a null background brush: the container must never
/// paint, or it would flash over the webview (and cover GPUI's poster while
/// the surface is hidden).
fn ensure_clip_class() -> Result<()> {
    use std::sync::OnceLock;
    static REGISTERED: OnceLock<()> = OnceLock::new();
    if REGISTERED.get().is_some() {
        return Ok(());
    }
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(clip_window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: module_handle(),
        hIcon: HICON::default(),
        hCursor: HCURSOR::default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: w!("gpui_webview_clip"),
        hIconSm: HICON::default(),
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        // A second window can race the first registration; only that failure
        // is benign.
        let error = unsafe { GetLastError() };
        if error != ERROR_CLASS_ALREADY_EXISTS {
            return Err(Error::Platform(format!(
                "RegisterClassExW(gpui_webview_clip): {error:?}"
            )));
        }
    }
    let _ = REGISTERED.set(());
    Ok(())
}

fn module_handle() -> windows::Win32::Foundation::HINSTANCE {
    unsafe {
        GetModuleHandleW(PCWSTR::null())
            .map(Into::into)
            .unwrap_or_default()
    }
}

/// The clip container's window procedure: `DefWindowProcW` (the windows-rs
/// binding is a Rust `fn`, the class needs an `extern "system"` pointer).
unsafe extern "system" fn clip_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
