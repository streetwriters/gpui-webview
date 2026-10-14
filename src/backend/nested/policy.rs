//! Engine policy: the WebKit settings, the navigation decision and the
//! context-menu trim.

use super::*;
use crate::debug::debug_log;

/// The parts of a surface's policy that GTK signal handlers read at event time.
/// wry connects its handlers once (at create), so a property changed by
/// `navigate` must be reachable through shared state, not captured by value.
#[derive(Clone)]
pub(super) struct SurfaceControl {
    pub(super) id: SurfaceId,
    /// Consulted for every navigation decision; `navigate` overwrites it.
    pub(super) navigation: Rc<RefCell<Navigation>>,
    /// Consulted for every context-menu request; `navigate` overwrites it.
    pub(super) menu: Rc<RefCell<MenuState>>,
    /// Backend → app events (title, load failure, crash, fullscreen, close).
    pub(super) events: SharedEvents,
}


/// The effective values behind the engine's context menu.
#[derive(Clone, Copy, Default)]
pub(super) struct MenuState {
    pub(super) menu: ContextMenu,
    pub(super) devtools: bool,
    pub(super) fullscreen: bool,
}


impl MenuState {
    pub(super) fn of(spec: &SurfaceSpec) -> Self {
        Self {
            menu: spec.policy.engine.context_menu,
            devtools: spec.policy.engine.devtools || devtools_override(),
            fullscreen: spec.policy.engine.fullscreen,
        }
    }
}


impl SurfaceControl {
    /// The navigation handler wry installs: reads the live policy cell so a
    /// navigation after `navigate` is judged by the new policy.
    pub(super) fn navigation_handler(&self) -> impl Fn(String) -> bool + 'static {
        let navigation = self.navigation.clone();
        let events = self.events.clone();
        let id = self.id;
        move |url| {
            let decision = *navigation.borrow();
            debug_log!("navigation decision for {url}: {decision:?}");
            match decision {
                Navigation::Allow => true,
                Navigation::Block => false,
                Navigation::OpenExternal => {
                    // The engine's own loading geometry (`about:blank`, `data:`,
                    // `blob:`) is not a navigation a browser should open.
                    if url.starts_with("http://") || url.starts_with("https://") {
                        lock(&events).push(WebViewEvent::OpenExternal { id, url });
                        false
                    } else {
                        true
                    }
                }
            }
        }
    }

    /// The engine → app events every surface reports: title, load failure,
    /// crash, fullscreen and `window.close()`.
    pub(super) fn connect_handlers(&self, widget: &webkit2gtk::WebView) {
        use webkit2gtk::WebViewExt as _;

        let events = self.events.clone();
        let id = self.id;
        widget.connect_title_notify(move |webview| {
            let title = webview.title().map(|title| title.to_string()).unwrap_or_default();
            lock(&events).push(WebViewEvent::TitleChanged { id, title });
        });

        let events = self.events.clone();
        let id = self.id;
        widget.connect_load_failed(move |_webview, event, uri, error| {
            lock(&events).push(WebViewEvent::LoadFailed {
                id,
                error: Error::Load(format!("{event:?} {uri}: {error}")),
            });
            false
        });

        let events = self.events.clone();
        let id = self.id;
        widget.connect_web_process_terminated(move |_webview, reason| {
            if reason == webkit2gtk::WebProcessTerminationReason::Crashed {
                lock(&events).push(WebViewEvent::Crashed { id });
            } else {
                debug_log!("web process terminated: {reason:?}");
            }
        });

        // The engine's fullscreen API is *enabled* (see `apply_surface_policy`)
        // so the request can be reported even though `engine.fullscreen` is
        // normally off. `true` stops WebKit from fullscreening the hidden
        // nested window; the host presents (`FullscreenRequested`). A caller
        // that set `engine.fullscreen` gets WebKit's default behavior instead.
        let events = self.events.clone();
        let id = self.id;
        let menu = self.menu.clone();
        widget.connect_enter_fullscreen(move |_| {
            let engine_fullscreen = menu.borrow().fullscreen;
            lock(&events).push(WebViewEvent::FullscreenRequested { id, on: true });
            !engine_fullscreen
        });
        let events = self.events.clone();
        let id = self.id;
        widget.connect_leave_fullscreen(move |_| {
            lock(&events).push(WebViewEvent::FullscreenRequested { id, on: false });
            false
        });

        // `window.close()`: the engine's own `close` handler destroys the
        // widget (wry installs one); this tells the manager so it can tear the
        // surface down for real. The key is the manager's to fill in.
        let events = self.events.clone();
        let id = self.id;
        widget.connect_close(move |_| {
            lock(&events).push(WebViewEvent::Closed { id, key: 0 });
        });
    }
}


/// `GPUI_WEBVIEW_DEVTOOLS=1` forces developer extras on (this backend only):
/// the way to inspect a live embed while debugging, whatever the policy says.
pub(super) fn devtools_override() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GPUI_WEBVIEW_DEVTOOLS").is_some())
}


/// Applies the parts of a `Policy` WebKitGTK can enforce on a live view:
/// settings (scripts, devtools, fullscreen, media, popups), the context-menu
/// trim and the navigation rule. Called by `create` and `navigate`, so a
/// reused engine never keeps the previous caller's policy.
pub(super) fn apply_surface_policy(
    widget: &webkit2gtk::WebView,
    control: &SurfaceControl,
    spec: &SurfaceSpec,
) {
    let engine = &spec.policy.engine;
    let menu = MenuState::of(spec);
    *control.menu.borrow_mut() = menu;
    *control.navigation.borrow_mut() = spec.policy.navigation;
    if let Some(settings) = webkit2gtk::WebViewExt::settings(widget) {
        webkit2gtk::SettingsExt::set_enable_javascript(&settings, spec.policy.scripts);
        webkit2gtk::SettingsExt::set_enable_developer_extras(&settings, menu.devtools);
        // Always on: the request must reach us even when the *engine* is not
        // allowed to fullscreen. `connect_enter_fullscreen` reports it to the
        // host and blocks WebKit's own fullscreen unless `engine.fullscreen`
        // explicitly asks for it (the product never does).
        webkit2gtk::SettingsExt::set_enable_fullscreen(&settings, true);
        webkit2gtk::SettingsExt::set_enable_media_stream(&settings, engine.media_stream);
        webkit2gtk::SettingsExt::set_enable_webrtc(&settings, engine.media_stream);
        webkit2gtk::SettingsExt::set_javascript_can_open_windows_automatically(
            &settings,
            engine.popups,
        );
        // Reading the clipboard needs a permission prompt on some engines;
        // a note embed never needs it.
        webkit2gtk::SettingsExt::set_javascript_can_access_clipboard(&settings, false);
        // A WebKit test hook (fake camera/microphone), never an embed feature:
        // it stays off even if a caller asks.
        webkit2gtk::SettingsExt::set_enable_mock_capture_devices(&settings, false);
    }
}


/// Drops the items this crate cannot honour from an engine context menu and
/// tidies the separators they leave behind; returns how many items went.
///
/// Page navigation stays (Back/Forward/Stop/Reload): it is real — the page is a
/// page — and on a page that offers nothing else it is the whole menu, so
/// dropping it would leave a right click with *nothing* to show (the mistake
/// that once made the menu look broken). `devtools` and `fullscreen` are the
/// *effective* flags, so "Inspect Element" and "Enter Full Screen" survive
/// exactly when those features are actually available.
pub(super) fn trim_context_menu(menu: &webkit2gtk::ContextMenu, devtools: bool, fullscreen: bool) -> usize {
    use webkit2gtk::{ContextMenuExt as _, ContextMenuItemExt as _};

    let unusable = |action: webkit2gtk::ContextMenuAction| {
        use webkit2gtk::ContextMenuAction as Action;
        match action {
            // We show no engine windows and have no download UI (EMBEDS.md
            // §13), so every "new window" and "download" item would be a lie.
            Action::OpenLinkInNewWindow
            | Action::OpenImageInNewWindow
            | Action::OpenFrameInNewWindow
            | Action::OpenVideoInNewWindow
            | Action::OpenAudioInNewWindow
            | Action::DownloadLinkToDisk
            | Action::DownloadImageToDisk
            | Action::DownloadVideoToDisk
            | Action::DownloadAudioToDisk => true,
            // Fullscreen is the host's when the engine's is off.
            Action::EnterVideoFullscreen => !fullscreen,
            Action::InspectElement => !devtools,
            _ => false,
        }
    };

    let mut dropped = 0;
    let doomed: Vec<webkit2gtk::ContextMenuItem> = menu
        .items()
        .into_iter()
        .filter(|item| !item.is_separator() && unusable(item.stock_action()))
        .collect();
    for item in &doomed {
        menu.remove(item);
        dropped += 1;
    }
    // Separators that no longer separate anything: at an edge, doubled, or
    // trailing after a removal.
    let items = menu.items();
    let mut strays: Vec<webkit2gtk::ContextMenuItem> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if !item.is_separator() {
            continue;
        }
        let doubled = index > 0 && items[index - 1].is_separator();
        let trailing = items.get(index + 1).is_none_or(|next| next.is_separator());
        if index == 0 || doubled || trailing {
            strays.push(item.clone());
        }
    }
    for item in &strays {
        menu.remove(item);
        dropped += 1;
    }
    dropped
}
