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
}

impl SurfaceSpec {
    pub fn new(source: Source, policy: Policy, title: impl Into<SharedString>) -> Self {
        Self {
            source,
            policy,
            title: title.into(),
        }
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
