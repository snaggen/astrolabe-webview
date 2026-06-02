# cosmic-webview

A Servo-based WebView widget for the COSMIC Desktop environment. Uses Servo 0.2.0's embedding
API for web rendering and libcosmic (iced + wgpu) as the UI toolkit.

See `PLAN.md` for the full architecture and implementation plan.
See `REQUIREMENTS.md` for the original requirements.

---

## Repository Layout

This crate lives at `~/src/cosmic-webview/`. Two sibling repos are used as local references
during development but are **not** path dependencies — servo is taken from crates.io:

| Path | Purpose |
|------|---------|
| `~/src/servo/` | Local Servo checkout (v0.2.0 tag + one backport commit). Used to read source and embedding examples when needed. |
| `~/src/cosmic-epoch/libcosmic/` | Local libcosmic checkout. Used as a **path dependency** in `Cargo.toml`. |

---

## Key Technical Decisions

- **Servo from crates.io**: `servo = "0.2.0"` — do not switch to a path dependency without a
  specific reason.
- **libcosmic as path dependency**: `libcosmic` is not yet published; use the local checkout.
- **GPU rendering primary**: `GpuRenderingContext` (custom, in `src/rendering.rs`) uses
  surfman with `connection.create_adapter()` (real GPU) and a `SurfaceType::Generic` surface
  — no window handle required. Falls back automatically to `SoftwareRenderingContext` if the
  GPU adapter is unavailable.
- **CPU readback unavoidable in v1**: Servo uses surfman/OpenGL; iced uses wgpu. Frames cross
  from GL to wgpu via `glReadPixels` + `Queue::write_texture`. A zero-copy DMABuf path is
  planned for v2 (see `PLAN.md`).
- **Servo is `!Send`**: All Servo code runs on the main thread. The iced `Subscription` uses
  an `async_channel` to signal `Message::ServoWake`; `App::update` calls
  `servo.spin_event_loop()`.

---

## Build and Run

```bash
# Check the library compiles
cargo check

# Run all tests (including doctests)
cargo test

# Run the browser example (requires a display)
cargo run --example browser

# Force software rendering (for headless / CI)
LIBGL_ALWAYS_SOFTWARE=1 cargo run --example browser
```

Always run before submitting for review:
```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

---

## Code Quality

### Readability and structure

Code must be easy to read, understand, and maintain — not just correct.

- **Short functions, one responsibility each.** If a function needs a mental summary to follow,
  split it. Functions should fit on one screen.
- **No deep nesting.** Prefer early returns and `?` over nested `if let` / `match` chains.
- **Descriptive names.** Names should read as plain English. Avoid abbreviations unless
  universally understood (`url`, `ctx`, `tx`/`rx`).
- **Struct field order:** primary owned state first, configuration second, internal
  bookkeeping last.

### Panic avoidance

The library (`src/`) must not panic on bad input, unexpected engine state, or recoverable
OS failures.

- **`unwrap()` is banned** in `src/`. Use `?`, `unwrap_or`, `unwrap_or_else`, or `match`.
- **`expect()` only for true invariants** — programming errors, not runtime conditions. The
  message must explain the invariant. Every `expect` is a review item.
- **`SoftwareRenderingContext::new`** is the one pre-approved `expect` at startup — software
  rendering must always be available and failure is unrecoverable.
- **In `examples/`**, `expect` on initial setup is acceptable (no caller to propagate to).
- **Recoverable failures**: use `log::warn!` / `log::error!` and return a sensible default
  rather than aborting.

### Writing style: documentation, comments, commits

Be precise and concise. Include what the reader needs; leave out everything else.

- **Rustdoc**: One sentence on what the item does and why, if not obvious from the name.
  Add a runnable example for non-trivial public functions. Do not restate the signature.
- **Inline comments**: Explain *why*, not *what*. If the code needs explaining, simplify it
  first; comment only what remains non-obvious after that.
- **Commit messages**: State what changed and why in one short line. Add a body only when
  the reason is non-obvious from the diff. No bullet lists of sub-changes the diff already shows.

---

## Workflow: No Commits Before Review

**No code is committed to git before a code review has been completed.**

The implementation is split into 5 pull requests (see `PLAN.md`). Each PR is pushed to a
branch and reviewed before merging. Do not run `git commit` or `git push` to the main branch
until the reviewer has approved.

PR order:
1. Project scaffolding (`Cargo.toml`, empty `src/lib.rs`)
2. GPU rendering context (`src/rendering.rs`)
3. Servo lifecycle (`src/waker.rs`, `src/delegate.rs`, `src/state.rs`)
4. iced Widget (`src/widget.rs`, public API in `src/lib.rs`)
5. Browser example (`examples/browser.rs`)

---

## Servo Embedding API Quick Reference

All types below are re-exported from the `servo` crate:

```rust
// Initialisation
ServoBuilder::default()
    .event_loop_waker(Box::new(waker))
    .build()  // → Servo

// WebView
WebViewBuilder::new(&servo, rendering_ctx)
    .url(url)
    .hidpi_scale_factor(Scale::new(factor))
    .delegate(delegate)
    .build()  // → WebView

// Driving the engine (call from App::update on main thread)
servo.spin_event_loop();

// WebViewDelegate — implement these:
fn notify_new_frame_ready(&self, webview: WebView)  // call webview.paint() then read_to_image()
fn notify_url_changed(&self, webview: WebView, url: Url)
fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus)
fn notify_page_title_changed(&self, webview: WebView, title: Option<String>)
fn show_embedder_control(&self, webview: WebView, control: EmbedderControl)  // drop = auto-reply ok
fn hide_embedder_control(&self, webview: WebView, id: EmbedderControlId)

// Navigation
webview.load(url);
webview.go_back(1);
webview.go_forward(1);
webview.reload();
webview.resize(PhysicalSize { width, height });

// Input
webview.notify_input_event(InputEvent::MouseMove(...));
webview.notify_input_event(InputEvent::MouseButton(...));
webview.notify_input_event(InputEvent::Wheel(...));
webview.notify_input_event(InputEvent::Keyboard(...));

// Reading the rendered frame
rendering_ctx.read_to_image(DeviceIntRect::from_size(webview.size().to_i32()))
    // → Option<RgbaImage>
```

rustls must be initialised before creating Servo:
```rust
rustls::crypto::aws_lc_rs::default_provider()
    .install_default()
    .expect("Failed to install crypto provider");
```
