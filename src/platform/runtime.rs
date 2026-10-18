//! The app's background Tokio runtime.
//!
//! gpui has its own executor, but the crates the app talks to the network with
//! (twilight, reqwest) need to run inside a Tokio context, so a dedicated
//! runtime is kept alive on its own thread and shared by every caller.

use std::sync::OnceLock;

use tokio::runtime::Handle;
use tokio::task::{AbortHandle, JoinError};

static RUNTIME: OnceLock<Handle> = OnceLock::new();

pub fn handle() -> &'static Handle {
    RUNTIME.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
            tx.send(rt.handle().clone())
                .expect("failed to send tokio runtime handle");
            rt.block_on(std::future::pending::<()>());
        });
        rx.recv().expect("failed to receive tokio runtime handle")
    })
}

/// Runs `future` on the runtime and resolves with its output. The returned
/// future can be awaited from any executor, gpui's included. Dropping it also
/// cancels the background task.
pub async fn run<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T, JoinError> {
    let task = handle().spawn(future);
    let _abort = AbortOnDrop(task.abort_handle());
    task.await
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::Pin, sync::mpsc, time::Duration};

    use super::*;

    #[test]
    fn cancelled_caller_drops_background_work_and_completed_results_are_preserved() {
        struct Dropped(mpsc::SyncSender<()>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (started, mut ready) = futures::channel::oneshot::channel();
        let (stopped, observed_stop) = mpsc::sync_channel(1);
        let mut request = Box::pin(run(async move {
            let _drop = Dropped(stopped);
            let _ = started.send(());
            std::future::pending::<()>().await;
        }));
        futures::executor::block_on(futures::future::poll_fn(|cx| {
            assert!(request.as_mut().poll(cx).is_pending());
            Pin::new(&mut ready).poll(cx)
        }))
        .expect("background task started");
        drop(request);
        observed_stop
            .recv_timeout(Duration::from_secs(2))
            .expect("cancelled background task stopped");
        assert_eq!(
            futures::executor::block_on(run(async { 42 })).expect("result"),
            42
        );
    }
}
