//! Cooperative shutdown signaling.
//!
//! Every spawned task holds a [`ShutdownToken`]. The main loop waits for
//! `Ctrl+C`, then calls [`Shutdown::broadcast`]. Tasks `select!` on their work
//! and `token.wait()` and exit cleanly when the latter resolves.

use tokio::sync::watch;

/// Owner of the shutdown signal. Hold this in the main loop.
#[derive(Debug)]
pub struct Shutdown {
    tx: watch::Sender<bool>,
}

impl Shutdown {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self { tx }
    }

    /// Tasks subscribe by calling `token()` and awaiting `wait()`.
    pub fn token(&self) -> ShutdownToken {
        ShutdownToken {
            rx: self.tx.subscribe(),
        }
    }

    /// Notify all subscribers to shut down. Idempotent.
    pub fn broadcast(&self) {
        // Best-effort send; if no receivers exist, that's fine.
        let _ = self.tx.send(true);
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct ShutdownToken {
    rx: watch::Receiver<bool>,
}

impl ShutdownToken {
    /// Resolves once shutdown has been broadcast. Idempotent — safe to await
    /// multiple times.
    pub async fn wait(&mut self) {
        // Initial value is `false`; wait until it flips to `true` (or the
        // sender is dropped, which we treat as shutdown).
        loop {
            if *self.rx.borrow() {
                return;
            }
            if self.rx.changed().await.is_err() {
                // Sender dropped — treat as shutdown.
                return;
            }
        }
    }

    pub fn is_shutdown(&self) -> bool {
        *self.rx.borrow()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[tokio::test]
    async fn broadcast_unblocks_token_wait() {
        let shutdown = Shutdown::new();
        let mut token = shutdown.token();
        assert!(!token.is_shutdown());

        let handle = tokio::spawn(async move {
            token.wait().await;
            42
        });

        // Give the task a moment to subscribe (not strictly necessary but
        // proves the wakeup, not a race).
        tokio::task::yield_now().await;
        shutdown.broadcast();

        let v = handle.await.unwrap();
        assert_eq!(v, 42);
    }

    #[tokio::test]
    async fn token_wait_resolves_if_sender_dropped() {
        let shutdown = Shutdown::new();
        let mut token = shutdown.token();
        drop(shutdown);
        token.wait().await;
    }
}
