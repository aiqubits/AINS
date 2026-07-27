//! Native 端 RuntimeAdapter 实现（仅 `cfg(not(target_arch = "wasm32"))` 编译）。

use std::future::Future;
use std::time::Duration;

use crate::marker::MaybeSend;
use crate::runtime_adapter::RuntimeAdapter;

pub struct TokioRuntimeAdapter;

impl RuntimeAdapter for TokioRuntimeAdapter {
    fn spawn<F>(future: F)
    where
        F: Future<Output = ()> + MaybeSend + 'static,
    {
        tokio::spawn(future);
    }

    fn sleep(duration: Duration) -> impl Future<Output = ()> {
        tokio::time::sleep(duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sleep_completes() {
        TokioRuntimeAdapter::sleep(Duration::from_millis(5)).await;
    }

    #[tokio::test]
    async fn spawn_runs_task() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        TokioRuntimeAdapter::spawn(async move {
            let _ = tx.send(42u8);
        });
        assert_eq!(rx.await.unwrap(), 42);
    }
}
