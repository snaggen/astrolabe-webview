# Architecture

`astrolabe-webview` embeds the [Servo](https://servo.org) web engine as a widget
for [libcosmic](https://github.com/pop-os/libcosmic) (iced) applications. It
owns the glue between three worlds that do not naturally talk to each other:
Servo's OpenGL rendering, iced's wgpu compositor, and the iced event loop.

## Component map

```
┌─────────────────────────── embedding app (e.g. astrolabe)  ──────────────────────────┐
│  view():  WebViewWidget::new(servo.frame())          update(): servo.spin(), input   │
└──────────────┬───────────────────────────────────────────────┬───────────────────────┘
               │                                               │
        WebViewWidget (widget.rs)                       ServoState (state.rs)
        · forwards mouse/keyboard → InputEvent          · one per tab: WebView +
        · kinetic scrolling                               rendering context + frame
        · draws WebViewPrimitive                        · shared Servo engine
               │                                          (thread_local singleton)
        WebViewPrimitive / WebViewPipeline (primitive.rs)      │
        · persistent wgpu textures, flip pipelines      WebViewDelegateImpl (delegate.rs)
        · zero-copy state machine (wgpu side)           · Servo callbacks → SharedState
               │                                        · produces frames (GL side)
               └───────────── WebViewFrame ─────────────────────┘
                              (shared pixel/GPU-buffer state)
```

Supporting modules: `rendering.rs` (GL contexts and readback),
`gpu_share.rs` (dmabuf zero-copy), `input.rs` (iced→Servo event conversion),
`waker.rs` (engine wake → iced subscription), `dialogs.rs` /
`dialog_handler.rs` (JS dialogs, pickers, context menus).

## Threading model

Everything runs on the main thread. The Servo engine is a process-wide
singleton (`thread_local` in `state.rs`); all `ServoState` instances share it
and one `spin()` call processes events for every WebView. Servo signals
pending work through `IcedEventLoopWaker`, a `bounded(1)` channel — rapid
wakes coalesce into a single `ServoWake` message in the app's iced
subscription.

Although producer (Servo delegate) and consumer (wgpu prepare/draw) never run
concurrently, frame state crosses the iced widget boundary, so `WebViewFrame`
keeps its pixel buffer behind a `Mutex` and its flags in atomics.

## Frame pipeline

When Servo has composited a new frame it calls
`notify_new_frame_ready`. From there, one of four paths brings the pixels to
the screen. The first available path in this order wins:

### 1. Zero-copy dmabuf sharing (Linux, Vulkan backend) — preferred

Feature `dmabuf` (default on). The frame never touches the CPU:

```
Servo GL surface ──glBlitFramebuffer──▶ shared dmabuf image ──sampled by──▶ wgpu
                      (GPU→GPU)          (double-buffered)
```

- **Export (wgpu/Vulkan side, `gpu_share::try_create_offer`):** two
  linear-tiled `VkImage`s with exportable memory are created on iced's wgpu
  device and exported as dmabuf fds (`VK_KHR_external_memory_fd` +
  `VK_EXT_external_memory_dma_buf`). They are wrapped as `wgpu::Texture`s via
  `create_texture_from_hal`, so wgpu owns their lifetime.
- **Import (GL side, `gpu_share::GlImporter`):** Servo's surfman context
  imports the fds as EGLImages (`EGL_EXT_image_dma_buf_import`), binds them to
  GL textures and wraps them in FBOs.
- **Per frame:** the delegate blits Servo's surface FBO into the next shared
  buffer and calls `glFinish` as the GL→Vulkan barrier. wgpu samples the same
  memory directly — zero CPU copies, zero CPU allocations.

The handshake is a state machine stored in `FrameInner.gpu`, advanced by both
sides under the frame lock:

```
            wgpu exports buffers          GL imports + first blit
 Inactive ──────────────────────▶ Offered ───────────────────────▶ Active ──┐
    ▲                                │                               │      │ blit per
    │ resize (buffers wrong size)    │ unsupported / import failed   │      │ frame
    └────────────────────────────────┼───────────────────────◀──────┘      │
                                     ▼                                      │
                                  Failed  (permanent CPU fallback)   ◀──────┘ GL error
```

Linear tiling sidesteps DRM format modifier negotiation at a small sampling
cost. Known follow-up: replace `glFinish` with a fence-fd handoff
(`EGL_ANDROID_native_fence_sync` → imported `VkSemaphore`) to remove the host
stall.

### 2. CPU readback — fallback

Used while zero-copy is `Inactive`/`Offered`, and permanently after `Failed`
(non-Vulkan wgpu backend, missing EGL/Vulkan extensions, software GL):

```
Servo GL surface ──glReadPixels──▶ WebViewFrame.pixels ──write_texture──▶ wgpu
                    (GPU→CPU,        (reused buffer,       (CPU→GPU)
                     direct)          bottom-up rows)
```

`rendering::read_frame_into_sink` reads **directly into the shared upload
buffer** (`FrameSink::fill_with`) — there is no intermediate image and no
allocation after the first frame. Rows stay in GL's bottom-up order; the
vertical flip happens for free in the vertex shader (`vs_main_flip`), selected
per frame via `FrameInner.bottom_up`. This costs one GPU→CPU readback and one
CPU→GPU upload per frame, but zero extra CPU copies.

### 3. Software rendering — context-level fallback

`ActiveRenderingContext::create` tries a GPU surfman context first
(`GpuRenderingContext`: real adapter, headless `SurfaceType::Generic`, swap
chain). If no GPU adapter is available it falls back to Servo's
`SoftwareRenderingContext` (OSMesa). The frame path is then always CPU
readback (path 2); `read_frame_into_sink` works against the
`RenderingContext` trait so it does not care which context is active.

### 4. Manual feeding — embedder-provided pixels

`WebViewFrame::update(width, height, rgba)` remains public for embedders that
produce their own top-down pixel data. It copies into the shared buffer and
renders through the non-flipped pipeline.

### On-demand captures

`ServoState::screenshot()` performs a one-off `read_to_image` (allocating,
top-down) for screenshots and tab thumbnails. It is intentionally not part of
the per-frame path.

## Upload discipline and texture lifetime

`WebViewFrame.needs_upload` is consumed atomically by
`WebViewPrimitive::from_frame`, so each CPU frame is uploaded at most once no
matter how often iced redraws. `WebViewPipeline` keeps one persistent texture
per frame source (updated in place — never recreated per frame, which would
flash black) plus the dmabuf textures keyed by offer epoch. Dropping a
`WebViewFrame` flips its `alive` flag; `trim()` reclaims the GPU resources.

## Visibility gating

`ServoState::set_visible(false)` (background tabs) makes the delegate skip
both compositing (`paint`) and readback entirely; a `frame_pending` flag
records that content changed. `set_visible(true)` immediately paints and reads
back, so switching tabs never shows stale content. Only the active tab pays
any per-frame cost.

## Input path

`WebViewWidget::update` converts iced mouse/keyboard events to Servo
`InputEvent`s (`input.rs`), with logical→device pixel scaling. The widget also
implements kinetic (momentum) scrolling: wheel/touchpad velocity is tracked
with an EWMA and, after the input stops, coast frames are synthesised on
`RedrawRequested` with exponential decay — libinput deliberately leaves
coasting to applications. Keyboard forwarding is suppressed while the app's
URL bar has focus (`keyboard_enabled(false)`).
