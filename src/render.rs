//! Draws NV12 video frames to the window with wgpu (Metal on macOS, D3D12/Vulkan on Windows).

use std::sync::Arc;

use anyhow::{Context, Result};
use eframe::egui;
use eframe::egui_wgpu;
use winit::window::Window;

use crate::platform::Nv12Frame;

const SHADER: &str = r#"
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    var o: VsOut;
    o.pos = vec4(p[i], 0.0, 1.0);
    o.uv = vec2((p[i].x + 1.0) * 0.5, 1.0 - (p[i].y + 1.0) * 0.5);
    return o;
}

@group(0) @binding(0) var ty: texture_2d<f32>;
@group(0) @binding(1) var tuv: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

// BT.709 limited range YCbCr -> RGB.
@fragment fn fs(v: VsOut) -> @location(0) vec4<f32> {
    let y = (textureSample(ty, samp, v.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let c = (textureSample(tuv, samp, v.uv).rg - vec2(128.0 / 255.0)) * (255.0 / 224.0);
    let rgb = vec3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);
    return vec4(clamp(rgb, vec3(0.0), vec3(1.0)), 1.0);
}
"#;

/// Text drawn over the video: a toast at the top, a centered banner, a stats panel.
#[derive(Default)]
pub struct Overlay {
    pub toast: Option<Vec<String>>,
    pub banner: Option<(String, String)>,
    pub stats: Option<Vec<String>>,
}

impl Overlay {
    pub fn is_empty(&self) -> bool {
        self.toast.is_none() && self.banner.is_none() && self.stats.is_none()
    }
}

struct VideoTextures {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    uv: wgpu::Texture,
    bind: wgpu::BindGroup,
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    video: Option<VideoTextures>,
    #[allow(dead_code)]
    pub adapter_name: String,
    egui: egui::Context,
    egui_renderer: egui_wgpu::Renderer,
    started: std::time::Instant,
}

impl Renderer {
    pub fn new(window: Arc<Window>, vsync: bool) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window.clone()).context("create surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .context("no GPU adapter")?;
        let adapter_name = format!("{} ({:?})", adapter.get_info().name, adapter.get_info().backend);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("hop"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("create device")?;

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface not supported by adapter")?;
        // The video is already gamma-encoded, so draw into a non-sRGB target.
        if let Some(f) = caps.formats.iter().find(|f| !f.is_srgb()) {
            config.format = *f;
        }
        config.present_mode = pick_present_mode(&caps.present_modes, vsync);
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);
        log::info!("renderer: {adapter_name}, {:?}, {:?}", config.format, config.present_mode);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let egui_renderer = egui_wgpu::Renderer::new(&device, config.format, egui_wgpu::RendererOptions::default());
        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            layout,
            sampler,
            video: None,
            adapter_name,
            egui: egui::Context::default(),
            egui_renderer,
            started: std::time::Instant::now(),
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 && (w != self.config.width || h != self.config.height) {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
        }
    }

    pub fn upload(&mut self, f: &Nv12Frame) {
        if f.width == 0 || f.height == 0 {
            return;
        }
        let (cw, ch) = (f.width.div_ceil(2), f.height.div_ceil(2));
        if self.video.as_ref().is_none_or(|v| v.width != f.width || v.height != f.height) {
            let mk = |w, h, format| {
                self.device.create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let y = mk(f.width, f.height, wgpu::TextureFormat::R8Unorm);
            let uv = mk(cw, ch, wgpu::TextureFormat::Rg8Unorm);
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&y.create_view(&Default::default())),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&uv.create_view(&Default::default())),
                    },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            self.video = Some(VideoTextures { width: f.width, height: f.height, y, uv, bind });
        }
        let v = self.video.as_ref().unwrap();
        let write = |tex: &wgpu::Texture, data: &[u8], w: u32, h: u32, bpp: u32| {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(f.width), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            let _ = bpp;
        };
        write(&v.y, &f.y, f.width, f.height, 1);
        write(&v.uv, &f.uv, cw, ch, 2);
    }

    /// The rectangle (x, y, w, h) in window pixels where the video is drawn.
    pub fn video_rect(&self) -> (f32, f32, f32, f32) {
        let (ww, wh) = (self.config.width as f32, self.config.height as f32);
        let Some(v) = &self.video else { return (0.0, 0.0, ww, wh) };
        let s = (ww / v.width as f32).min(wh / v.height as f32);
        let (w, h) = (v.width as f32 * s, v.height as f32 * s);
        (((ww - w) / 2.0).floor(), ((wh - h) / 2.0).floor(), w.round(), h.round())
    }

    pub fn render(&mut self, overlay: &Overlay, scale: f32) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            if let Some(v) = &self.video {
                let (x, y, w, h) = self.video_rect();
                if w >= 1.0 && h >= 1.0 {
                    pass.set_viewport(x, y, w, h, 0.0, 1.0);
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &v.bind, &[]);
                    pass.draw(0..3, 0..1);
                }
            }
        }
        if !overlay.is_empty() {
            self.paint_overlay(&mut enc, &view, overlay, scale);
        }
        self.queue.submit([enc.finish()]);
        self.queue.present(frame);
    }

    fn paint_overlay(&mut self, enc: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, ov: &Overlay, scale: f32) {
        let (w, h) = (self.config.width, self.config.height);
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w as f32 / scale, h as f32 / scale))),
            time: Some(self.started.elapsed().as_secs_f64()),
            ..Default::default()
        };
        if let Some(v) = raw.viewports.get_mut(&egui::ViewportId::ROOT) {
            v.native_pixels_per_point = Some(scale);
        }
        let panel = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(185))
                .corner_radius(10)
                .inner_margin(egui::Margin::symmetric(16, 12))
                .show(ui, |ui| add(ui));
        };
        let out = self.egui.run_ui(raw, |ui| {
            let ctx = ui.ctx().clone();
            if let Some(lines) = &ov.stats {
                egui::Area::new(egui::Id::new("stats")).anchor(egui::Align2::LEFT_TOP, [12.0, 12.0]).interactable(false).show(
                    &ctx,
                    |ui| {
                        panel(ui, &mut |ui| {
                            for l in lines {
                                ui.label(egui::RichText::new(l).monospace().size(12.5).color(egui::Color32::WHITE));
                            }
                        })
                    },
                );
            }
            if let Some(lines) = &ov.toast {
                egui::Area::new(egui::Id::new("toast")).anchor(egui::Align2::CENTER_TOP, [0.0, 18.0]).interactable(false).show(
                    &ctx,
                    |ui| {
                        panel(ui, &mut |ui| {
                            for (i, l) in lines.iter().enumerate() {
                                let t = egui::RichText::new(l);
                                ui.label(if i == 0 {
                                    t.size(16.0).strong().color(egui::Color32::WHITE)
                                } else {
                                    t.size(13.0).color(egui::Color32::from_gray(215))
                                });
                            }
                        })
                    },
                );
            }
            if let Some((title, detail)) = &ov.banner {
                egui::Area::new(egui::Id::new("banner")).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).interactable(false).show(
                    &ctx,
                    |ui| {
                        panel(ui, &mut |ui| {
                            ui.vertical_centered(|ui| {
                                ui.label(egui::RichText::new(title).size(20.0).strong().color(egui::Color32::WHITE));
                                if !detail.is_empty() {
                                    ui.label(egui::RichText::new(detail).size(14.0).color(egui::Color32::from_gray(215)));
                                }
                            });
                        })
                    },
                );
            }
        });
        let jobs = self.egui.tessellate(out.shapes, out.pixels_per_point);
        for (id, deltas) in &out.textures_delta.set {
            for delta in deltas {
                self.egui_renderer.update_texture(&self.device, &self.queue, *id, delta);
            }
        }
        let sd = egui_wgpu::ScreenDescriptor { size_in_pixels: [w, h], pixels_per_point: out.pixels_per_point };
        self.egui_renderer.update_buffers(&self.device, &self.queue, enc, &jobs, &sd);
        {
            let mut pass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("overlay"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                    })],
                    ..Default::default()
                })
                .forget_lifetime();
            self.egui_renderer.render(&mut pass, &jobs, &sd);
        }
        for id in &out.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }
    }
}

fn pick_present_mode(modes: &[wgpu::PresentMode], vsync: bool) -> wgpu::PresentMode {
    use wgpu::PresentMode::*;
    let prefs: &[wgpu::PresentMode] = if vsync { &[Fifo] } else { &[Immediate, Mailbox, Fifo] };
    prefs.iter().copied().find(|m| modes.contains(m)).unwrap_or(Fifo)
}
