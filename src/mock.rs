//! A recording backend: no engine, no window, every call recorded
//! (EMBEDS.md §15).
//!
//! Used by this crate's tests, by headless UI tests in an application, and by
//! CI on every target (wasm included). It can also be configured to fail
//! `attach`/`create` so an app's `Error::Unsupported` path is exercised.

use std::sync::{Arc, Mutex};

use gpui_kit::Window;

use crate::host::WebViewBackend;
use crate::spec::{
    Error, Placement, Presentation, Result, SurfaceId, SurfaceSpec, WebInputEvent, WebViewEvent,
};

#[derive(Default)]
struct MockState {
    attached: bool,
    created: Vec<SurfaceId>,
    created_specs: Vec<(SurfaceId, SurfaceSpec)>,
    inputs: Vec<(SurfaceId, WebInputEvent)>,
    frame_image: Option<crate::spec::SurfaceFrame>,
    navigated: Vec<(SurfaceId, SurfaceSpec)>,
    destroyed: Vec<SurfaceId>,
    placed: Vec<(SurfaceId, Placement)>,
    focus: Vec<(SurfaceId, bool)>,
    presented: Vec<(SurfaceId, Presentation)>,
    events: Vec<WebViewEvent>,
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
    presented_surfaces: Vec<SurfaceId>,
    supports_local: bool,
    bridges_selection: bool,
    host_selections: Vec<(crate::spec::Selection, Option<String>)>,
    fail_attach: bool,
    fail_create: bool,
}

/// A backend that records every call. Cheap to clone; all clones share one
/// recording.
#[derive(Clone, Default)]
pub struct MockBackend {
    state: Arc<Mutex<MockState>>,
}

impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// A mock that can serve `Source::Local` (for the clip path's tests).
    pub fn supporting_local(self) -> Self {
        self.with(|state| state.supports_local = true)
    }

    /// A mock that bridges the selection, like the nested Linux compositor.
    pub fn bridging_selection(self) -> Self {
        self.with(|state| state.bridges_selection = true)
    }

    /// The host selections handed over, in order.
    pub fn host_selections(&self) -> Vec<(crate::spec::Selection, Option<String>)> {
        self.read(|state| state.host_selections.clone())
    }

    /// A mock whose `attach` fails, so `Error::Unsupported` propagation is
    /// testable.
    pub fn failing_attach(self) -> Self {
        self.with(|state| state.fail_attach = true)
    }

    /// A mock whose `create` fails (a missing runtime library).
    pub fn failing_create(self) -> Self {
        self.with(|state| state.fail_create = true)
    }

    fn with(self, update: impl FnOnce(&mut MockState)) -> Self {
        if let Ok(mut state) = self.state.lock() {
            update(&mut state);
        }
        self
    }

    fn read<T>(&self, read: impl FnOnce(&MockState) -> T) -> T {
        let state = self.state.lock().expect("mock state");
        read(&state)
    }

    /// Makes `frame_image` return this frame for every surface (the nested
    /// compositor's shm path, without a compositor).
    pub fn with_frame_image(self, frame: crate::spec::SurfaceFrame) -> Self {
        self.with(|state| state.frame_image = Some(frame))
    }

    /// Queues a backend-originated event; the manager drains it on `frame`.
    pub fn send(&self, event: WebViewEvent) {
        if let Ok(mut state) = self.state.lock() {
            state.events.push(event);
        }
    }

    /// Simulates a commit: `frame_image` reports `frame` from now on and the
    /// waker fires once, exactly as a self-compositing backend publishes.
    pub fn publish_frame(&self, frame: crate::spec::SurfaceFrame) {
        let waker = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.frame_image = Some(frame);
            state.waker.clone()
        };
        if let Some(waker) = waker {
            waker();
        }
    }

    pub fn attached(&self) -> bool {
        self.read(|state| state.attached)
    }

    pub fn created(&self) -> Vec<SurfaceId> {
        self.read(|state| state.created.clone())
    }

    /// The specs handed to `create`, in order. Complements `navigated` (the
    /// reuse path), so a test can assert the exact source/policy either way.
    pub fn created_specs(&self) -> Vec<(SurfaceId, SurfaceSpec)> {
        self.read(|state| state.created_specs.clone())
    }

    pub fn navigated(&self) -> Vec<(SurfaceId, SurfaceSpec)> {
        self.read(|state| state.navigated.clone())
    }

    pub fn destroyed(&self) -> Vec<SurfaceId> {
        self.read(|state| state.destroyed.clone())
    }

    pub fn placed(&self) -> Vec<(SurfaceId, Placement)> {
        self.read(|state| state.placed.clone())
    }

    pub fn focus_changes(&self) -> Vec<(SurfaceId, bool)> {
        self.read(|state| state.focus.clone())
    }

    pub fn presented(&self) -> Vec<(SurfaceId, Presentation)> {
        self.read(|state| state.presented.clone())
    }

    /// Every "the app painted this surface" report, in order (present pacing).
    pub fn presented_surfaces(&self) -> Vec<SurfaceId> {
        self.read(|state| state.presented_surfaces.clone())
    }

    /// Every input event forwarded through the manager, in order.
    pub fn inputs(&self) -> Vec<(SurfaceId, WebInputEvent)> {
        self.read(|state| state.inputs.clone())
    }
}

impl WebViewBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn attach(&mut self, _window: &Window) -> Result<()> {
        let fail = self.read(|state| state.fail_attach);
        if fail {
            return Err(Error::Unsupported);
        }
        if let Ok(mut state) = self.state.lock() {
            state.attached = true;
        }
        Ok(())
    }

    fn create(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        let fail = self.read(|state| state.fail_create);
        if fail {
            return Err(Error::Platform("mock create failure".into()));
        }
        if let Ok(mut state) = self.state.lock() {
            state.created.push(id);
            state.created_specs.push((id, spec));
        }
        Ok(())
    }

    fn navigate(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        if let Ok(mut state) = self.state.lock() {
            state.navigated.push((id, spec));
        }
        Ok(())
    }

    fn place(&mut self, id: SurfaceId, at: Placement) {
        if let Ok(mut state) = self.state.lock() {
            state.placed.push((id, at));
        }
    }

    fn set_focus(&mut self, id: SurfaceId, focused: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.focus.push((id, focused));
        }
    }

    fn frame_image(&mut self, _id: SurfaceId) -> Option<crate::spec::SurfaceFrame> {
        self.read(|state| state.frame_image.clone())
    }

    fn set_waker(&mut self, waker: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut state) = self.state.lock() {
            state.waker = Some(waker);
        }
    }

    fn surface_presented(&mut self, id: SurfaceId) {
        if let Ok(mut state) = self.state.lock() {
            state.presented_surfaces.push(id);
        }
    }

    fn input(&mut self, id: SurfaceId, event: WebInputEvent) {
        if let Ok(mut state) = self.state.lock() {
            state.inputs.push((id, event));
        }
    }

    fn present(&mut self, id: SurfaceId, mode: Presentation) -> Result<()> {
        if let Ok(mut state) = self.state.lock() {
            state.presented.push((id, mode));
        }
        Ok(())
    }

    fn destroy(&mut self, id: SurfaceId) {
        if let Ok(mut state) = self.state.lock() {
            state.destroyed.push(id);
        }
    }

    fn supports_local(&self) -> bool {
        self.read(|state| state.supports_local)
    }

    fn bridges_selection(&self) -> bool {
        self.read(|state| state.bridges_selection)
    }

    fn set_host_selection(&mut self, selection: crate::spec::Selection, text: Option<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.host_selections.push((selection, text));
        }
    }

    fn events(&mut self) -> Vec<WebViewEvent> {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.events))
            .unwrap_or_default()
    }
}
