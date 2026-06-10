//! Zero-copy GPU frame sharing between Servo's GL context and iced's wgpu
//! device via Linux dmabuf.
//!
//! # How it works
//!
//! 1. The wgpu (Vulkan) side allocates two linear-tiled `VkImage`s with
//!    exportable memory and exports each as a dmabuf fd
//!    (`VK_KHR_external_memory_fd` + `VK_EXT_external_memory_dma_buf`).
//!    These become regular `wgpu::Texture`s via `create_texture_from_hal`,
//!    published to the GL side as a [`GpuOffer`].
//! 2. The GL side (Servo's surfman context) imports the fds as EGLImages
//!    (`EGL_EXT_image_dma_buf_import`), binds them to GL textures and wraps
//!    them in FBOs.
//! 3. Per frame, Servo's rendered surface is blitted GPU→GPU into the next
//!    shared buffer (`glBlitFramebuffer`) — the pixels never touch the CPU.
//!    The wgpu side then samples the same memory directly.
//!
//! Double buffering avoids sampling a buffer while GL writes the next frame.
//! `glFinish` after the blit acts as the GL→Vulkan synchronisation barrier;
//! a fence-fd handoff (`EGL_ANDROID_native_fence_sync` → `VkSemaphore`) would
//! avoid the host stall and is a possible follow-up.
//!
//! Every step is validated at runtime; any unsupported piece (non-Vulkan wgpu
//! backend, missing extensions, software GL) permanently falls back to the
//! CPU readback path for that frame source.

use std::ffi::c_void;
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ash::vk;
use gleam::gl::{self, Gl};
use iced_wgpu::wgpu;

/// Number of shared buffers; two is enough since producer and consumer run
/// on the same thread (blit N+1 never overlaps sampling N).
pub(crate) const BUFFER_COUNT: usize = 2;

// DRM fourcc 'AB24' — byte order R,G,B,A in memory, matching both GL RGBA8
// readback order and VK_FORMAT_R8G8B8A8_UNORM on little-endian.
const DRM_FORMAT_ABGR8888: i32 = 0x3432_4241;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

// EGL_EXT_image_dma_buf_import tokens (not exposed by khronos-egl).
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_NONE: i32 = 0x3038;

type EglImage = *mut c_void;
type EglCreateImageKhr = unsafe extern "system" fn(
    display: khronos_egl::EGLDisplay,
    context: khronos_egl::EGLContext,
    target: u32,
    buffer: *mut c_void,
    attrib_list: *const i32,
) -> EglImage;
type EglDestroyImageKhr =
    unsafe extern "system" fn(display: khronos_egl::EGLDisplay, image: EglImage) -> u32;
type GlEglImageTargetTexture2dOes = unsafe extern "system" fn(target: u32, image: EglImage);

// ── Shared state between the two sides ────────────────────────────────────────

/// One exported buffer: the dmabuf fd plus the layout the importer needs.
#[derive(Debug)]
pub(crate) struct DmabufImage {
    pub fd: OwnedFd,
    pub stride: u32,
    pub offset: u32,
}

/// A set of exportable buffers created by the wgpu side, waiting for (or in
/// use by) the GL side.
#[derive(Debug)]
pub(crate) struct GpuOffer {
    /// Distinguishes offers across resizes so stale imports are never used.
    pub epoch: u64,
    pub width: u32,
    pub height: u32,
    pub images: Vec<DmabufImage>,
}

/// Where the zero-copy path currently stands for one frame source.
///
/// Owned by `FrameInner` under its mutex. Producer (GL delegate) and consumer
/// (wgpu prepare) run on the same thread, so transitions never race.
#[derive(Debug, Default)]
pub(crate) enum GpuPath {
    /// CPU readback in use; the wgpu side may create an offer.
    #[default]
    Inactive,
    /// Buffers exported and registered with the render pipeline; the GL side
    /// imports them on the next frame.
    Offered(Arc<GpuOffer>),
    /// Zero-copy running: the wgpu side samples buffer `index`.
    Active { offer: Arc<GpuOffer>, index: usize },
    /// Some step was unsupported — CPU readback is used permanently.
    Failed,
}

impl GpuPath {
    /// Cheap snapshot (Arc clones) for inspection outside the frame lock.
    pub(crate) fn snapshot(&self) -> GpuPath {
        match self {
            GpuPath::Inactive => GpuPath::Inactive,
            GpuPath::Offered(o) => GpuPath::Offered(Arc::clone(o)),
            GpuPath::Active { offer, index } => GpuPath::Active {
                offer: Arc::clone(offer),
                index: *index,
            },
            GpuPath::Failed => GpuPath::Failed,
        }
    }
}

fn next_epoch() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ── Vulkan side: create exportable textures ───────────────────────────────────

/// An offer together with the wgpu textures that alias its buffers.
pub(crate) struct OfferBundle {
    pub offer: Arc<GpuOffer>,
    pub textures: Vec<wgpu::Texture>,
}

/// Tries to create [`BUFFER_COUNT`] exportable, linear-tiled images on wgpu's
/// Vulkan device and wrap them as `wgpu::Texture`s.
///
/// Returns `None` when the backend is not Vulkan, required extensions are
/// missing, or any Vulkan call fails — callers fall back to CPU readback.
pub(crate) fn try_create_offer(
    device: &wgpu::Device,
    width: u32,
    height: u32,
) -> Option<OfferBundle> {
    if width == 0 || height == 0 {
        return None;
    }

    struct RawImage {
        image: vk::Image,
        memory: vk::DeviceMemory,
        fd: OwnedFd,
        stride: u32,
        offset: u32,
    }

    // Collect raw Vulkan handles while holding the hal device guard, then
    // release it before re-entering wgpu via create_texture_from_hal.
    let raw_images: Vec<RawImage> = {
        // SAFETY: read-only access to the hal device; resources we create are
        // handed back to wgpu (texture_from_raw) which manages their lifetime.
        let hal_device = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }?;

        let enabled = hal_device.enabled_device_extensions();
        let has = |name: &std::ffi::CStr| enabled.contains(&name);
        if !has(ash::khr::external_memory_fd::NAME) || !has(ash::ext::external_memory_dma_buf::NAME)
        {
            tracing::info!("dmabuf zero-copy unavailable: external memory extensions not enabled");
            return None;
        }

        let raw = hal_device.raw_device();
        let phys = hal_device.raw_physical_device();
        let instance = hal_device.shared_instance().raw_instance();

        // Linear-tiled sampling must be supported for the shared format.
        // SAFETY: valid instance/physical device owned by wgpu.
        let format_props = unsafe {
            instance.get_physical_device_format_properties(phys, vk::Format::R8G8B8A8_UNORM)
        };
        if !format_props
            .linear_tiling_features
            .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
        {
            tracing::info!("dmabuf zero-copy unavailable: linear RGBA8 sampling not supported");
            return None;
        }
        // SAFETY: as above.
        let mem_props = unsafe { instance.get_physical_device_memory_properties(phys) };

        let fd_loader = ash::khr::external_memory_fd::Device::new(instance, raw);
        let mut images: Vec<RawImage> = Vec::with_capacity(BUFFER_COUNT);

        // Frees everything collected so far plus a possibly dangling image.
        let cleanup = |imgs: &[RawImage], extra: Option<vk::Image>| {
            for it in imgs {
                // SAFETY: handles were created on this device and not handed over.
                unsafe {
                    raw.destroy_image(it.image, None);
                    raw.free_memory(it.memory, None);
                }
            }
            if let Some(img) = extra {
                // SAFETY: as above.
                unsafe { raw.destroy_image(img, None) };
            }
        };

        for _ in 0..BUFFER_COUNT {
            let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let image_info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                // LINEAR: importable everywhere without DRM format modifier
                // negotiation; sampling linear images is still fully GPU-side.
                .tiling(vk::ImageTiling::LINEAR)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::PREINITIALIZED)
                .push_next(&mut external_info);

            // SAFETY: valid device; info structs outlive the call.
            let image = match unsafe { raw.create_image(&image_info, None) } {
                Ok(i) => i,
                Err(err) => {
                    tracing::info!("dmabuf zero-copy: create_image failed: {err:?}");
                    cleanup(&images, None);
                    return None;
                }
            };

            // SAFETY: image just created on this device.
            let reqs = unsafe { raw.get_image_memory_requirements(image) };
            let type_index = find_memory_type(&mem_props, reqs.memory_type_bits);
            let Some(type_index) = type_index else {
                tracing::info!("dmabuf zero-copy: no suitable memory type");
                cleanup(&images, Some(image));
                return None;
            };

            let mut export_info = vk::ExportMemoryAllocateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc_info = vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(type_index)
                .push_next(&mut export_info)
                .push_next(&mut dedicated_info);

            // SAFETY: valid device; dedicated allocation for `image`.
            let memory = match unsafe { raw.allocate_memory(&alloc_info, None) } {
                Ok(m) => m,
                Err(err) => {
                    tracing::info!("dmabuf zero-copy: allocate_memory failed: {err:?}");
                    cleanup(&images, Some(image));
                    return None;
                }
            };
            // SAFETY: image and memory both valid, sizes match (dedicated).
            if let Err(err) = unsafe { raw.bind_image_memory(image, memory, 0) } {
                tracing::info!("dmabuf zero-copy: bind_image_memory failed: {err:?}");
                // SAFETY: handles valid and unused.
                unsafe { raw.free_memory(memory, None) };
                cleanup(&images, Some(image));
                return None;
            }

            let get_fd_info = vk::MemoryGetFdInfoKHR::default()
                .memory(memory)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            // SAFETY: extension presence checked above; memory is exportable.
            let fd = match unsafe { fd_loader.get_memory_fd(&get_fd_info) } {
                Ok(fd) => fd,
                Err(err) => {
                    tracing::info!("dmabuf zero-copy: get_memory_fd failed: {err:?}");
                    // SAFETY: handles valid and unused.
                    unsafe { raw.free_memory(memory, None) };
                    cleanup(&images, Some(image));
                    return None;
                }
            };
            // SAFETY: Vulkan returned a fresh fd that we now own.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };

            let subresource = vk::ImageSubresource {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                array_layer: 0,
            };
            // SAFETY: linear image, valid subresource.
            let layout = unsafe { raw.get_image_subresource_layout(image, subresource) };

            images.push(RawImage {
                image,
                memory,
                fd,
                stride: layout.row_pitch as u32,
                offset: layout.offset as u32,
            });
        }
        images
        // hal device guard dropped here, before create_texture_from_hal.
    };

    // Wrap the raw images as wgpu textures. From here on wgpu owns image and
    // memory (TextureMemory::Dedicated) and destroys them with the texture.
    let mut textures = Vec::with_capacity(BUFFER_COUNT);
    let mut dmabuf_images = Vec::with_capacity(BUFFER_COUNT);
    for raw_image in raw_images {
        let hal_desc = wgpu::hal::TextureDescriptor {
            label: Some("webview dmabuf shared texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::wgt::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        let hal_texture = {
            // SAFETY: guard scope as above; image/memory belong to this device.
            let hal_device = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }?;
            // SAFETY: image created respecting hal_desc; ownership of image and
            // dedicated memory transfers to wgpu-hal (drop_callback None).
            unsafe {
                hal_device.texture_from_raw(
                    raw_image.image,
                    &hal_desc,
                    None,
                    wgpu::hal::vulkan::TextureMemory::Dedicated(raw_image.memory),
                )
            }
        };
        let desc = wgpu::TextureDescriptor {
            label: Some("webview dmabuf shared texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        // SAFETY: hal_texture created from this device respecting desc.
        let texture =
            unsafe { device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(hal_texture, &desc) };
        textures.push(texture);
        dmabuf_images.push(DmabufImage {
            fd: raw_image.fd,
            stride: raw_image.stride,
            offset: raw_image.offset,
        });
    }

    let offer = Arc::new(GpuOffer {
        epoch: next_epoch(),
        width,
        height,
        images: dmabuf_images,
    });
    tracing::info!(
        width,
        height,
        epoch = offer.epoch,
        "dmabuf zero-copy: buffers exported"
    );
    Some(OfferBundle { offer, textures })
}

fn find_memory_type(props: &vk::PhysicalDeviceMemoryProperties, type_bits: u32) -> Option<u32> {
    let pick = |flags: vk::MemoryPropertyFlags| {
        (0..props.memory_type_count).find(|&i| {
            type_bits & (1 << i) != 0
                && props.memory_types[i as usize]
                    .property_flags
                    .contains(flags)
        })
    };
    pick(vk::MemoryPropertyFlags::DEVICE_LOCAL).or_else(|| pick(vk::MemoryPropertyFlags::empty()))
}

// ── GL side: import + per-frame blit ──────────────────────────────────────────

/// Imported state for one offer epoch: EGLImages bound to GL textures wrapped
/// in framebuffers ready to be blit targets.
struct ImportedBuffers {
    epoch: u64,
    width: i32,
    height: i32,
    egl_images: Vec<EglImage>,
    gl_textures: Vec<u32>,
    gl_fbos: Vec<u32>,
}

/// GL-context-side importer. Created lazily on the thread where Servo's GL
/// context is current; owns the EGL function pointers and imported resources.
pub(crate) struct GlImporter {
    /// Keeps libEGL loaded — the raw function pointers below dangle otherwise.
    _egl: khronos_egl::DynamicInstance<khronos_egl::EGL1_4>,
    display: khronos_egl::Display,
    create_image: EglCreateImageKhr,
    destroy_image: EglDestroyImageKhr,
    image_target_texture: GlEglImageTargetTexture2dOes,
    supports_modifiers: bool,
    imported: Option<ImportedBuffers>,
}

impl GlImporter {
    /// Initialises EGL access for the **currently bound** GL context.
    ///
    /// Returns `None` when EGL or the dmabuf import extensions are missing
    /// (e.g. software rendering via OSMesa).
    pub(crate) fn try_new() -> Option<Self> {
        let egl =
            match unsafe { khronos_egl::DynamicInstance::<khronos_egl::EGL1_4>::load_required() } {
                Ok(egl) => egl,
                Err(err) => {
                    tracing::info!("dmabuf zero-copy: libEGL unavailable: {err}");
                    return None;
                }
            };
        let Some(display) = egl.get_current_display() else {
            tracing::info!("dmabuf zero-copy: no current EGL display");
            return None;
        };

        let extensions = egl
            .query_string(Some(display), khronos_egl::EXTENSIONS)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !extensions.contains("EGL_KHR_image_base")
            || !extensions.contains("EGL_EXT_image_dma_buf_import")
        {
            tracing::info!("dmabuf zero-copy: EGL dmabuf import extensions missing");
            return None;
        }
        let supports_modifiers = extensions.contains("EGL_EXT_image_dma_buf_import_modifiers");

        // SAFETY: transmuting eglGetProcAddress results to the documented
        // signatures of these extension entry points.
        let create_image: EglCreateImageKhr =
            unsafe { std::mem::transmute(egl.get_proc_address("eglCreateImageKHR")?) };
        // SAFETY: as above.
        let destroy_image: EglDestroyImageKhr =
            unsafe { std::mem::transmute(egl.get_proc_address("eglDestroyImageKHR")?) };
        // SAFETY: as above (EGL loads client-API extension functions too).
        let image_target_texture: GlEglImageTargetTexture2dOes =
            unsafe { std::mem::transmute(egl.get_proc_address("glEGLImageTargetTexture2DOES")?) };

        Some(Self {
            _egl: egl,
            display,
            create_image,
            destroy_image,
            image_target_texture,
            supports_modifiers,
            imported: None,
        })
    }

    /// Imports the offer's buffers into the current GL context.
    ///
    /// Must be called with Servo's GL context current. Returns `false` (and
    /// releases partial state) when any import step fails.
    pub(crate) fn import(&mut self, gl: &Rc<dyn Gl>, offer: &GpuOffer) -> bool {
        self.release(gl);

        let mut egl_images = Vec::with_capacity(offer.images.len());
        let mut gl_textures = Vec::with_capacity(offer.images.len());
        let mut gl_fbos = Vec::with_capacity(offer.images.len());

        for buf in &offer.images {
            let mut attribs: Vec<i32> = vec![
                EGL_WIDTH,
                offer.width as i32,
                EGL_HEIGHT,
                offer.height as i32,
                EGL_LINUX_DRM_FOURCC_EXT,
                DRM_FORMAT_ABGR8888,
                EGL_DMA_BUF_PLANE0_FD_EXT,
                std::os::fd::AsRawFd::as_raw_fd(&buf.fd),
                EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                buf.offset as i32,
                EGL_DMA_BUF_PLANE0_PITCH_EXT,
                buf.stride as i32,
            ];
            if self.supports_modifiers {
                attribs.extend_from_slice(&[
                    EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                    (DRM_FORMAT_MOD_LINEAR & 0xffff_ffff) as i32,
                    EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                    (DRM_FORMAT_MOD_LINEAR >> 32) as i32,
                ]);
            }
            attribs.push(EGL_NONE);

            // SAFETY: valid display; attrib list is EGL_NONE-terminated; the
            // EGL implementation dups the fd, we retain ownership.
            let image = unsafe {
                (self.create_image)(
                    self.display.as_ptr(),
                    khronos_egl::NO_CONTEXT,
                    EGL_LINUX_DMA_BUF_EXT,
                    std::ptr::null_mut(),
                    attribs.as_ptr(),
                )
            };
            if image.is_null() {
                tracing::info!("dmabuf zero-copy: eglCreateImageKHR failed");
                self.destroy_partial(gl, &egl_images, &gl_textures, &gl_fbos);
                return false;
            }
            egl_images.push(image);

            let texture = gl.gen_textures(1)[0];
            gl.bind_texture(gl::TEXTURE_2D, texture);
            // SAFETY: image is a valid EGLImage; binds it as the texture storage.
            unsafe { (self.image_target_texture)(gl::TEXTURE_2D, image) };
            gl.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
            gl.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
            gl.bind_texture(gl::TEXTURE_2D, 0);
            gl_textures.push(texture);

            let fbo = gl.gen_framebuffers(1)[0];
            gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, fbo);
            gl.framebuffer_texture_2d(
                gl::DRAW_FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                texture,
                0,
            );
            let status = gl.check_frame_buffer_status(gl::DRAW_FRAMEBUFFER);
            gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, 0);
            if status != gl::FRAMEBUFFER_COMPLETE {
                tracing::info!("dmabuf zero-copy: imported FBO incomplete (0x{status:x})");
                gl_fbos.push(fbo);
                self.destroy_partial(gl, &egl_images, &gl_textures, &gl_fbos);
                return false;
            }
            gl_fbos.push(fbo);
        }

        let error = gl.get_error();
        if error != gl::NO_ERROR {
            tracing::info!("dmabuf zero-copy: GL error 0x{error:x} during import");
            self.destroy_partial(gl, &egl_images, &gl_textures, &gl_fbos);
            return false;
        }

        self.imported = Some(ImportedBuffers {
            epoch: offer.epoch,
            width: offer.width as i32,
            height: offer.height as i32,
            egl_images,
            gl_textures,
            gl_fbos,
        });
        tracing::info!(epoch = offer.epoch, "dmabuf zero-copy: buffers imported");
        true
    }

    /// Returns the epoch of the currently imported buffers, if any.
    pub(crate) fn imported_epoch(&self) -> Option<u64> {
        self.imported.as_ref().map(|i| i.epoch)
    }

    /// Blits the currently bound (Servo) framebuffer into shared buffer
    /// `index`. `src_fbo` must belong to the current GL context.
    ///
    /// Returns `false` if the blit produced a GL error.
    pub(crate) fn blit(&self, gl: &Rc<dyn Gl>, src_fbo: u32, index: usize) -> bool {
        let Some(imported) = &self.imported else {
            return false;
        };
        let Some(&dst_fbo) = imported.gl_fbos.get(index) else {
            return false;
        };

        gl.bind_framebuffer(gl::READ_FRAMEBUFFER, src_fbo);
        gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, dst_fbo);
        gl.blit_framebuffer(
            0,
            0,
            imported.width,
            imported.height,
            0,
            0,
            imported.width,
            imported.height,
            gl::COLOR_BUFFER_BIT,
            gl::NEAREST,
        );
        gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, 0);
        // GL→Vulkan synchronisation barrier: guarantee the blit has fully
        // landed in the shared memory before wgpu samples it. A fence-fd
        // handoff would avoid this host stall (documented follow-up).
        gl.finish();

        let error = gl.get_error();
        if error != gl::NO_ERROR {
            tracing::warn!("dmabuf zero-copy: GL error 0x{error:x} during blit");
            return false;
        }
        true
    }

    /// Releases all imported GL/EGL resources. Requires the GL context current.
    pub(crate) fn release(&mut self, gl: &Rc<dyn Gl>) {
        if let Some(imported) = self.imported.take() {
            self.destroy_partial(
                gl,
                &imported.egl_images,
                &imported.gl_textures,
                &imported.gl_fbos,
            );
        }
    }

    fn destroy_partial(
        &self,
        gl: &Rc<dyn Gl>,
        egl_images: &[EglImage],
        gl_textures: &[u32],
        gl_fbos: &[u32],
    ) {
        if !gl_fbos.is_empty() {
            gl.delete_framebuffers(gl_fbos);
        }
        if !gl_textures.is_empty() {
            gl.delete_textures(gl_textures);
        }
        for &image in egl_images {
            // SAFETY: image was created on self.display and not yet destroyed.
            unsafe { (self.destroy_image)(self.display.as_ptr(), image) };
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_fd() -> OwnedFd {
        std::fs::File::open("/dev/null")
            .expect("/dev/null always opens")
            .into()
    }

    fn dummy_offer(epoch: u64) -> Arc<GpuOffer> {
        Arc::new(GpuOffer {
            epoch,
            width: 4,
            height: 4,
            images: vec![
                DmabufImage {
                    fd: dummy_fd(),
                    stride: 16,
                    offset: 0,
                },
                DmabufImage {
                    fd: dummy_fd(),
                    stride: 16,
                    offset: 0,
                },
            ],
        })
    }

    #[test]
    fn gpu_path_default_is_inactive() {
        assert!(matches!(GpuPath::default(), GpuPath::Inactive));
    }

    #[test]
    fn snapshot_preserves_active_index() {
        let path = GpuPath::Active {
            offer: dummy_offer(7),
            index: 1,
        };
        match path.snapshot() {
            GpuPath::Active { offer, index } => {
                assert_eq!(offer.epoch, 7);
                assert_eq!(index, 1);
            }
            _ => panic!("snapshot changed variant"),
        }
    }

    #[test]
    fn epochs_are_unique() {
        assert_ne!(next_epoch(), next_epoch());
    }
}
