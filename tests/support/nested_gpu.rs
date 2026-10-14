//! The headless GPU harness behind `E2E_DMABUF=1`: a windowless `wgpu` device,
//! the same dma-buf import the app performs, and a readback pass so the test
//! can assert the imported pixels. Included by `nested_e2e.rs`.

use gpui_kit::DmaBufDescriptor;
use gpui_webview::DmaBufFrame;

/// A `wgpu` device with no window and no display. `None` when no adapter can be
/// opened.
pub struct HeadlessGpu {
    pub device: gpui_wgpu::wgpu::Device,
    pub queue: gpui_wgpu::wgpu::Queue,
    pub adapter: gpui_wgpu::wgpu::Adapter,
}

pub fn headless_gpu() -> Option<HeadlessGpu> {
    use gpui_wgpu::wgpu;
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    /// A minimal executor: the test is single-threaded and the adapter/device
    /// futures complete on the first or second poll.
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(future);
        loop {
            match Future::poll(future.as_mut(), &mut cx) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::park(),
            }
        }
    }

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapters = block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    let adapter = adapters.into_iter().next()?;
    let (device, queue) =
        block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
    Some(HeadlessGpu {
        device,
        queue,
        adapter,
    })
}

/// Imports `frame` as a texture and samples it back; the same import the app
/// performs.
pub fn import_and_sample(gpu: &HeadlessGpu, frame: &DmaBufFrame) -> anyhow::Result<Vec<u8>> {
    let descriptor = DmaBufDescriptor {
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
    let external = gpui_wgpu::dmabuf::import(&gpu.device, &descriptor)?;
    sample_external(gpu, &external, frame.width, frame.height)
}

/// Samples an [`gpui_kit::ExternalTexture`] through the readback pass; shared
/// by the dma-buf import and the shm self-check.
pub fn sample_external(
    gpu: &HeadlessGpu,
    external: &gpui_kit::ExternalTexture,
    width: u32,
    height: u32,
) -> anyhow::Result<Vec<u8>> {
    let surface = external
        .downcast::<gpui_wgpu::SurfaceTexture>()
        .ok_or_else(|| anyhow::anyhow!("the texture did not wrap a surface texture"))?;
    readback_texture(gpu, &surface.texture, width, height)
}

fn readback_texture(
    gpu: &HeadlessGpu,
    texture: &gpui_wgpu::wgpu::Texture,
    width: u32,
    height: u32,
) -> anyhow::Result<Vec<u8>> {
    use gpui_wgpu::wgpu;

    // T20: two explicit triangles (0,0),(1,0),(0,1) and (1,0),(1,1),(0,1); a
    // strip's second triangle came out missing on ANV/Mesa 26.
    const SHADER: &str = r#"
        @group(0) @binding(0) var t_src: texture_2d<f32>;
        @group(0) @binding(1) var s_src: sampler;
        struct Varying {
            @builtin(position) position: vec4<f32>,
            @location(0) uv: vec2<f32>,
        }
        @vertex
        fn vs_main(@builtin(vertex_index) index: u32) -> Varying {
            var uv: vec2<f32>;
            if (index == 0u) { uv = vec2<f32>(0.0, 0.0); }
            else if (index == 1u) { uv = vec2<f32>(1.0, 0.0); }
            else if (index == 2u) { uv = vec2<f32>(0.0, 1.0); }
            else if (index == 3u) { uv = vec2<f32>(1.0, 0.0); }
            else if (index == 4u) { uv = vec2<f32>(1.0, 1.0); }
            else { uv = vec2<f32>(0.0, 1.0); }
            var out = Varying();
            out.position = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
            out.uv = uv;
            return out;
        }
        @fragment
        fn fs_main(in: Varying) -> @location(0) vec4<f32> {
            return textureSampleLevel(t_src, s_src, in.uv, 0.0);
        }
    "#;

    let module = gpu
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("dmabuf-readback"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
    // The bind group layout comes from the shader (auto layout).
    let pipeline = gpu
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("dmabuf-readback"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
    let layout = pipeline.get_bind_group_layout(0);
    let source_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("dmabuf-readback"),
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });
    let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("dmabuf-readback"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&source_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("dmabuf-readback-target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
    // `copy_texture_to_buffer` needs 256-byte aligned rows.
    let unpadded_row = width * 4;
    let padded_row = unpadded_row.div_ceil(256) * 256;
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("dmabuf-readback-buffer"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("dmabuf-readback"),
        });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("dmabuf-readback"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..6, 0..1);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit(std::iter::once(encoder.finish()));

    let (sender, receiver) = std::sync::mpsc::channel();
    readback
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
    let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    receiver
        .recv()
        .map_err(|_| anyhow::anyhow!("the readback callback never ran"))?
        .map_err(|error| anyhow::anyhow!("mapping the readback buffer: {error}"))?;

    let mapped = readback.slice(..).get_mapped_range();
    let mut pixels = Vec::with_capacity((unpadded_row * height) as usize);
    for row in 0..height {
        let start = (row * padded_row) as usize;
        pixels.extend_from_slice(&mapped[start..start + unpadded_row as usize]);
    }
    drop(mapped);
    readback.unmap();
    Ok(pixels)
}
