//! Custom wgpu primitive for rendering Servo WebView frames without flickering.
//!
//! Using `image::Handle::from_rgba` creates a new GPU texture every frame,
//! causing a one-frame black flash while the new texture uploads. This module
//! instead maintains a **persistent** `wgpu::Texture` and updates it in-place
//! with `Queue::write_texture` — the same approach used by `iced_video_player`.
//!
//! # Integration
//!
//! 1. Allocate a [`WebViewFrame`] once (shared state, stable across `view()` calls).
//! 2. When a new Servo frame arrives, call [`WebViewFrame::update`].
//! 3. Pass `&frame` to [`crate::WebViewWidget::new`] — the widget calls
//!    `renderer.draw_primitive(bounds, WebViewPrimitive::from_frame(frame))` internally.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

// ── Shared frame state ────────────────────────────────────────────────────────

/// Pixel data and dimensions, updated atomically under a single `Mutex`.
///
/// Combining `pixels` and the size into one lock prevents the data race where
/// a producer sets new pixels (lock 1) but hasn't updated the size (lock 2)
/// yet, and a consumer snapshots an inconsistent pair.
#[derive(Debug)]
pub(crate) struct FrameInner {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

/// Shared pixel buffer and upload flag, held by the application state and
/// referenced by [`WebViewPrimitive`] on every draw.
///
/// # Example
///
/// ```
/// use cosmic_webview::WebViewFrame;
///
/// let frame = WebViewFrame::new();
/// assert!(!frame.has_frame());
///
/// // Simulate a Servo frame arriving:
/// frame.update(4, 4, &[0u8; 64]);
/// assert!(frame.has_frame());
/// ```
pub struct WebViewFrame {
    pub(crate) id: u64,
    /// Pixel data and dimensions, always updated together under one lock.
    pub(crate) inner: Arc<std::sync::Mutex<FrameInner>>,
    pub(crate) needs_upload: Arc<AtomicBool>,
    /// Set to `true` on the first successful `update()`, checked by `has_frame()`
    /// without taking the `inner` lock.
    has_pixels: AtomicBool,
    pub(crate) alive: Arc<AtomicBool>,
}

impl WebViewFrame {
    /// Creates a new, initially empty frame.
    pub fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            inner: Arc::new(std::sync::Mutex::new(FrameInner {
                pixels: Vec::new(),
                width: 0,
                height: 0,
            })),
            needs_upload: Arc::new(AtomicBool::new(false)),
            has_pixels: AtomicBool::new(false),
            alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Copies new RGBA pixel data into the shared buffer and marks it for upload.
    ///
    /// Call this from `App::update()` when a new Servo frame arrives.
    pub fn update(&self, width: u32, height: u32, rgba: &[u8]) {
        // Update pixels and dimensions atomically under a single lock to prevent
        // a consumer from reading new pixels with the old size (or vice versa).
        if let Ok(mut inner) = self.inner.lock() {
            inner.pixels.clear();
            inner.pixels.extend_from_slice(rgba);
            inner.width = width;
            inner.height = height;
        }
        // Release ordering: the writes above must be visible before the flag.
        self.has_pixels.store(true, Ordering::Release);
        self.needs_upload.store(true, Ordering::Release);
    }

    /// Returns `true` if at least one frame has been received.
    ///
    /// Uses an `AtomicBool` to avoid taking the `inner` lock on the draw path.
    pub fn has_frame(&self) -> bool {
        self.has_pixels.load(Ordering::Acquire)
    }
}

impl Default for WebViewFrame {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WebViewFrame {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

// ── Primitive ─────────────────────────────────────────────────────────────────

use iced_wgpu::primitive::{Pipeline, Primitive};
use iced_wgpu::wgpu;

/// iced draw-primitive that renders the latest Servo WebView frame.
///
/// Construct one per widget `draw()` call from a shared [`WebViewFrame`]:
/// ```ignore
/// renderer.draw_primitive(bounds, WebViewPrimitive::from_frame(&state.frame));
/// ```
#[derive(Clone, Debug)]
pub(crate) struct WebViewPrimitive {
    id: u64,
    inner: Arc<std::sync::Mutex<FrameInner>>,
    upload: bool,
    alive: Arc<AtomicBool>,
}

impl WebViewPrimitive {
    /// Creates a primitive from the shared frame, atomically consuming the
    /// pending-upload flag so that each frame is uploaded at most once.
    pub(crate) fn from_frame(frame: &WebViewFrame) -> Self {
        // AcqRel: the swap acts as both acquire (reads above become visible) and
        // release (ensuring the flag clear is seen by subsequent loads). This is
        // sufficient for a single producer / single consumer flag.
        let upload = frame.needs_upload.swap(false, Ordering::AcqRel);
        Self {
            id: frame.id,
            inner: Arc::clone(&frame.inner),
            upload,
            alive: Arc::clone(&frame.alive),
        }
    }
}

// ── wgpu pipeline ─────────────────────────────────────────────────────────────

pub(crate) struct WebViewEntry {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    alive: Arc<AtomicBool>,
}

pub(crate) struct WebViewPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    entries: std::collections::BTreeMap<u64, WebViewEntry>,
}

impl Pipeline for WebViewPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("webview shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("webview bind group layout"),
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
            label: Some("webview pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("webview render pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // REPLACE: the WebView fills its entire bounds with opaque pixels.
                    // Alpha blending would waste blend hardware and composite against
                    // the iced background for no visual benefit.
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("webview sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
            entries: std::collections::BTreeMap::new(),
        }
    }

    /// Drops wgpu resources for any [`WebViewFrame`] that has been dropped.
    fn trim(&mut self) {
        let dead: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(id, e)| (!e.alive.load(Ordering::Acquire)).then_some(*id))
            .collect();
        for id in dead {
            if let Some(e) = self.entries.remove(&id) {
                e.texture.destroy();
            }
        }
    }
}

impl WebViewPipeline {
    /// Uploads new pixel data, (re-)creating the GPU texture if the dimensions changed.
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: u64,
        alive: &Arc<AtomicBool>,
        (width, height): (u32, u32),
        pixels: &[u8],
    ) {
        if width == 0 || height == 0 || pixels.is_empty() {
            return;
        }

        let needs_new_texture = self
            .entries
            .get(&id)
            .map(|e| {
                let size = e.texture.size();
                size.width != width || size.height != height
            })
            .unwrap_or(true);

        if needs_new_texture {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("webview texture"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // Rgba8Unorm: glReadPixels returns linear bytes regardless of
                // whether the GL framebuffer is sRGB-capable (OpenGL linearises
                // on read from sRGB attachments; non-sRGB attachments already
                // store linear values). Using Rgba8UnormSrgb would cause wgpu to
                // linearise again on sample, doubling the gamma correction and
                // making images too dark.
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });

            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("webview bind group"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });

            self.entries.insert(
                id,
                WebViewEntry {
                    texture,
                    bind_group,
                    alive: Arc::clone(alive),
                },
            );
        }

        // Safe: entry was just inserted above if it was missing.
        let entry = self
            .entries
            .get(&id)
            .expect("entry must exist after insert");
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &entry.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }
}

// ── Primitive impl ────────────────────────────────────────────────────────────

impl Primitive for WebViewPrimitive {
    type Pipeline = WebViewPipeline;

    fn prepare(
        &self,
        pipeline: &mut WebViewPipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &iced_wgpu::core::Rectangle,
        _viewport: &iced_wgpu::graphics::Viewport,
    ) {
        if self.upload {
            // Lock inner once to get both pixels and dimensions atomically.
            if let Ok(inner) = self.inner.lock() {
                pipeline.upload(
                    device,
                    queue,
                    self.id,
                    &self.alive,
                    (inner.width, inner.height),
                    &inner.pixels,
                );
            }
        }
    }

    /// Draws the webview texture into the render pass provided by iced.
    ///
    /// iced has already set the viewport/scissor to the widget bounds, so the
    /// fullscreen-triangle vertex shader naturally fills exactly that area.
    fn draw(&self, pipeline: &WebViewPipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let Some(entry) = pipeline.entries.get(&self.id) else {
            return false;
        };
        render_pass.set_pipeline(&pipeline.pipeline);
        render_pass.set_bind_group(0, &entry.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
        true
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_frame_has_no_frame() {
        let frame = WebViewFrame::new();
        assert!(!frame.has_frame());
    }

    #[test]
    fn update_marks_frame_available() {
        let frame = WebViewFrame::new();
        frame.update(4, 4, &[0u8; 64]);
        assert!(frame.has_frame());
    }

    #[test]
    fn update_stores_pixels_and_dimensions_atomically() {
        let frame = WebViewFrame::new();
        frame.update(4, 4, &[0xffu8; 64]);
        let inner = frame.inner.lock().unwrap();
        assert_eq!(inner.width, 4);
        assert_eq!(inner.height, 4);
        assert_eq!(inner.pixels.len(), 64);
    }

    #[test]
    fn from_frame_consumes_upload_flag() {
        let frame = WebViewFrame::new();
        frame.update(4, 4, &[0u8; 64]);
        assert!(frame.needs_upload.load(Ordering::SeqCst));
        let _primitive = WebViewPrimitive::from_frame(&frame);
        assert!(!frame.needs_upload.load(Ordering::SeqCst));
    }

    #[test]
    fn second_from_frame_does_not_re_upload() {
        let frame = WebViewFrame::new();
        frame.update(4, 4, &[0u8; 64]);
        let p1 = WebViewPrimitive::from_frame(&frame);
        let p2 = WebViewPrimitive::from_frame(&frame);
        assert!(p1.upload);
        assert!(!p2.upload, "second primitive should not trigger upload");
    }

    #[test]
    fn drop_sets_alive_false() {
        let alive = {
            let frame = WebViewFrame::new();
            Arc::clone(&frame.alive)
        };
        assert!(!alive.load(Ordering::SeqCst));
    }

    #[test]
    fn each_frame_gets_unique_id() {
        let a = WebViewFrame::new();
        let b = WebViewFrame::new();
        assert_ne!(a.id, b.id);
    }
}
