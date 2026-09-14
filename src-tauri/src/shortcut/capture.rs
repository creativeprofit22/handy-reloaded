//! Session-local worker termination and suspension ownership for HandyKeys capture.
//! The caller holds shortcut admission. Neither the session lock nor restoration
//! is used by the polling worker; its listener lock must be released before join.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

pub(super) struct Worker {
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    pub(super) fn spawn(
        run: impl FnOnce(Arc<AtomicBool>) + Send + 'static,
    ) -> Result<Self, String> {
        let running = Arc::new(AtomicBool::new(true));
        let signal = running.clone();
        let handle = thread::Builder::new()
            .name("handy-keys-capture".into())
            .spawn(move || run(signal))
            .map_err(|error| format!("Failed to start capture worker: {error}"))?;
        Ok(Self {
            running,
            handle: Some(handle),
        })
    }

    fn stop(&mut self) -> Result<(), String> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| "Capture worker panicked".to_string())?;
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Default)]
pub(super) struct Session {
    worker: Option<Worker>,
    restore_pending: bool,
}

impl Session {
    #[cfg(all(test, target_os = "windows"))]
    pub(super) fn has_worker(&self) -> bool {
        self.worker.is_some()
    }

    pub(super) fn owns_suspension(&self) -> bool {
        self.restore_pending
    }

    pub(super) fn start(
        &mut self,
        suspend: impl FnOnce() -> Result<(), String>,
        start: impl FnOnce() -> Result<Worker, String>,
        restore: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if self.worker.is_some() || self.restore_pending {
            return Err("Already recording or awaiting shortcut restoration".into());
        }
        suspend()?;
        self.restore_pending = true;
        match start() {
            Ok(worker) => {
                self.worker = Some(worker);
                Ok(())
            }
            Err(error) => match self.restore(restore) {
                Ok(()) => Err(error),
                Err(restoration) => Err(format!(
                    "capture-restoration-failed: {error}; failed to restore shortcuts after recorder startup: {restoration}"
                )),
            },
        }
    }

    fn restore(&mut self, restore: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        if self.restore_pending {
            restore().map_err(|error| format!("capture-restoration-failed: {error}"))?;
            self.restore_pending = false;
        }
        Ok(())
    }

    pub(super) fn stop(
        &mut self,
        dispose_listener: impl FnOnce() -> Result<(), String>,
        restore: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        // Join first: no old session can read the next listener or emit after stop.
        // Still dispose the native listener when a worker panics.
        let stopped = self
            .worker
            .take()
            .map_or(Ok(()), |mut worker| worker.stop());
        let disposed = dispose_listener();
        stopped?;
        disposed?;
        self.restore(restore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn no_session_and_repeated_stop_never_restore_another_captures_shortcuts() {
        let mut session = Session::default();
        for _ in 0..2 {
            session
                .stop(|| Ok(()), || panic!("no owned suspension"))
                .unwrap();
        }
        session
            .start(|| Ok(()), || Worker::spawn(|_| {}), || panic!())
            .unwrap();
        let mut restored = 0;
        session
            .stop(
                || Ok(()),
                || {
                    restored += 1;
                    Ok(())
                },
            )
            .unwrap();
        session
            .stop(|| Ok(()), || panic!("already restored"))
            .unwrap();
        assert_eq!(restored, 1);
    }

    #[test]
    fn failed_restore_is_owned_until_retry_and_duplicate_start_preserves_session() {
        let mut session = Session::default();
        session
            .start(|| Ok(()), || Worker::spawn(|_| {}), || panic!())
            .unwrap();
        assert!(session
            .start(|| panic!(), || panic!(), || panic!())
            .is_err());
        let error = session
            .stop(|| Ok(()), || Err("occupied".into()))
            .unwrap_err();
        assert!(error.contains("capture-restoration-failed"));
        assert!(session.restore_pending);
        assert!(session.worker.is_none());
        session.stop(|| Ok(()), || Ok(())).unwrap();
        assert!(!session.restore_pending);
        session
            .start(|| Ok(()), || Worker::spawn(|_| {}), || panic!())
            .unwrap();
        session.stop(|| Ok(()), || Ok(())).unwrap();
    }

    #[test]
    fn startup_failure_retains_failed_compensation_for_public_stop() {
        let mut session = Session::default();
        assert!(session
            .start(
                || Ok(()),
                || Err("listener failed".into()),
                || Err("restore failed".into())
            )
            .unwrap_err()
            .starts_with("capture-restoration-failed:"));
        assert!(session.restore_pending);
        session.stop(|| Ok(()), || Ok(())).unwrap();
        assert!(!session.restore_pending);
    }

    #[test]
    fn delayed_old_worker_cannot_be_revived_by_rapid_restart() {
        let (signal_tx, signal_rx) = mpsc::channel();
        let (exit_tx, exit_rx) = mpsc::channel();
        let mut old = Worker::spawn(move |running| {
            signal_tx.send(running.clone()).unwrap();
            exit_rx.recv().unwrap();
            assert!(!running.load(Ordering::SeqCst));
        })
        .unwrap();
        let old_signal = signal_rx.recv().unwrap();
        let (stopping_tx, stopping_rx) = mpsc::channel();
        let stopper = thread::spawn(move || {
            old.running.store(false, Ordering::SeqCst);
            stopping_tx.send(()).unwrap();
            old.stop().unwrap();
        });
        stopping_rx.recv().unwrap();
        let (new_tx, new_rx) = mpsc::channel();
        let mut new = Worker::spawn(move |running| {
            new_tx.send(running).unwrap();
        })
        .unwrap();
        assert!(new_rx.recv().unwrap().load(Ordering::SeqCst));
        assert!(!old_signal.load(Ordering::SeqCst));
        exit_tx.send(()).unwrap();
        stopper.join().unwrap();
        new.stop().unwrap();
    }
}
