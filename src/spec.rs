//! What a live surface shows and how it may behave (EMBEDS.md §8).
//!
//! The crate enforces the [`Policy`] it is handed; the caller decides it.

use gpui_kit::{Bounds, Pixels, SharedString, px};

/// An opaque live-surface handle; the manager mints it, everyone else passes it back.
pub type SurfaceId = u64;

/// What a surface shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A remote page; the caller has validated the scheme.
    Url(String),
    /// Inline HTML: the small-clip fallback. `base` is the `<base href>`.
    Html { html: String, base: Option<String> },
}

/// How the engine's storage is scoped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Storage {
    /// No persistence: a fresh data store.
    #[default]
    Ephemeral,
    Shared,
}

/// What happens when the page navigates somewhere (a link, a redirect, JS).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Navigation {
    #[default]
    Allow,
    /// Refuse the navigation and ask the host to open it externally
    /// (`WebViewEvent::OpenExternal`). What a clip's links do.
    OpenExternal,
    Block,
}

/// What the engine's *own* context menu may contain (EMBEDS.md §5). A page's own
/// menu is page content and is never affected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContextMenu {
    /// Nothing: the menu never appears.
    Off,
    /// The engine's menu minus what this crate cannot honour: no downloads or new
    /// windows, no engine fullscreen, no "Inspect Element" without
    /// [`Engine::devtools`]. Navigation stays: on a bare page it *is* the menu.
    #[default]
    Trimmed,
}

/// What the *engine* exposes beyond the page (EMBEDS.md §5); everything is off
/// in the product. These are engine features, not content permissions: a backend
/// enforces them at the view. The wasm `iframe` has no say either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Engine {
    /// Developer extras: the inspector and the "Inspect Element" menu item.
    /// `GPUI_WEBVIEW_DEVTOOLS=1` forces it on for the nested backend.
    pub devtools: bool,
    /// What the engine's own context menu may contain; see [`ContextMenu`].
    pub context_menu: ContextMenu,
    /// Windows the page opens itself; off, so [`Policy::navigation`] decides.
    pub popups: bool,
    /// `getUserMedia` and WebRTC: camera, microphone, peer connections.
    pub media_stream: bool,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            devtools: false,
            context_menu: ContextMenu::default(),
            popups: false,
            media_stream: false,
        }
    }
}

/// What the engine may do; each backend enforces it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Whether the page may run JavaScript.
    pub scripts: bool,
    /// Whether the page may use its own origin (cookies, storage APIs).
    pub same_origin: bool,
    /// Feature-policy allow-list (the product's YouTube list). Empty = none.
    pub allow: Vec<&'static str>,
    /// The `Referer` to send (the desktop's `corsHost` origin); `None` = backend default.
    pub referrer: Option<String>,
    pub storage: Storage,
    pub navigation: Navigation,
    /// Engine-level toggles; all off by default. See [`Engine`].
    pub engine: Engine,
}

impl Default for Policy {
    fn default() -> Self {
        Self::strict()
    }
}

impl Policy {
    /// The saved-web-clip policy: no scripts, no same-origin, no storage (EMBEDS.md §5).
    pub fn strict() -> Self {
        Self {
            scripts: false,
            same_origin: false,
            allow: Vec::new(),
            referrer: None,
            storage: Storage::Ephemeral,
            navigation: Navigation::OpenExternal,
            engine: Engine::default(),
        }
    }

    /// Scripts, forms and media: a third-party embed (the product's sandbox).
    pub fn media() -> Self {
        Self {
            scripts: true,
            same_origin: true,
            allow: vec![
                "accelerometer",
                "autoplay",
                "clipboard-write",
                "encrypted-media",
                "gyroscope",
                "picture-in-picture",
                "web-share",
            ],
            referrer: None,
            storage: Storage::Shared,
            navigation: Navigation::Allow,
            engine: Engine::default(),
        }
    }

    /// The product's `sandbox.ts` feature list.
    pub fn sandbox_features(&self) -> Vec<&'static str> {
        let mut features = Vec::new();
        if self.scripts {
            features.push("allow-scripts");
        }
        if self.same_origin {
            features.push("allow-same-origin");
        }
        features.extend(["allow-popups", "allow-popups-to-escape-sandbox"]);
        features
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceSpec {
    pub source: Source,
    pub policy: Policy,
    pub title: SharedString,
    /// The initial logical size; the nested compositor configures its client to it.
    pub size: (u32, u32),
}

impl SurfaceSpec {
    pub fn new(source: Source, policy: Policy, title: impl Into<SharedString>) -> Self {
        Self {
            source,
            policy,
            title: title.into(),
            size: (1, 1),
        }
    }

    /// Sets the initial logical size.
    pub fn with_size(mut self, width: u32, height: u32) -> Self {
        self.size = (width.max(1), height.max(1));
        self
    }
}

/// Where a live surface wants to be, in window coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// The full block rect; the surface keeps this size, so clipping never
    /// relayouts the page (EMBEDS.md §8).
    pub rect: Bounds<Pixels>,
    /// The visible region: the viewport minus any overlay the surface must not cover.
    pub clip: Bounds<Pixels>,
    /// Whether the surface should be shown at all this frame.
    pub visible: bool,
    /// A GPUI overlay intersects the surface; `Config::on_overlay` decides.
    pub overlay: bool,
}

impl Placement {
    pub fn new(rect: Bounds<Pixels>, clip: Bounds<Pixels>) -> Self {
        Self {
            rect,
            clip,
            visible: true,
            overlay: false,
        }
    }

    /// Only the block's part inside `viewport` is visible.
    pub fn clipped_to(rect: Bounds<Pixels>, viewport: Bounds<Pixels>) -> Self {
        let clip = rect.intersect(&viewport);
        Self {
            rect,
            clip,
            visible: clip.size.width > px(0.) && clip.size.height > px(0.),
            overlay: false,
        }
    }
}

/// What to do when a GPUI overlay intersects a live surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlayBehavior {
    /// Lower the surface below GPUI: the page stays live and visible.
    #[default]
    Lower,
    /// Hide the surface and paint the poster: frame-exact, but frozen.
    Freeze,
}

/// How a surface is presented.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Presentation {
    #[default]
    Inline,
    Window,
    Fullscreen,
}

/// Manager configuration.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Live engines kept at once (default 1; 2 is the practical desktop max).
    pub capacity: usize,
    /// What to do when a GPUI overlay intersects a surface.
    pub on_overlay: OverlayBehavior,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capacity: 1,
            on_overlay: OverlayBehavior::Lower,
        }
    }
}

/// Why a surface or backend is unavailable; never a panic (EMBEDS.md §8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Unsupported,
    Platform(String),
    Load(String),
}

/// A decoded frame a self-compositing backend (the nested compositor) asks the
/// app to paint; child-view backends return `None`.
#[derive(Clone)]
pub struct SurfaceFrame {
    /// CPU pixels (`wl_shm`) or a dma-buf the app imports into a GPU texture.
    pub content: SurfaceContent,
    /// The committed logical size, for placement maths.
    pub size: (u32, u32),
}

impl SurfaceFrame {
    /// A frame of copied pixels (the `wl_shm` path).
    pub fn from_image(image: std::sync::Arc<gpui_kit::RenderImage>, size: (u32, u32)) -> Self {
        Self {
            content: SurfaceContent::Image(image),
            size,
        }
    }

    /// The pixels, when this is a CPU-copied frame.
    pub fn image(&self) -> Option<&std::sync::Arc<gpui_kit::RenderImage>> {
        match &self.content {
            SurfaceContent::Image(image) => Some(image),
            #[cfg(dmabuf)]
            SurfaceContent::Dmabuf(_) => None,
        }
    }

    /// Whether two frames are the same commit (a new commit triggers a repaint).
    pub fn same_as(&self, other: &Self) -> bool {
        match (&self.content, &other.content) {
            (SurfaceContent::Image(a), SurfaceContent::Image(b)) => a.id == b.id,
            #[cfg(dmabuf)]
            (SurfaceContent::Dmabuf(a), SurfaceContent::Dmabuf(b)) => a.sequence == b.sequence,
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }
}

impl std::fmt::Debug for SurfaceFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurfaceFrame")
            .field("size", &self.size)
            .field("content", &self.content)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum SurfaceContent {
    /// Pixels copied on the CPU (the `wl_shm` path), in GPUI's BGRA form.
    Image(std::sync::Arc<gpui_kit::RenderImage>),
    /// A dma-buf the app imports into a GPU texture. Its fds stay open until the
    /// last clone drops.
    #[cfg(dmabuf)]
    Dmabuf(DmaBufFrame),
}

/// A committed dma-buf, ready to import: a plain descriptor (owned fds, no
/// smithay types). Linux + `dma-buf` only.
#[cfg(dmabuf)]
#[derive(Clone, Debug)]
pub struct DmaBufFrame {
    /// The compositor's per-buffer generation (the import cache key) (T10).
    pub buffer_id: u64,
    /// Monotonic per-commit sequence.
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    /// The buffer's DRM fourcc.
    pub fourcc: u32,
    /// The buffer's DRM format modifier.
    pub modifier: u64,
    /// The buffer's planes.
    pub planes: Vec<DmaBufPlane>,
}

/// One plane of a [`DmaBufFrame`], owning its file descriptor.
#[cfg(dmabuf)]
#[derive(Clone, Debug)]
pub struct DmaBufPlane {
    /// The plane's fd; `Arc` so a frame clones cheaply, closed with the last clone.
    pub fd: std::sync::Arc<std::os::fd::OwnedFd>,
    pub offset: u32,
    /// Row stride, in bytes.
    pub stride: u32,
}

/// A native popup above a live surface (WebKit's context menu, a `<select>`
/// dropdown). The client commits it like the page; the app paints it on top.
#[derive(Clone)]
pub struct PopupFrame {
    /// Stable while the popup lives; keys its dma-buf import cache slot.
    pub id: u64,
    /// The popup's top-left in the page's own coordinate space, logical pixels.
    pub position: (i32, i32),
    /// The popup's own pixels.
    pub frame: SurfaceFrame,
}

impl PopupFrame {
    /// Whether two popup states are the same popup, place and commit.
    pub fn same_as(&self, other: &Self) -> bool {
        self.id == other.id && self.position == other.position && self.frame.same_as(&other.frame)
    }
}

impl std::fmt::Debug for PopupFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PopupFrame")
            .field("id", &self.id)
            .field("position", &self.position)
            .field("size", &self.frame.size)
            .finish()
    }
}

/// A pointer button a live page can receive; the backend maps it to its own code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// One input event forwarded to a live surface (EMBEDS.md §10.3). Only
/// self-compositing backends consume these; child views see platform events.
#[derive(Clone, Debug, PartialEq)]
pub enum WebInputEvent {
    PointerMotion {
        x: f64,
        y: f64,
    },
    /// The pointer entered the surface's rect; explicit so hover starts without motion.
    PointerEnter {
        x: f64,
        y: f64,
    },
    /// The pointer left the surface's rect; the compositor releases the pointer focus.
    PointerLeave,
    /// A pointer button press/release ([`PointerButton`]; the backend maps it).
    PointerButton {
        x: f64,
        y: f64,
        button: PointerButton,
        pressed: bool,
    },
    /// A scroll axis update at `(x, y)`, in GPUI's convention (positive = up/left,
    /// one tick = 60 units). The backend flips sign and maps units (T12).
    PointerAxis {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    /// A key press/release; the backend maps it to the keymap it sent the client.
    Key {
        keystroke: gpui_kit::Keystroke,
        pressed: bool,
    },
    /// The held modifier set changed without another key; the backend matches the
    /// protocol modifier keys (T9).
    Modifiers {
        modifiers: gpui_kit::Modifiers,
    },
}

impl WebInputEvent {
    /// The pointer position, when the event carries one (popup hit-testing (T25)).
    #[allow(dead_code)] // Only self-compositing backends have a hit-test.
    pub(crate) fn pointer_position(&self) -> Option<(f64, f64)> {
        match self {
            Self::PointerMotion { x, y }
            | Self::PointerEnter { x, y }
            | Self::PointerButton { x, y, .. }
            | Self::PointerAxis { x, y, .. } => Some((*x, *y)),
            Self::PointerLeave | Self::Key { .. } | Self::Modifiers { .. } => None,
        }
    }

    /// Whether the event routes by pointer position rather than keyboard focus
    /// (motion, buttons, wheel, enter/leave) (T25).
    pub(crate) fn is_positional(&self) -> bool {
        matches!(
            self,
            Self::PointerMotion { .. }
                | Self::PointerEnter { .. }
                | Self::PointerLeave
                | Self::PointerButton { .. }
                | Self::PointerAxis { .. }
        )
    }
}

/// Which system selection a text transfer is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Selection {
    /// The clipboard (copy/paste).
    Clipboard,
    /// The primary selection (select, then middle-click paste; Linux).
    Primary,
}

/// What the manager and backends report back (EMBEDS.md §8); drained in order.
#[derive(Clone, Debug, PartialEq)]
pub enum WebViewEvent {
    /// A navigation was refused and should open in the host's browser.
    OpenExternal {
        id: SurfaceId,
        url: String,
    },
    TitleChanged {
        id: SurfaceId,
        title: String,
    },
    /// The engine failed to load the current content.
    LoadFailed {
        id: SurfaceId,
        error: Error,
    },
    /// The page asked for fullscreen (or left it); the app decides via `present`.
    FullscreenRequested {
        id: SurfaceId,
        on: bool,
    },
    Crashed {
        id: SurfaceId,
    },
    Closed {
        id: SurfaceId,
        key: u64,
    },
    /// The page set `selection` to `text` (a copy, or a new primary
    /// selection); the app writes it to the host's own. Only backends whose
    /// engine has no system clipboard of its own raise it
    /// ([`WebViewBackend::bridges_selection`](crate::WebViewBackend::bridges_selection)).
    SelectionChanged {
        selection: Selection,
        text: String,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => write!(f, "live web surfaces are unsupported here"),
            Self::Platform(message) => write!(f, "platform error: {message}"),
            Self::Load(message) => write!(f, "load error: {message}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{point, px, size};

    #[test]
    fn strict_policy_is_the_clip_policy() {
        let policy = Policy::strict();
        assert!(!policy.scripts && !policy.same_origin);
        assert_eq!(policy.storage, Storage::Ephemeral);
        assert_eq!(policy.navigation, Navigation::OpenExternal);
        assert_eq!(
            policy.sandbox_features(),
            vec!["allow-popups", "allow-popups-to-escape-sandbox"]
        );
    }

    /// Every engine feature is off in both shipped policies by default.
    #[test]
    fn engine_features_are_off_by_default() {
        let engine = Engine::default();
        assert!(!engine.devtools);
        assert_eq!(engine.context_menu, ContextMenu::Trimmed);
        assert!(!engine.popups);
        assert!(!engine.media_stream);
        for policy in [Policy::strict(), Policy::media()] {
            assert_eq!(policy.engine, Engine::default());
        }

        let mut with_devtools = Policy::strict();
        with_devtools.engine.devtools = true;
        assert_ne!(
            with_devtools,
            Policy::strict(),
            "the toggle is part of the policy"
        );
    }

    #[test]
    fn media_policy_matches_the_product_sandbox() {
        let policy = Policy::media();
        let features = policy.sandbox_features();
        assert!(features.contains(&"allow-scripts"));
        assert!(features.contains(&"allow-same-origin"));
        assert!(policy.allow.contains(&"autoplay"));
        assert_eq!(policy.storage, Storage::Shared);
        assert_eq!(policy.navigation, Navigation::Allow);
    }

    #[test]
    fn placement_clips_to_the_viewport() {
        let rect = Bounds::new(point(px(0.), px(50.)), size(px(100.), px(100.)));
        let viewport = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.)));
        let placement = Placement::clipped_to(rect, viewport);
        assert!(placement.visible);
        assert_eq!(placement.clip.size.height, px(50.));

        let off = Placement::clipped_to(
            rect,
            Bounds::new(point(px(0.), px(200.)), size(px(100.), px(100.))),
        );
        assert!(!off.visible);
    }

    #[test]
    fn default_config_is_one_live_surface() {
        let config = Config::default();
        assert_eq!(config.capacity, 1);
        assert_eq!(config.on_overlay, OverlayBehavior::Lower);
    }
}
