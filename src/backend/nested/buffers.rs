//! Frame transport: `wl_shm` copies, dma-buf descriptors, held buffers, per-buffer identity.

#[cfg(dmabuf)]
use crate::spec::DmaBufFrame;
#[cfg(dmabuf)]
use crate::spec::DmaBufPlane;
#[cfg(dmabuf)]
use crate::spec::SurfaceContent;
use crate::spec::SurfaceFrame;
use crate::spec::SurfaceId;
#[cfg(dmabuf)]
use smithay::backend::allocator::Format;
#[cfg(dmabuf)]
use smithay::backend::allocator::Fourcc;
#[cfg(dmabuf)]
use smithay::backend::allocator::Modifier;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Logical;
use smithay::utils::Rectangle;
use smithay::wayland::compositor::BufferAssignment;
use smithay::wayland::compositor::SurfaceAttributes;
#[cfg(dmabuf)]
use smithay::wayland::dmabuf::DmabufFeedbackBuilder;
#[cfg(dmabuf)]
use smithay::wayland::dmabuf::DmabufGlobal;
#[cfg(dmabuf)]
use smithay::wayland::dmabuf::DmabufState;
#[cfg(dmabuf)]
use smithay::wayland::dmabuf::ImportNotifier;
use smithay::wayland::shell::xdg::SurfaceCachedState;
use smithay::wayland::shm::{self, ShmHandler, ShmState};
use std::sync::Arc;

use super::compositor::NestedState;
#[cfg(dmabuf)]
use crate::debug::debug_log;
use crate::debug::debug_log2;
#[cfg(dmabuf)]
use smithay::backend::allocator::Buffer as _;
use smithay::wayland::buffer::BufferHandler;
#[cfg(dmabuf)]
use smithay::wayland::dmabuf::DmabufHandler;
#[cfg(dmabuf)]
use smithay::wayland::dmabuf::get_dmabuf;

/// T14: the client buffer held back one commit while the app imports the previous frame.
#[derive(Default)]
pub(super) struct HeldBuffer(Option<WlBuffer>);

impl HeldBuffer {
    /// Holds `buffer`, releasing the one it replaces.
    pub(super) fn hold(&mut self, buffer: WlBuffer) {
        if let Some(previous) = self.0.replace(buffer) {
            previous.release();
        }
    }

    /// Releases the held buffer, if any.
    pub(super) fn release(&mut self) {
        if let Some(buffer) = self.0.take() {
            buffer.release();
        }
    }

    /// Takes the held buffer for the caller to release.
    pub(super) fn take(&mut self) -> Option<WlBuffer> {
        self.0.take()
    }
}

/// The content rect of a commit clamped to the buffer: the client's window
/// geometry when sane, the whole buffer otherwise. Shared by both transports.
pub(super) fn content_rect(
    geometry: Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>>,
    buffer_width: u32,
    buffer_height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let (x0, y0, width, height) = match geometry {
        Some(rect) if rect.size.w > 0 && rect.size.h > 0 => {
            let x0 = rect.loc.x.clamp(0, buffer_width as i32) as u32;
            let y0 = rect.loc.y.clamp(0, buffer_height as i32) as u32;
            (
                x0,
                y0,
                (rect.size.w as u32).min(buffer_width - x0),
                (rect.size.h as u32).min(buffer_height - y0),
            )
        }
        _ => (0, 0, buffer_width, buffer_height),
    };
    (width != 0 && height != 0).then_some((x0, y0, width, height))
}

impl NestedState {
    /// T10: the app-facing identity of a committed buffer, a never-reused generation.
    #[cfg(dmabuf)]
    pub(super) fn buffer_key(&mut self, buffer: &WlBuffer) -> u64 {
        use smithay::reexports::wayland_server::Resource as _;
        let id = buffer.id().protocol_id() as u64;
        if let Some(key) = self.buffer_generations.get(&id) {
            return *key;
        }
        self.next_buffer_generation += 1;
        let key = self.next_buffer_generation;
        self.buffer_generations.insert(id, key);
        key
    }

    /// Enables `linux-dmabuf` with the renderer's importable set; returns whether the global was created.
    #[cfg(dmabuf)]
    pub(super) fn enable_dmabuf(
        &mut self,
        display: &smithay::reexports::wayland_server::DisplayHandle,
        capabilities: gpui_kit::DmaBufCapabilities,
    ) -> bool {
        if self.dmabuf_global.is_some() {
            // `attach` runs after `new_with_dmabuf` in tests; a second global would duplicate it.
            return false;
        }
        // `GPUI_WEBVIEW_DMABUF_LINEAR=1`: linear buffers only (diagnostic path).
        let linear_only = std::env::var_os("GPUI_WEBVIEW_DMABUF_LINEAR").is_some();
        let advertised: Vec<(u32, u64)> = capabilities
            .formats
            .iter()
            .filter(|(_, modifier)| !linear_only || *modifier == 0)
            // T5: XR24's alpha is undefined and sampled premultiplied; filter it (shm forces 0xff).
            .filter(|(fourcc, _)| *fourcc != Fourcc::Xrgb8888 as u32)
            .copied()
            .collect();
        if advertised.is_empty() {
            debug_log!("dmabuf: no formats left after filtering");
            return false;
        }
        // SAFETY: `makedev` is pure arithmetic on the two integers.
        #[allow(unused_unsafe)]
        let device = unsafe { libc::makedev(capabilities.device.0, capabilities.device.1) };
        let formats: Vec<Format> = advertised
            .iter()
            .filter_map(|(fourcc, modifier)| {
                let code = Fourcc::try_from(*fourcc).ok()?;
                Some(Format {
                    code,
                    modifier: Modifier::from(*modifier),
                })
            })
            .collect();
        if formats.is_empty() {
            debug_log!("dmabuf: capabilities contained no known fourccs");
            return false;
        }
        let feedback = match DmabufFeedbackBuilder::new(device, formats.clone()).build() {
            Ok(feedback) => feedback,
            Err(error) => {
                eprintln!("gpui-webview: linux-dmabuf feedback: {error}");
                return false;
            }
        };
        self.dmabuf_formats = advertised;
        self.dmabuf_global = Some(
            self.dmabuf_state
                .create_global_with_default_feedback::<NestedState>(display, &feedback),
        );
        debug_log!(
            "dmabuf: global created for {} format(s), device {device}",
            formats.len()
        );
        true
    }

    /// Whether the feedback advertised this `(fourcc, modifier)` pair.
    #[cfg(dmabuf)]
    pub(super) fn dmabuf_format_supported(&self, fourcc: u32, modifier: u64) -> bool {
        self.dmabuf_formats
            .iter()
            .any(|(code, value)| *code == fourcc && *value == modifier)
    }

    /// Releases the dma-buf held back for `id`, if any (called when the surface goes away).
    pub(super) fn release_held_buffer(&mut self, id: SurfaceId) {
        if let Some(mut held) = self.held_buffers.remove(&id) {
            held.release();
        }
    }

    /// Answers the frame callbacks a commit brought, now (popups; the toplevel
    /// path parks them via [`Self::queue_frame_callbacks`] until present).
    pub(super) fn fire_frame_callbacks(&mut self, surface: &WlSurface) {
        let callbacks = self.take_frame_callbacks(surface);
        self.finish_frame_callbacks(callbacks);
    }

    /// Parks the callbacks a commit brought until the app reports the surface
    /// painted (present pacing): the client's next frame waits for the next
    /// paint, so the page animates at the app's rate.
    pub(super) fn queue_frame_callbacks(&mut self, id: SurfaceId, surface: &WlSurface) {
        let callbacks = self.take_frame_callbacks(surface);
        if !callbacks.is_empty() {
            self.pending_frame_callbacks
                .entry(id)
                .or_default()
                .extend(callbacks);
        }
    }

    /// Releases the callbacks parked for `id`; `WebViews::presented` got here.
    pub(super) fn release_frame_callbacks(&mut self, id: SurfaceId) {
        let Some(callbacks) = self.pending_frame_callbacks.remove(&id) else {
            return;
        };
        self.finish_frame_callbacks(callbacks);
    }

    /// Drains the surface's pending frame callbacks (they arrive with a commit).
    fn take_frame_callbacks(&mut self, surface: &WlSurface) -> Vec<WlCallback> {
        smithay::wayland::compositor::with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
                .collect()
        })
    }

    /// Times and answers a batch of frame callbacks.
    fn finish_frame_callbacks(&mut self, callbacks: Vec<WlCallback>) {
        let time = self.now_ms();
        if !callbacks.is_empty() {
            debug_log2!("commit: fired {} frame callback(s)", callbacks.len());
        }
        for callback in callbacks {
            callback.done(time);
        }
    }
}

impl ShmHandler for NestedState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

#[cfg(dmabuf)]
impl DmabufHandler for NestedState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Accept exactly what the feedback advertised (T17); anything else makes WebKit fall back.
        let format = dmabuf.format();
        let fourcc = format.code as u32;
        let modifier = u64::from(format.modifier);
        if dmabuf.num_planes() != 1 || !self.dmabuf_format_supported(fourcc, modifier) {
            debug_log!(
                "dmabuf: rejected {:?} modifier {modifier:#x} ({} plane(s))",
                format.code,
                dmabuf.num_planes()
            );
            notifier.failed();
            return;
        }
        if notifier.successful::<NestedState>().is_err() {
            debug_log!("dmabuf: the client went away before the import reply");
        }
    }
}

/// Drops a destroyed buffer's generation so a reused id gets a fresh key (T10).
impl BufferHandler for NestedState {
    fn buffer_destroyed(&mut self, buffer: &WlBuffer) {
        #[cfg(dmabuf)]
        {
            use smithay::reexports::wayland_server::Resource as _;
            self.buffer_generations
                .remove(&(buffer.id().protocol_id() as u64));
        }
        #[cfg(not(dmabuf))]
        let _ = buffer;
    }
}

/// What one commit's buffer means for the compositor.
pub(super) enum CommittedBuffer {
    /// A usable new frame; `hold` is the buffer to keep back one commit (dma-buf only).
    New {
        frame: SurfaceFrame,
        hold: Option<WlBuffer>,
    },
    /// The client detached its buffer.
    Removed,
    /// A state-only commit.
    Unchanged,
    /// A buffer we cannot sample; the previous frame stays.
    Rejected,
}

/// The content rect a surface commits (`set_window_geometry`); `None` before its first one.
pub(super) fn content_geometry(surface: &WlSurface) -> Option<Rectangle<i32, Logical>> {
    smithay::wayland::compositor::with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
    })
}

/// Turns the committed buffer into a frame: dma-buf with the buffer held, or copied pixels with it released.
impl NestedState {
    pub(super) fn take_committed(
        &mut self,
        surface: &WlSurface,
        geometry: Option<Rectangle<i32, Logical>>,
        sequence: u64,
    ) -> CommittedBuffer {
        #[cfg(not(dmabuf))]
        let _ = sequence;
        let assignment = smithay::wayland::compositor::with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .buffer
                .take()
        });
        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                #[cfg(dmabuf)]
                if let Some(dmabuf) = get_dmabuf(&buffer).ok().cloned() {
                    // T10: the import cache keys on this generation, not the recycled protocol id.
                    let key = self.buffer_key(&buffer);
                    return match dmabuf_frame(key, &dmabuf, geometry, sequence) {
                        Some(frame) => CommittedBuffer::New {
                            frame,
                            hold: Some(buffer),
                        },
                        None => {
                            buffer.release();
                            CommittedBuffer::Rejected
                        }
                    };
                }
                // Copied: the buffer is free for the client to reuse.
                let frame = copy_shm_buffer(&buffer, geometry);
                buffer.release();
                match frame {
                    Some(frame) => CommittedBuffer::New { frame, hold: None },
                    None => CommittedBuffer::Rejected,
                }
            }
            Some(BufferAssignment::Removed) => CommittedBuffer::Removed,
            None => CommittedBuffer::Unchanged,
        }
    }
}

/// Turns a committed dma-buf into a frame: plane fds duplicated, pixels cropped
/// to the window geometry like the shm path. `key` is the buffer's generation.
#[cfg(dmabuf)]
pub(super) fn dmabuf_frame(
    key: u64,
    dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf,
    geometry: Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>>,
    sequence: u64,
) -> Option<SurfaceFrame> {
    use std::os::fd::AsFd;

    if dmabuf.num_planes() != 1 {
        return None;
    }
    let (buffer_width, buffer_height) = (dmabuf.width().max(1), dmabuf.height().max(1));
    // The content rect, clamped to the buffer; same rule as `copy_pixels`.
    let (x0, y0, width, height) = content_rect(geometry, buffer_width, buffer_height)?;

    let format = dmabuf.format();
    let fd = dmabuf.handles().next()?;
    let offset = dmabuf.offsets().next()?;
    let stride = dmabuf.strides().next()?;
    // Tiled planes describe whole tiles, so a nonzero crop origin would sample garbage.
    if format.modifier != Modifier::Linear && (x0 != 0 || y0 != 0) {
        static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if WARNED.set(()).is_ok() {
            eprintln!(
                "gpui-webview: refusing to crop a tiled dma-buf at ({x0},{y0}); \
                 keeping the last frame"
            );
        }
        return None;
    }
    // BGRA byte order, four bytes per pixel, one row per stride. The plane's own
    // byte offset stays: a sub-allocated buffer can have a nonzero one.
    let cropped_offset = offset
        .checked_add(y0.checked_mul(stride)?)?
        .checked_add(x0.checked_mul(4)?)?;

    // `try_clone_to_owned` duplicates the descriptor close-on-exec.
    let fd = fd.as_fd().try_clone_to_owned().ok()?;

    Some(SurfaceFrame {
        content: SurfaceContent::Dmabuf(DmaBufFrame {
            buffer_id: key,
            sequence,
            width,
            height,
            fourcc: format.code as u32,
            modifier: u64::from(format.modifier),
            planes: vec![DmaBufPlane {
                fd: Arc::new(fd),
                offset: cropped_offset,
                stride,
            }],
        }),
        size: (width, height),
    })
}

/// Copies a committed `wl_shm` buffer into a GPUI frame image, cropped to the
/// window geometry; little-endian ARGB/XRGB bytes are already BGRA for `RenderImage`.
pub(super) fn copy_shm_buffer(
    buffer: &WlBuffer,
    geometry: Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>>,
) -> Option<SurfaceFrame> {
    let mut out: Option<SurfaceFrame> = None;
    let _ = shm::with_buffer_contents(buffer, |pointer, length, data| {
        out = copy_pixels(pointer, length, data, geometry);
    });
    out
}

/// Copies the content rect of a buffer into a `RenderImage`; `None` on malformed data.
pub(super) fn copy_pixels(
    pointer: *const u8,
    length: usize,
    data: smithay::wayland::shm::BufferData,
    geometry: Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>>,
) -> Option<SurfaceFrame> {
    let (buffer_width, buffer_height, stride) = (
        data.width.max(1) as usize,
        data.height.max(1) as usize,
        data.stride.max(0) as usize,
    );
    if stride < buffer_width * 4 {
        return None;
    }

    // Without a window geometry the whole buffer is the content.
    let (x0, y0, width, height) =
        content_rect(geometry, buffer_width as u32, buffer_height as u32)?;
    let (x0, y0, width, height) = (x0 as usize, y0 as usize, width as usize, height as usize);

    let mut pixels = Vec::with_capacity(width * height * 4);
    for row in y0..y0 + height {
        let start = data.offset.max(0) as usize + row * stride + x0 * 4;
        let end = start + width * 4;
        if end > length {
            return None;
        }
        // SAFETY: the caller (`with_buffer_contents`) guarantees `pointer` is
        // valid for `length` bytes while the callback runs, and the range was
        // checked above.
        let row_bytes = unsafe { std::slice::from_raw_parts(pointer.add(start), width * 4) };
        pixels.extend_from_slice(row_bytes);
    }
    // T5: XRGB8888's alpha is undefined; GPUI blends with it, so force opaque here.
    if data.format == wl_shm::Format::Xrgb8888 {
        for alpha in pixels.iter_mut().skip(3).step_by(4) {
            *alpha = 0xff;
        }
    }
    let image = image::RgbaImage::from_raw(width as u32, height as u32, pixels)?;
    Some(SurfaceFrame::from_image(
        Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(image)])),
        (width as u32, height as u32),
    ))
}
