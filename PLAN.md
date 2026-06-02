# cosmic-webview — Implementation Plan

A Servo-based WebView widget for the COSMIC Desktop environment, using libcosmic (iced) as
the UI toolkit and Servo 0.2.0's embedding API for web rendering.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  libcosmic Application  (main thread)                           │
│                                                                 │
│  ┌─────────────────┐        ┌────────────────────────────────┐  │
│  │   cosmic::App   │◄──────►│       WebViewWidget            │  │
│  │                 │ image  │  (iced::Widget)                │  │
│  │  update()       │ handle │                                │  │
│  │  view()         │        │  draw frame / forward events   │  │
│  │  subscription() │        └────────────────────────────────┘  │
│  └────────┬────────┘                    ▲                       │
│           │ spin_event_loop()           │ InputEvent            │
│           │ navigate() / resize()       │ (mouse, keyboard)     │
│           ▼                             │                       │
│  ┌─────────────────────────────────────┴──────────────────────┐ │
│  │                    ServoState                               │ │
│  │                                                            │ │
│  │   servo: Servo          webview: WebView                   │ │
│  │   rendering_ctx: ActiveRenderingContext                    │ │
│  │   current_frame: Option<RgbaImage>                         │ │
│  └──────────────┬─────────────────────────┬───────────────────┘ │
│                 │                         │                      │
│                 ▼                         ▼                      │
│  ┌──────────────────────┐  ┌─────────────────────────────────┐  │
│  │   Servo 0.2.0        │  │   ActiveRenderingContext        │  │
│  │                      │  │                                 │  │
│  │  WebRender (GPU)     │  │  Primary: GpuRenderingContext   │  │
│  │  SpiderMonkey (JS)   │  │   surfman + real GPU adapter    │  │
│  │  CSS layout          │  │                                 │  │
│  │                      │  │  Fallback: SoftwareRendering-   │  │
│  │  notify_new_frame    │  │   Context (Mesa software GL)    │  │
│  │  ─ready()──────────► │  └─────────────────────────────────┘  │
│  │                      │           │ read_to_image()            │
│  └──────────────────────┘           │ (CPU readback)             │
│                                     ▼                            │
│                             RgbaImage → image::Handle            │
└─────────────────────────────────────────────────────────────────┘
                    ▲
                    │  wake signal
         ┌──────────┴──────────┐
         │   async_channel     │
         │  Sender ◄─ waker   │
         │  Receiver ─► Sub   │
         └─────────────────────┘
```

**Frame flow:** Servo renders (GPU) → `webview.paint()` → GL framebuffer → `read_to_image()` →
`RgbaImage` (CPU readback) → `image::Handle` → iced widget → wgpu upload → display.

**Event flow:** User input → iced event → `WebViewWidget::on_event` → `Message::Input` →
`App::update` → `ServoState::send_input` → `webview.notify_input_event`.

**Wake flow:** Servo needs redraw → `EventLoopWaker::wake()` → `async_channel::Sender` →
iced `Subscription` → `Message::ServoWake` → `App::update` → `servo.spin_event_loop()`.

> **Note on CPU readback:** Servo uses surfman/OpenGL while iced uses wgpu. These GPU APIs
> cannot share textures directly without platform-specific code. The CPU readback between
> GL and wgpu is the unavoidable cost for v1. A future DMABuf-based zero-copy path is
> described at the end of this document.

---

## Project Structure

```
cosmic-webview/
├── Cargo.toml
├── REQUIREMENTS.md
├── PLAN.md
└── src/
    ├── lib.rs           # Public API re-exports
    ├── rendering.rs     # GpuRenderingContext + ActiveRenderingContext
    ├── waker.rs         # EventLoopWaker via async_channel
    ├── delegate.rs      # WebViewDelegate + ServoDelegate impls
    ├── state.rs         # ServoState (owns Servo, WebView, rendering ctx)
    └── widget.rs        # WebViewWidget (iced::Widget impl)
└── examples/
    └── browser.rs       # Demo libcosmic browser application
```

---

## Dependencies

```toml
[dependencies]
# Web engine — servo 0.2.0 is not yet on crates.io (only 0.1.x is published).
# Using main branch until 0.2.0 is released; switch to version = "0.2.0" then.
servo = { path = "../servo/components/servo", default-features = false, features = ["js_jit", "vello"] }

# surfman, gleam, glow — must match versions servo 0.2.0 was compiled with
# (Cargo semver resolution handles unification, but these must be compatible)
surfman  = { version = "0.12.0", features = ["chains"] }
gleam    = "0.15"
glow     = "0.17.0"

# UI toolkit
libcosmic = { package = "libcosmic", path = "../cosmic-epoch/libcosmic" }

# Event loop waker channel
async-channel = "2"

# URL handling and window types
url = "2.5"
dpi = "0.1"
```

`surfman`, `gleam`, and `glow` are direct dependencies because `src/rendering.rs` implements
the `servo::RenderingContext` trait from scratch using surfman primitives — the built-in
`SoftwareRenderingContext` and `WindowRenderingContext` do not cover the GPU-offscreen case
without a window handle.

---

## Module Responsibilities

Each module has one job. Crossing these boundaries is a review finding.

| File | Single responsibility |
|---|---|
| `rendering.rs` | OpenGL context creation and pixel readback — nothing else |
| `waker.rs` | Waking the iced event loop — nothing else |
| `delegate.rs` | Receiving callbacks from Servo and routing them to the right handler |
| `dialogs.rs` | Building and presenting libcosmic dialogs in response to delegate callbacks |
| `state.rs` | Owning and driving the Servo engine — the public API surface of the library |
| `widget.rs` | The iced `Widget` implementation — layout, draw, and event forwarding |

---

## Implementation Phases

Each phase corresponds to one pull request. All phases must compile and have passing tests
before the next begins.

---

### Phase 1 — Project Scaffolding

`Cargo.toml` with all dependencies declared. `src/lib.rs` with empty `mod` declarations for
each planned module. Nothing functional yet.

**Verify:** `cargo check` passes, dependency tree resolves without conflicts.

---

### Phase 2 — GPU Rendering Context (`src/rendering.rs`)

`GpuRenderingContext` implements `servo::RenderingContext` using surfman with
`connection.create_adapter()` (real GPU) and a `SurfaceType::Generic` surface — no window
handle required. `ActiveRenderingContext` wraps both variants and tries GPU first:

```rust
pub struct GpuRenderingContext { device, context, gleam_gl, glow_gl, swap_chain, size }

impl GpuRenderingContext {
    pub fn new(size: PhysicalSize<u32>) -> Result<Self, surfman::Error> {
        let connection = Connection::new()?;
        let adapter    = connection.create_adapter()?;  // real GPU
        // ...
    }
}

pub enum ActiveRenderingContext {
    Gpu(Rc<GpuRenderingContext>),
    Software(Rc<SoftwareRenderingContext>),
}

impl ActiveRenderingContext {
    pub fn create(size: PhysicalSize<u32>) -> Self {
        match GpuRenderingContext::new(size) {
            Ok(ctx)  => Self::Gpu(Rc::new(ctx)),
            Err(err) => {
                log::warn!("GPU context unavailable ({err:?}), falling back to software");
                Self::Software(Rc::new(SoftwareRenderingContext::new(size).unwrap()))
            }
        }
    }
}
```

**Verify:** unit tests — GPU context creates or falls back gracefully, `read_to_image` returns
`Some` after a clear, resize does not panic.

---

### Phase 3 — Event Loop Waker (`src/waker.rs`)

Single small file. `IcedEventLoopWaker` wraps an `async_channel::Sender<()>` and implements
`servo::EventLoopWaker`. Cloneable and `Send + Sync` so it can be given to Servo.

```rust
#[derive(Clone)]
pub(crate) struct IcedEventLoopWaker(async_channel::Sender<()>);

impl servo::EventLoopWaker for IcedEventLoopWaker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> { Box::new(self.clone()) }
    fn wake(&self) { let _ = self.0.try_send(()); }
}
```

**Verify:** unit test — `wake()` sends a signal that `Receiver::try_recv()` picks up;
a second `wake()` while the channel is full does not panic.

---

### Phase 4 — Servo Lifecycle (`src/delegate.rs`, `src/state.rs`)

`CosmicWebViewDelegate` with **stub implementations** for all desktop integration methods
(drop = auto-reply). Full implementations come in later phases. Frame capture and waker
signal are the only real logic here.

`ServoState` owns `Servo`, `WebView`, `ActiveRenderingContext`, and the current frame buffer.

```rust
pub(crate) struct CosmicWebViewDelegate {
    rendering_ctx: Rc<dyn servo::RenderingContext>,
    current_frame: Rc<RefCell<Option<RgbaImage>>>,
    waker_tx:      async_channel::Sender<()>,
    on_url_changed:   Option<Box<dyn Fn(Url)>>,
    on_title_changed: Option<Box<dyn Fn(Option<String>)>>,
    on_load_status:   Option<Box<dyn Fn(LoadStatus)>>,
}

impl servo::WebViewDelegate for CosmicWebViewDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        webview.paint();
        let rect = DeviceIntRect::from_size(webview.size().to_i32());
        if let Some(img) = self.rendering_ctx.read_to_image(rect) {
            *self.current_frame.borrow_mut() = Some(img);
        }
        let _ = self.waker_tx.try_send(());
    }
    fn notify_url_changed(&self, _: WebView, url: Url) {
        if let Some(f) = &self.on_url_changed { f(url); }
    }
    fn show_embedder_control(&self, _: WebView, _: EmbedderControl) {} // stub — drop = auto-reply
    fn hide_embedder_control(&self, _: WebView, _: EmbedderControlId) {}
    // all other methods: default (drop) stubs
}

pub struct ServoState {
    servo:         Servo,
    webview:       WebView,
    rendering_ctx: ActiveRenderingContext,
    current_frame: Rc<RefCell<Option<RgbaImage>>>,
    waker_rx:      async_channel::Receiver<()>,
}

impl ServoState {
    pub fn new(initial_url: Url, size: PhysicalSize<u32>) -> (Self, async_channel::Receiver<()>)
    pub fn spin(&self)
    pub fn navigate(&self, url: Url)
    pub fn resize(&self, size: PhysicalSize<u32>)
    pub fn go_back(&self) / go_forward(&self) / reload(&self)
    pub fn send_input(&self, event: InputEvent)
    pub fn current_frame(&self) -> Ref<'_, Option<RgbaImage>>
    pub fn waker_rx(&self) -> async_channel::Receiver<()>
}
```

**Verify:** integration test — create `ServoState`, load `about:blank`, spin until
`LoadStatus::Complete`, assert `current_frame()` is `Some`.

---

### Phase 5 — Dialogs: SimpleDialog + FilePicker (`src/delegate.rs`, `src/dialogs.rs`)

Replace the stubs for the two **Must** `EmbedderControl` variants with real implementations.

**`SimpleDialog`** (`alert` / `confirm` / `prompt`):
- Use `cosmic::dialog` modal.
- `AlertDialog`: message + OK → `dialog.confirm()`.
- `ConfirmDialog`: message + OK / Cancel → `confirm()` or `dismiss()`.
- `PromptDialog`: message + text input + OK / Cancel → `confirm()` with value or `dismiss()`.

**`FilePicker`** (`<input type=file>`):
- Use `cosmic` file dialog (or `rfd::AsyncFileDialog` as fallback).
- Respect `filter_patterns()` and `allow_select_multiple()`.
- On confirm: `picker.select(paths); picker.submit()`. On cancel: `picker.dismiss()`.

**Verify:** unit tests — auto-reply on drop sends the correct default; dismiss sends `None`;
`filter_patterns` are passed through correctly.

---

### Phase 6 — UI Pickers: ColorPicker + SelectElement + ContextMenu (`src/delegate.rs`)

Replace stubs for the three **Should** `EmbedderControl` variants.

**`ColorPicker`** (`<input type=color>`):
- `cosmic::widget::color_picker` in a popover at `picker.position()`.
- Initialised with `picker.current_color()`. On close: `picker.submit()`.

**`SelectElement`** (`<select>`):
- `cosmic::widget::popover` list at the element rect.
- Options from `SelectElementOptionOrOptgroup`. On selection: respond via responder.

**`ContextMenu`** (right-click):
- `cosmic::widget::context_menu` from `menu.items()`.
- On item: `menu.select(action)`. On dismiss: `menu.dismiss()`.

**Verify:** each picker receives its control, renders, and sends back the correct response.

---

### Phase 7 — Window Integration (`src/delegate.rs`)

System-level hooks that don't fit in the dialog phases.

**Cursor mapping** (`notify_cursor_changed`):
- Map `servo::Cursor` → winit `CursorIcon` and set on the window.
- Minimum set: Default, Pointer, Text, Move, Grab, Grabbing, resize variants, NotAllowed,
  Wait, Crosshair.

**Fullscreen** (`notify_fullscreen_state_changed`):
- Toggle the libcosmic window fullscreen. Hide/show header bar accordingly.

**Crash handling** (`notify_crashed`):
- Log via `tracing::error!`. Emit `Message::WebViewCrashed(reason)` for the app to show a
  reload prompt.

**Authentication** (`request_authentication`):
- Show username / password dialog. On confirm: `request.authenticate(u, p)`. Drop = cancel.

**Console + errors** (`show_console_message`, `ServoDelegate::notify_error`):
- Forward to `tracing` at matching log level.

**Verify:** cursor changes propagate to the window; fullscreen toggle works; crashed emits a
message; console messages appear in tracing output.

---

### Phase 8 — WebView Widget: Draw (`src/widget.rs`, `src/lib.rs`)

`WebViewWidget` implements `iced::advanced::Widget` — layout and draw only, no events yet.
The widget renders the current frame and shows a placeholder while loading. Fully functional
as a read-only display.

```rust
pub struct WebViewWidget<'a, Message> {
    frame:        Option<Arc<RgbaImage>>,
    width:        Length,   // default: Fill
    height:       Length,   // default: Fill
    scale_factor: f32,
    _phantom:     PhantomData<&'a Message>,
}
```

Draw converts `RgbaImage` → `image::Handle::from_rgba(w, h, bytes)` and renders via the iced
image renderer.

**Verify:** the widget appears in a minimal test app and renders a frame without panicking.

---

### Phase 9 — WebView Widget: Events (`src/widget.rs`)

Add `on_event` to `WebViewWidget`. Map iced events to Servo `InputEvent`s. Coordinates must
be scaled from iced logical pixels to Servo device pixels (`position * scale_factor`).

| iced event | Servo `InputEvent` |
|---|---|
| `Mouse::ButtonPressed/Released` | `MouseButtonEvent` |
| `Mouse::CursorMoved { position }` | `MouseMoveEvent` |
| `Mouse::WheelScrolled { delta }` | `WheelEvent` |
| `Keyboard::KeyPressed/Released` | `KeyboardEvent` |

Public API finalised in `src/lib.rs`:
```rust
pub use state::ServoState;
pub use widget::WebViewWidget;
```

**Verify:** unit tests — each iced event produces the correct `InputEvent` variant with
correctly scaled coordinates.

---

### Phase 10 — Browser Example (`examples/browser.rs`)

Minimal libcosmic application that demonstrates the full widget. Not production quality.

**Layout:**
```
┌──────────────────────────────────────────┐
│ [←] [→] [↺]  [ https://example.com  ]   │  ← header_end
├──────────────────────────────────────────┤
│                                          │
│           WebViewWidget                  │  ← view()
│                                          │
└──────────────────────────────────────────┘
```

Messages: `ServoWake`, `UrlBarChanged`, `Navigate`, `GoBack`, `GoForward`, `Reload`,
`Input(InputEvent)`, `UrlChanged`, `TitleChanged`, `LoadStatusChanged`, `WebViewCrashed`.

**`subscription()`:**
```rust
iced::subscription::unfold("servo-waker", rx, |rx| async move {
    rx.recv().await.ok();
    (Message::ServoWake, rx)
})
```

**Verify:** `cargo run --example browser` opens a window, loads a URL, responds to
back/forward/reload, and forwards mouse and keyboard to the page.

---

## Desktop Integration Points

Servo delegates all native UI to the embedder. The table below lists every integration point,
its priority for this project, and where it is handled.

### `EmbedderControl` — `show_embedder_control` / `hide_embedder_control`

These arrive via `WebViewDelegate::show_embedder_control`. Each variant is a struct that must
be answered (or dropped, which sends a default reply automatically).

| Variant | Web trigger | v1 priority | libcosmic implementation |
|---|---|---|---|
| `SimpleDialog` | `alert()` / `confirm()` / `prompt()` | **Must** | `cosmic::dialog` — modal with message text and OK / Cancel / text-input |
| `FilePicker` | `<input type=file>` | **Must** | `cosmic` open-file dialog; respects `filter_patterns()` and `allow_select_multiple()` |
| `ColorPicker` | `<input type=color>` | **Should** | `cosmic::widget::color_picker`; initialised with `current_color()`, positioned near `position()` |
| `SelectElement` | `<select>` | **Should** | Popover list positioned at the element; options from `SelectElementOptionOrOptgroup` |
| `ContextMenu` | Right-click | **Should** | `cosmic::widget::context_menu`; items from `ContextMenu::items()`, result via `select()` / `dismiss()` |
| `InputMethod` | Focus on text field | **Later** | IME integration via libcosmic/iced text-input; fields: `text()`, `insertion_point()`, `position()`, `multiline()` |

### `WebViewDelegate` — navigation and window control

| Method | v1 priority | Implementation note |
|---|---|---|
| `notify_cursor_changed` | **Must** | Map `servo::Cursor` to `cosmic` / winit cursor shape and set on the window |
| `notify_fullscreen_state_changed` | **Must** | Toggle the libcosmic window between normal and fullscreen; show/hide browser chrome |
| `request_navigation` | **Must** | Default: allow all. Drop the `NavigationRequest` to allow; call `.deny()` to block |
| `request_create_new` | **Should** | Open a new tab or window using `CreateNewWebViewRequest::builder()` |
| `notify_favicon_changed` | **Should** | Update window icon via `WebView::favicon()` (returns `Option<image::RgbaImage>`) |
| `notify_history_changed` | **Should** | Enable / disable back and forward buttons in browser chrome |
| `request_unload` | **Should** | Default: allow (drop = allow). Show "leave page?" dialog for user-facing embedders |
| `request_resize_to` | **Later** | Honour JS `window.resizeTo()` — resize the libcosmic window |
| `request_move_to` | **Later** | Honour JS `window.moveTo()` — reposition the window |
| `request_protocol_handler` | **Later** | Show a permissions prompt for `mailto:` / `web+foo:` registration |
| `notify_crashed` | **Should** | Show an error page or reload prompt; log reason and backtrace |

### `WebViewDelegate` — permissions and authentication

| Method | v1 priority | Implementation note |
|---|---|---|
| `request_authentication` | **Should** | Show a username / password dialog for HTTP Basic/Digest auth; `AuthenticationRequest::authenticate()` or drop to cancel |
| `request_permission` | **Later** | Show a permission prompt (camera, microphone, geolocation, notifications); `PermissionRequest::allow()` or `.deny()` |
| `show_bluetooth_device_dialog` | **Later** | Show a device-picker for Web Bluetooth; `BluetoothDeviceSelectionRequest::pick_device()` or `.cancel()` |

### `WebViewDelegate` — system notifications and accessibility

| Method | v1 priority | Implementation note |
|---|---|---|
| `show_notification` | **Later** | Forward to the COSMIC / portal notification API |
| `notify_accessibility_tree_update` | **Later** | Pass `accesskit::TreeUpdate` to an AccessKit adapter |
| `notify_media_session_event` | **Later** | Expose playback state to OS media controls via MPRIS |
| `show_console_message` | **Should** | Log to `tracing` at matching level; useful for debugging embedded pages |

### Resource interception — `load_web_resource`

| Scope | v1 priority | Implementation note |
|---|---|---|
| `WebViewDelegate::load_web_resource` | **Should** | Hook point for custom schemes (`app://`, `data://`), ad blocking, or offline cache. Call `load.intercept(response)` or drop to load normally |
| `ServoDelegate::load_web_resource` | **Later** | Same but for service-worker and non-WebView loads |

### `ServoDelegate`

| Method | v1 priority | Implementation note |
|---|---|---|
| `notify_error` | **Must** | Log the `ServoError`; show a fatal-error dialog if unrecoverable |
| `request_devtools_connection` | **Later** | Allow/deny remote DevTools connections; default: deny |
| `notify_devtools_server_started` | **Later** | Log port and token for developer use |

### Cursor mapping (`notify_cursor_changed`)

Servo's `Cursor` enum must be translated to winit / libcosmic cursor icons. Minimum set for v1:

| `servo::Cursor` | System cursor |
|---|---|
| `Default` | `Default` |
| `Pointer` | `Pointer` |
| `Text` | `Text` |
| `Move` | `Move` |
| `Grab` / `Grabbing` | `Grab` / `Grabbing` |
| `NResize` / `SResize` / `EResize` / `WResize` / … | Respective resize arrows |
| `NotAllowed` | `NotAllowed` |
| `Wait` | `Wait` |
| `Crosshair` | `Crosshair` |

### Impact on PR structure

Desktop integration is substantial enough to warrant its own PR. The PR table is updated
accordingly (see Code Review Strategy).

---

## Code Review Strategy

The implementation is split into **10 pull requests**, one per phase. Each PR builds,
compiles, and has passing tests before the next begins. Each PR description must state what
it solves, key design decisions, and how to manually test it.

| PR | Phase | Title | Files touched |
|----|-------|-------|--------------|
| 1  | 1 | Project scaffolding | `Cargo.toml`, `src/lib.rs` |
| 2  | 2 | GPU rendering context | `src/rendering.rs` |
| 3  | 3 | Event loop waker | `src/waker.rs` |
| 4  | 4 | Servo lifecycle + delegate stubs | `src/delegate.rs`, `src/state.rs` |
| 5  | 5 | Dialogs: SimpleDialog + FilePicker | `src/delegate.rs`, `src/dialogs.rs` |
| 6  | 6 | UI pickers: ColorPicker + SelectElement + ContextMenu | `src/delegate.rs` |
| 7  | 7 | Window integration: cursor, fullscreen, crash, auth, console | `src/delegate.rs` |
| 8  | 8 | WebView widget: layout + draw | `src/widget.rs`, `src/lib.rs` |
| 9  | 9 | WebView widget: event forwarding | `src/widget.rs` |
| 10 | 10 | Browser example | `examples/browser.rs` |

**No code is committed before review.** Each PR is reviewed before the next is opened.
PR 10 is the integration proof — reviewer runs `cargo run --example browser` to verify.

**No code is committed before review.** Each PR is written, pushed to a branch, and reviewed
before merging. The reviewer runs the example to confirm behaviour before approving.

---

## Risks and Open Questions

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| surfman GPU adapter unavailable on some machines | Medium | `ActiveRenderingContext` fallback to software |
| `glReadPixels` stalls at 60 fps | Medium | Acceptable for v1; profile before optimising |
| iced scale factor not accessible from Widget | Low | Use `Shell::scale_factor()` or pass from App |
| Servo `!Send` constrains subscription design | Low | All Servo calls in `App::update()` on main thread |
| rustls crypto provider not initialised | Low | Call `rustls::crypto::aws_lc_rs::default_provider().install_default()` in `main()` |

---

## Future Work: Zero-Copy GPU Path (v2)

On Linux/Wayland, `glReadPixels` can be eliminated by sharing the rendered texture directly
between OpenGL and wgpu via DMABuf:

```
surfman GL texture
    → eglExportDMABUFImageMESA      (EGL_MESA_image_dma_buf_export)
    → DMABuf fd
    → VK_EXT_external_memory_dma_buf
    → wgpu::Texture                 (zero CPU copies)
    → iced image widget
```

This requires `khronos_egl`, unsafe `wgpu::hal` access, and a
`#[cfg(all(target_os = "linux", feature = "dmabuf"))]` feature gate. It is not part of v1.
