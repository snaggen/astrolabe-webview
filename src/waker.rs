// Wakes the iced event loop when Servo needs to repaint or process events.
// Wraps an async_channel::Sender so the wake signal is Send + Sync + Clone
// without requiring Servo itself to be Send.
