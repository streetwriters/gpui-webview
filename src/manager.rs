//! The pool and its policy: capacity, reuse, yield and teardown
//! (EMBEDS.md §8).
//!
//! The manager is the only stateful piece of the crate. It owns the backend,
//! mints [`SurfaceId`]s, keeps at most `Config::capacity` engines alive, and
//! forwards only *changes* to the backend so a caller can drive it from its
//! paint pass every frame.

use std::sync::Arc;

use gpui_kit::{Bounds, Pixels, Window};

use crate::host::{SchemeLoader, WebViewBackend, spec_supported};
use crate::spec::{
    Config, Error, OverlayBehavior, Placement, Presentation, Result, SurfaceId, SurfaceSpec,
    WebViewEvent,
};
pub use crate::spec::WebViewEvent as Event;


/// One live surface as the manager knows it.
struct Surface {
    id: SurfaceId,
    key: u64,
    spec: SurfaceSpec,
    focused: bool,
    /// Monotonic frame counter of the last frame this surface was placed; the
    /// eviction order for reuse.
    last_seen: u64,
    /// The last effective placement forwarded to the backend, so unchanged
    /// frames are skipped.
    last_placement: Option<Placement>,
}

/// The pool + policy on top of a backend.
pub struct WebViews {
    backend: Box<dyn WebViewBackend>,
    config: Config,
    attached: bool,
    next_id: SurfaceId,
    frame_serial: u64,
    surfaces: Vec<Surface>,
    events: Vec<WebViewEvent>,
    loader: Option<Arc<dyn SchemeLoader>>,
}

impl std::fmt::Debug for WebViews {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebViews")
            .field("backend", &self.backend.name())
            .field("capacity", &self.config.capacity)
            .field("surfaces", &self.surfaces.len())
            .finish()
    }
}

impl WebViews {
    /// Picks the best backend this target/session can run. `Unsupported` when
    /// no backend feature is compiled in or the runtime is missing, so the app
    /// always falls back to posters + `openUrl`.
    pub fn platform_default(config: Config) -> Result<Self> {
        #[cfg(all(target_arch = "wasm32", feature = "web"))]
        {
            return Ok(Self::new(crate::backend::web::WebBackend::new(), config));
        }
        #[cfg(all(not(target_arch = "wasm32"), feature = "wry"))]
        {
            match crate::backend::native::NativeBackend::new() {
                Ok(backend) => return Ok(Self::new(backend, config)),
                // No usable engine on this session (Wayland without the nested
                // compositor, a broken runtime): fall through to Unsupported.
                Err(_) => return Err(Error::Unsupported),
            }
        }
        #[cfg(not(any(
            all(target_arch = "wasm32", feature = "web"),
            all(not(target_arch = "wasm32"), feature = "wry")
        )))]
        {
            let _ = config;
            Err(Error::Unsupported)
        }
    }

    /// Wraps an explicit backend (the composition root, tests, the example).
    pub fn new(backend: impl WebViewBackend, config: Config) -> Self {
        Self {
            backend: Box::new(backend),
            config: Config {
                capacity: config.capacity.max(1),
                ..config
            },
            attached: false,
            next_id: 1,
            frame_serial: 0,
            surfaces: Vec::new(),
            events: Vec::new(),
            loader: None,
        }
    }

    /// Installs the streaming source for `Source::Local`. Must happen before
    /// the first `activate` of a local surface.
    pub fn set_scheme_loader(&mut self, loader: Arc<dyn SchemeLoader>) {
        self.loader = Some(loader.clone());
        self.backend.set_scheme_loader(loader);
    }

    /// Binds the backend to the window. Idempotent; the app calls it from its
    /// first paint, before any activation. A backend that cannot attach
    /// reports the failure here, and `activate` then maps the backend's own
    /// error to `Error::Unsupported`.
    pub fn attach(&mut self, window: &Window) -> Result<()> {
        if self.attached {
            return Ok(());
        }
        self.backend.attach(window)?;
        self.attached = true;
        Ok(())
    }

    /// Whether the backend is bound to its window.
    pub fn is_attached(&self) -> bool {
        self.attached
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// The number of live surfaces (never above `capacity`).
    pub fn live_count(&self) -> usize {
        self.surfaces.len()
    }

    /// The key of the focused surface, if any.
    pub fn focused_key(&self) -> Option<u64> {
        self.surfaces
            .iter()
            .find(|surface| surface.focused)
            .map(|surface| surface.key)
    }

    /// Whether `key` currently has a live surface.
    pub fn is_active(&self, key: u64) -> bool {
        self.surfaces.iter().any(|surface| surface.key == key)
    }

    /// The surface id behind `key`, if live.
    pub fn surface_id(&self, key: u64) -> Option<SurfaceId> {
        self.surfaces
            .iter()
            .find(|surface| surface.key == key)
            .map(|surface| surface.id)
    }

    /// Makes `key` live and returns its surface id. Repeating the call for an
    /// already-live key refreshes the spec (navigating only when the content
    /// or policy actually changed); over capacity, the least recently placed
    /// engine is **reused by navigation, never recreated**.
    pub fn activate(&mut self, key: u64, spec: SurfaceSpec) -> Result<SurfaceId> {
        if !spec_supported(self.backend.as_ref(), &spec) {
            return Err(Error::Unsupported);
        }
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|surface| surface.key == key)
        {
            if self.surfaces[index].spec != spec {
                let id = self.surfaces[index].id;
                self.backend.navigate(id, spec.clone())?;
                self.surfaces[index].spec = spec;
            }
            return Ok(self.surfaces[index].id);
        }
        if self.surfaces.len() < self.config.capacity {
            let id = self.next_id;
            self.next_id += 1;
            self.backend.create(id, spec.clone())?;
            self.surfaces.push(Surface {
                id,
                key,
                spec,
                focused: false,
                last_seen: self.frame_serial,
                last_placement: None,
            });
            return Ok(id);
        }
        // Over capacity: evict the least recently placed surface and navigate
        // its engine to the new content.
        let victim = self
            .surfaces
            .iter()
            .enumerate()
            .min_by_key(|(_, surface)| surface.last_seen)
            .map(|(index, _)| index)
            .expect("capacity >= 1 implies a surface exists");
        let id = self.surfaces[victim].id;
        let old_key = self.surfaces[victim].key;
        self.backend.navigate(id, spec.clone())?;
        self.events.push(WebViewEvent::Closed { id, key: old_key });
        self.surfaces[victim] = Surface {
            id,
            key,
            spec,
            focused: false,
            last_seen: self.frame_serial,
            last_placement: None,
        };
        Ok(id)
    }

    /// Renames a live key without touching the backend: the surface keeps its
    /// engine and page, only its caller-facing identity moves (an edit shifted
    /// the block's index). `frame` places it by the new key from then on.
    pub fn rekey(&mut self, old_key: u64, new_key: u64) {
        if let Some(surface) = self.surfaces.iter_mut().find(|surface| surface.key == old_key) {
            surface.key = new_key;
        }
    }

    /// Tears down `key`'s surface (Esc, the toolbar's Done, scroll-out). A
    /// no-op for a key that is not live.
    pub fn deactivate(&mut self, key: u64) {
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|surface| surface.key == key)
        {
            let surface = self.surfaces.remove(index);
            self.backend.destroy(surface.id);
            self.events.push(WebViewEvent::Closed {
                id: surface.id,
                key: surface.key,
            });
        }
    }

    /// Tears every surface down (a document change, unmount).
    pub fn deactivate_all(&mut self) {
        for surface in std::mem::take(&mut self.surfaces) {
            self.backend.destroy(surface.id);
            self.events.push(WebViewEvent::Closed {
                id: surface.id,
                key: surface.key,
            });
        }
    }

    /// Routes keyboard focus into (or out of) `key`'s surface.
    pub fn set_focus(&mut self, key: u64, focused: bool) {
        let Some(index) = self.surfaces.iter().position(|surface| surface.key == key) else {
            return;
        };
        if self.surfaces[index].focused == focused {
            return;
        }
        self.surfaces[index].focused = focused;
        let id = self.surfaces[index].id;
        self.backend.set_focus(id, focused);
    }

    /// Presents `key`'s surface (inline, window, fullscreen).
    pub fn present(&mut self, key: u64, mode: Presentation) -> Result<()> {
        let Some(surface) = self.surfaces.iter().find(|surface| surface.key == key) else {
            return Err(Error::Platform("not live".into()));
        };
        self.backend.present(surface.id, mode)
    }

    /// Once per frame: the live placements, the viewport clip and the GPUI
    /// overlay rects. Forwards only changes; applies the overlay and teardown
    /// policy. The backend must have been `attach`ed by the caller's paint
    /// path before the first activation.
    pub fn frame(
        &mut self,
        live: &[(u64, Placement)],
        viewport: Bounds<Pixels>,
        overlays: &[Bounds<Pixels>],
    ) {
        self.frame_serial += 1;
        self.events.extend(self.backend.events());

        // Teardown first, so an engine freed this frame can be reused by an
        // activation later in the same frame's input handling.
        let mut index = 0;
        while index < self.surfaces.len() {
            let surface = &self.surfaces[index];
            let live_entry = live.iter().find(|(key, _)| *key == surface.key);
            let should_close = match live_entry {
                None => true,
                Some((_, placement)) => !placement.visible,
            };
            if should_close {
                let surface = self.surfaces.remove(index);
                self.backend.destroy(surface.id);
                self.events.push(WebViewEvent::Closed {
                    id: surface.id,
                    key: surface.key,
                });
                continue;
            }
            index += 1;
        }

        for (key, placement) in live {
            let serial = self.frame_serial;
            let Some(surface) = self.surfaces.iter_mut().find(|surface| surface.key == *key) else {
                continue;
            };
            surface.last_seen = serial;
            // Defensive clamp: the caller computes the clip from the same
            // viewport, but a stale placement must never draw outside it.
            let mut effective = *placement;
            effective.clip = effective.clip.intersect(&viewport);
            if effective.clip.size.width <= Pixels::ZERO
                || effective.clip.size.height <= Pixels::ZERO
            {
                effective.visible = false;
            }
            let intersects = overlays.iter().any(|overlay| {
                intersects_rects(&effective.rect.intersect(&effective.clip), overlay)
            });
            match (intersects, self.config.on_overlay) {
                (false, _) => effective.overlay = false,
                (true, OverlayBehavior::Lower) => {
                    // The backend lowers the surface below GPUI where it can;
                    // a backend that cannot must hide it instead.
                    effective.overlay = true;
                }
                (true, OverlayBehavior::Freeze) => {
                    effective.visible = false;
                    effective.overlay = false;
                }
            }
            if surface.last_placement.as_ref() == Some(&effective) {
                continue;
            }
            surface.last_placement = Some(effective);
            let id = surface.id;
            self.backend.place(id, effective);
        }
    }

    /// Drains the queued events, oldest first.
    pub fn events(&mut self) -> Vec<WebViewEvent> {
        self.events.extend(self.backend.events());
        std::mem::take(&mut self.events)
    }
}

/// Whether two rects overlap at all (edge contact does not count).
fn intersects_rects(a: &Bounds<Pixels>, b: &Bounds<Pixels>) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && b.origin.x < a.origin.x + a.size.width
        && a.origin.y < b.origin.y + b.size.height
        && b.origin.y < a.origin.y + a.size.height
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockBackend;
    use crate::spec::{Policy, Source};
    use gpui_kit::{point, px, size};

    fn spec(label: &str) -> SurfaceSpec {
        SurfaceSpec::new(
            Source::Url(format!("https://example.com/{label}")),
            Policy::media(),
            label,
        )
    }

    fn placement(x: f32) -> Placement {
        Placement::new(
            Bounds::new(point(px(x), px(0.)), size(px(100.), px(80.))),
            Bounds::new(point(px(0.), px(0.)), size(px(800.), px(600.))),
        )
    }

    fn viewport() -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), size(px(800.), px(600.)))
    }

    /// The tests drive the manager with a window-free mock: the backend trait
    /// is stubbed, so no window is needed.
    fn manager(mock: MockBackend, config: Config) -> WebViews {
        WebViews::new(mock, config)
    }

    #[test]
    fn frame_forwards_only_changes() {
        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config::default());
        webviews.activate(1, spec("a")).expect("activate");
        let live = [(1u64, placement(10.))];
        webviews.frame(&live, viewport(), &[]);
        webviews.frame(&live, viewport(), &[]);
        webviews.frame(&live, viewport(), &[]);
        assert_eq!(recorder.placed().len(), 1, "unchanged frames are skipped");
        webviews.frame(&[(1u64, placement(20.))], viewport(), &[]);
        assert_eq!(recorder.placed().len(), 2);
    }

    #[test]
    fn scroll_out_tears_down_the_surface() {
        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config::default());
        let id = webviews.activate(1, spec("a")).expect("activate");
        // Fully outside the viewport: `visible` is false.
        let mut outside = placement(0.);
        outside.visible = false;
        webviews.frame(&[(1u64, outside)], viewport(), &[]);
        assert_eq!(webviews.live_count(), 0, "torn down on scroll-out");
        assert_eq!(recorder.destroyed(), vec![id]);
        assert!(webviews.events().iter().any(|event| matches!(
            event,
            WebViewEvent::Closed { id: destroyed, key: 1 } if *destroyed == id
        )));
    }

    #[test]
    fn overlay_lowers_with_lower_and_freezes_with_freeze() {
        let overlay = Bounds::new(point(px(20.), px(20.)), size(px(200.), px(200.)));

        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut lower = manager(
            mock,
            Config {
                capacity: 1,
                on_overlay: OverlayBehavior::Lower,
            },
        );
        lower.activate(1, spec("a")).expect("activate");
        lower.frame(&[(1u64, placement(0.))], viewport(), &[overlay]);
        let placed = recorder.placed();
        assert_eq!(placed.len(), 1);
        assert!(placed[0].1.overlay && placed[0].1.visible, "lowered, still visible");

        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut freeze = manager(
            mock,
            Config {
                capacity: 1,
                on_overlay: OverlayBehavior::Freeze,
            },
        );
        freeze.activate(1, spec("a")).expect("activate");
        freeze.frame(&[(1u64, placement(0.))], viewport(), &[overlay]);
        let placed = recorder.placed();
        assert_eq!(placed.len(), 1);
        assert!(!placed[0].1.visible, "frozen to the poster");
    }

    #[test]
    fn activate_creates_and_deactivate_destroys() {
        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config::default());
        let id = webviews.activate(7, spec("a")).expect("activate");
        assert_eq!(webviews.live_count(), 1);
        assert!(webviews.is_active(7));
        assert_eq!(recorder.created(), vec![id]);
        webviews.deactivate(7);
        assert_eq!(webviews.live_count(), 0);
        assert_eq!(recorder.destroyed(), vec![id]);
        assert_eq!(
            webviews.events(),
            vec![WebViewEvent::Closed { id, key: 7 }]
        );
    }

    #[test]
    fn a_second_activation_over_capacity_reuses_by_navigation() {
        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config::default());
        let first = webviews.activate(1, spec("a")).expect("activate a");
        let second = webviews.activate(2, spec("b")).expect("activate b");
        assert_eq!(first, second, "the engine is reused, not recreated");
        assert_eq!(recorder.created().len(), 1, "one engine ever created");
        assert_eq!(recorder.navigated().len(), 1, "reuse navigates");
        assert!(!webviews.is_active(1));
        assert!(webviews.is_active(2));
        assert_eq!(webviews.live_count(), 1);
    }

    #[test]
    fn re_activating_the_same_key_does_not_navigate() {
        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config::default());
        webviews.activate(1, spec("a")).expect("activate");
        webviews.activate(1, spec("a")).expect("re-activate");
        assert!(recorder.navigated().is_empty(), "same spec is a no-op");
        webviews.activate(1, spec("b")).expect("content change");
        assert_eq!(recorder.navigated().len(), 1, "content change navigates");
    }

    #[test]
    fn capacity_two_keeps_both_and_evicts_the_least_recent() {
        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config { capacity: 2, ..Config::default() });
        webviews.activate(1, spec("a")).expect("a");
        webviews.activate(2, spec("b")).expect("b");
        assert_eq!(recorder.created().len(), 2);
        assert_eq!(webviews.live_count(), 2);
        let third = webviews.activate(3, spec("c")).expect("c");
        assert_eq!(recorder.created().len(), 2, "no third engine");
        assert_eq!(recorder.navigated().len(), 1);
        assert!(webviews.is_active(3));
        assert_eq!(webviews.live_count(), 2);
        // The first surface (never placed) was the least recent.
        assert!(!webviews.is_active(1));
        assert!(webviews.surface_id(3) == Some(third));
    }

    #[test]
    fn unsupported_sources_never_create_an_engine() {
        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config::default());
        let bad = SurfaceSpec::new(
            Source::Url("javascript:alert(1)".into()),
            Policy::media(),
            "bad",
        );
        assert_eq!(webviews.activate(1, bad), Err(Error::Unsupported));
        assert!(recorder.created().is_empty());
        // Local sources need a scheme loader: the unsupported mock refuses.
        let local = SurfaceSpec::new(
            Source::Local {
                scheme: "nn-clip",
                path: "hash".into(),
            },
            Policy::strict(),
            "clip",
        );
        assert_eq!(webviews.activate(2, local), Err(Error::Unsupported));
    }

    #[test]
    fn focus_is_forwarded_only_on_change() {
        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config::default());
        webviews.activate(1, spec("a")).expect("activate");
        webviews.set_focus(1, true);
        webviews.set_focus(1, true);
        webviews.set_focus(1, false);
        assert_eq!(recorder.focus_changes(), vec![(1, true), (1, false)]);
    }

    #[test]
    fn events_drain_in_order() {
        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config::default());
        let id = webviews.activate(1, spec("a")).expect("activate");
        recorder.send(WebViewEvent::TitleChanged {
            id,
            title: "loaded".into(),
        });
        recorder.send(WebViewEvent::LoadFailed {
            id,
            error: Error::Load("boom".into()),
        });
        assert_eq!(webviews.events().len(), 2);
        assert!(webviews.events().is_empty());
    }

    #[test]
    fn a_failing_backend_reports_an_error_instead_of_panicking() {
        let mock = MockBackend::new().failing_create();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config::default());
        assert!(matches!(
            webviews.activate(1, spec("a")),
            Err(Error::Platform(_))
        ));
        assert!(recorder.created().is_empty());
        assert_eq!(webviews.live_count(), 0);
    }
}
