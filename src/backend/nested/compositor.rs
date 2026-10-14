//! The compositor state and thread: smithay handlers, toplevel mapping and
//! the command loop.

use super::*;
use crate::debug::debug_log;
use super::buffers::{content_geometry, CommittedBuffer, HeldBuffer};
use super::input::apply_input;
use super::popups::{popup_geometry, popup_parent, Popup};

/// What a compositor surface is: a block's toplevel or a popup with the block
/// it belongs to. Stored on the surface itself (smithay's `data_map`), so
/// routing a commit, a destroy or an input event is a lookup instead of a scan
/// over every mapped surface.
#[derive(Clone, Copy, Debug)]
pub(super) enum SurfaceRole {
    Toplevel(SurfaceId),
    Popup {
        id: u64,
        block: SurfaceId,
        /// Top-left in the page's coordinates (kept current across
        /// repositions, so a submenu resolves through its parent popup).
        position: (i32, i32),
    },
}

/// Stores `role` on `surface`, overwriting an earlier one (a reposition updates
/// a popup's position).
pub(super) fn set_surface_role(surface: &WlSurface, role: SurfaceRole) {
    smithay::wayland::compositor::with_states(surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(|| std::sync::Mutex::new(role));
        if let Some(cell) = states.data_map.get::<std::sync::Mutex<SurfaceRole>>() {
            *cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = role;
        }
    });
}

/// The role stored on `surface`, if any.
pub(super) fn surface_role(surface: &WlSurface) -> Option<SurfaceRole> {
    smithay::wayland::compositor::with_states(surface, |states| {
        states
            .data_map
            .get::<std::sync::Mutex<SurfaceRole>>()
            .and_then(|cell| cell.lock().ok().map(|role| *role))
    })
}

/// The block id in a toplevel's title (`gpui-webview-{id}`), once the client
/// has set it.
fn toplevel_title_id(surface: &ToplevelSurface) -> Option<SurfaceId> {
    let title = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok().and_then(|data| data.title.clone()))
    });
    title
        .as_deref()
        .and_then(|title| title.strip_prefix("gpui-webview-"))
        .and_then(|id| id.parse::<SurfaceId>().ok())
}

/// The compositor state: the core globals plus per-surface bookkeeping. Lives
/// on the compositor thread only.
pub struct NestedState {
    pub(super) compositor_state: CompositorState,
    pub(super) shm_state: ShmState,
    pub(super) xdg_shell_state: XdgShellState,
    /// Kept alive so the `zxdg_output_manager_v1` global stays advertised.
    #[allow(dead_code)]
    pub(super) output_manager_state: OutputManagerState,
    pub(super) seat_state: SeatState<Self>,
    pub(super) pointer: PointerHandle<Self>,
    pub(super) keyboard: KeyboardHandle<Self>,
    /// Kept alive so the `wl_output` global stays advertised.
    #[allow(dead_code)]
    pub(super) output: Output,
    /// The size to configure each not-yet-mapped toplevel to.
    pub(super) requested: HashMap<SurfaceId, (u32, u32)>,
    /// Toplevels whose title (`gpui-webview-{id}`) has not arrived yet: the
    /// client sends `set_title` right after creating the shell surface, so the
    /// next dispatch can identify them (see `accept_pending_toplevels`).
    pub(super) pending_toplevels: Vec<ToplevelSurface>,
    /// Mapped toplevels by surface key. Kept as the shell handle (not just the
    /// `wl_surface`) so a resize can configure it again.
    pub(super) mapped: HashMap<SurfaceId, ToplevelSurface>,
    /// The page's content size per block (from `set_window_geometry`), which is
    /// the rectangle popups are positioned against.
    pub(super) content_sizes: HashMap<SurfaceId, (i32, i32)>,
    /// Live popups, in map order = paint order.
    pub(super) popups: Vec<Popup>,
    pub(super) next_popup_id: u64,
    pub(super) popups_shared: SharedPopups,
    /// `wl_data_device_manager`. GTK3 postpones binding `wl_seat` until this
    /// global exists (`required_device_manager_globals` in GDK), so without it
    /// the client has no pointer or keyboard at all — clicks and keys never
    /// reach the page. The selection itself is not bridged to the app's
    /// clipboard yet.
    #[allow(dead_code)]
    pub(super) data_device_state: DataDeviceState,
    /// `linux-dmabuf` state; the global only exists once the renderer's
    /// capabilities arrived from the main thread. Only built with `dma-buf`.
    #[cfg(all(target_os = "linux", feature = "dma-buf"))]
    pub(super) dmabuf_state: DmabufState,
    #[cfg(all(target_os = "linux", feature = "dma-buf"))]
    pub(super) dmabuf_global: Option<DmabufGlobal>,
    /// The `(fourcc, modifier)` pairs the global advertises; the import
    /// handler validates against them.
    #[cfg(all(target_os = "linux", feature = "dma-buf"))]
    pub(super) dmabuf_formats: Vec<(u32, u64)>,
    /// A per-commit sequence for frames (repaint detection).
    pub(super) commit_sequence: u64,
    /// The previous dma-buf buffer of each surface, held back one commit so the
    /// client cannot rewrite pixels the app may still import (see `commit`).
    pub(super) held_buffers: HashMap<SurfaceId, HeldBuffer>,
    /// Per-buffer identity for the app's dma-buf texture cache. A `wl_buffer`
    /// protocol id is *reused* once the client destroys the buffer (WebKit
    /// reallocating its pool on resize does exactly that), so the app cannot
    /// key its imports on the raw id: each buffer gets a generation here that
    /// is never reused, removed in `buffer_destroyed`.
    #[cfg(all(target_os = "linux", feature = "dma-buf"))]
    pub(super) buffer_generations: HashMap<u64, u64>,
    #[cfg(all(target_os = "linux", feature = "dma-buf"))]
    pub(super) next_buffer_generation: u64,
    /// The modifier keys the client currently has pressed, from the app's
    /// `Modifiers` events. GPUI suppresses modifier-only key events on Linux,
    /// so this is the only source; the state must match what the client's XKB
    /// sees for Shift+click and chords to work.
    pub(super) modifiers: gpui_kit::Modifiers,
    pub(super) focus: SharedFocus,
    pub(super) frames: SharedFrames,
    /// A monotonic clock for the millisecond timestamps the protocol wants.
    pub(super) epoch: std::time::Instant,
}


impl NestedState {
    /// The millisecond timestamp the protocol expects (monotonic).
    pub(super) fn now_ms(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }


    /// Forgets a toplevel completely: the requested size, the held buffer, the
    /// content size, its popups, the mapped surface, the frame and both focus
    /// records (handing the keyboard back before the surface goes away), and
    /// the held-modifier state. The one teardown path for `Command::Destroy`
    /// (the host closed it) and the client's own `destroyed` (a crash,
    /// `window.close()`).
    pub(super) fn forget_toplevel(&mut self, id: SurfaceId) {
        self.requested.remove(&id);
        self.release_held_buffer(id);
        self.content_sizes.remove(&id);
        self.dismiss_popups(id);
        let surface = self.mapped.remove(&id);
        if let Some(surface) = &surface {
            self.output.leave(surface.wl_surface());
        }
        lock(&self.frames).remove(&id);
        {
            let mut focused = lock(&self.focus);
            if *focused == Some(id) {
                *focused = None;
            }
        }
        let protocol_focused = surface.as_ref().is_some_and(|surface| {
            self.keyboard.current_focus().as_ref() == Some(surface.wl_surface())
        });
        if protocol_focused {
            // A dead surface must not keep the keyboard: smithay would send
            // key events nowhere.
            let keyboard = self.keyboard.clone();
            keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        }
        // A destroyed client starts fresh; a stale held-modifier state would
        // swallow the next surface's first press.
        self.modifiers = gpui_kit::Modifiers::default();
    }


    /// The content rectangle of `block`'s page: the rectangle popups are fitted
    /// into. Falls back to the size we configured the toplevel with.
    pub(super) fn content_size(&self, block: SurfaceId) -> (i32, i32) {
        self.content_sizes
            .get(&block)
            .copied()
            .or_else(|| {
                self.requested
                    .get(&block)
                    .map(|(width, height)| (*width as i32, *height as i32))
            })
            .unwrap_or((0, 0))
    }

    /// Identifies the toplevels waiting for their title and maps the ones that
    /// now have one. `new_toplevel` runs on the client's `get_toplevel`
    /// request, before the `set_title` that follows it, so the configure has
    /// to wait for the next dispatch.
    pub(super) fn accept_pending_toplevels(&mut self) {
        use smithay::reexports::wayland_server::Resource as _;

        let pending = std::mem::take(&mut self.pending_toplevels);
        for surface in pending {
            if !surface.wl_surface().is_alive() {
                continue;
            }
            let Some(id) = toplevel_title_id(&surface) else {
                self.pending_toplevels.push(surface);
                continue;
            };
            let requested = self.requested.get(&id).copied();
            surface.with_pending_state(|state| {
                state.states.set(
                    smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated,
                );
                if let Some((width, height)) = requested {
                    state.size = Some(Size::from((width as i32, height as i32)));
                }
            });
            let configured = surface.send_configure();
            // GDK's frame clock follows the output a surface is on; without
            // `wl_surface.enter` GTK kept the window mapped but never drew a
            // frame (the client-side trace showed no attach after the
            // configure).
            let wl_surface = surface.wl_surface().clone();
            self.output.enter(&wl_surface);
            debug_log!(
                "new_toplevel for block {id}, requested={requested:?}, configured={configured:?}"
            );
            self.mapped.insert(id, surface);
            set_surface_role(&wl_surface, SurfaceRole::Toplevel(id));
        }
    }


}

/// Per-client compositor state (smithay's protocol bookkeeping).
#[derive(Debug)]
pub(super) struct WebClientData {
    compositor: CompositorClientState,
}

impl WebClientData {
    fn new() -> Self {
        Self {
            compositor: CompositorClientState::default(),
        }
    }
}

impl smithay::reexports::wayland_server::backend::ClientData for WebClientData {}


/// The compositor thread: owns the `Display`, accepts clients, applies the
/// commands and publishes committed frames. Never touches GTK.
pub(super) fn compositor_main(
    socket: ListeningSocket,
    commands: Receiver<Command>,
    frames: SharedFrames,
    popups: SharedPopups,
    focus: SharedFocus,
    ready: Sender<std::result::Result<(), String>>,
) {
    let mut display = match Display::<NestedState>::new() {
        Ok(display) => display,
        Err(error) => {
            let _ = ready.send(Err(format!("wayland display: {error}")));
            return;
        }
    };
    let handle = display.handle();
    let compositor_state = CompositorState::new::<NestedState>(&handle);
    // Both 32-bit formats: GDK's SHM path allocates XRGB8888 for opaque
    // windows and ARGB8888 for translucent ones, and refuses to paint if its
    // preferred format was not advertised. (Missing XRGB8888 was why GTK
    // committed state without ever attaching a buffer.)
    let shm_state = ShmState::new::<NestedState>(
        &handle,
        vec![wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888],
    );
    let xdg_shell_state = XdgShellState::new::<NestedState>(&handle);
    let data_device_state = DataDeviceState::new::<NestedState>(&handle);
    let output_manager_state = OutputManagerState::new_with_xdg_output::<NestedState>(&handle);
    let output = Output::new(
        "gpui-webview".to_string(),
        PhysicalProperties {
            size: Size::from((1920, 1080)),
            subpixel: smithay::output::Subpixel::Unknown,
            make: "Notesnook".to_string(),
            model: "gpui-webview".to_string(),
        },
    );
    // First (and only) output: without `wl_output` GTK has no monitor at all
    // and floods `gdk_monitor_get_scale_factor` criticals; WebKit wants a
    // screen size and scale too. One mode at 1x, 60 Hz.
    output.change_current_state(
        Some(Mode {
            size: Size::from((1920, 1080)),
            refresh: 60_000,
        }),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        None,
    );
    output.set_preferred(Mode {
        size: Size::from((1920, 1080)),
        refresh: 60_000,
    });
    let _output_global = output.create_global::<NestedState>(&handle);

    let mut seat_state = SeatState::<NestedState>::new();
    let mut seat = seat_state.new_wl_seat(&handle, "gpui-webview");
    let pointer = seat.add_pointer();
    let keyboard = match seat.add_keyboard(XkbConfig::default(), 500, 25) {
        Ok(keyboard) => keyboard,
        Err(error) => {
            let _ = ready.send(Err(format!("xkb keymap: {error}")));
            return;
        }
    };

    let mut state = NestedState {
        compositor_state,
        shm_state,
        xdg_shell_state,
        output_manager_state,
        seat_state,
        pointer,
        keyboard,
        output,
        requested: HashMap::new(),
        pending_toplevels: Vec::new(),
        mapped: HashMap::new(),
        content_sizes: HashMap::new(),
        popups: Vec::new(),
        next_popup_id: 1,
        popups_shared: popups,
        data_device_state,
        #[cfg(all(target_os = "linux", feature = "dma-buf"))]
        dmabuf_state: DmabufState::new(),
        #[cfg(all(target_os = "linux", feature = "dma-buf"))]
        dmabuf_global: None,
        #[cfg(all(target_os = "linux", feature = "dma-buf"))]
        dmabuf_formats: Vec::new(),
        commit_sequence: 0,
        held_buffers: HashMap::new(),
        #[cfg(all(target_os = "linux", feature = "dma-buf"))]
        buffer_generations: HashMap::new(),
        #[cfg(all(target_os = "linux", feature = "dma-buf"))]
        next_buffer_generation: 0,
        modifiers: gpui_kit::Modifiers::default(),
        focus,
        frames,
        epoch: std::time::Instant::now(),
    };
    let _ = ready.send(Ok(()));

    loop {
        // Commands from the main thread (surface lifecycle and input).
        loop {
            match commands.try_recv() {
                Ok(Command::Create { id, width, height }) => {
                    // The size a not-yet-mapped toplevel is configured to. The
                    // toplevel itself is matched by its window title
                    // (`gpui-webview-{id}`), not by creation order.
                    state.requested.insert(id, (width, height));
                }
                Ok(Command::Resize { id, width, height }) => {
                    state.requested.insert(id, (width, height));
                    // A mapped toplevel re-layouts now; one not yet mapped
                    // picks the new size up when `new_toplevel` configures it.
                    if let Some(toplevel) = state.mapped.get(&id).cloned() {
                        toplevel.with_pending_state(|pending| {
                            pending.size = Some(Size::from((width as i32, height as i32)));
                        });
                        let _serial = toplevel.send_configure();
                        debug_log!("resize: block {id} configured to {width}x{height}");
                    }
                }
                Ok(Command::Destroy { id }) => {
                    state.forget_toplevel(id);
                }
                Ok(Command::SetFocus { id, focused }) => {
                    let serial = SERIAL_COUNTER.next_serial();
                    let keyboard = state.keyboard.clone();
                    if focused {
                        if let Some(surface) = state
                            .mapped
                            .get(&id)
                            .map(|surface| surface.wl_surface().clone())
                        {
                            keyboard.set_focus(&mut state, Some(surface), serial);
                        }
                    } else {
                        // The protocol focus is authoritative. The main thread
                        // already cleared the shared flag *before* this command
                        // is processed (`NestedBackend::set_focus` sets it
                        // optimistically), so reading that flag here would
                        // always see `false` and never send `wl_keyboard.leave`.
                        let protocol_focus = state
                            .mapped
                            .get(&id)
                            .map(|surface| surface.wl_surface().clone())
                            .filter(|surface| state.keyboard.current_focus().as_ref() == Some(surface));
                        if protocol_focus.is_some() {
                            keyboard.set_focus(&mut state, None, serial);
                            // Focus left the block (the user clicked elsewhere
                            // in the editor): an open menu cannot outlive it.
                            state.dismiss_popups(id);
                        }
                    }
                    let mut target = lock(&state.focus);
                    if focused {
                        *target = Some(id);
                    } else if *target == Some(id) {
                        *target = None;
                    }
                }
                Ok(Command::Input { id, event }) => apply_input(&mut state, id, event),
                #[cfg(all(target_os = "linux", feature = "dma-buf"))]
                Ok(Command::DmabufFeedback(capabilities)) => {
                    let handle = display.handle();
                    state.enable_dmabuf(&handle, capabilities);
                }
                Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => break,
            }
        }

        // New client connections. The socket was bound before this thread
        // started, so a client may already be waiting.
        while let Ok(Some(stream)) = socket.accept() {
            let _ = stream.set_nonblocking(true);
            let mut display_handle = display.handle();
            match display_handle.insert_client(stream, Arc::new(WebClientData::new())) {
                Ok(client) => debug_log!("client connected: {:?}", client.id()),
                Err(error) => eprintln!("gpui-webview: inserting client failed: {error}"),
            }
        }

        if let Err(error) = display.dispatch_clients(&mut state) {
            eprintln!("gpui-webview: dispatch failed: {error}");
        }
        let _ = display.flush_clients();
        state.accept_pending_toplevels();

        // The design's calloop integration would block on the socket here;
        // until then, a short poll keeps latency low while idle cost is a few
        // cheap syscalls per tick.
        std::thread::sleep(Duration::from_millis(4));
    }
}


impl SeatHandler for NestedState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}


impl CompositorHandler for NestedState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<WebClientData>()
            .expect("clients are created with WebClientData")
            .compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        // A popup (menu, dropdown) above one of our toplevels: the same buffer
        // handling, a different sink. The role stored on the surface says which
        // it is, so no scan over every live surface is needed.
        let id = match surface_role(surface) {
            Some(SurfaceRole::Popup { id, .. }) => {
                self.commit_popup(id, surface);
                return;
            }
            Some(SurfaceRole::Toplevel(id)) => id,
            None => {
                debug_log!("commit for an unmapped surface; ignoring");
                return;
            }
        };

        // The content rect (`set_window_geometry`): GTK draws client-side
        // decorations and their shadows *around* the content, and only the
        // content belongs in the block. It is also what popups are positioned
        // against.
        let geometry = content_geometry(surface);
        if let Some(geometry) = geometry
            && geometry.size.w > 0
            && geometry.size.h > 0
        {
            self.content_sizes
                .insert(id, (geometry.size.w, geometry.size.h));
        }
        self.commit_sequence += 1;
        let sequence = self.commit_sequence;
        let committed = self.take_committed(surface, geometry, sequence);
        match committed {
            CommittedBuffer::New { frame, hold } => {
                debug_log!(
                    "commit: {}x{} frame for block {id}",
                    frame.size.0,
                    frame.size.1
                );
                lock(&self.frames).insert(id, frame);
                if let Some(buffer) = hold {
                    // Hold the buffer back one commit: releasing it now would
                    // let WebKit rewrite the pixels while the app imports and
                    // samples them. Implicit dma-fence sync orders the GPU work;
                    // explicit sync is a follow-up.
                    self.held_buffers.entry(id).or_default().hold(buffer);
                }
            }
            CommittedBuffer::Removed => {
                lock(&self.frames).remove(&id);
                self.release_held_buffer(id);
            }
            CommittedBuffer::Unchanged => debug_log!("commit: without a new buffer (state only)"),
            CommittedBuffer::Rejected => {
                debug_log!("commit: unusable buffer; keeping the last frame")
            }
        }

        self.fire_frame_callbacks(surface);
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        // One role per surface, so one branch: a popup is forgotten (no
        // `popup_done`, it is gone), a toplevel goes through the same teardown
        // as a host-side destroy.
        match surface_role(surface) {
            Some(SurfaceRole::Popup { id, .. }) => self.forget_popup(id),
            Some(SurfaceRole::Toplevel(id)) => self.forget_toplevel(id),
            None => {}
        }
    }
}


impl SelectionHandler for NestedState {
    // The selection is not bridged to the app's clipboard yet; the global
    // exists so that GDK binds the seat (see `data_device_state`).
    type SelectionUserData = ();
}


impl ClientDndGrabHandler for NestedState {}


impl ServerDndGrabHandler for NestedState {}


impl DataDeviceHandler for NestedState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}


impl OutputHandler for NestedState {
    fn output_bound(
        &mut self,
        _output: Output,
        _wl_output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    ) {
        debug_log!("client bound wl_output");
    }
}


impl XdgShellHandler for NestedState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // The title (`gpui-webview-{id}`) arrives with the client's initial
        // state, *after* this handler (which runs on `get_toplevel`), so the
        // surface is parked until the next dispatch can identify it.
        self.pending_toplevels.push(surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        // A `<select>` dropdown, a context menu, a date picker. The client
        // commits it like any other surface; the app paints it above the page's
        // frame (`PopupFrame`) and the compositor routes input into it.
        let Some(parent) = popup_parent(&surface) else {
            // The parent is not one of ours: refuse politely rather than paint
            // a popup at a meaningless position.
            debug_log!("popup without a parent; dismissing");
            surface.send_popup_done();
            return;
        };
        let Some((block, origin, geometry)) = popup_geometry(self, &parent, &positioner) else {
            debug_log!("popup without a mapped parent; dismissing");
            surface.send_popup_done();
            return;
        };
        surface.with_pending_state(|state| state.geometry = geometry);
        if let Err(error) = surface.send_configure() {
            debug_log!("popup configure failed: {error:?}");
        }
        let id = self.next_popup_id;
        self.next_popup_id += 1;
        let position = (origin.0 + geometry.loc.x, origin.1 + geometry.loc.y);
        debug_log!(
            "new_popup {id} for block {block} at {position:?} size {}x{}",
            geometry.size.w,
            geometry.size.h
        );
        // GDK's frame clock follows the output a surface is on: without
        // `wl_surface.enter` GTK keeps the popup mapped but never draws a frame
        // (the same trap as the toplevel, see `new_toplevel`).
        self.output.enter(surface.wl_surface());
        // A popup is about to cover part of the page: drop the pointer focus so
        // the next event enters whichever surface is under it (see
        // `refresh_pointer_focus`).
        self.refresh_pointer_focus();
        set_surface_role(
            surface.wl_surface(),
            SurfaceRole::Popup { id, block, position },
        );
        self.popups.push(Popup {
            id,
            parent: block,
            wl_surface: surface.wl_surface().clone(),
            surface,
            position,
            size: (geometry.size.w, geometry.size.h),
            mapped: false,
            held: HeldBuffer::default(),
        });
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        _token: u32,
    ) {
        // A menu re-anchors when the page scrolls under it. Recompute like the
        // first configure; `send_configure` refuses on a non-reactive positioner
        // or an old client, which is exactly the fallback we want.
        let Some(parent) = popup_parent(&surface) else {
            return;
        };
        let Some((block, origin, geometry)) = popup_geometry(self, &parent, &positioner) else {
            return;
        };
        surface.with_pending_state(|state| state.geometry = geometry);
        if let Err(error) = surface.send_configure() {
            debug_log!("popup reposition refused: {error:?}");
            return;
        }
        let position = (origin.0 + geometry.loc.x, origin.1 + geometry.loc.y);
        debug_log!("popup repositioned to {position:?}");
        let mut popup_id = None;
        if let Some(popup) = self
            .popups
            .iter_mut()
            .find(|popup| popup.wl_surface == *surface.wl_surface())
        {
            popup.position = position;
            popup.size = (geometry.size.w, geometry.size.h);
            popup_id = Some(popup.id);
        }
        if let Some(id) = popup_id {
            // The role must follow: a submenu anchors to its parent popup's
            // *current* position.
            set_surface_role(
                surface.wl_surface(),
                SurfaceRole::Popup { id, block, position },
            );
        }
    }

    fn grab(
        &mut self,
        surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: Serial,
    ) {
        // A grab is the client saying "dismiss me when the user clicks
        // elsewhere"; dismissal itself is driven by the press in `apply_input`.
        let id = match surface_role(surface.wl_surface()) {
            Some(SurfaceRole::Popup { id, .. }) => Some(id),
            _ => None,
        };
        debug_log!("popup {id:?} grabbed the seat");
    }
}

delegate_compositor!(NestedState);
delegate_shm!(NestedState);
#[cfg(all(target_os = "linux", feature = "dma-buf"))]
delegate_dmabuf!(NestedState);
delegate_data_device!(NestedState);
delegate_xdg_shell!(NestedState);
delegate_output!(NestedState);
delegate_seat!(NestedState);

// -- buffer copy -------------------------------------------------------------
