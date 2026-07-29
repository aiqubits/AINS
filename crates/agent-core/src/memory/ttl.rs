//! TTL 后台定期清理（AINS_PLAN Phase 2.3）。
//!
//! 读时检查由各 KvStore 实现内置（`get` 惰性删除过期条目，`list_prefix`
//! 仅跳过，删除交给 sweep）；本模块提供跨平台的后台定期清理任务，经
//! [`RuntimeAdapter`] 派发，业务逻辑不直接触碰 tokio / wasm_bindgen。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::memory::kv::KvStore;
use crate::runtime_adapter::RuntimeAdapter;

/// 默认清理间隔。
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// 后台清理任务句柄；`stop()` 后任务在下一个周期退出。
#[derive(Clone)]
pub struct SweeperHandle {
    stopped: Arc<AtomicBool>,
}

impl SweeperHandle {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }
}

/// 派发 TTL 后台清理任务：每 `interval` 对全部 `stores` 执行一次
/// `sweep_expired`；清理失败不中断任务（下一周期重试）。
///
/// 嵌入方应在启动时派发本任务：`list_prefix` 只跳过过期条目不删除，
/// 未启动 sweeper 时过期行仅在被 `get` 命中时惰性回收，否则持续占用
/// 存储（Web/IndexedDB 后端尤需注意：无进程重启时机，积累不可见）。
pub fn spawn_ttl_sweeper<R: RuntimeAdapter>(
    stores: Vec<Arc<dyn KvStore>>,
    interval: Duration,
) -> SweeperHandle {
    let stopped = Arc::new(AtomicBool::new(false));
    let handle = SweeperHandle {
        stopped: Arc::clone(&stopped),
    };
    R::spawn(async move {
        loop {
            R::sleep(interval).await;
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            for store in &stores {
                // 清理失败不中断任务，但静默降级路径需可观测
                if let Err(e) = store.sweep_expired().await {
                    tracing::warn!(error = %e, "TTL sweep failed; retrying next interval");
                }
            }
        }
    });
    handle
}
