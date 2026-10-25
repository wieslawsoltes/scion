use scion_types::{Error, Result};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::{runtime::Runtime, task::AbortHandle};

/// A private runtime keeps the synchronous storage API independent of the
/// caller's Tokio runtime, including current-thread runtimes and async Drop.
pub(crate) struct IoRuntime {
    runtime: Option<Runtime>,
    active: Arc<AtomicUsize>,
    limit: usize,
    timeout: Duration,
}

pub(crate) struct Admission(Arc<AtomicUsize>);
impl Drop for Admission {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

struct AbortOnDrop(AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl IoRuntime {
    pub(crate) fn new(limit: usize, timeout: Duration) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("scion-spanner-io")
            .enable_all()
            .build()?;
        Ok(Self {
            runtime: Some(runtime),
            active: Arc::new(AtomicUsize::new(0)),
            limit,
            timeout,
        })
    }

    pub(crate) fn admit(&self) -> Result<Admission> {
        let mut active = self.active.load(Ordering::Acquire);
        loop {
            if active >= self.limit {
                return Err(Error::Limit("Spanner concurrency limit reached".into()));
            }
            match self.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => active = observed,
            }
        }
        Ok(Admission(self.active.clone()))
    }

    pub(crate) fn run<T: Send + 'static>(
        &self,
        admission: Admission,
        future: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let (send, receive) = mpsc::sync_channel(1);
        let timeout = self.timeout;
        let task = self
            .runtime
            .as_ref()
            .expect("runtime exists until final owner drops")
            .spawn(async move {
                let result = tokio::time::timeout(timeout, future)
                    .await
                    .unwrap_or_else(|_| {
                        Err(Error::Transport("Spanner operation timed out".into()))
                    });
                drop(admission);
                let _ = send.send(result);
            });
        let _abort = AbortOnDrop(task.abort_handle());
        // A second bound also covers an unexpectedly stalled executor. Dropping
        // the guard aborts the submitted future on every exit path.
        receive
            .recv_timeout(timeout + Duration::from_secs(1))
            .map_err(|_| Error::Transport("Spanner executor unavailable or timed out".into()))?
    }
}

impl Drop for IoRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // Runtime::drop can panic when called from async code. This cancels
            // remaining tasks without blocking an unrelated runtime's worker.
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test(flavor = "current_thread")]
    async fn nested_current_thread_runtime_and_async_destruction_are_safe() {
        let io = IoRuntime::new(2, Duration::from_secs(1)).unwrap();
        assert_eq!(io.run(io.admit().unwrap(), async { Ok(42) }).unwrap(), 42);
        drop(io);
    }

    #[test]
    fn timed_out_work_is_cancelled_and_admission_is_released() {
        let io = IoRuntime::new(1, Duration::from_millis(20)).unwrap();
        let permit = io.admit().unwrap();
        assert!(matches!(io.admit(), Err(Error::Limit(_))));
        let started = Instant::now();
        let result: Result<()> = io.run(permit, std::future::pending());
        assert!(matches!(result, Err(Error::Transport(_))));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(io.run(io.admit().unwrap(), async { Ok(7) }).unwrap(), 7);
    }
}
