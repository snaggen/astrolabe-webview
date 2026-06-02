// GPU-accelerated offscreen rendering context for Servo.
// Implements servo::RenderingContext using surfman with a real GPU adapter
// and a SurfaceType::Generic surface — no window handle required.
// Falls back to SoftwareRenderingContext if the GPU adapter is unavailable.
