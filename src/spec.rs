//! What a live surface shows and how it may behave (EMBEDS.md §8).
//!
//! The crate enforces the [`Policy`] it is handed; it never decides policy.
//! "Clips run no scripts, embeds run the provider's" is the caller's rule.

use gpui_kit::{Bounds, Pixels, SharedString, px};

/// An opaque live-surface handle, minted by the manager. A backend never mints
/// one and never interprets one; the caller only ever passes it back.
pub type SurfaceId = u64;

/// What a surface shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A remote page (`embed`). The caller has already validated the scheme.
    Url(String),
    /// Inline HTML (`Source::Html`): the small-clip fallback. `base` is the
    /// `<base href>` the host wants navigation resolved against.
    Html { html: String, base: Option<String> },
    /// Host-provided local content streamed through a [`SchemeLoader`]
    /// (a multi-MB web clip). The crate never sees the bytes.
    ///
    /// [`SchemeLoader`]: crate::SchemeLoader
    Local { scheme: &'static str, path: String },
}

impl Source {
    /// Whether the source needs a [`SchemeLoader`](crate::SchemeLoader).
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }
}

/// How the engine's storage is scoped. A clip is `Ephemeral`; an embed shares
/// the host's cookie jar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Storage {
    /// No persistence: a fresh, non-persistent data store.
    #[default]
    Ephemeral,
    /// The platform's normal store (cookies, localStorage).
    Shared,
}

/// What happens when the page navigates somewhere (a link, a redirect, JS).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Navigation {
    /// Let the engine navigate inside the surface.
    #[default]
    Allow,
    /// Refuse the navigation and ask the host to open it externally
    /// (`WebViewEvent::OpenExternal`). What a clip's links do.
    OpenExternal,
    /// Refuse silently.
    Block,
}

/// What the engine's *own* context menu (right click) may contain. A **page's**
/// menu — a `contextmenu` handler, YouTube's and Vimeo's — is page content and
/// is never affected by this; this is only the menu the engine would show
/// itself (EMBEDS.md §5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContextMenu {
    /// Nothing: the engine's menu never appears.
    Off,
    /// The engine's menu minus what this crate cannot honour: no downloads or
    /// "open in new window" (a note shows no engine windows), no engine
    /// fullscreen unless [`Engine::fullscreen`] is on, and no "Inspect Element"
    /// unless [`Engine::devtools`] is on. Everything else stays, page
    /// navigation included — on a page that offers nothing else, Back/Forward/
    /// Stop/Reload *are* the menu, and dropping them would leave a right click
    /// with nothing to show.
    #[default]
    Trimmed,
    /// The engine's menu, untouched.
    Engine,
}

/// What the *engine* exposes beyond the page itself (EMBEDS.md §5). A note
/// shows a page, not a browser: everything here is off in the product, and a
/// backend turns on exactly what it is asked for.
///
/// These are engine features, not content permissions — a page cannot ask for
/// them and a sandbox attribute cannot express them, so they are enforced at
/// the view (WebKitGTK settings, `WKWebView` preferences). The wasm target is
/// an `iframe` inside the host's browser: there, the browser owns all of this
/// and the flags are inert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Engine {
    /// Developer extras: the inspector, and the "Inspect Element" item in the
    /// engine's own context menu. `GPUI_WEBVIEW_DEVTOOLS=1` forces it on for
    /// the nested Linux backend while debugging.
    pub devtools: bool,
    /// What the engine's own context menu (Back, Reload, Save Page As,
    /// Inspect…) may contain; trimmed by default, see [`ContextMenu`].
    pub context_menu: ContextMenu,
    /// Windows the page opens itself (`window.open`, and `target=_blank` links
    /// the host does not claim): off, so the host decides where a navigation
    /// goes ([`Policy::navigation`]).
    pub popups: bool,
    /// The engine's fullscreen API. Off: the request is intercepted and
    /// reported to the host (`WebViewEvent::FullscreenRequested`), which
    /// presents it itself — that is what keeps the page inside the editor's
    /// own chrome. On: the engine is allowed to try its own fullscreen.
    pub fullscreen: bool,
    /// `getUserMedia` and WebRTC: camera, microphone, peer connections.
    pub media_stream: bool,
}

impl Default for Engine {
    /// What an embed needs: none of it.
    fn default() -> Self {
        Self {
            devtools: false,
            context_menu: ContextMenu::default(),
            popups: false,
            fullscreen: false,
            media_stream: false,
        }
    }
}

/// What the engine is allowed to do. The caller picks these values; the crate
/// maps them to each backend and enforces them there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Whether the page may run JavaScript.
    pub scripts: bool,
    /// Whether the page may use its own origin (cookies, storage APIs).
    pub same_origin: bool,
    /// Feature-policy allow-list (the product's YouTube list). Empty = none.
    pub allow: Vec<&'static str>,
    /// The `Referer` to send (the desktop's `corsHost` origin). `None` = the
    /// backend default.
    pub referrer: Option<String>,
    pub storage: Storage,
    pub navigation: Navigation,
    /// Engine-level toggles (devtools, the engine's own menus, fullscreen…).
    /// All off by default; see [`Engine`].
    pub engine: Engine,
}

impl Default for Policy {
    fn default() -> Self {
        Self::strict()
    }
}

impl Policy {
    /// No scripts, no same-origin, no storage, external navigation: a saved web
    /// clip (EMBEDS.md §5).
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

    /// The product's `sandbox.ts` feature list, as `allow` entries.
    pub fn sandbox_features(&self) -> Vec<&'static str> {
        let mut features = Vec::new();
        if self.scripts {
            features.push("allow-scripts");
        }
        if self.same_origin {
            features.push("allow-same-origin");
        }
        features.extend([
            "allow-popups",
            "allow-popups-to-escape-sandbox",
        ]);
        features
    }
}

/// One surface's content and policy.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceSpec {
    pub source: Source,
    pub policy: Policy,
    /// The a11y label / window title.
    pub title: SharedString,
    /// The initial logical size in pixels (the block's measured size). The
    /// nested Linux compositor configures its client to exactly this, so the
    /// page's layout matches the poster's; child-view backends size the view
    /// from `Placement` instead.
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

    /// Sets the initial logical size (builder form, so existing call sites
    /// stay readable).
    pub fn with_size(mut self, width: u32, height: u32) -> Self {
        self.size = (width.max(1), height.max(1));
        self
    }
}

/// Where a live surface wants to be, in window coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// The full block rect. The surface keeps this size; clipping the *view*
    /// must never relayout the page (EMBEDS.md §8).
    pub rect: Bounds<Pixels>,
    /// The visible region: the viewport (or the block's visible part) minus any
    /// GPUI overlay the surface must not draw over.
    pub clip: Bounds<Pixels>,
    /// Whether the surface should be shown at all this frame.
    pub visible: bool,
    /// A GPUI overlay intersects the surface: lower it if the backend can,
    /// hide it if it cannot (`Config::on_overlay`).
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

/// What to do when a GPUI overlay (menu, find bar, IME candidates, drag)
/// intersects a live surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlayBehavior {
    /// Ask the backend to lower the surface below GPUI (web z-index flip, the
    /// compositor `place_below` path): the page stays live and visible.
    #[default]
    Lower,
    /// Hide the surface and let the app paint its poster: frame-exact, at the
    /// cost of a frozen page.
    Freeze,
}

/// How a surface is presented.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Presentation {
    /// In the editor, at its block's rect.
    #[default]
    Inline,
    /// A separate window or sheet.
    Window,
    /// Fullscreen (a special window).
    Fullscreen,
}

/// Manager configuration. The engine cost is per *live* engine, so the default
/// is one.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Live engines kept at once. Default 1; 2 is the practical desktop max.
    pub capacity: usize,
    /// What the manager does when a GPUI overlay intersects a surface.
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

/// Why a surface (or the whole backend) is unavailable. Never a panic: the app
/// always has the poster + `openUrl` fallback (EMBEDS.md §8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// This target, session or build cannot host a surface.
    Unsupported,
    /// The platform refused the operation.
    Platform(String),
    /// The content failed to load.
    Load(String),
}

/// A decoded frame a **self-compositing** backend asks the app to paint — the
/// nested Linux compositor. Child-view backends (macOS, the wasm iframe)
/// composite through the platform and return `None`.
#[derive(Clone)]
pub struct SurfaceFrame {
    /// What the client committed: CPU pixels (`wl_shm`) or a dma-buf the app
    /// imports into a GPU texture.
    pub content: SurfaceContent,
    /// The committed logical size (buffer size divided by the output scale),
    /// for placement maths.
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
            #[cfg(all(target_os = "linux", feature = "dma-buf"))]
            SurfaceContent::Dmabuf(_) => None,
        }
    }

    /// Whether two frames are the same commit; a new client commit always
    /// differs, which is what triggers a repaint.
    pub fn same_as(&self, other: &Self) -> bool {
        match (&self.content, &other.content) {
            (SurfaceContent::Image(a), SurfaceContent::Image(b)) => a.id == b.id,
            #[cfg(all(target_os = "linux", feature = "dma-buf"))]
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

/// The content of a committed frame.
#[derive(Clone, Debug)]
pub enum SurfaceContent {
    /// Pixels copied on the CPU (the `wl_shm` path), in GPUI's BGRA
    /// `RenderImage` form.
    Image(std::sync::Arc<gpui_kit::RenderImage>),
    /// A dma-buf the app imports into a GPU texture through the window (the
    /// `linux-dmabuf` path). The planes' fds stay open until the frame (and
    /// every clone of it) is dropped.
    #[cfg(all(target_os = "linux", feature = "dma-buf"))]
    Dmabuf(DmaBufFrame),
}

/// A committed dma-buf, ready to import. Kept as a plain descriptor (owned
/// fds, no smithay types) so the app can import it wherever it has a window.
///
/// Only built for the Linux dma-buf path: the descriptor owns file
/// descriptors (`std::os::fd`), which do not exist on wasm or Windows.
#[cfg(all(target_os = "linux", feature = "dma-buf"))]
#[derive(Clone, Debug)]
pub struct DmaBufFrame {
    /// The *compositor's* per-buffer generation, never reused for a recycled
    /// client id. The app's import cache is keyed by it.
    pub buffer_id: u64,
    /// Monotonic per-commit sequence; two frames with the same sequence are
    /// the same commit.
    pub sequence: u64,
    /// Buffer width in pixels.
    pub width: u32,
    /// Buffer height in pixels.
    pub height: u32,
    /// The buffer's DRM fourcc.
    pub fourcc: u32,
    /// The buffer's DRM format modifier.
    pub modifier: u64,
    /// The buffer's planes.
    pub planes: Vec<DmaBufPlane>,
}

/// One plane of a [`DmaBufFrame`], owning its file descriptor.
#[cfg(all(target_os = "linux", feature = "dma-buf"))]
#[derive(Clone, Debug)]
pub struct DmaBufPlane {
    /// The plane's fd. `Arc` so a frame stays cheap to clone; closed when the
    /// last clone drops.
    pub fd: std::sync::Arc<std::os::fd::OwnedFd>,
    /// Offset into the plane, in bytes.
    pub offset: u32,
    /// Row stride, in bytes.
    pub stride: u32,
}

/// A native popup above a live surface: WebKit's context menu, a `<select>`
/// dropdown, a date picker. The client commits it as its own surface (an
/// `xdg_popup`, so shm or dma-buf like the page) and the app paints it above
/// the page's frame. Pointer input over it is routed to it by the compositor,
/// so the app only has to draw it.
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
    /// Whether two popup states are the same popup, at the same place, showing
    /// the same commit — so the app can skip a repaint.
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

/// A pointer button a live page can receive. The spec stays platform-neutral;
/// the nested backend translates the variants to Linux `BTN_*` codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// One input event forwarded to a live surface (EMBEDS.md §10.3: the nested
/// compositor synthesizes `wl_seat` events from the app's own input). Only
/// self-compositing backends consume these; native child views and DOM
/// iframes already receive the platform's events themselves.
#[derive(Clone, Debug, PartialEq)]
pub enum WebInputEvent {
    /// Pointer moved, in surface-local logical pixels.
    PointerMotion { x: f64, y: f64 },
    /// The pointer entered the surface's rectangle at `(x, y)` surface-local.
    /// Enter is explicit so hover starts even when no motion follows (the
    /// pointer can arrive from a popup or another window).
    PointerEnter { x: f64, y: f64 },
    /// The pointer left the surface's rectangle (window-level hover end). The
    /// nested compositor releases the protocol pointer focus, so the page sees
    /// `mouseleave` and its hover/cursor state stops sticking.
    PointerLeave,
    /// A pointer button press/release. `button` is the platform-neutral
    /// [`PointerButton`]; the backend maps it to its own code.
    PointerButton {
        x: f64,
        y: f64,
        button: PointerButton,
        pressed: bool,
    },
    /// A scroll axis update at `(x, y)` surface-local, in GPUI's delta
    /// convention (positive = up/left, one wheel tick = 60 units). The backend
    /// flips the sign and maps the units to the platform's (the nested
    /// compositor scales to GDK/Wayland's libinput axis units).
    PointerAxis {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    /// A key press/release, in GPUI's own keystroke form. The backend maps it
    /// to the keycodes of the keymap it sent to clients.
    Key {
        keystroke: gpui_kit::Keystroke,
        pressed: bool,
    },
    /// The held modifier set changed (a Shift/Ctrl/Alt/Super press or release
    /// without another key). The backend presses/releases the protocol
    /// modifier keys to match, so a held modifier stays down across keys.
    Modifiers { modifiers: gpui_kit::Modifiers },
}

impl WebInputEvent {
    /// The pointer position of the event, when it carries one. The nested
    /// compositor hit-tests popups with it before routing to the page.
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

    /// Whether the event is routed by pointer position rather than by keyboard
    /// focus: motion, buttons, the wheel and enter/leave. A page is an iframe:
    /// hovering and scrolling it must work before it has been clicked, and
    /// leaving it must always be delivered.
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

/// What the manager/backends report back to the app (EMBEDS.md §8), drained in
/// order with `WebViews::events`.
#[derive(Clone, Debug, PartialEq)]
pub enum WebViewEvent {
    /// A navigation was refused by policy and should open in the host's
    /// browser instead.
    OpenExternal { id: SurfaceId, url: String },
    /// The page's title changed (a clip's saved title is empty until it loads).
    TitleChanged { id: SurfaceId, title: String },
    /// The engine failed to load the current content.
    LoadFailed { id: SurfaceId, error: Error },
    /// The page asked for fullscreen (or left it). The app decides whether to
    /// honour it with `present`.
    FullscreenRequested { id: SurfaceId, on: bool },
    /// The engine's process crashed.
    Crashed { id: SurfaceId },
    /// The surface was torn down (`deactivate`, scroll-out or eviction). The
    /// app's `key` no longer maps to a live surface.
    Closed { id: SurfaceId, key: u64 },
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
        assert_eq!(policy.sandbox_features(), vec!["allow-popups", "allow-popups-to-escape-sandbox"]);
    }

    /// A note shows a page, not a browser: every engine feature is off in both
    /// policies the editor ships, and turning one on is the caller's decision.
    #[test]
    fn engine_features_are_off_by_default() {
        let engine = Engine::default();
        assert!(!engine.devtools);
        assert_eq!(engine.context_menu, ContextMenu::Trimmed);
        assert!(!engine.popups);
        assert!(!engine.fullscreen);
        assert!(!engine.media_stream);
        for policy in [Policy::strict(), Policy::media()] {
            assert_eq!(policy.engine, Engine::default());
        }

        let mut with_devtools = Policy::strict();
        with_devtools.engine.devtools = true;
        assert_ne!(with_devtools, Policy::strict(), "the toggle is part of the policy");
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

        let off = Placement::clipped_to(rect, Bounds::new(point(px(0.), px(200.)), size(px(100.), px(100.))));
        assert!(!off.visible);
    }

    #[test]
    fn default_config_is_one_live_surface() {
        let config = Config::default();
        assert_eq!(config.capacity, 1);
        assert_eq!(config.on_overlay, OverlayBehavior::Lower);
    }
}
