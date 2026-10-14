//! The compositor state and thread: smithay handlers, toplevel mapping, command loop.

use super::Command;
use super::Shared;
use crate::spec::SurfaceId;
use smithay::delegate_compositor;
use smithay::delegate_data_device;
#[cfg(dmabuf)]
use smithay::delegate_dmabuf;
use smithay::delegate_output;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_shell;
use smithay::input::SeatState;
use smithay::input::keyboard::KeyboardHandle;
use smithay::input::keyboard::XkbConfig;
use smithay::input::pointer::PointerHandle;
use smithay::output::Mode;
use smithay::output::Output;
use smithay::output::PhysicalProperties;
use smithay::output::Scale;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::calloop::Interest;
use smithay::reexports::calloop::Mode as PollMode;
use smithay::reexports::calloop::PostAction;
use smithay::reexports::calloop::channel::Channel;
use smithay::reexports::calloop::channel::Event as ChannelEvent;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::wayland_server::ListeningSocket;
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;
use smithay::utils::Serial;
use smithay::utils::Size;
use smithay::utils::Transform;
use smithay::wayland::compositor::CompositorClientState;
use smithay::wayland::compositor::CompositorState;
#[cfg(dmabuf)]
use smithay::wayland::dmabuf::DmabufGlobal;
#[cfg(dmabuf)]
use smithay::wayland::dmabuf::DmabufState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::PopupSurface;
use smithay::wayland::shell::xdg::PositionerState;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::wayland::shm::ShmState;
use std::collections::HashMap;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use super::buffers::{CommittedBuffer, HeldBuffer, content_geometry};
use super::input::apply_input;
use super::lock;
use super::popups::{Popup, popup_geometry, popup_parent};
use crate::debug::{debug_log, debug_log2};
use smithay::input::SeatHandler;
use smithay::wayland::compositor::CompositorHandler;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::ClientDndGrabHandler;
use smithay::wayland::selection::data_device::DataDeviceHandler;
use smithay::wayland::selection::data_device::ServerDndGrabHandler;
use smithay::wayland::shell::xdg::XdgShellHandler;

/// A surface's role: a block's toplevel, or a popup with its block. Stored in
/// the surface's `data_map`, so routing is a lookup.
#[derive(Clone, Copy, Debug)]
pub(super) enum SurfaceRole {
    Toplevel(SurfaceId),
    Popup { id: u64, block: SurfaceId },
}

/// Stores `role` on `surface`, overwriting an earlier one.
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

/// The block id in a toplevel's title (`gpui-webview-{id}`).
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

/// The compositor state: globals plus per-surface bookkeeping (compositor thread only).
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
    /// Key names to evdev codes, read from `keyboard`'s compiled keymap.
    pub(super) key_table: super::keymap::KeyTable,
    /// Kept alive so the `wl_output` global stays advertised.
    #[allow(dead_code)]
    pub(super) output: Output,
    /// The size to configure each not-yet-mapped toplevel to.
    pub(super) requested: HashMap<SurfaceId, (u32, u32)>,
    /// T11: toplevels whose title (`gpui-webview-{id}`) has not arrived yet.
    pub(super) pending_toplevels: Vec<ToplevelSurface>,
    /// Mapped toplevels by block; the shell handle, so a resize can configure them.
    pub(super) mapped: HashMap<SurfaceId, ToplevelSurface>,
    /// Page content size per block (`set_window_geometry`); popups fit into it.
    pub(super) content_sizes: HashMap<SurfaceId, (i32, i32)>,
    /// Live popups, in map order = paint order.
    pub(super) popups: Vec<Popup>,
    pub(super) next_popup_id: u64,
    /// T2: GTK3 binds no `wl_seat` without this global, so there is no input at all.
    #[allow(dead_code)]
    pub(super) data_device_state: DataDeviceState,
    /// `linux-dmabuf`; global appears once the renderer's capabilities arrive.
    #[cfg(dmabuf)]
    pub(super) dmabuf_state: DmabufState,
    #[cfg(dmabuf)]
    pub(super) dmabuf_global: Option<DmabufGlobal>,
    /// The advertised `(fourcc, modifier)` pairs; imports validate against them.
    #[cfg(dmabuf)]
    pub(super) dmabuf_formats: Vec<(u32, u64)>,
    /// A per-commit sequence for frames (repaint detection).
    pub(super) commit_sequence: u64,
    /// Toplevel frame callbacks parked until the app reports the surface
    /// painted (present pacing).
    pub(super) pending_frame_callbacks: HashMap<SurfaceId, Vec<WlCallback>>,
    /// The held dma-buf per surface (T14).
    pub(super) held_buffers: HashMap<SurfaceId, HeldBuffer>,
    /// T10: the never-reused generation per `wl_buffer` id (the app's cache key).
    #[cfg(dmabuf)]
    pub(super) buffer_generations: HashMap<u64, u64>,
    #[cfg(dmabuf)]
    pub(super) next_buffer_generation: u64,
    /// T9: the modifiers currently held client-side (GPUI sends no modifier-only key events).
    pub(super) modifiers: gpui_kit::Modifiers,
    /// The maps shared with the main thread.
    pub(super) shared: Arc<Shared>,
    /// A monotonic clock for the millisecond timestamps the protocol wants.
    pub(super) epoch: std::time::Instant,
}

impl NestedState {
    /// The millisecond timestamp the protocol expects (monotonic).
    pub(super) fn now_ms(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    /// Wakes the app: a frame was just published. The waker is cloned out of
    /// the lock first, so a waker that reads shared state cannot deadlock.
    pub(super) fn wake(&self) {
        let waker = lock(&self.shared.waker).clone();
        if let Some(waker) = waker {
            waker();
        }
    }

    /// Forgets a toplevel completely. The one teardown path for `Command::Destroy`
    /// and the client's own `destroyed`.
    pub(super) fn forget_toplevel(&mut self, id: SurfaceId) {
        self.requested.remove(&id);
        self.release_held_buffer(id);
        self.pending_frame_callbacks.remove(&id);
        self.content_sizes.remove(&id);
        self.dismiss_popups(id);
        let surface = self.mapped.remove(&id);
        if let Some(surface) = &surface {
            self.output.leave(surface.wl_surface());
        }
        lock(&self.shared.frames).remove(&id);
        {
            let mut focused = lock(&self.shared.focus);
            if *focused == Some(id) {
                *focused = None;
            }
        }
        let protocol_focused = surface.as_ref().is_some_and(|surface| {
            self.keyboard.current_focus().as_ref() == Some(surface.wl_surface())
        });
        if protocol_focused {
            // A dead surface must not keep the keyboard.
            let keyboard = self.keyboard.clone();
            keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        }
        // A stale held-modifier state would swallow the next surface's first press.
        self.modifiers = gpui_kit::Modifiers::default();
    }

    /// The rectangle popups are fitted into; falls back to the configured size.
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

    /// Identifies parked toplevels by title and maps them; T11 defers this a dispatch.
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
            // T3: without `wl_surface.enter` GDK's frame clock never ticks.
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

/// The loop's data: the display, the private socket and the compositor state.
/// Both fds are registered as `Generic` sources; every callback reaches them
/// through this struct.
struct CalloopData {
    state: NestedState,
    display: Display<NestedState>,
    socket: ListeningSocket,
}

/// The compositor thread: owns the `Display`, applies commands, publishes frames; never touches GTK.
pub(super) fn compositor_main(
    socket: ListeningSocket,
    commands: Channel<Command>,
    shared: Arc<Shared>,
    ready: Sender<std::result::Result<(), String>>,
) {
    let display = match Display::<NestedState>::new() {
        Ok(display) => display,
        Err(error) => {
            let _ = ready.send(Err(format!("wayland display: {error}")));
            return;
        }
    };
    let handle = display.handle();
    let compositor_state = CompositorState::new::<NestedState>(&handle);
    // T4: GDK allocates XRGB8888 for opaque and ARGB8888 for translucent; advertise both.
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
    // One output at 60 Hz: GTK needs a monitor (and floods criticals without
    // one). 1x until the app reports the window's scale (`set_output_scale`).
    let (mode, scale) = output_state(1.0);
    output.set_preferred(mode);
    output.change_current_state(Some(mode), Some(Transform::Normal), Some(scale), None);
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
        key_table: Default::default(),
        output,
        requested: HashMap::new(),
        pending_toplevels: Vec::new(),
        mapped: HashMap::new(),
        content_sizes: HashMap::new(),
        popups: Vec::new(),
        next_popup_id: 1,
        data_device_state,
        #[cfg(dmabuf)]
        dmabuf_state: DmabufState::new(),
        #[cfg(dmabuf)]
        dmabuf_global: None,
        #[cfg(dmabuf)]
        dmabuf_formats: Vec::new(),
        commit_sequence: 0,
        pending_frame_callbacks: HashMap::new(),
        held_buffers: HashMap::new(),
        #[cfg(dmabuf)]
        buffer_generations: HashMap::new(),
        #[cfg(dmabuf)]
        next_buffer_generation: 0,
        modifiers: gpui_kit::Modifiers::default(),
        shared,
        epoch: std::time::Instant::now(),
    };
    state.rebuild_key_table();
    // The loop blocks in `poll` while nothing happens, so an idle compositor
    // costs no CPU (EMBEDS_REFACTOR.md §Phase 5); the M3 baseline was a 4 ms poll.
    let mut event_loop: EventLoop<'static, CalloopData> = match EventLoop::try_new() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            let _ = ready.send(Err(format!("compositor event loop: {error}")));
            return;
        }
    };
    let mut data = CalloopData {
        state,
        display,
        socket,
    };

    // Commands from the main thread (surface lifecycle and input); the channel's
    // ping fd wakes the loop. `Shutdown` and the last sender dropping both stop it.
    let signal = event_loop.get_signal();
    event_loop
        .handle()
        .insert_source(
            commands,
            move |event, _, data: &mut CalloopData| match event {
                ChannelEvent::Msg(Command::Shutdown) => signal.stop(),
                ChannelEvent::Msg(command) => {
                    apply_command(data, command);
                    // The display fd wakes on *incoming* data only; anything just
                    // queued for a client (input, configure, popup done) must be
                    // written now, or it waits for the client's next message.
                    let _ = data.display.flush_clients();
                }
                ChannelEvent::Closed => signal.stop(),
            },
        )
        .expect("the command channel source is always insertable");

    // The display fd: wayland client messages. Level-triggered, so the source
    // stays armed; the dispatch drains the fd.
    let display_fd = data
        .display
        .backend()
        .poll_fd()
        .try_clone_to_owned()
        .expect("the display fd cannot be cloned");
    event_loop
        .handle()
        .insert_source(
            Generic::new(display_fd, Interest::READ, PollMode::Level),
            |_, _, data: &mut CalloopData| {
                if let Err(error) = data.display.dispatch_clients(&mut data.state) {
                    eprintln!("gpui-webview: dispatch failed: {error}");
                }
                let _ = data.display.flush_clients();
                // T11: a toplevel's title arrives in the same client batch.
                data.state.accept_pending_toplevels();
                Ok(PostAction::Continue)
            },
        )
        .expect("the display source is always insertable");

    // New client connections; the socket was bound before this thread started.
    let socket_fd = data
        .socket
        .as_fd()
        .try_clone_to_owned()
        .expect("the socket fd cannot be cloned");
    event_loop
        .handle()
        .insert_source(
            Generic::new(socket_fd, Interest::READ, PollMode::Level),
            |_, _, data: &mut CalloopData| {
                while let Ok(Some(stream)) = data.socket.accept() {
                    let _ = stream.set_nonblocking(true);
                    let mut display_handle = data.display.handle();
                    match display_handle.insert_client(stream, Arc::new(WebClientData::new())) {
                        Ok(client) => debug_log!("client connected: {:?}", client.id()),
                        Err(error) => eprintln!("gpui-webview: inserting client failed: {error}"),
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("the socket source is always insertable");

    // Globals exist before a client can connect (T1): `start` may return as
    // soon as the loop is armed.
    let _ = ready.send(Ok(()));
    if let Err(error) = event_loop.run(None, &mut data, |_| {}) {
        eprintln!("gpui-webview: compositor loop stopped: {error}");
    }
}

/// The output's mode and scale at the host window's `scale` factor. The mode
/// grows with the scale, so the logical output stays 1920x1080 at any density.
/// A fractional scale advertises its ceiling on `wl_output.scale` (smithay's
/// `Scale::Fractional`): GTK3 renders at integer scales only, and the app
/// samples the denser frame down into the block.
fn output_state(scale: f64) -> (Mode, Scale) {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let mode = Mode {
        size: Size::from((
            (1920.0 * scale).round() as i32,
            (1080.0 * scale).round() as i32,
        )),
        refresh: 60_000,
    };
    let scale = if scale.fract() == 0.0 {
        Scale::Integer(scale as i32)
    } else {
        Scale::Fractional(scale)
    };
    (mode, scale)
}

impl NestedState {
    /// Advertises the host window's scale factor, so clients render at its
    /// density instead of being upscaled by the app.
    pub(super) fn set_output_scale(&mut self, scale: f64) {
        let (mode, scale) = output_state(scale);
        if self.output.current_scale().fractional_scale() == scale.fractional_scale() {
            return;
        }
        debug_log!("output: scale {:?}, mode {:?}", scale, mode.size);
        // Preferred first, so the `wl_output.mode` event carries the flag.
        self.output.set_preferred(mode);
        self.output
            .change_current_state(Some(mode), None, Some(scale), None);
    }
}

/// Applies one command from the main thread (compositor thread).
fn apply_command(data: &mut CalloopData, command: Command) {
    match command {
        Command::Configure { id, width, height } => {
            // The toplevel is matched by its window title (`gpui-webview-{id}`),
            // not creation order; a mapped one re-layouts now, an unmapped one
            // picks the size up at configure.
            data.state.requested.insert(id, (width, height));
            if let Some(toplevel) = data.state.mapped.get(&id).cloned() {
                toplevel.with_pending_state(|pending| {
                    pending.size = Some(Size::from((width as i32, height as i32)));
                });
                let _serial = toplevel.send_configure();
                debug_log!("configure: block {id} resized to {width}x{height}");
            }
        }
        Command::Destroy { id } => data.state.forget_toplevel(id),
        Command::SetFocus { id, focused } => {
            let serial = SERIAL_COUNTER.next_serial();
            let keyboard = data.state.keyboard.clone();
            if focused {
                if let Some(surface) = data
                    .state
                    .mapped
                    .get(&id)
                    .map(|surface| surface.wl_surface().clone())
                {
                    keyboard.set_focus(&mut data.state, Some(surface), serial);
                }
            } else {
                // Check the protocol focus: the main thread already cleared the shared
                // flag optimistically, so it cannot say whether to send `wl_keyboard.leave`.
                let protocol_focus = data
                    .state
                    .mapped
                    .get(&id)
                    .map(|surface| surface.wl_surface().clone())
                    .filter(|surface| {
                        data.state.keyboard.current_focus().as_ref() == Some(surface)
                    });
                if protocol_focus.is_some() {
                    keyboard.set_focus(&mut data.state, None, serial);
                    // Focus left the block: an open menu cannot outlive it.
                    data.state.dismiss_popups(id);
                }
            }
            let mut target = lock(&data.state.shared.focus);
            if focused {
                *target = Some(id);
            } else if *target == Some(id) {
                *target = None;
            }
        }
        Command::Input { id, event } => apply_input(&mut data.state, id, event),
        // The app painted the surface: release its parked frame callbacks.
        Command::Presented { id } => data.state.release_frame_callbacks(id),
        #[cfg(dmabuf)]
        Command::DmabufFeedback(capabilities) => {
            let handle = data.display.handle();
            data.state.enable_dmabuf(&handle, capabilities);
        }
        Command::KeyboardLayout(name) => data.state.set_host_layout(&name),
        Command::OutputScale(scale) => data.state.set_output_scale(scale),
        // Handled by the channel callback, which stops the loop.
        Command::Shutdown => {}
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
        // A popup commit: same buffer handling, a different sink.
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

        // The content rect (`set_window_geometry`): shadows are not content, popups use it.
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
                let size = frame.size;
                let transport = if frame.image().is_some() {
                    "shm"
                } else {
                    "dmabuf"
                };
                let first = {
                    let mut frames = lock(&self.shared.frames);
                    let first = !frames.contains_key(&id);
                    frames.insert(id, frame);
                    first
                };
                if first {
                    debug_log!(
                        "commit: block {id} first frame {}x{} over {transport}",
                        size.0,
                        size.1
                    );
                }
                debug_log2!("commit: block {id} {}x{} over {transport}", size.0, size.1);
                if let Some(buffer) = hold {
                    // T14: hold the buffer one commit, or WebKit rewrites pixels the app may still import.
                    self.held_buffers.entry(id).or_default().hold(buffer);
                }
                self.wake();
            }
            CommittedBuffer::Removed => {
                lock(&self.shared.frames).remove(&id);
                self.release_held_buffer(id);
                self.wake();
            }
            CommittedBuffer::Unchanged | CommittedBuffer::Rejected => {}
        }

        // Present pacing: the callbacks this commit brought wait until the app
        // reports the surface painted (an off-screen page stops rendering).
        self.queue_frame_callbacks(id, surface);
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        // One role per surface: popups are forgotten, toplevels get the full teardown.
        match surface_role(surface) {
            Some(SurfaceRole::Popup { id, .. }) => self.forget_popup(id),
            Some(SurfaceRole::Toplevel(id)) => self.forget_toplevel(id),
            None => {}
        }
    }
}

impl SelectionHandler for NestedState {
    // T2: the global exists so GDK binds the seat; selection is not bridged yet.
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
        // T11: `get_toplevel` precedes `set_title`; park until the next dispatch.
        self.pending_toplevels.push(surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        // A `<select>` dropdown, context menu or date picker: painted above the page.
        let Some(parent) = popup_parent(&surface) else {
            // Not one of ours: refuse rather than paint at a meaningless position.
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
        // T3: without `wl_surface.enter` the popup never draws either.
        self.output.enter(surface.wl_surface());
        // T8: drop the pointer focus; the next event re-enters what is under the pointer.
        self.refresh_pointer_focus();
        set_surface_role(surface.wl_surface(), SurfaceRole::Popup { id, block });
        self.popups.push(Popup {
            id,
            parent: block,
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
        // A menu re-anchors on scroll; `send_configure` refuses non-reactive positioners.
        let Some(parent) = popup_parent(&surface) else {
            return;
        };
        let Some((_block, origin, geometry)) = popup_geometry(self, &parent, &positioner) else {
            return;
        };
        surface.with_pending_state(|state| state.geometry = geometry);
        if let Err(error) = surface.send_configure() {
            debug_log!("popup reposition refused: {error:?}");
            return;
        }
        let position = (origin.0 + geometry.loc.x, origin.1 + geometry.loc.y);
        // T40: submenus resolve through the popup's *current* position.
        if let Some(popup) = self
            .popups
            .iter_mut()
            .find(|popup| popup.surface.wl_surface() == surface.wl_surface())
        {
            popup.position = position;
            popup.size = (geometry.size.w, geometry.size.h);
        }
    }

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: Serial,
    ) {
        // A grab says "dismiss me on an outside click"; the press in `apply_input` does it.
    }
}

delegate_compositor!(NestedState);
delegate_shm!(NestedState);
#[cfg(dmabuf)]
delegate_dmabuf!(NestedState);
delegate_data_device!(NestedState);
delegate_xdg_shell!(NestedState);
delegate_output!(NestedState);
delegate_seat!(NestedState);
