//! Frame transport: `wl_shm` copies, dma-buf descriptors, held buffers and
//! the per-buffer identity the app's texture cache keys on.

use super::*;
use crate::debug::debug_log;
use super::compositor::NestedState;

/// A client buffer held back one commit so the app can finish importing the
/// previous frame: WebKit would otherwise rewrite the pixels under the app's
/// feet (see `commit`). Dropping or replacing it releases the buffer at the
/// right moment. The page keeps one per surface (`held_buffers`), a popup one
/// per popup.
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

    /// Takes the held buffer for the caller to release (the popup teardown
    /// releases it through `release_popup_frame`).
    pub(super) fn take(&mut self) -> Option<WlBuffer> {
        self.0.take()
    }
}

/// The content rectangle of a commit, clamped to the buffer: the client's
/// window geometry (the page content, without decoration shadows), or the
/// whole buffer when there is none or it is degenerate. `None` when nothing is
/// left to draw. Shared by the shm and dma-buf paths so they cannot drift.
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
    /// The app-facing identity of a committed buffer: a generation assigned on
    /// first sight and never reused. See the `buffer_generations` field.
    #[cfg(all(target_os = "linux", feature = "dma-buf"))]
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


    /// Enables `linux-dmabuf` with the renderer's exact importable set: the
    /// device is the render node the imported buffers must come from, and the
    /// formats are `(fourcc, modifier)` pairs the renderer's Vulkan driver
    /// verified. Returns whether the global was created.
    #[cfg(all(target_os = "linux", feature = "dma-buf"))]
    pub(super) fn enable_dmabuf(
        &mut self,
        display: &smithay::reexports::wayland_server::DisplayHandle,
        capabilities: gpui_kit::DmaBufCapabilities,
    ) -> bool {
        if self.dmabuf_global.is_some() {
            // `attach` runs after `new_with_dmabuf` in tests; a second global
            // would advertise the same formats twice.
            return false;
        }
        // `GPUI_WEBVIEW_DMABUF_LINEAR=1` advertises only linear buffers. It is
        // the diagnostic/failsafe path: a linear buffer's layout is trivial to
        // import, so a machine whose tiled import misbehaves can still run the
        // GPU path.
        let linear_only = std::env::var_os("GPUI_WEBVIEW_DMABUF_LINEAR").is_some();
        let advertised: Vec<(u32, u64)> = capabilities
            .formats
            .iter()
            .filter(|(_, modifier)| !linear_only || *modifier == 0)
            // XR24's alpha byte is undefined (WebKit writes 0) and the
            // renderer samples it with premultiplied blending, which would
            // make an opaque page transparent. Advertise AR24 (and the rest)
            // only; WebKitGTK picks AR24 without complaint. The shm path
            // forces the alpha byte itself (`copy_pixels`), the dma-buf path
            // cannot cheaply, so it is filtered out here.
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


    /// Whether the feedback advertised this `(fourcc, modifier)` pair (which
    /// means the renderer can import it).
    #[cfg(all(target_os = "linux", feature = "dma-buf"))]
    pub(super) fn dmabuf_format_supported(&self, fourcc: u32, modifier: u64) -> bool {
        self.dmabuf_formats
            .iter()
            .any(|(code, value)| *code == fourcc && *value == modifier)
    }


    /// Releases the dma-buf buffer held back for `id`, if any. Called when the
    /// surface goes away; the steady-state release happens on the next commit.
    pub(super) fn release_held_buffer(&mut self, id: SurfaceId) {
        if let Some(mut held) = self.held_buffers.remove(&id) {
            held.release();
        }
    }


    /// Releases a surface's pending frame callbacks. The client loops on
    /// these, but not at *our* paint rate: it is self-paced and may commit
    /// while the app is idle. The callback is released as soon as the commit's
    /// buffer is consumed so the client can reuse or redraw it.
    pub(super) fn fire_frame_callbacks(&mut self, surface: &WlSurface) {
        let callbacks: Vec<smithay::reexports::wayland_server::protocol::wl_callback::WlCallback> =
            smithay::wayland::compositor::with_states(surface, |states| {
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .frame_callbacks
                    .drain(..)
                    .collect()
            });
        let time = self.now_ms();
        if !callbacks.is_empty() {
            debug_log!("commit: fired {} frame callbacks", callbacks.len());
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


#[cfg(all(target_os = "linux", feature = "dma-buf"))]
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
        // Accept exactly what the feedback advertised: the renderer proved it
        // can import those pairs, and a rejected buffer makes WebKitGTK fall
        // back instead of committing pixels nobody can sample.
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
        } else {
            debug_log!("dmabuf: {:?} modifier {modifier:#x} accepted", format.code);
        }
    }
}


/// The `wl_buffer` lifecycle hook. With the `dma-buf` feature it drops the
/// buffer's generation, which is what makes a *reused* protocol id get a fresh
/// import-cache key (see `buffer_key`); shm buffers only need the empty hook.
impl BufferHandler for NestedState {
    fn buffer_destroyed(&mut self, buffer: &WlBuffer) {
        #[cfg(all(target_os = "linux", feature = "dma-buf"))]
        {
            use smithay::reexports::wayland_server::Resource as _;
            self.buffer_generations
                .remove(&(buffer.id().protocol_id() as u64));
        }
        #[cfg(not(all(target_os = "linux", feature = "dma-buf")))]
        let _ = buffer;
    }
}


/// What one commit's buffer means for the compositor.
pub(super) enum CommittedBuffer {
    /// A usable new frame. `hold` is the client buffer to keep back one commit
    /// (dma-buf only — shm pixels are copied, so that buffer is released at
    /// once).
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


/// The content rect a surface commits (`xdg_surface.set_window_geometry`).
/// `None` for surfaces that never set one (popups, and toplevels before their
/// first geometry commit).
pub(super) fn content_geometry(surface: &WlSurface) -> Option<Rectangle<i32, Logical>> {
    smithay::wayland::compositor::with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
    })
}


/// Takes the committed buffer out of `surface`'s state and turns it into a
/// frame: a dma-buf descriptor with the buffer held, or copied pixels with the
/// buffer released. `sequence` identifies the commit for repaint detection.
impl NestedState {
    pub(super) fn take_committed(
        &mut self,
        surface: &WlSurface,
        geometry: Option<Rectangle<i32, Logical>>,
        sequence: u64,
    ) -> CommittedBuffer {
        #[cfg(not(all(target_os = "linux", feature = "dma-buf")))]
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
                #[cfg(all(target_os = "linux", feature = "dma-buf"))]
                if let Some(dmabuf) = get_dmabuf(&buffer).ok().cloned() {
                    // The app keys its import cache on this generation, not
                    // the recycled protocol id.
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
                // Once the pixels are copied the buffer is free for the client
                // to reuse, which is what keeps WebKit's shm pool flowing (we
                // never sample it again).
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


/// Turns a committed dma-buf into a frame the app imports. The plane fds are
/// duplicated so the compositor can release the client buffer afterwards, and
/// the pixels are cropped to the client's window geometry exactly like the shm
/// path. Only single-plane buffers are supported (which is all the feedback
/// advertises). `key` is the buffer's never-reused generation (see
/// `NestedState::buffer_key`).
#[cfg(all(target_os = "linux", feature = "dma-buf"))]
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
    // The crop below turns `(x0, y0)` into a byte offset, which is only valid
    // for a linear layout. A tiled modifier's plane describes whole tiles; a
    // nonzero geometry origin would need a blit, not an offset. The geometry
    // is `(0, 0)` in practice (`set_decorated(false)`), so this is a guard
    // that degrades to "keep the last frame" instead of sampling garbage.
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
    // BGRA/XRGB byte order: four bytes per pixel, one row per stride. The
    // plane's own offset is kept: it is in *bytes* into the fd and may be
    // nonzero for a sub-allocated buffer (the app's importer applies it to
    // the Vulkan plane layout).
    let cropped_offset = offset
        .checked_add(y0.checked_mul(stride)?)?
        .checked_add(x0.checked_mul(4)?)?;

    // `try_clone_to_owned` duplicates the descriptor and sets close-on-exec,
    // so no `fcntl`/`from_raw_fd` pair is needed.
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
/// client's window geometry (the content, without decoration shadows).
/// `ARGB8888`/`XRGB8888` bytes are BGRA in little-endian memory, which is
/// exactly what `RenderImage` expects, so the rows copy through unchanged.
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


/// Copies the content rectangle of one buffer into a `RenderImage`. Returns
/// `None` for a malformed size/stride or a truncated memory map.
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

    // The content rect, clamped to the buffer. Without a window geometry the
    // whole buffer is the content.
    let (x0, y0, width, height) = content_rect(geometry, buffer_width as u32, buffer_height as u32)?;
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
    // XRGB8888 leaves the alpha byte undefined (zero in practice); GPUI blends
    // with it, so an opaque buffer must become opaque here.
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
