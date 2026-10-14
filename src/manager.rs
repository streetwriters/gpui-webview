//! The pool and its policy: capacity, reuse, yield, teardown (EMBEDS.md §8).
//!
//! The manager owns the backend, mints [`SurfaceId`]s, keeps at most
//! `Config::capacity` engines alive and forwards only changes, so a caller can
//! drive it from its paint pass every frame.

use std::sync::Arc;

use gpui_kit::{Bounds, Pixels, Window};

use crate::host::{SchemeLoader, WebViewBackend, spec_supported};
pub use crate::spec::WebViewEvent as Event;
use crate::spec::{
    Config, Error, OverlayBehavior, Placement, Presentation, Result, SurfaceId, SurfaceSpec,
    WebViewEvent,
};

/// One live surface as the manager knows it.
struct Surface {
    id: SurfaceId,
    key: u64,
    spec: SurfaceSpec,
    focused: bool,
    /// Frame counter of its last placement; the eviction order for reuse.
    last_seen: u64,
    /// The last placement forwarded, so unchanged frames are skipped.
    last_placement: Option<Placement>,
}

/// Imported dma-buf textures and failed-import memos, keyed by the backend
/// `SurfaceId` (never the caller key, so a `rekey` cannot strand entries) and
/// popup id (`0` = the page), then by the buffer's never-reused generation
/// (T10). The last few imports per slot are kept; a failed generation is
/// remembered until an import succeeds again, so the import is not retried per
/// paint. No-op without Linux + `dma-buf`, so teardown call sites need no `cfg`.
#[derive(Default)]
struct DmaBufCache {
    #[cfg(dmabuf)]
    textures: std::collections::HashMap<(SurfaceId, u64), Vec<(u64, gpui_kit::ExternalTexture)>>,
    #[cfg(dmabuf)]
    failed: std::collections::HashMap<(SurfaceId, u64), Vec<u64>>,
}

impl DmaBufCache {
    /// Drops every slot of `id`'s surface (page and popups).
    fn forget(&mut self, id: SurfaceId) {
        #[cfg(dmabuf)]
        {
            self.textures.retain(|(owner, _), _| *owner != id);
            self.failed.retain(|(owner, _), _| *owner != id);
        }
        #[cfg(not(dmabuf))]
        let _ = id;
    }

    /// Keeps `id`'s page slot and the popups in `live`; drops its other popup slots.
    fn retain_popups(&mut self, id: SurfaceId, live: &[u64]) {
        #[cfg(dmabuf)]
        {
            self.textures
                .retain(|(owner, popup), _| *owner != id || *popup == 0 || live.contains(popup));
            self.failed
                .retain(|(owner, popup), _| *owner != id || *popup == 0 || live.contains(popup));
        }
        #[cfg(not(dmabuf))]
        let _ = (id, live);
    }

    /// Drops every slot whose surface id is not in `live`.
    fn retain(&mut self, live: &[SurfaceId]) {
        #[cfg(dmabuf)]
        {
            self.textures.retain(|(owner, _), _| live.contains(owner));
            self.failed.retain(|(owner, _), _| live.contains(owner));
        }
        #[cfg(not(dmabuf))]
        let _ = live;
    }

    /// Imports a dma-buf in `slot`; `None` for image content or a failed import.
    #[cfg(dmabuf)]
    fn import(
        &mut self,
        slot: (SurfaceId, u64),
        content: &crate::spec::SurfaceContent,
        window: &mut Window,
    ) -> Option<gpui_kit::ExternalTexture> {
        use crate::spec::SurfaceContent;

        let SurfaceContent::Dmabuf(frame) = content else {
            return None;
        };
        let generation = frame.buffer_id;
        if self
            .failed
            .get(&slot)
            .is_some_and(|failed| failed.contains(&generation))
        {
            return None;
        }
        let cache = self.textures.entry(slot).or_default();
        if let Some((_, texture)) = cache.iter().find(|(cached, _)| *cached == generation) {
            return Some(texture.clone());
        }

        let descriptor = gpui_kit::DmaBufDescriptor {
            width: frame.width,
            height: frame.height,
            fourcc: frame.fourcc,
            modifier: frame.modifier,
            planes: frame
                .planes
                .iter()
                .map(|plane| gpui_kit::DmaBufPlane {
                    fd: std::os::fd::AsRawFd::as_raw_fd(&*plane.fd),
                    offset: plane.offset,
                    stride: plane.stride,
                })
                .collect(),
        };
        match window.import_dmabuf(descriptor) {
            Ok(texture) => {
                self.failed.remove(&slot);
                // A rotating pool: dropping the oldest handle frees its GPU texture.
                if cache.len() >= 4 {
                    cache.remove(0);
                }
                cache.push((generation, texture.clone()));
                Some(texture)
            }
            Err(error) => {
                // One log per failure burst; every failing generation is remembered so
                // the import is not retried per commit. The poster stays meanwhile.
                let failed = self.failed.entry(slot).or_default();
                let first_of_burst = failed.is_empty();
                if !failed.contains(&generation) {
                    // Bounded: a client allocating failing buffers forever must not grow this.
                    if failed.len() >= 8 {
                        failed.remove(0);
                    }
                    failed.push(generation);
                }
                if first_of_burst {
                    eprintln!(
                        "gpui-webview: dma-buf generation {generation} for {slot:?} failed to \
                         import (keeping the poster): {error:#}"
                    );
                }
                None
            }
        }
    }
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
    dmabuf: DmaBufCache,
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
    /// What `Config::on_overlay` should be for `platform_default`'s backend on
    /// this target: `Lower` where the backend self-composites (the nested Linux
    /// compositor), `Freeze` elsewhere. A caller that needs the exact answer
    /// reads [`WebViewBackend::default_overlay_behavior`].
    pub fn platform_overlay_behavior() -> OverlayBehavior {
        #[cfg(all(target_os = "linux", feature = "nested"))]
        {
            OverlayBehavior::Lower
        }
        #[cfg(not(all(target_os = "linux", feature = "nested")))]
        {
            OverlayBehavior::Freeze
        }
    }

    /// Picks the best backend this target/session can run; `Unsupported` when
    /// none is available (the app then uses posters + `openUrl`).
    pub fn platform_default(config: Config) -> Result<Self> {
        #[cfg(all(target_arch = "wasm32", feature = "web"))]
        {
            return Ok(Self::new(crate::backend::web::WebBackend::new(), config));
        }
        // The nested compositor is the only inline path with clip + overlay parity.
        #[cfg(all(target_os = "linux", feature = "nested"))]
        {
            if let Ok(backend) = crate::backend::nested::NestedBackend::new() {
                return Ok(Self::new(backend, config));
            }
        }
        #[cfg(all(not(target_arch = "wasm32"), feature = "wry"))]
        {
            match crate::backend::native::NativeBackend::new() {
                Ok(backend) => return Ok(Self::new(backend, config)),
                // No usable engine on this session: fall through to Unsupported.
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
            dmabuf: DmaBufCache::default(),
        }
    }

    /// Installs the streaming source for `Source::Local`, before any local `activate`.
    pub fn set_scheme_loader(&mut self, loader: Arc<dyn SchemeLoader>) {
        self.loader = Some(loader.clone());
        self.backend.set_scheme_loader(loader);
    }

    /// Binds the backend to the window (idempotent; call from the first paint).
    /// A failure here becomes `Error::Unsupported` on `activate`.
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

    /// Makes `key` live. Re-activation refreshes the spec (navigating only on a
    /// real change); over capacity the least recently placed engine is reused.
    pub fn activate(&mut self, key: u64, spec: SurfaceSpec) -> Result<SurfaceId> {
        if !spec_supported(self.backend.as_ref(), &spec) {
            return Err(Error::Unsupported);
        }
        if let Some(index) = self.surfaces.iter().position(|surface| surface.key == key) {
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
        // Over capacity: evict the least recently placed surface by navigation.
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
        // The reused engine's old page/popup textures are dead; release them now.
        self.dmabuf.forget(id);
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

    /// Renames a live key without touching the backend (an edit shifted the
    /// block's index); the dma-buf cache keys on the surface id, so nothing moves.
    pub fn rekey(&mut self, old_key: u64, new_key: u64) {
        if let Some(surface) = self
            .surfaces
            .iter_mut()
            .find(|surface| surface.key == old_key)
        {
            surface.key = new_key;
        }
    }

    /// Tears down `key`'s surface; a no-op when it is not live.
    pub fn deactivate(&mut self, key: u64) {
        if let Some(index) = self.surfaces.iter().position(|surface| surface.key == key) {
            self.remove_surface(index);
        }
    }

    /// Tears every surface down (a document change, unmount).
    pub fn deactivate_all(&mut self) {
        while !self.surfaces.is_empty() {
            self.remove_surface(0);
        }
    }

    /// Destroys the surface at `index`, drops its dma-buf slots and queues
    /// `Closed`. The one teardown path for every caller.
    fn remove_surface(&mut self, index: usize) {
        let surface = self.surfaces.remove(index);
        self.backend.destroy(surface.id);
        self.dmabuf.forget(surface.id);
        self.events.push(WebViewEvent::Closed {
            id: surface.id,
            key: surface.key,
        });
    }

    /// The latest decoded frame of `key`'s surface (self-compositing backends
    /// only). Raw read: use [`Self::ready_frame`] to paint (it imports dma-buf
    /// content and caches the texture).
    pub fn frame_image(&mut self, key: u64) -> Option<crate::spec::SurfaceFrame> {
        let surface = self.surfaces.iter().find(|surface| surface.key == key)?;
        self.backend.frame_image(surface.id)
    }

    /// Test-support (`test-support` feature): `key`'s page as PNG bytes,
    /// rendered by the backend itself (WebView2 `CapturePreview`), so it needs
    /// no desktop compositor. `None` when the backend cannot render one or
    /// `key` is not live; the Windows E2E calls it.
    #[cfg(feature = "test-support")]
    pub fn page_snapshot(&mut self, key: u64) -> Option<Vec<u8>> {
        let id = self.surfaces.iter().find(|surface| surface.key == key)?.id;
        self.backend.page_snapshot(id)
    }

    /// Native popups of `key`'s surface, bottom to top. Raw read: use
    /// [`Self::ready_popups`] to paint them.
    pub fn popup_frames(&mut self, key: u64) -> Vec<crate::spec::PopupFrame> {
        let Some(surface) = self.surfaces.iter().find(|surface| surface.key == key) else {
            return Vec::new();
        };
        let popups = self.backend.popup_frames(surface.id);
        // Keep only live popup cache slots (and the page's).
        let live: Vec<u64> = popups.iter().map(|popup| popup.id).collect();
        self.dmabuf.retain_popups(surface.id, &live);
        popups
    }

    /// The latest frame of `key`'s surface, ready to paint: dma-buf content is
    /// imported through `window` on first sight of a client buffer; an image
    /// frame (or a backend without dma-buf) carries its pixels directly.
    pub fn ready_frame(
        &mut self,
        key: u64,
        window: &mut Window,
    ) -> Option<crate::paint::ReadyFrame> {
        let id = self.surfaces.iter().find(|surface| surface.key == key)?.id;
        let frame = self.backend.frame_image(id)?;
        #[cfg(dmabuf)]
        let texture = self.dmabuf.import((id, 0), &frame.content, window);
        #[cfg(not(dmabuf))]
        let _ = window;
        Some(crate::paint::ReadyFrame {
            frame,
            #[cfg(dmabuf)]
            texture,
        })
    }

    /// The native popups of `key`'s surface, bottom to top, ready to paint
    /// (dma-buf content is imported like [`Self::ready_frame`]).
    pub fn ready_popups(&mut self, key: u64, window: &mut Window) -> Vec<crate::paint::ReadyPopup> {
        let Some(surface) = self.surfaces.iter().find(|surface| surface.key == key) else {
            return Vec::new();
        };
        let id = surface.id;
        let popups = self.backend.popup_frames(id);
        // Keep only live popup cache slots (and the page's).
        let live: Vec<u64> = popups.iter().map(|popup| popup.id).collect();
        self.dmabuf.retain_popups(id, &live);
        popups
            .into_iter()
            .map(|popup| {
                #[cfg(dmabuf)]
                let texture = self
                    .dmabuf
                    .import((id, popup.id), &popup.frame.content, window);
                #[cfg(not(dmabuf))]
                let _ = window;
                crate::paint::ReadyPopup {
                    id: popup.id,
                    position: popup.position,
                    frame: crate::paint::ReadyFrame {
                        frame: popup.frame,
                        #[cfg(dmabuf)]
                        texture,
                    },
                }
            })
            .collect()
    }

    /// Advances the backend's main-context work (the GTK/WebKit pump on Linux)
    /// and reports whether any surface is still live, so the host's pump task
    /// knows when to stop. `frame_image`/`popup_frames` are pure reads.
    pub fn pump(&mut self) -> bool {
        self.backend.pump();
        !self.surfaces.is_empty()
    }

    /// Forwards one input event to `key`'s surface. Keys need keyboard focus;
    /// motion, buttons, wheel and enter/leave route by position (T25).
    pub fn input(&mut self, key: u64, event: crate::spec::WebInputEvent) {
        let Some(surface) = self.surfaces.iter().find(|surface| surface.key == key) else {
            return;
        };
        if !surface.focused && !event.is_positional() {
            return;
        }
        self.backend.input(surface.id, event);
    }

    /// The caller-facing key of a live surface id, if it is still live.
    pub fn key_for_surface(&self, id: SurfaceId) -> Option<u64> {
        self.surfaces
            .iter()
            .find(|surface| surface.id == id)
            .map(|surface| surface.key)
    }

    /// Installs the commit waker (see [`WebViewBackend::set_waker`]).
    pub fn set_waker(&mut self, waker: Arc<dyn Fn() + Send + Sync>) {
        self.backend.set_waker(waker);
    }

    /// Forwards the host's active keyboard layout (see
    /// [`WebViewBackend::set_keyboard_layout`]).
    pub fn set_keyboard_layout(&mut self, name: &str) {
        self.backend.set_keyboard_layout(name);
    }

    /// Reports that `key`'s latest frame was painted (see
    /// [`WebViewBackend::surface_presented`]); a no-op when the key is gone.
    pub fn presented(&mut self, key: u64) {
        if let Some(surface) = self.surfaces.iter().find(|surface| surface.key == key) {
            self.backend.surface_presented(surface.id);
        }
    }

    /// Moves backend events into the caller-facing queue; a backend `Closed` is
    /// translated to the caller's key and tears the surface down.
    fn drain_backend_events(&mut self) {
        for event in self.backend.events() {
            match event {
                WebViewEvent::Closed { id, .. } => {
                    if let Some(index) = self.surfaces.iter().position(|surface| surface.id == id) {
                        self.remove_surface(index);
                    }
                }
                event => self.events.push(event),
            }
        }
    }

    /// Whether `key`'s surface owns keyboard focus.
    pub fn is_focused(&self, key: u64) -> bool {
        self.surfaces
            .iter()
            .find(|surface| surface.key == key)
            .is_some_and(|surface| surface.focused)
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

    /// Once per frame: placements, viewport clip, overlays. Forwards changes and
    /// applies the overlay/teardown policy. The backend must be attached first.
    pub fn frame(
        &mut self,
        live: &[(u64, Placement)],
        viewport: Bounds<Pixels>,
        overlays: &[Bounds<Pixels>],
    ) {
        self.frame_serial += 1;
        self.drain_backend_events();

        // Teardown first, so a freed engine can be reused within the same frame.
        let mut index = 0;
        while index < self.surfaces.len() {
            let surface = &self.surfaces[index];
            let live_entry = live.iter().find(|(key, _)| *key == surface.key);
            let should_close = match live_entry {
                None => true,
                Some((_, placement)) => !placement.visible,
            };
            if should_close {
                self.remove_surface(index);
                continue;
            }
            index += 1;
        }

        // Textures live as long as their surface; dropping the handles frees them.
        let live_ids: Vec<SurfaceId> = self.surfaces.iter().map(|surface| surface.id).collect();
        self.dmabuf.retain(&live_ids);

        for (key, placement) in live {
            let serial = self.frame_serial;
            let Some(surface) = self.surfaces.iter_mut().find(|surface| surface.key == *key) else {
                continue;
            };
            surface.last_seen = serial;
            // Defensive clamp: a stale placement must never draw outside the viewport.
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
                    // The backend lowers where it can, hides where it cannot.
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
        self.drain_backend_events();
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

    /// Tests drive the manager with a window-free mock backend.
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
        assert!(
            placed[0].1.overlay && placed[0].1.visible,
            "lowered, still visible"
        );

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
        assert_eq!(webviews.events(), vec![WebViewEvent::Closed { id, key: 7 }]);
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
        let mut webviews = manager(
            mock,
            Config {
                capacity: 2,
                ..Config::default()
            },
        );
        webviews.activate(1, spec("a")).expect("a");
        webviews.activate(2, spec("b")).expect("b");
        assert_eq!(recorder.created().len(), 2);
        assert_eq!(webviews.live_count(), 2);
        let third = webviews.activate(3, spec("c")).expect("c");
        assert_eq!(recorder.created().len(), 2, "no third engine");
        assert_eq!(recorder.navigated().len(), 1);
        assert!(webviews.is_active(3));
        assert_eq!(webviews.live_count(), 2);
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
    fn pointer_events_are_positional_and_keys_wait_for_focus() {
        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config::default());
        let id = webviews.activate(1, spec("a")).expect("activate");
        let key = |pressed| crate::spec::WebInputEvent::Key {
            keystroke: gpui_kit::Keystroke {
                modifiers: Default::default(),
                key: "a".into(),
                key_char: Some("a".into()),
            },
            pressed,
        };
        webviews.input(
            1,
            crate::spec::WebInputEvent::PointerMotion { x: 1.0, y: 2.0 },
        );
        webviews.input(
            1,
            crate::spec::WebInputEvent::PointerEnter { x: 1.0, y: 2.0 },
        );
        webviews.input(1, crate::spec::WebInputEvent::PointerLeave);
        webviews.input(1, key(true));
        assert_eq!(recorder.inputs().len(), 3, "keys wait for keyboard focus");
        assert!(recorder.inputs().iter().all(|(surface, _)| *surface == id));
        webviews.set_focus(1, true);
        webviews.input(1, key(true));
        webviews.input(1, key(false));
        assert_eq!(
            recorder.inputs().len(),
            5,
            "focused keys pass straight through"
        );
    }

    #[test]
    fn a_backend_closed_event_tears_the_surface_down_with_its_key() {
        let mock = MockBackend::new();
        let recorder = mock.clone();
        let mut webviews = manager(mock, Config::default());
        let id = webviews.activate(9, spec("a")).expect("activate");
        // The backend reports the id only; the manager fills in the key.
        recorder.send(WebViewEvent::Closed { id, key: 0 });
        assert_eq!(webviews.events(), vec![WebViewEvent::Closed { id, key: 9 }]);
        assert_eq!(webviews.live_count(), 0);
        assert_eq!(recorder.destroyed(), vec![id]);
        recorder.send(WebViewEvent::Closed { id, key: 0 });
        assert!(webviews.events().is_empty());
    }

    #[test]
    fn key_for_surface_maps_back_to_the_caller_key() {
        let mock = MockBackend::new();
        let mut webviews = manager(mock, Config::default());
        let id = webviews.activate(4, spec("a")).expect("activate");
        assert_eq!(webviews.key_for_surface(id), Some(4));
        assert_eq!(webviews.key_for_surface(999), None);
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
        recorder.send(WebViewEvent::FullscreenRequested { id, on: true });
        assert_eq!(webviews.events().len(), 3);
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
