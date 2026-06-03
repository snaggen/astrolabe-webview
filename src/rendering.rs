use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use dpi::PhysicalSize;
use euclid::default::Size2D as UntypedSize2D;
use gleam::gl::{self, Gl};
use glow::NativeFramebuffer;
use surfman::chains::{PreserveBuffer, SwapChain};
use surfman::{
    Connection, Context, ContextAttributeFlags, ContextAttributes, Device, GLApi, GLVersion,
    Surface, SurfaceAccess, SurfaceTexture, SurfaceType,
};

// servo re-exports image::RgbaImage and webrender_api::units::* so we don't
// need those crates as direct dependencies.
use servo::{DeviceIntRect, RgbaImage, SoftwareRenderingContext};

/// GPU-accelerated offscreen rendering context for Servo.
///
/// Uses surfman with the system's real GPU adapter and a `SurfaceType::Generic`
/// surface — no window handle required. See [`ActiveRenderingContext::create`]
/// for automatic fallback to software rendering.
///
/// # Example
///
/// ```no_run
/// use cosmic_webview::GpuRenderingContext;
/// use dpi::PhysicalSize;
///
/// let ctx = GpuRenderingContext::new(PhysicalSize::new(800_u32, 600)).unwrap();
/// ```
pub struct GpuRenderingContext {
    // Primary owned state
    size: Cell<PhysicalSize<u32>>,
    device: RefCell<Device>,
    context: RefCell<Context>,
    swap_chain: SwapChain<Device>,
    // GL API handles — stored after context creation
    gleam_gl: Rc<dyn Gl>,
    glow_gl: Arc<glow::Context>,
    // Cached FBO id for the surface currently attached to the context.
    // Updated in new() and after each successful resize(). Using a cached value
    // avoids a driver round-trip on every prepare_for_rendering() call.
    fbo_id: Cell<u32>,
}

impl GpuRenderingContext {
    /// Creates a GPU-accelerated offscreen rendering context.
    ///
    /// Returns `Err` if the GPU adapter is unavailable or context creation fails.
    /// Use [`ActiveRenderingContext::create`] to get automatic software fallback.
    pub fn new(size: PhysicalSize<u32>) -> Result<Self, surfman::Error> {
        if size.width == 0 || size.height == 0 {
            tracing::error!("Cannot create GpuRenderingContext with size under 1x1 ({size:?})");
            return Err(surfman::Error::Failed);
        }

        let connection = Connection::new()?;
        let adapter = connection.create_adapter()?; // real GPU — key difference from software
        let device = connection.create_device(&adapter)?;

        let flags = ContextAttributeFlags::ALPHA
            | ContextAttributeFlags::DEPTH
            | ContextAttributeFlags::STENCIL;
        let version = match connection.gl_api() {
            GLApi::GLES => GLVersion { major: 3, minor: 0 },
            GLApi::GL => GLVersion { major: 3, minor: 2 },
        };
        let descriptor = device.create_context_descriptor(&ContextAttributes { flags, version })?;
        let mut context = device.create_context(&descriptor, None)?;

        // SAFETY: function pointers loaded from the GL driver via surfman.
        let gleam_gl = unsafe {
            match connection.gl_api() {
                GLApi::GL => gl::GlFns::load_with(|s| device.get_proc_address(&context, s)),
                GLApi::GLES => gl::GlesFns::load_with(|s| device.get_proc_address(&context, s)),
            }
        };
        // SAFETY: same driver as above.
        let glow_gl = unsafe {
            glow::Context::from_loader_function(|s| device.get_proc_address(&context, s))
        };

        let surfman_size = euclid::Size2D::new(size.width as i32, size.height as i32);
        let surface = device.create_surface(
            &context,
            SurfaceAccess::GPUOnly,
            SurfaceType::Generic { size: surfman_size },
        )?;
        device
            .bind_surface_to_context(&mut context, surface)
            .map_err(|(err, mut s)| {
                let _ = device.destroy_surface(&mut context, &mut s);
                err
            })?;
        device.make_context_current(&context)?;

        let device = RefCell::new(device);
        let context = RefCell::new(context);
        let swap_chain = {
            let dev = device.borrow();
            let mut ctx = context.borrow_mut();
            // &*dev: Ref<Device> → &Device (device args only need shared refs).
            // &mut *ctx: RefMut<Context> → &mut Context (context args need &mut).
            SwapChain::create_attached(&*dev, &mut *ctx, SurfaceAccess::GPUOnly)?
        };

        // Cache the FBO id now that the swap chain is attached. The FBO id stays
        // stable between frames; it only changes after a successful resize().
        let initial_fbo = current_fbo(&device.borrow(), &context.borrow()).unwrap_or_else(|| {
            tracing::warn!("GpuRenderingContext::new: no FBO bound after swap chain creation");
            0
        });

        // WebRender renders into an sRGB framebuffer (GL_SRGB8_ALPHA8), so
        // glReadPixels returns sRGB-encoded bytes. WebViewPrimitive uploads them
        // as Rgba8UnormSrgb, which is correct: the GPU sampler linearises on read.
        // If this assumption ever breaks (e.g. on a driver that allocates a linear
        // buffer for GLES 3.0), colours will be double-gamma-corrected and appear
        // washed out. Use glGetFramebufferAttachmentParameteriv to verify if
        // unexpected colour shifts are observed.
        tracing::debug!(
            gl_api = ?connection.gl_api(),
            "GpuRenderingContext created; sRGB framebuffer assumed"
        );

        Ok(Self {
            size: Cell::new(size),
            device,
            context,
            swap_chain,
            gleam_gl,
            glow_gl: Arc::new(glow_gl),
            fbo_id: Cell::new(initial_fbo),
        })
    }

    /// Re-queries the FBO id from the driver and updates the cache.
    ///
    /// Called after operations that replace the attached surface (resize).
    fn update_fbo_cache(&self) {
        match current_fbo(&self.device.borrow(), &self.context.borrow()) {
            Some(fbo) => self.fbo_id.set(fbo),
            None => tracing::warn!("update_fbo_cache: no surface bound to context"),
        }
    }
}

impl Drop for GpuRenderingContext {
    fn drop(&mut self) {
        let device = self.device.borrow();
        let mut context = self.context.borrow_mut();
        let _ = self.swap_chain.destroy(&*device, &mut *context);
        let _ = device.destroy_context(&mut *context);
    }
}

impl servo::RenderingContext for GpuRenderingContext {
    fn prepare_for_rendering(&self) {
        self.gleam_gl
            .bind_framebuffer(gl::FRAMEBUFFER, self.fbo_id.get());
        debug_assert_eq!(
            self.gleam_gl.get_error(),
            gl::NO_ERROR,
            "GL error after bind_framebuffer in prepare_for_rendering"
        );
    }

    fn read_to_image(&self, rect: DeviceIntRect) -> Option<RgbaImage> {
        read_pixels_from_fbo(&self.gleam_gl, self.fbo_id.get(), rect)
    }

    fn size(&self) -> PhysicalSize<u32> {
        self.size.get()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            tracing::error!("Cannot resize to under 1x1 ({size:?})");
            return;
        }
        if self.size.get() == size {
            return;
        }
        let new_size = euclid::Size2D::new(size.width as i32, size.height as i32);

        // Scope the borrows so they are released before update_fbo_cache borrows again.
        let resize_ok = {
            let device = self.device.borrow();
            let mut context = self.context.borrow_mut();
            match self.swap_chain.resize(&*device, &mut *context, new_size) {
                Ok(()) => true,
                Err(err) => {
                    tracing::warn!("Failed to resize swap chain to {size:?}: {err:?}");
                    false
                }
            }
        };

        // Only update the cached size and FBO id after the resize succeeded.
        // If we updated size first and the resize failed, read_to_image would
        // pass a rectangle larger than the actual surface to glReadPixels.
        if resize_ok {
            self.size.set(size);
            self.update_fbo_cache();
        }
    }

    fn present(&self) {
        let device = self.device.borrow();
        let mut context = self.context.borrow_mut();
        if let Err(err) = self
            .swap_chain
            .swap_buffers(&*device, &mut *context, PreserveBuffer::No)
        {
            tracing::warn!("Failed to present frame: {err:?}");
        }
    }

    fn make_current(&self) -> Result<(), surfman::Error> {
        self.device
            .borrow()
            .make_context_current(&self.context.borrow())
    }

    fn gleam_gl_api(&self) -> Rc<dyn Gl> {
        self.gleam_gl.clone()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        self.glow_gl.clone()
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        let device = self.device.borrow();
        let mut context = self.context.borrow_mut();
        let size = device.surface_info(&surface).size;
        let texture = device.create_surface_texture(&mut context, surface).ok()?;
        let gl_id = device
            .surface_texture_object(&texture)
            .map_or(0, |t| t.0.get());
        Some((texture, gl_id, size))
    }

    fn destroy_texture(&self, texture: SurfaceTexture) -> Option<Surface> {
        let device = self.device.borrow();
        let mut context = self.context.borrow_mut();
        device
            .destroy_surface_texture(&mut context, texture)
            .map_err(|(err, _)| err)
            .ok()
    }

    fn connection(&self) -> Option<surfman::Connection> {
        Some(self.device.borrow().connection())
    }
}

/// Returns the FBO id for the surface currently bound to `context`, or `None`
/// if no surface is bound or `context_surface_info` fails.
///
/// Returns `Some(0)` if the surface has no explicit FBO (default framebuffer).
fn current_fbo(device: &Device, context: &Context) -> Option<u32> {
    let info = device.context_surface_info(context).ok()??;
    Some(
        info.framebuffer_object
            .map_or(0, |fbo: NativeFramebuffer| fbo.0.get()),
    )
}

/// Reads RGBA pixels from `fbo_id` within `rect` into an [`RgbaImage`].
///
/// Flips the image vertically in-place because OpenGL stores rows bottom-up.
fn read_pixels_from_fbo(gl: &Rc<dyn Gl>, fbo_id: u32, rect: DeviceIntRect) -> Option<RgbaImage> {
    gl.bind_framebuffer(gl::FRAMEBUFFER, fbo_id);
    // Unbind vertex array to work around an OSMesa sync bug (servo/servo#18606).
    gl.bind_vertex_array(0);

    let width = rect.width() as usize;
    let height = rect.height() as usize;
    let mut pixels = gl.read_pixels(
        rect.min.x,
        rect.min.y,
        rect.width(),
        rect.height(),
        gl::RGBA,
        gl::UNSIGNED_BYTE,
    );

    let error = gl.get_error();
    if error != gl::NO_ERROR {
        tracing::warn!("GL error 0x{error:x} after read_pixels");
    }

    // Flip vertically in-place: GL rows are bottom-up, image rows are top-down.
    // Swap rows from the outside in using split_at_mut to avoid an O(n) clone.
    let stride = width * 4;
    for y in 0..(height / 2) {
        let top = y * stride;
        let bot = (height - y - 1) * stride;
        // split_at_mut requires non-overlapping ranges; bot > top always holds here.
        let (head, tail) = pixels.split_at_mut(bot);
        head[top..top + stride].swap_with_slice(&mut tail[..stride]);
    }

    RgbaImage::from_raw(width as u32, height as u32, pixels)
}

/// Wraps either a GPU or software rendering context, chosen at creation time.
///
/// Use [`ActiveRenderingContext::create`] to get the best available context for
/// the current system.
///
/// # Example
///
/// ```no_run
/// use cosmic_webview::ActiveRenderingContext;
/// use dpi::PhysicalSize;
///
/// let ctx = ActiveRenderingContext::create(PhysicalSize::new(800_u32, 600));
/// let rc = ctx.as_rc(); // pass to servo::WebViewBuilder
/// ```
pub struct ActiveRenderingContext(Rc<dyn servo::RenderingContext>);

impl ActiveRenderingContext {
    /// Creates the best available rendering context for `size`.
    ///
    /// Tries GPU-accelerated first; falls back to software rendering if the GPU
    /// adapter is unavailable.
    pub fn create(size: PhysicalSize<u32>) -> Self {
        match GpuRenderingContext::new(size) {
            Ok(ctx) => {
                tracing::info!("Using GPU rendering context");
                Self(Rc::new(ctx))
            }
            Err(err) => {
                tracing::warn!("GPU context unavailable ({err:?}), falling back to software");
                Self(Rc::new(
                    SoftwareRenderingContext::new(size)
                        // Software rendering must always be available; if it is not,
                        // there is no display path and we cannot continue.
                        .expect("software rendering context must be available"),
                ))
            }
        }
    }

    /// Returns the inner context as a reference-counted trait object for use
    /// with [`servo::WebViewBuilder`].
    pub fn as_rc(&self) -> Rc<dyn servo::RenderingContext> {
        self.0.clone()
    }

    /// Creates a software-only context, bypassing the GPU attempt.
    ///
    /// Intended for tests, where GPU WebRender rendering may not be available
    /// or reliable. Matches what Servo's own test suite uses.
    #[cfg(test)]
    pub(crate) fn software(size: PhysicalSize<u32>) -> Self {
        use servo::RenderingContext as _;
        let ctx = SoftwareRenderingContext::new(size)
            .expect("software rendering context must be available");
        ctx.make_current().expect("make_current");
        Self(Rc::new(ctx))
    }
}

#[cfg(test)]
mod tests {
    use servo::DeviceIntSize;

    use super::*;

    fn make_rect(w: i32, h: i32) -> DeviceIntRect {
        DeviceIntRect::from_size(DeviceIntSize::new(w, h))
    }

    #[test]
    fn context_creates_or_falls_back() {
        let ctx = ActiveRenderingContext::create(PhysicalSize::new(64_u32, 64));
        assert!(ctx.as_rc().make_current().is_ok());
    }

    #[test]
    fn read_to_image_returns_some_after_clear() {
        let ctx = ActiveRenderingContext::create(PhysicalSize::new(64_u32, 64));
        let rc = ctx.as_rc();
        rc.make_current().expect("make_current");
        rc.prepare_for_rendering();

        let gl = rc.gleam_gl_api();
        gl.clear_color(1.0, 0.0, 0.0, 1.0);
        gl.clear(gl::COLOR_BUFFER_BIT);

        assert!(rc.read_to_image(make_rect(64, 64)).is_some());
    }

    #[test]
    fn resize_does_not_panic() {
        let ctx = ActiveRenderingContext::create(PhysicalSize::new(64_u32, 64));
        let rc = ctx.as_rc();
        rc.make_current().expect("make_current");
        rc.resize(PhysicalSize::new(128_u32, 128));
        rc.resize(PhysicalSize::new(64_u32, 64));
    }

    #[test]
    fn zero_size_is_rejected() {
        assert!(GpuRenderingContext::new(PhysicalSize::new(0_u32, 64)).is_err());
        assert!(GpuRenderingContext::new(PhysicalSize::new(64_u32, 0)).is_err());
    }
}
