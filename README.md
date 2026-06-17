# cosmic-webview

> **Note:** This library has no official connection to [System76](https://system76.com/) or the
> [COSMIC Desktop Environment](https://github.com/pop-os/cosmic-epoch). The name is lazy naming,
> nothing more.
>
> This is a personal, experimental project. See [cosmic-web](../cosmic-web) for context.

A [Servo](https://servo.org/)-based WebView widget for applications built on the
[COSMIC](https://github.com/pop-os/cosmic-epoch) UI toolkit (iced + wgpu).

## Features

- `ServoState` — manages the Servo engine and a single WebView instance
- `WebViewWidget` — iced widget that renders the WebView via wgpu (GPU path or software fallback)
- Kinetic (momentum) scrolling, HiDPI, cursor and context-menu integration
- Cookie and session data persistence
- HTML5 media via GStreamer
- Permission request callbacks (storage, camera/mic)
- Site data management (list, clear cookies and storage per domain)
- Tracker/ad-blocking filter API
- Find in page (`find` / `find_next` / `find_prev` / `find_clear`)
- Remote developer tools (Firefox Remote Debugging Protocol)
- Built-in dialog handler for auth, file picker, select element, etc.

## Quick start

```rust
use cosmic_webview::{ServoState, WebViewWidget, IcedEventLoopWaker};
use dpi::PhysicalSize;
use url::Url;

// Before the first ServoState, configure optional globals:
cosmic_webview::set_profile_dir(data_dir);   // cookie persistence
cosmic_webview::set_web_fonts(fonts);         // custom fonts
cosmic_webview::set_devtools_enabled(6080);   // remote devtools (optional)

// Create a WebView (call once per tab):
let url = Url::parse("https://example.com").unwrap();
let size = PhysicalSize::new(1280_u32, 800);
let (servo, waker_rx) = ServoState::new(url, size, scale_factor);

// In your iced view():
WebViewWidget::new(&servo)

// Forward input events and drive the event loop:
servo.send_input(event);
servo.spin();  // call whenever the waker fires
```

## Build

```bash
cargo build

# Run the bundled single-tab browser example:
cargo run --example browser

# Software rendering:
LIBGL_ALWAYS_SOFTWARE=1 cargo run --example browser
```

Requires a wgpu-capable GPU driver and GStreamer development libraries for media playback.
