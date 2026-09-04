use std::time::Duration;

use tokio::time::{Instant, Interval, sleep_until};

/// Watcher notifications are edge-triggered: a transient read failure needs
/// its own bounded retry, even when the user does not press Copy a second time.
#[derive(Default)]
pub(super) struct CaptureRetry {
    failures: u32,
    deadline: Option<Instant>,
}

impl CaptureRetry {
    pub(super) fn failed(&mut self) {
        self.failures += 1;
        self.deadline = (self.failures <= 4)
            .then(|| Instant::now() + Duration::from_millis(250 * (1 << (self.failures - 1))));
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) async fn next(
        &mut self,
        changes: &mut Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
        interval: &mut Interval,
    ) {
        let deadline = self.deadline;
        tokio::select! {
            () = super::next_clipboard_change(changes, interval) => self.reset(),
            () = async {
                match deadline {
                    Some(deadline) => sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => self.deadline = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_are_bounded_and_new_changes_reset_the_budget() {
        let mut retry = CaptureRetry::default();
        for _ in 0..4 {
            retry.failed();
            assert!(retry.deadline.is_some());
        }
        retry.failed();
        assert!(retry.deadline.is_none());
        retry.reset();
        retry.failed();
        assert!(retry.deadline.is_some());
    }
}
