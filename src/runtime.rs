//! Callback executor, started only when this engine needs it.
use anyhow::{Result, anyhow};
use std::cell::Cell;
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

tokio::task_local! { static IN_CALLBACK: bool; }
thread_local! { static SYNC_IN_CALLBACK: Cell<bool> = const { Cell::new(false) }; }
pub(crate) fn check_reentry() -> Result<()> {
    anyhow::ensure!(
        !IN_CALLBACK.try_with(|active| *active).unwrap_or(false)
            && !SYNC_IN_CALLBACK.with(Cell::get),
        "database calls from query/storage callbacks are not supported"
    );
    Ok(())
}

pub(crate) fn run_sync_callback<T>(callback: impl FnOnce() -> Result<T>) -> Result<T> {
    struct Guard(bool);
    impl Drop for Guard {
        fn drop(&mut self) {
            SYNC_IN_CALLBACK.with(|active| active.set(self.0));
        }
    }
    let previous = SYNC_IN_CALLBACK.with(|active| active.replace(true));
    let _guard = Guard(previous);
    callback()
}

pub(crate) struct EngineRuntime(parking_lot::Mutex<Option<tokio::runtime::Runtime>>);
impl EngineRuntime {
    pub fn new() -> Result<Self> {
        Ok(Self(parking_lot::Mutex::new(None)))
    }
    pub fn run<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        self.run_cancellable(None, future)
    }
    pub fn run_cancellable<T: Send + 'static>(
        &self,
        stop: Option<Arc<AtomicBool>>,
        future: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let (send, receive) = mpsc::sync_channel(1);
        let mut runtime = self.0.lock();
        if runtime.is_none() {
            *runtime = Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .thread_name("sqweel-callback")
                    .build()?,
            );
        }
        runtime.as_ref().unwrap().spawn(async move {
            let operation = async move {
                tokio::pin!(future);
                loop {
                    if stop
                        .as_ref()
                        .is_some_and(|stop| stop.load(Ordering::Acquire))
                    {
                        return Err(anyhow!("SQL endpoint stopped"));
                    }
                    tokio::select! {
                        result = &mut future => return result,
                        _ = tokio::time::sleep(Duration::from_millis(10)), if stop.is_some() => {}
                    }
                }
            };
            let result = tokio::spawn(IN_CALLBACK.scope(true, operation))
                .await
                .map_err(|error| anyhow!("database callback failed: {error}"))
                .and_then(|result| result);
            let _ = send.send(result);
        });
        drop(runtime);
        receive
            .recv()
            .map_err(|_| anyhow!("database runtime stopped"))?
    }
}
impl Drop for EngineRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.get_mut().take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn synchronous_callback_reentry_is_rejected() {
        assert!(super::run_sync_callback(super::check_reentry).is_err());
        assert!(super::check_reentry().is_ok());
    }
}
