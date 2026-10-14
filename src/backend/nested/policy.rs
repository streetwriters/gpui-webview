//! Engine policy: WebKit settings, navigation decisions, context-menu trim.

use super::Shared;
use crate::spec::ContextMenu;
use crate::spec::Error;
use crate::spec::Navigation;
use crate::spec::SurfaceId;
use crate::spec::SurfaceSpec;
use crate::spec::WebViewEvent;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use super::lock;
use crate::debug::debug_log;

/// Policy state the GTK handlers read at event time. wry connects its handlers
/// once, so `navigate` must mutate shared cells, not captured values.
#[derive(Clone)]
pub(super) struct SurfaceControl {
    pub(super) id: SurfaceId,
    /// Read on every decision; `navigate` overwrites it.
    pub(super) navigation: Rc<RefCell<Navigation>>,
    /// Read on every context-menu request; `navigate` overwrites it.
    pub(super) menu: Rc<RefCell<MenuState>>,
    /// The compositor's shared maps: events out, focus in.
    pub(super) shared: Arc<Shared>,
}

/// The effective values behind the engine's context menu.
#[derive(Clone, Copy, Default)]
pub(super) struct MenuState {
    pub(super) menu: ContextMenu,
    pub(super) devtools: bool,
}

impl MenuState {
    pub(super) fn of(spec: &SurfaceSpec) -> Self {
        Self {
            menu: spec.policy.engine.context_menu,
            devtools: spec.policy.engine.devtools || devtools_override(),
        }
    }
}

impl SurfaceControl {
    /// The navigation handler wry installs; reads the live cell, not a captured value.
    pub(super) fn navigation_handler(&self) -> impl Fn(String) -> bool + 'static {
        let navigation = self.navigation.clone();
        let shared = self.shared.clone();
        let id = self.id;
        move |url| {
            let decision = *navigation.borrow();
            debug_log!("navigation decision for {url}: {decision:?}");
            match decision {
                Navigation::Allow => true,
                Navigation::Block => false,
                Navigation::OpenExternal => {
                    // `about:blank`, `data:` and `blob:` are loading geometry, not navigations.
                    if url.starts_with("http://") || url.starts_with("https://") {
                        lock(&shared.events).push(WebViewEvent::OpenExternal { id, url });
                        false
                    } else {
                        true
                    }
                }
            }
        }
    }

    /// Engine → app events: title, load failure, crash, fullscreen, `window.close()`.
    pub(super) fn connect_handlers(&self, widget: &webkit2gtk::WebView) {
        use webkit2gtk::WebViewExt as _;

        let shared = self.shared.clone();
        let id = self.id;
        widget.connect_title_notify(move |webview| {
            let title = webview
                .title()
                .map(|title| title.to_string())
                .unwrap_or_default();
            lock(&shared.events).push(WebViewEvent::TitleChanged { id, title });
        });

        let shared = self.shared.clone();
        let id = self.id;
        widget.connect_load_failed(move |_webview, event, uri, error| {
            lock(&shared.events).push(WebViewEvent::LoadFailed {
                id,
                error: Error::Load(format!("{event:?} {uri}: {error}")),
            });
            false
        });

        let shared = self.shared.clone();
        let id = self.id;
        widget.connect_web_process_terminated(move |_webview, reason| {
            if reason == webkit2gtk::WebProcessTerminationReason::Crashed {
                lock(&shared.events).push(WebViewEvent::Crashed { id });
            } else {
                debug_log!("web process terminated: {reason:?}");
            }
        });

        // Fullscreen stays enabled so the request reaches us; `true` blocks WebKit's
        // own fullscreen, and the host presents (`FullscreenRequested`).
        let shared = self.shared.clone();
        let id = self.id;
        widget.connect_enter_fullscreen(move |_| {
            lock(&shared.events).push(WebViewEvent::FullscreenRequested { id, on: true });
            true
        });
        let shared = self.shared.clone();
        let id = self.id;
        widget.connect_leave_fullscreen(move |_| {
            lock(&shared.events).push(WebViewEvent::FullscreenRequested { id, on: false });
            false
        });

        // wry's own handler destroys the widget; tell the manager to tear the surface down.
        let shared = self.shared.clone();
        let id = self.id;
        widget.connect_close(move |_| {
            lock(&shared.events).push(WebViewEvent::Closed { id, key: 0 });
        });
    }
}

/// `GPUI_WEBVIEW_DEVTOOLS=1`: forces developer extras on for local debugging.
pub(super) fn devtools_override() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GPUI_WEBVIEW_DEVTOOLS").is_some())
}

/// Applies the policy WebKitGTK can enforce: settings, the context-menu trim and
/// the navigation rule. Called by `create` and `navigate`, so a reused engine
/// never keeps the previous caller's policy.
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
        // Always on: the request must reach us; `enter-fullscreen` blocks WebKit's own.
        webkit2gtk::SettingsExt::set_enable_fullscreen(&settings, true);
        webkit2gtk::SettingsExt::set_enable_media_stream(&settings, engine.media_stream);
        webkit2gtk::SettingsExt::set_enable_webrtc(&settings, engine.media_stream);
        webkit2gtk::SettingsExt::set_javascript_can_open_windows_automatically(
            &settings,
            engine.popups,
        );
        // A note embed never needs clipboard access (it prompts on some engines).
        webkit2gtk::SettingsExt::set_javascript_can_access_clipboard(&settings, false);
        // A WebKit test hook (fake camera/microphone), never an embed feature.
        webkit2gtk::SettingsExt::set_enable_mock_capture_devices(&settings, false);
    }
}

/// Drops what this crate cannot honour (new windows, downloads, engine fullscreen,
/// "Inspect Element") and tidies the separators; navigation stays (T23).
pub(super) fn trim_context_menu(menu: &webkit2gtk::ContextMenu, devtools: bool) {
    use webkit2gtk::{ContextMenuExt as _, ContextMenuItemExt as _};

    let unusable = |action: webkit2gtk::ContextMenuAction| {
        use webkit2gtk::ContextMenuAction as Action;
        match action {
            // No engine windows and no download UI (EMBEDS.md §13).
            Action::OpenLinkInNewWindow
            | Action::OpenImageInNewWindow
            | Action::OpenFrameInNewWindow
            | Action::OpenVideoInNewWindow
            | Action::OpenAudioInNewWindow
            | Action::DownloadLinkToDisk
            | Action::DownloadImageToDisk
            | Action::DownloadVideoToDisk
            | Action::DownloadAudioToDisk => true,
            // The engine never fullscreens (the host only sees the request).
            Action::EnterVideoFullscreen => true,
            Action::InspectElement => !devtools,
            _ => false,
        }
    };

    let doomed: Vec<webkit2gtk::ContextMenuItem> = menu
        .items()
        .into_iter()
        .filter(|item| !item.is_separator() && unusable(item.stock_action()))
        .collect();
    for item in &doomed {
        menu.remove(item);
    }
    // Separators that no longer separate: at an edge, doubled, or trailing. One
    // pass over the snapshot; removing as we go cannot shift what it reads.
    let items = menu.items();
    for (index, item) in items.iter().enumerate() {
        if !item.is_separator() {
            continue;
        }
        let doubled = index > 0 && items[index - 1].is_separator();
        let trailing = items.get(index + 1).is_none_or(|next| next.is_separator());
        if index == 0 || doubled || trailing {
            menu.remove(item);
        }
    }
}
