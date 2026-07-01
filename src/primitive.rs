//! Custom wgpu primitive for rendering Servo WebView frames without flickering.
//!
//! Using `image::Handle::from_rgba` creates a new GPU texture every frame,
//! causing a one-frame black flash while the new texture uploads. This module
//! instead maintains a **persistent** `wgpu::Texture` and updates it in-place
//! with `Queue::write_texture` — the same approach used by `iced_video_player`.
//!
//! # Integration
//!
//! [`crate::ServoState`] owns a [`WebViewFrame`] and fills it directly from the
//! engine — pass `servo.frame()` to [`crate::WebViewWidget::new`]. The manual
//! [`WebViewFrame::update`] path remains available for embedders that produce
//! their own pixel data.
//!
//! # Pixel orientation
//!
//! Frames written by the engine readback path are stored **bottom-up** exactly
//! as OpenGL produces them; the vertical flip happens for free in the vertex
//! shader. Frames fed through [`WebViewFrame::update`] are top-down (normal
//! image order). The orientation travels with the pixels internally and
//! selects the matching render pipeline.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[cfg(all(target_os = "linux", feature = "dmabuf"))]
use crate::gpu_share::{self, GpuPath};

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
    /// `true` when rows are stored bottom-up (GL readback order); the shader
    /// flips them at draw time. `false` for top-down data from `update()`.
    bottom_up: bool,
    /// State of the zero-copy dmabuf path (see [`crate::gpu_share`]).
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    gpu: GpuPath,
}

/// Shared pixel buffer and upload flag, held by the application state and
/// referenced by the internal render primitive on every draw.
///
/// # Example
///
/// ```
/// use cosmic_webview::WebViewFrame;
///
/// let frame = WebViewFrame::new();
/// assert!(!frame.has_frame());
///
/// // Simulate a frame arriving from a manual producer:
/// frame.update(4, 4, &[0u8; 64]);
/// assert!(frame.has_frame());
/// ```
pub struct WebViewFrame {
    pub(crate) id: u64,
    /// Pixel data and dimensions, always updated together under one lock.
    pub(crate) inner: Arc<std::sync::Mutex<FrameInner>>,
    pub(crate) needs_upload: Arc<AtomicBool>,
    /// Set to `true` on the first successful update, checked by `has_frame()`
    /// without taking the `inner` lock.
    has_pixels: Arc<AtomicBool>,
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
                bottom_up: false,
                #[cfg(all(target_os = "linux", feature = "dmabuf"))]
                gpu: GpuPath::Inactive,
            })),
            needs_upload: Arc::new(AtomicBool::new(false)),
            has_pixels: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Copies new top-down RGBA pixel data into the shared buffer and marks it
    /// for upload.
    ///
    /// This is the manual feeding path; the engine fills the frame directly
    /// through its internal frame sink without this extra copy.
    pub fn update(&self, width: u32, height: u32, rgba: &[u8]) {
        // Update pixels and dimensions atomically under a single lock to prevent
        // a consumer from reading new pixels with the old size (or vice versa).
        if let Ok(mut inner) = self.inner.lock() {
            inner.pixels.clear();
            inner.pixels.extend_from_slice(rgba);
            inner.width = width;
            inner.height = height;
            inner.bottom_up = false;
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

    /// Creates a producer handle that writes pixels directly into this frame's
    /// buffer, avoiding any intermediate copy.
    pub(crate) fn sink(&self) -> FrameSink {
        FrameSink {
            inner: Arc::clone(&self.inner),
            needs_upload: Arc::clone(&self.needs_upload),
            has_pixels: Arc::clone(&self.has_pixels),
        }
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

/// Producer handle that fills a [`WebViewFrame`] in place.
///
/// Held by the engine delegate so that `glReadPixels` can target the shared
/// upload buffer directly — the readback **is** the only CPU copy.
pub(crate) struct FrameSink {
    inner: Arc<std::sync::Mutex<FrameInner>>,
    needs_upload: Arc<AtomicBool>,
    has_pixels: Arc<AtomicBool>,
}

impl FrameSink {
    /// Locks the frame buffer and lets `fill` write pixels directly into it.
    ///
    /// `fill` must return `Some((width, height, bottom_up))` describing the
    /// data it wrote, or `None` on failure (the frame is then left unmarked so
    /// stale pixels are never uploaded).
    ///
    /// Returns `true` when a new frame was committed.
    pub(crate) fn fill_with(
        &self,
        fill: impl FnOnce(&mut Vec<u8>) -> Option<(u32, u32, bool)>,
    ) -> bool {
        let committed = {
            let Ok(mut inner) = self.inner.lock() else {
                return false;
            };
            let inner = &mut *inner;
            match fill(&mut inner.pixels) {
                Some((w, h, flipped)) => {
                    inner.width = w;
                    inner.height = h;
                    inner.bottom_up = flipped;
                    true
                }
                None => false,
            }
        };
        if committed {
            self.has_pixels.store(true, Ordering::Release);
            self.needs_upload.store(true, Ordering::Release);
        }
        committed
    }
}

/// Zero-copy state transitions, driven by the GL-side producer (delegate).
#[cfg(all(target_os = "linux", feature = "dmabuf"))]
impl FrameSink {
    /// Cheap snapshot of the current zero-copy state.
    pub(crate) fn gpu_path(&self) -> GpuPath {
        self.inner
            .lock()
            .map(|inner| inner.gpu.snapshot())
            .unwrap_or(GpuPath::Failed)
    }

    /// Returns to CPU readback; the wgpu side may create a fresh offer
    /// (used after a resize invalidated the shared buffers).
    pub(crate) fn set_gpu_inactive(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.gpu = GpuPath::Inactive;
        }
    }

    /// Permanently disables the zero-copy path for this frame source.
    pub(crate) fn set_gpu_failed(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.gpu = GpuPath::Failed;
        }
    }

    /// Commits shared buffer `index` as the frame to display.
    ///
    /// Valid from `Offered` (first activation) and `Active` (subsequent
    /// frames). Marks the frame visible without flagging a CPU upload.
    pub(crate) fn set_gpu_active(&self, index: usize) {
        let activated = {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            match inner.gpu.snapshot() {
                GpuPath::Offered(offer) | GpuPath::Active { offer, .. } => {
                    inner.gpu = GpuPath::Active { offer, index };
                    true
                }
                _ => false,
            }
        };
        if activated {
            self.has_pixels.store(true, Ordering::Release);
        }
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
    /// Selects the flipped pipeline when the stored pixels are bottom-up.
    bottom_up: bool,
    alive: Arc<AtomicBool>,
}

/// wgpu resources for one frame source's zero-copy buffers.
#[cfg(all(target_os = "linux", feature = "dmabuf"))]
pub(crate) struct GpuEntry {
    /// Matches `GpuOffer::epoch`; a new offer (resize) replaces this entry.
    epoch: u64,
    /// Keeps the dmabuf-backed textures (and their Vulkan memory) alive.
    _textures: Vec<wgpu::Texture>,
    bind_groups: Vec<wgpu::BindGroup>,
    /// Buffer to sample, or `None` until the GL side commits a first frame.
    active: Option<usize>,
    alive: Arc<AtomicBool>,
}

pub(crate) struct WebViewPipeline {
    /// Renders top-down pixel data (normal image order).
    pipeline: wgpu::RenderPipeline,
    /// Renders bottom-up pixel data (GL readback order) by flipping V in the
    /// vertex shader — no CPU flip pass needed.
    pipeline_flipped: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    entries: std::collections::BTreeMap<u64, WebViewEntry>,
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    gpu_entries: std::collections::BTreeMap<u64, GpuEntry>,
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

        let make_pipeline = |label: &str, vs_entry: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs_entry),
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
            })
        };

        let pipeline = make_pipeline("webview render pipeline", "vs_main");
        let pipeline_flipped = make_pipeline("webview render pipeline (flipped)", "vs_main_flip");

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
            pipeline_flipped,
            bind_group_layout,
            sampler,
            entries: std::collections::BTreeMap::new(),
            #[cfg(all(target_os = "linux", feature = "dmabuf"))]
            gpu_entries: std::collections::BTreeMap::new(),
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
        #[cfg(all(target_os = "linux", feature = "dmabuf"))]
        self.gpu_entries
            .retain(|_, e| e.alive.load(Ordering::Acquire));
    }
}

impl WebViewPipeline {
    /// Uploads new pixel data, (re-)creating the GPU texture if the dimensions changed.
    #[allow(clippy::too_many_arguments)]
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: u64,
        alive: &Arc<AtomicBool>,
        (width, height): (u32, u32),
        bottom_up: bool,
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
                    bottom_up,
                    alive: Arc::clone(alive),
                },
            );
        }

        // Safe: entry was just inserted above if it was missing.
        let Some(entry) = self.entries.get_mut(&id) else {
            return;
        };
        entry.bottom_up = bottom_up;
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

    /// Advances the zero-copy state machine from the wgpu side.
    ///
    /// - `Inactive` with known dimensions: export shared buffers and publish
    ///   an offer for the GL side (or mark `Failed` when unsupported).
    /// - `Active`: mirror the committed buffer index into the entry so
    ///   `draw()` samples the right texture.
    /// - `Failed`: drop any zero-copy resources.
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    fn sync_gpu(
        &mut self,
        device: &wgpu::Device,
        id: u64,
        alive: &Arc<AtomicBool>,
        inner: &mut FrameInner,
    ) {
        match &inner.gpu {
            GpuPath::Inactive => {
                if inner.width == 0 || inner.height == 0 {
                    return;
                }
                match gpu_share::try_create_offer(device, inner.width, inner.height) {
                    Some(bundle) => {
                        let bind_groups = bundle
                            .textures
                            .iter()
                            .map(|texture| {
                                let view =
                                    texture.create_view(&wgpu::TextureViewDescriptor::default());
                                device.create_bind_group(&wgpu::BindGroupDescriptor {
                                    label: Some("webview dmabuf bind group"),
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
                                })
                            })
                            .collect();
                        self.gpu_entries.insert(
                            id,
                            GpuEntry {
                                epoch: bundle.offer.epoch,
                                _textures: bundle.textures,
                                bind_groups,
                                active: None,
                                alive: Arc::clone(alive),
                            },
                        );
                        inner.gpu = GpuPath::Offered(bundle.offer);
                    }
                    None => {
                        inner.gpu = GpuPath::Failed;
                        self.gpu_entries.remove(&id);
                    }
                }
            }
            GpuPath::Offered(_) => {
                // Waiting for the GL side to import; nothing to do here.
            }
            GpuPath::Active { offer, index } => {
                let (epoch, index) = (offer.epoch, *index);
                if let Some(entry) = self.gpu_entries.get_mut(&id)
                    && entry.epoch == epoch
                {
                    entry.active = Some(index);
                }
            }
            GpuPath::Failed => {
                self.gpu_entries.remove(&id);
            }
        }
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
        // Lock inner once for both the CPU upload and zero-copy maintenance.
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if self.upload {
            pipeline.upload(
                device,
                queue,
                self.id,
                &self.alive,
                (inner.width, inner.height),
                inner.bottom_up,
                &inner.pixels,
            );
        }
        #[cfg(all(target_os = "linux", feature = "dmabuf"))]
        pipeline.sync_gpu(device, self.id, &self.alive, &mut inner);
        #[cfg(not(all(target_os = "linux", feature = "dmabuf")))]
        let _ = &mut inner;
    }

    /// Draws the webview texture into the render pass provided by iced.
    ///
    /// iced has already set the viewport/scissor to the widget bounds, so the
    /// fullscreen-triangle vertex shader naturally fills exactly that area.
    fn draw(&self, pipeline: &WebViewPipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        // Zero-copy path: sample the dmabuf-shared texture directly. Blitted
        // GL content is bottom-up, so the flipped pipeline always applies.
        #[cfg(all(target_os = "linux", feature = "dmabuf"))]
        if let Some(gpu) = pipeline.gpu_entries.get(&self.id)
            && let Some(index) = gpu.active
            && let Some(bind_group) = gpu.bind_groups.get(index)
        {
            render_pass.set_pipeline(&pipeline.pipeline_flipped);
            render_pass.set_bind_group(0, bind_group, &[]);
            render_pass.draw(0..3, 0..1);
            return true;
        }

        let Some(entry) = pipeline.entries.get(&self.id) else {
            return false;
        };
        let render_pipeline = if entry.bottom_up {
            &pipeline.pipeline_flipped
        } else {
            &pipeline.pipeline
        };
        render_pass.set_pipeline(render_pipeline);
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
        assert!(!inner.bottom_up, "manual update stores top-down data");
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

    #[test]
    fn sink_fill_commits_bottom_up_frame() {
        let frame = WebViewFrame::new();
        let sink = frame.sink();
        let committed = sink.fill_with(|pixels| {
            pixels.resize(64, 0xab);
            Some((4, 4, true))
        });
        assert!(committed);
        assert!(frame.has_frame());
        assert!(frame.needs_upload.load(Ordering::SeqCst));
        let inner = frame.inner.lock().unwrap();
        assert_eq!((inner.width, inner.height), (4, 4));
        assert!(inner.bottom_up);
        assert_eq!(inner.pixels.len(), 64);
    }

    #[test]
    fn sink_fill_failure_leaves_frame_unmarked() {
        let frame = WebViewFrame::new();
        let sink = frame.sink();
        let committed = sink.fill_with(|_pixels| None);
        assert!(!committed);
        assert!(!frame.has_frame());
        assert!(!frame.needs_upload.load(Ordering::SeqCst));
    }

    #[test]
    fn sink_fill_reuses_buffer_capacity() {
        let frame = WebViewFrame::new();
        let sink = frame.sink();
        sink.fill_with(|pixels| {
            pixels.resize(1024, 0);
            Some((16, 16, true))
        });
        let cap_before = frame.inner.lock().unwrap().pixels.capacity();
        sink.fill_with(|pixels| {
            pixels.resize(1024, 1);
            Some((16, 16, true))
        });
        let cap_after = frame.inner.lock().unwrap().pixels.capacity();
        assert_eq!(cap_before, cap_after, "no reallocation between frames");
    }
}
