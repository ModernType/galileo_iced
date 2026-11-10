use std::sync::atomic::Ordering;

use galileo::galileo_types::cartesian::Size as MapSize;
use galileo::render::WgpuRenderer;
use iced_core::Rectangle;
use iced_graphics::Viewport;
use iced_wgpu::primitive::{Pipeline, Primitive};
use iced_wgpu::wgpu;

use crate::state::{Inner, MapGpu, MapState};

/// A zero-sized texture is invalid
const MIN_SIZE: u32 = 1;

#[derive(Debug)]
pub(crate) struct MapPrimitive {
    pub state: MapState,
}

// Necessary to avoid overflow on auto-trait check
unsafe impl Send for MapPrimitive {}
unsafe impl Sync for MapPrimitive {}

impl Primitive for MapPrimitive {
    type Pipeline = BlitPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        let handle = &self.state.handle;
        let mut inner = handle.lock();

        let scale_factor = viewport.scale_factor();
        let logical = (bounds.width, bounds.height);
        let physical = MapSize::new(
            ((bounds.width * scale_factor).round() as u32).max(MIN_SIZE),
            ((bounds.height * scale_factor).round() as u32).max(MIN_SIZE),
        );

        if inner.gpu.is_none() {
            // Reuse iced's device and queue: one GPU context for the whole app.
            let renderer =
                WgpuRenderer::new_with_device_and_texture(device.clone(), queue.clone(), physical);
            let texture_view = renderer
                .get_target_texture_view()
                .expect("galileo renderer was created with a texture target");
            let bind_group = pipeline.bind_group(device, &texture_view);

            inner.gpu = Some(MapGpu {
                renderer,
                texture_view,
                bind_group,
                physical,
            });
        }

        let Inner {
            map,
            gpu,
            logical: last_logical,
            scale_factor: last_scale_factor,
            sized,
            ..
        } = &mut *inner;

        let gpu = gpu.as_mut().expect("GPU state was just initialized");

        if gpu.physical != physical {
            // `resize` recreates the target texture, so the view and the bind
            // group pointing at it are both stale afterwards.
            gpu.renderer.resize(physical);
            gpu.texture_view = gpu
                .renderer
                .get_target_texture_view()
                .expect("galileo renderer has a texture target");
            gpu.bind_group = pipeline.bind_group(device, &gpu.texture_view);
            gpu.physical = physical;
        }

        if *last_logical != logical || *last_scale_factor != scale_factor || !*sized {
            // The map works in logical pixels; the renderer in physical ones.
            map.set_size(MapSize::new(logical.0 as f64, logical.1 as f64));
            let view = map.view().with_dpi_scale_factor(scale_factor);
            map.set_view(view);

            *last_logical = logical;
            *last_scale_factor = scale_factor;
            *sized = true;
        }

        map.animate();

        if *sized {
            handle.in_runtime(|| map.load_layers());
        }

        if handle.needs_redraw.swap(false, Ordering::Relaxed) {
            log::trace!("rendering map at {physical:?}");

            gpu.renderer.trim_textures();
            gpu.renderer.render_to_texture_view(map, &gpu.texture_view);
        }
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let inner = self.state.handle.lock();
        let Some(gpu) = inner.gpu.as_ref() else {
            return false;
        };

        render_pass.set_pipeline(&pipeline.pipeline);
        render_pass.set_bind_group(0, &gpu.bind_group, &[]);
        render_pass.draw(0..3, 0..1);

        true
    }
}

#[derive(Debug)]
pub(crate) struct BlitPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

// Necessary to avoid overflow on auto-trait check
unsafe impl Send for BlitPipeline {}
unsafe impl Sync for BlitPipeline {}

impl BlitPipeline {
    pub fn bind_group(
        &self,
        device: &wgpu::Device,
        texture_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("galileo_iced.map.bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }
}

impl Pipeline for BlitPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("galileo_iced.blit.shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("blit.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("galileo_iced.blit.bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("galileo_iced.blit.pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        // Galileo writes display-encoded colours into a linear texture, so the
        // decode has to happen here when iced's target is an sRGB format.
        let fragment_entry = if format.is_srgb() {
            "fs_srgb"
        } else {
            "fs_passthrough"
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("galileo_iced.blit.pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(fragment_entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("galileo_iced.blit.sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
        }
    }
}
