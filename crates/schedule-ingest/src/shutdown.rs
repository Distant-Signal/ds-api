//! Stopping on SIGTERM between files (DS-403).
//!
//! schedulefeed is a `Recreate` Deployment, so every roll stops this process
//! before the new pod starts. Without a handler SIGTERM's default action
//! killed it at once, wherever it was: part way through extracting a zip or
//! loading a CORPUS file. Both recover on the next start (the scratch
//! directory is removed, the load is one transaction), but a stop between
//! files needs no recovery at all.
//!
//! [`Shutdown::on_sigterm`] installs the handler; the main loop checks the
//! flag before each lane's CIF cycle and its CORPUS cycle, and waits for it
//! alongside the next tick. A cycle that has started runs to its end, so a
//! stop waits for at most one file, within the pod's
//! `terminationGracePeriodSeconds` (after which the kubelet kills it, as
//! before). Install it only once the loop is about to start: until then
//! SIGTERM keeps its default action, so a pod stopped while it waits at the
//! schema gate still stops promptly.

use tokio::sync::watch;

/// A one-way "stop after this file" flag, cheap to clone and to check.
#[derive(Debug, Clone)]
pub(crate) struct Shutdown {
    rx: watch::Receiver<bool>,
}

/// Sets a [`Shutdown`]'s flag (tests set it by hand).
#[derive(Debug)]
pub(crate) struct Trigger(watch::Sender<bool>);

impl Trigger {
    /// Requests the stop. Idempotent.
    pub(crate) fn trigger(&self) {
        self.0.send_replace(true);
    }
}

impl Shutdown {
    /// A flag and the [`Trigger`] that sets it.
    pub(crate) fn manual() -> (Trigger, Self) {
        let (tx, rx) = watch::channel(false);
        (Trigger(tx), Self { rx })
    }

    /// A flag set on SIGTERM (what the kubelet sends) or Ctrl-C. The
    /// handler is registered before this returns, so a SIGTERM from then
    /// on no longer kills the process.
    pub(crate) fn on_sigterm() -> Self {
        let (trigger, shutdown) = Self::manual();
        let sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .inspect_err(|err| {
                tracing::warn!(error = ?err, "could not install a SIGTERM handler; Ctrl-C only");
            })
            .ok();
        tokio::spawn(async move {
            match sigterm {
                Some(mut sigterm) => {
                    tokio::select! {
                        _ = sigterm.recv() => {}
                        _ = tokio::signal::ctrl_c() => {}
                    }
                }
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            tracing::info!("shutdown requested; stopping after the file in hand");
            trigger.trigger();
        });
        shutdown
    }

    /// Whether the stop has been requested.
    pub(crate) fn is_requested(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolves once the stop has been requested (at once if it has). A
    /// dropped [`Trigger`] that never fired never resolves it.
    pub(crate) async fn requested(&self) {
        let mut rx = self.rx.clone();
        if rx.wait_for(|requested| *requested).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn the_trigger_sets_the_flag_and_wakes_a_waiter() {
        let (trigger, shutdown) = Shutdown::manual();
        assert!(!shutdown.is_requested());
        let waiter = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { shutdown.requested().await }
        });
        trigger.trigger();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the waiter wakes")
            .expect("the waiter task");
        assert!(shutdown.is_requested());
    }

    #[tokio::test]
    async fn a_dropped_trigger_is_not_a_stop() {
        let (trigger, shutdown) = Shutdown::manual();
        drop(trigger);
        assert!(!shutdown.is_requested());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), shutdown.requested())
                .await
                .is_err()
        );
    }

    /// A real SIGTERM to this test process sets the flag instead of killing
    /// it (were the handler missing, the whole test binary would die here).
    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_sets_the_flag() {
        let shutdown = Shutdown::on_sigterm();
        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .expect("run kill");
        assert!(status.success());
        tokio::time::timeout(Duration::from_secs(5), shutdown.requested())
            .await
            .expect("SIGTERM requests the stop");
    }
}
