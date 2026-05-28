//! Per-turn typing-indicator heartbeat.
//!
//! `TypingHeartbeatController` mirrors `StatusReactionController`'s ownership
//! shape: held as an `Arc` from `handle_message` / `dispatch_batch`, lives one
//! turn, releases the typing badge on `Drop`.
//!
//! The background task fires `start_typing` immediately, then every `interval`
//! until the controller is dropped. Drop signals the task via a `watch` channel
//! — the task posts one final `stop_typing` and exits.

use crate::adapter::{ChannelRef, ChatAdapter};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::debug;

/// Owns a background task that keeps the typing badge alive for one turn.
///
/// Drop the controller to stop the badge — the task posts `stop_typing` once
/// then exits within one tick. Errors from `start_typing` / `stop_typing` are
/// logged at `debug` and swallowed so a flaky typing endpoint cannot kill the
/// dispatch loop.
pub struct TypingHeartbeatController {
    kill_tx: watch::Sender<bool>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl TypingHeartbeatController {
    /// Spawn the heartbeat task. Posts one `start_typing` immediately, then
    /// every `interval` until dropped.
    #[allow(dead_code)] // wired in DispatcherSpawnsTyping.
    pub fn new(adapter: Arc<dyn ChatAdapter>, channel: ChannelRef, interval: Duration) -> Self {
        let (kill_tx, mut kill_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            // Initial fire so the badge appears immediately.
            if let Err(e) = adapter.start_typing(&channel).await {
                debug!(error = %e, "start_typing initial fire failed");
            }
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        if let Err(e) = adapter.start_typing(&channel).await {
                            debug!(error = %e, "start_typing heartbeat failed");
                        }
                    }
                    _ = kill_rx.changed() => break,
                }
            }
            if let Err(e) = adapter.stop_typing(&channel).await {
                debug!(error = %e, "stop_typing on drop failed");
            }
        });
        Self {
            kill_tx,
            handle: Some(handle),
        }
    }
}

impl Drop for TypingHeartbeatController {
    fn drop(&mut self) {
        // Signal the loop to exit; the task posts stop_typing then exits.
        let _ = self.kill_tx.send(true);
        // We do NOT await the handle here — Drop is sync. The task runs to
        // completion on the tokio runtime; callers that need to observe the
        // final stop_typing in tests can yield with `tokio::time::sleep`.
        self.handle.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::MessageRef;
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Records every typing call so tests can assert lifecycle ordering.
    #[derive(Default)]
    struct TypingMock {
        calls: Mutex<Vec<&'static str>>,
        fail_start: bool,
    }

    impl TypingMock {
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ChatAdapter for TypingMock {
        fn platform(&self) -> &'static str {
            "mock"
        }
        fn message_limit(&self) -> usize {
            2000
        }
        async fn send_message(&self, _: &ChannelRef, _: &str) -> Result<MessageRef> {
            unimplemented!()
        }
        async fn create_thread(
            &self,
            _: &ChannelRef,
            _: &MessageRef,
            _: &str,
        ) -> Result<ChannelRef> {
            unimplemented!()
        }
        async fn add_reaction(&self, _: &MessageRef, _: &str) -> Result<()> {
            Ok(())
        }
        async fn remove_reaction(&self, _: &MessageRef, _: &str) -> Result<()> {
            Ok(())
        }
        async fn start_typing(&self, _: &ChannelRef) -> Result<()> {
            self.calls.lock().unwrap().push("start");
            if self.fail_start {
                return Err(anyhow!("simulated"));
            }
            Ok(())
        }
        async fn stop_typing(&self, _: &ChannelRef) -> Result<()> {
            self.calls.lock().unwrap().push("stop");
            Ok(())
        }
        fn use_streaming(&self, _: bool) -> bool {
            false
        }
    }

    fn fake_channel() -> ChannelRef {
        ChannelRef {
            platform: "mock".into(),
            channel_id: "c1".into(),
            thread_id: Some("t1".into()),
            parent_id: None,
            origin_event_id: None,
        }
    }

    #[tokio::test]
    async fn heartbeat_fires_repeatedly_and_stops_once_on_drop() {
        let mock = Arc::new(TypingMock::default());
        let ctrl = TypingHeartbeatController::new(
            mock.clone() as Arc<dyn ChatAdapter>,
            fake_channel(),
            Duration::from_millis(20),
        );
        tokio::time::sleep(Duration::from_millis(70)).await;
        drop(ctrl);
        // Give the background task a tick to post its terminator stop.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let calls = mock.calls();
        let start_count = calls.iter().filter(|c| **c == "start").count();
        let stop_count = calls.iter().filter(|c| **c == "stop").count();
        assert!(start_count >= 3, "expected >=3 start, got {calls:?}");
        assert_eq!(stop_count, 1, "expected exactly 1 stop, got {calls:?}");
        assert_eq!(
            calls.last().copied(),
            Some("stop"),
            "stop must be the last call: {calls:?}"
        );
    }

    #[tokio::test]
    async fn immediate_drop_records_one_start_then_one_stop() {
        let mock = Arc::new(TypingMock::default());
        let ctrl = TypingHeartbeatController::new(
            mock.clone() as Arc<dyn ChatAdapter>,
            fake_channel(),
            Duration::from_secs(10),
        );
        drop(ctrl);
        // Yield long enough for the task to run start + stop.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let calls = mock.calls();
        assert_eq!(
            calls,
            vec!["start", "stop"],
            "expected exactly [start, stop], got {calls:?}"
        );
    }

    #[tokio::test]
    async fn heartbeat_loop_resilient_to_start_errors() {
        let mock = Arc::new(TypingMock {
            fail_start: true,
            ..Default::default()
        });
        let ctrl = TypingHeartbeatController::new(
            mock.clone() as Arc<dyn ChatAdapter>,
            fake_channel(),
            Duration::from_millis(20),
        );
        tokio::time::sleep(Duration::from_millis(70)).await;
        drop(ctrl);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let calls = mock.calls();
        let start_count = calls.iter().filter(|c| **c == "start").count();
        let stop_count = calls.iter().filter(|c| **c == "stop").count();
        assert!(
            start_count >= 3,
            "loop should keep firing despite errors, got {calls:?}"
        );
        assert_eq!(stop_count, 1);
    }
}
