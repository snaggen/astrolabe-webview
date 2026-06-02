use async_channel::Sender;

/// Wakes the iced event loop when Servo needs to repaint or process events.
///
/// Pass the paired `Receiver<()>` to an iced `Subscription` that emits
/// `Message::ServoWake` on each signal, then call `servo.spin_event_loop()`
/// in `App::update`.
///
/// # Example
///
/// ```
/// let (waker, rx) = cosmic_webview::IcedEventLoopWaker::new();
/// // Give `waker` to ServoBuilder, poll `rx` in an iced Subscription.
/// ```
#[derive(Clone)]
pub struct IcedEventLoopWaker(Sender<()>);

impl IcedEventLoopWaker {
    /// Creates a waker and the paired receiver.
    ///
    /// The channel is bounded to 1 so that multiple rapid wake calls collapse
    /// into a single pending signal rather than queuing up redundant work.
    pub fn new() -> (Self, async_channel::Receiver<()>) {
        let (tx, rx) = async_channel::bounded(1);
        (Self(tx), rx)
    }

    /// Returns a clone of the underlying sender for use inside the delegate.
    ///
    /// The delegate calls `try_send(())` directly instead of going through
    /// the `EventLoopWaker` trait, avoiding a trait import at the call site.
    pub(crate) fn sender(&self) -> async_channel::Sender<()> {
        self.0.clone()
    }
}

impl servo::EventLoopWaker for IcedEventLoopWaker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        // try_send is non-blocking. If the channel is already full (a wake is
        // pending but not yet consumed), dropping the signal is correct — the
        // receiver will call spin_event_loop on the next iteration regardless.
        let _ = self.0.try_send(());
    }
}

#[cfg(test)]
mod tests {
    use servo::EventLoopWaker as _;

    use super::*;

    #[test]
    fn wake_delivers_signal() {
        let (waker, rx) = IcedEventLoopWaker::new();
        waker.wake();
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn wake_when_full_does_not_panic() {
        let (waker, _rx) = IcedEventLoopWaker::new();
        waker.wake(); // fills the bounded channel
        waker.wake(); // channel full — must not panic
    }

    #[test]
    fn clone_box_produces_working_waker() {
        let (waker, rx) = IcedEventLoopWaker::new();
        let cloned = waker.clone_box();
        cloned.wake();
        assert!(rx.try_recv().is_ok());
    }
}
