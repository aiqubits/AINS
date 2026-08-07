//! Tool 执行面板视图（Phase 6.7）：展示当前运行时已注册的工具，并支持
//! 逐工具切换活跃状态。非活跃工具不会进入智能体上下文（ToolRuntime
//! `api_schemas` 过滤），状态持久化到本地 KvStore。

use std::sync::atomic::Ordering;

// spawn_forever 不在 prelude 中：任务须挂 root scope，不随组件卸载取消
// （review H1：`spawn` 的任务在 Tools 视图卸载时被取消，在途落盘任务
// 消失而 PERSIST_STATE 永久停在 RUNNING/PENDING，此后切换永不落盘）。
use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use futures::FutureExt;

use agent_core::tools::ToolCategory;
use ui::{
    I18nContext, PERSIST_ERROR, PERSIST_IDLE, PERSIST_PENDING, PERSIST_STATE,
    TOOL_STATE_LOAD_ERROR, ToolCardView, ToolCategoryView, ToolPanel, ToolStateBanner,
    ToolStateBannerKind, persist_on_round_done, persist_on_toggle,
};

use crate::agent::service;

// 进程级全局信号与落盘状态机（review C1 修复，自本文件提升至 ui crate，
// review Minor 2 修复）：spawn_forever 落盘任务挂 ROOT scope、不随 Tools
// 组件卸载取消，组件作用域 Signal 在卸载后被销毁（copy_value 文档：组件
// drop 时值随之 drop），任务完成时写入已销毁的信号会 panic（Signal::set →
// try_write().unwrap()；web release panic=abort 直接中止 wasm 实例）。
// 全局信号存储随进程存活：卸载后写入无害，且重挂载继续显示最新状态
// （与失败 marker 跨挂载提示语义一致）。定义与同步函数见
// ui::PERSIST_ERROR / ui::sync_persist_error；状态机（PERSIST_STATE 与
// PERSIST_IDLE/RUNNING/PENDING）与挂载同步决策（should_sync_persist_error
// / persist_task_in_flight）同样在 ui crate，供 /tools 与会话视图共用。

/// agent-core 工具分类 → 面板徽标视图（MCP 桥接工具按名称前缀识别，
/// 其余按 `Tool::category()` 自报分类）。
fn to_category_view(name: &str, category: ToolCategory) -> ToolCategoryView {
    if name.starts_with("mcp__") {
        return ToolCategoryView::Mcp;
    }
    match category {
        ToolCategory::Compute => ToolCategoryView::Compute,
        ToolCategory::FileSystem => ToolCategoryView::FileSystem,
        ToolCategory::System => ToolCategoryView::System,
        ToolCategory::Network => ToolCategoryView::Network,
        ToolCategory::Browser => ToolCategoryView::Browser,
        ToolCategory::AgentInternal => ToolCategoryView::Meta,
    }
}

#[component]
pub fn Tools() -> Element {
    let i18n = use_context::<I18nContext>();
    let t = i18n.t();
    let mut tools = use_signal(Vec::<ToolCardView>::new);
    // 状态恢复失败横幅：存储不可读时无法恢复禁用清单，所有工具默认活跃，
    // 必须让用户知晓（避免误以为此前停用仍生效）。状态存于进程级全局
    // 信号 [`TOOL_STATE_LOAD_ERROR`]（与 /agent 会话视图共享订阅，替代
    // 各视图独立的组件级信号/装配时快照）：由 /tools 挂载或会话装配的
    // 加载失败置位、任一加载成功清空，两视图状态源一致。
    // 持久化失败提示：切换已生效但未落盘（如本地存储配额满/写失败），
    // 跨重启可能静默回滚——与加载失败的横幅对称，必须告知用户。状态存于
    // 进程级全局信号 [`PERSIST_ERROR`]（review C1，见 ui crate 定义）：
    // 落盘任务经 spawn_forever 挂 ROOT scope、不随组件卸载取消，若写入
    // 组件作用域 Signal，卸载后任务完成会 panic（web release panic=abort
    // 直接崩溃）。

    // 异步初始化：从本地存储恢复活跃状态，再合并运行时工具清单。
    // 仅构造 ToolRuntime 读 schema + 分类，不装配 Kernel/会话。
    use_future(move || {
        let mut tools = tools;
        async move {
            if let Err(e) = service::load_tool_states().await {
                // 读取失败 ≠ 无记录：升级为 error 并展示横幅，明确当前
                // 全部工具为活跃状态（fail-open 回退：禁用清单无法恢复，
                // 工具默认全活跃，必须显式告知用户）。
                tracing::error!("tool states load failed, tools default to active: {e}");
                // 本进程已有未落盘切换，或内存禁用清单非空（成功落盘后
                // dirty 已清零但禁用仍生效）时，加载失败 ≠ "全部活跃"——
                // 文案需区分，避免误导用户以为此前的停用已失效。快照恢复
                // 失败时刻的状态写入进程级信号，渲染期拼文案（文案选择
                // 不得重读内存，否则随会话期间 dirty/禁用集合变化而漂移）。
                let retained = service::tool_state_service().has_retained_state();
                *TOOL_STATE_LOAD_ERROR.write() = Some((e, retained));
            } else {
                // 加载成功：存储可读且状态已恢复，清空进程级信号——/agent
                // 会话装配与 /tools 挂载共享同一状态源，任一加载成功即
                // 说明恢复失败不再是当前事实（两视图横幅一致消失）。
                *TOOL_STATE_LOAD_ERROR.write() = None;
            }
            // 跨挂载的持久化失败提示：读取存储 marker 同步到进程级信号
            // PERSIST_ERROR。无标记时也显式清空信号（review Minor 2）：
            // 落盘任务 panic 路径只置位信号而未写存储标记，若不清理，
            // 重挂载后陈旧横幅残留且无自愈手段。
            // 在途落盘任务存在时（挂载瞬间任务尚未收敛）另做保护（review
            // Minor 3）：marker 可能是更早失败的残留而任务即将成功——设置
            // 提示方向安全（宁多提示，任务完成即清空）；反向（任务将失败
            // 而 marker 尚未写入）时清空会误清任务即将写入的失败信号，故
            // 在途且无 marker 时跳过同步，由任务完成后的写入收敛最终状态。
            // 陈旧 marker 竞态修复（review Minor 1）：首次读取 marker 可能
            // 恰在在途任务成功收敛前完成（读到陈旧失败标记）——任务成功
            // 会删除 marker 并清空 PERSIST_ERROR；sync_persist_error_on_mount
            // 在无在途任务时重读一次 marker 作为权威值再同步，避免陈旧
            // marker 置位造成"保存失败"横幅在状态已成功落盘后长期残留。
            // 与 /tools 挂载共用同一实现（service.rs），两视图对"在途任务
            // 未收敛"感知一致。
            service::sync_persist_error_on_mount(t.tool_states_save_failed).await;
            let svc = service::tool_state_service();
            let cards = service::tool_schema_snapshot()
                .into_iter()
                .map(|(name, description, category)| {
                    let enabled = svc.is_enabled(&name);
                    ToolCardView {
                        category: to_category_view(&name, category),
                        name,
                        description,
                        enabled,
                    }
                })
                .collect::<Vec<_>>();
            tools.set(cards);
        }
    });

    // 切换活跃状态：更新进程级单例（Kernel 下一轮 api_schemas 自动生效）
    // + 本地卡片 + 异步持久化。持久化失败时设置错误 Signal 提示用户：
    // UI 翻转与落盘结果形成对称反馈，避免跨重启静默回滚。
    let on_toggle = move |(name, enabled): (String, bool)| {
        let svc = service::tool_state_service();
        svc.set_enabled(&name, enabled);
        {
            let mut list = tools.write();
            for card in list.iter_mut() {
                if card.name == name {
                    card.enabled = enabled;
                    break;
                }
            }
        }
        // 落盘任务合并：空闲时由本次调用 spawn 任务；已有在途任务时仅
        // 标记挂起，由在途任务消费后补写最新快照（合并快速连点，避免
        // 任务风暴）。状态机转移用 ui crate 纯函数 [`ui::persist_on_toggle`]
        // （review 中等问题 4：转移逻辑提取为可测纯函数，表驱动测试见
        // ui crate；本闭包仅剩 fetch_update 原子包装）。
        let prev = PERSIST_STATE.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
            Some(persist_on_toggle(state))
        });
        if prev != Ok(PERSIST_IDLE) {
            return;
        }
        // 挂 root scope 的后台任务（review H1）：dioxus `spawn` 的任务随
        // Tools 组件卸载即取消——切换后立刻导航离开（如去会话视图验证
        // 停用效果）会丢弃在途落盘任务，而 PERSIST_STATE 停在 RUNNING/
        // PENDING，此后所有切换只置 PENDING 而无人消费，持久化永久静默
        // 失效（重启才恢复）。spawn_forever 的任务不随卸载取消（review C1）；
        // 错误提示写入进程级全局信号 PERSIST_ERROR——组件作用域 Signal 在
        // 卸载后被销毁，任务完成时写入会 panic。t 为 &'static，卸载后安全。
        spawn_forever(async move {
            // panic 有界重试计数器（review 中等问题 1 修复）：第一轮 panic
            // 后若存在挂起切换（PENDING）补一轮，瞬时故障（如单次 IO panic）
            // 下用户刚做的切换立即重试落盘；第二轮仍 panic 则放弃（RwLock
            // poison 等持久性故障下重试无益且会死循环）。
            let mut panic_retries = 0u8;
            loop {
                // catch_unwind 兜底（review 中等问题 1）：persist 内部 panic
                // （如 disabled 锁被 poison 触发 expect）时任务不能静默死亡——
                // 否则 PERSIST_STATE 永久停在 RUNNING，此后所有切换只置
                // PENDING 而无人消费，持久化静默失效且无任何提示。注意：
                // 该兜底仅对 native/desktop 生效（wasm release 构建 panic=
                // abort，panic 直接终止 wasm 实例，review L4）。
                let outcome = std::panic::AssertUnwindSafe(service::persist_tool_states())
                    .catch_unwind()
                    .await;
                let panicked = outcome.is_err();
                match outcome {
                    Ok(Ok(())) => *PERSIST_ERROR.write() = None,
                    Ok(Err(e)) => {
                        tracing::error!("tool states persist failed: {e}");
                        *PERSIST_ERROR.write() =
                            Some(format!("{}: {e}", t.tool_states_save_failed));
                    }
                    Err(panic) => {
                        // 任务 panic 对用户零反馈（仅日志）会掩盖"切换未落盘"：
                        // 同样升级为可见横幅（review H1 建议）。
                        tracing::error!("tool states persist task panicked: {panic:?}");
                        *PERSIST_ERROR.write() = Some(t.tool_states_save_failed.to_string());
                    }
                }
                if panicked {
                    // panic 路径与 Ok(Err) 失败路径的"消费 PENDING 继续"对称
                    // （review 中等问题 1 修复）：第一轮 panic 且存在挂起切换
                    // 时补一轮，而非直接收敛丢弃排队切换；无挂起切换（prev
                    // 非 PENDING）或第二轮仍 panic 则放弃。转移复用
                    // [`ui::persist_on_round_done`]（PENDING→RUNNING 触发补轮，
                    // RUNNING→IDLE 收敛放弃），与正常消费路径同一实现。
                    if panic_retries == 0 {
                        panic_retries += 1;
                        let prev = PERSIST_STATE.fetch_update(
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                            |state| Some(persist_on_round_done(state)),
                        );
                        if prev == Ok(PERSIST_PENDING) {
                            continue;
                        }
                    }
                    // 放弃：写失败 marker（dirty != 0 时，被收敛丢弃的挂起
                    // 切换信息不丢失）再收敛状态机到 IDLE（marker 先于收敛，
                    // 挂载路径不得在 marker 写入前读到 in_flight=false 而清空
                    // 失败信号——见 service::recover_persist_panic）。收敛后
                    // 由下次切换经 0→1 转移重新 spawn 任务。
                    //
                    // recover 返回 true 表示收敛期间存在挂起切换（marker
                    // await 中用户又切换，review 建议 1 加固）：补最后一轮
                    // 消费——该切换的禁用清单仍在内存（dirty 未落盘），不补
                    // 轮即静默不落盘。有界：仅当重试未耗尽（panic_retries <
                    // 2），此轮再 panic 则直接退出，PENDING 由 marker 提示
                    // 兜底（方向安全，宁多提示）。
                    if service::recover_persist_panic("persist task panicked").await
                        && panic_retries < 2
                    {
                        panic_retries += 1;
                        continue;
                    }
                    break;
                }
                // 落盘期间又有新切换：原子消费挂起标记后补一轮（写最新
                // 快照）；失败路径同样消费，避免连续失败时无限循环。
                // fetch_update 原子完成"检查+转移"：PENDING→RUNNING
                // 继续，RUNNING→IDLE 结束——不存在"释放在途标记后挂起
                // 标记悬空"的竞态窗口（desktop 多线程）。转移用
                // [`ui::persist_on_round_done`]（与 panic 重试路径共用）。
                let prev =
                    PERSIST_STATE.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                        Some(persist_on_round_done(state))
                    });
                if prev == Ok(PERSIST_PENDING) {
                    continue;
                }
                break;
            }
        });
    };

    rsx! {
        div { style: "padding:16px;",
            if let Some((err, retained)) = TOOL_STATE_LOAD_ERROR.read().as_ref() {
                // 恢复失败（Error）：禁用清单无法恢复、全部工具活跃，影响面
                // 大；文案依据恢复失败时刻是否已有本地保留状态区分（进程级
                // 信号存快照，渲染期不再重读内存）。
                ToolStateBanner {
                    message: format!(
                        "{}: {err}",
                        if *retained {
                            t.tool_states_load_failed_local
                        } else {
                            t.tool_states_load_failed
                        },
                    ),
                }
            }
            if let Some(err) = PERSIST_ERROR.read().as_ref() {
                // 持久化失败用 Warning 变体：与恢复失败（Error）影响面/自愈性
                // 不同，两类横幅可能同显，需色系区分（review Minor 1）。
                ToolStateBanner { kind: ToolStateBannerKind::Warning, message: err.clone() }
            }
            ToolPanel { tools, on_toggle }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus_core::VirtualDom;
    use std::sync::atomic::{AtomicBool, Ordering};
    // 挂载同步决策/信号写入函数现仅由测试直接引用（挂载路径已收敛到
    // service::sync_persist_error_on_mount），故在测试模块内导入。
    use ui::{should_sync_persist_error, sync_persist_error};

    // dioxus 0.7 在 VirtualDom 渲染内部用 catch_unwind 包裹组件闭包，闭包内
    // panic 不传播到测试框架（frontend_test.rs / personal_center.rs 同模式）
    // ——用 AtomicBool 作 side channel 将结果传出 VirtualDom 后再断言。
    #[test]
    fn sync_persist_error_clears_stale_banner_when_no_marker() {
        // PERSIST_ERROR 是进程级 GlobalSignal，跨测试存活且被 service.rs
        // 的挂载同步测试同时读写（review 建议 2）：并行执行时交叉污染
        // 会导致偶发断言失败，须持 SIGNAL_TEST_LOCK 串行。
        let _signal_guard = crate::test_shared::SIGNAL_TEST_LOCK.lock().unwrap();
        static CLEARED: AtomicBool = AtomicBool::new(false);
        static SHOWN: AtomicBool = AtomicBool::new(false);
        static RESTORED: AtomicBool = AtomicBool::new(false);

        let mut dom = VirtualDom::new(|| {
            // 模拟上次挂载残留的陈旧横幅（落盘任务 panic 路径只置位信号、
            // 未写入存储 marker）：无 pending 时必须清空，不得残留
            *PERSIST_ERROR.write() = Some("stale banner".to_string());
            sync_persist_error(None, "prefix");
            if PERSIST_ERROR.read().is_none() {
                CLEARED.store(true, Ordering::SeqCst);
            }
            // 有 pending 标记时正常展示（既有语义不变）
            sync_persist_error(Some("boom".to_string()), "prefix");
            if PERSIST_ERROR.read().as_deref() == Some("prefix: boom") {
                SHOWN.store(true, Ordering::SeqCst);
            }
            // 收尾（review m3）：PERSIST_ERROR 是进程级 GlobalSignal，跨
            // 测试存活。测试不得残留写入值——恢复初始值 None，避免污染后续
            // 断言信号初始状态的测试（cargo test 并行时顺序依赖会偶发失败）。
            // 读写必须在 VirtualDom runtime 内（测试函数体无 runtime）。
            *PERSIST_ERROR.write() = None;
            if PERSIST_ERROR.read().is_none() {
                RESTORED.store(true, Ordering::SeqCst);
            }
            rsx! {
                div {}
            }
        });
        dom.rebuild_in_place();

        assert!(CLEARED.load(Ordering::SeqCst), "无失败标记时须清空陈旧横幅");
        assert!(SHOWN.load(Ordering::SeqCst), "有失败标记时须展示横幅");
        assert!(
            RESTORED.load(Ordering::SeqCst),
            "PERSIST_ERROR must be restored to its initial value"
        );
    }

    #[test]
    fn persist_error_sync_skips_clear_when_task_in_flight_without_marker() {
        // 回归（review Minor 3）：在途落盘任务存在且无失败 marker 时，挂载
        // 同步必须跳过清空——任务可能即将写入失败信号，清空会让横幅先消失
        // 后重现（反向漂移，方向为"宁少提示"）；有 marker 时设置方向安全
        // （宁多提示，任务完成后收敛）。四分支全覆盖：
        assert!(
            should_sync_persist_error(&None, false),
            "空闲且无 marker：正常清空"
        );
        assert!(
            should_sync_persist_error(&Some("boom".to_string()), false),
            "空闲且有 marker：设置提示"
        );
        assert!(
            should_sync_persist_error(&Some("boom".to_string()), true),
            "在途且有 marker：设置提示（宁多提示）"
        );
        assert!(
            !should_sync_persist_error(&None, true),
            "在途且无 marker：跳过清空，由任务完成后收敛"
        );
    }

    #[test]
    fn banner_coexist_when_both_signals_are_set() {
        // TOOL_STATE_LOAD_ERROR / PERSIST_ERROR 是进程级 GlobalSignal，
        // 跨测试存活（review 建议 2）：与 sync_persist_error_clears_*
        // 及 service.rs 挂载同步测试并行执行时交叉污染会导致偶发断言
        // 失败，须持 SIGNAL_TEST_LOCK 串行。
        let _signal_guard = crate::test_shared::SIGNAL_TEST_LOCK.lock().unwrap();
        // 恢复失败（Error）与持久化失败（Warning）可能同时展示（review
        // 建议补测）：两个进程级信号同置时，视图的两个横幅分支都必须渲染
        // 出各自文案——两错误影响面/自愈性不同，任一缺失都会误导用户对
        // 当前工具状态的判断。用 rebuild_to_vec 收集 CreateTextNode 文本
        // 断言 DOM 输出（dioxus 0.7 headless 渲染，无需浏览器）。
        // 信号收尾（review m3）用第二次 rebuild 完成：测试函数体无 dioxus
        // runtime，GlobalSignal 读写必须在组件闭包内；首次渲染置位信号、
        // 第二次渲染恢复 None，避免污染并行测试。
        static FIRST_RUN: AtomicBool = AtomicBool::new(false);
        static RESTORED: AtomicBool = AtomicBool::new(false);

        let mut dom = VirtualDom::new(|| {
            if !FIRST_RUN.swap(true, Ordering::SeqCst) {
                // 首次：置位两信号（模拟 Tools 视图横幅区的两个条件分支）
                *TOOL_STATE_LOAD_ERROR.write() = Some(("store broken".to_string(), false));
                *PERSIST_ERROR.write() = Some("save failed".to_string());
            } else {
                // 收尾：恢复初始值
                *TOOL_STATE_LOAD_ERROR.write() = None;
                *PERSIST_ERROR.write() = None;
                if TOOL_STATE_LOAD_ERROR.read().is_none() && PERSIST_ERROR.read().is_none() {
                    RESTORED.store(true, Ordering::SeqCst);
                }
            }
            rsx! {
                div {
                    if let Some((err, retained)) = TOOL_STATE_LOAD_ERROR.read().as_ref() {
                        ToolStateBanner { message: format!("{}: {err}", if *retained { "local kept" } else { "all active" }) }
                    }
                    if let Some(err) = PERSIST_ERROR.read().as_ref() {
                        ToolStateBanner {
                            kind: ToolStateBannerKind::Warning,
                            message: err.clone(),
                        }
                    }
                }
            }
        });
        let mutations = dom.rebuild_to_vec();
        let texts: Vec<String> = mutations
            .edits
            .iter()
            .filter_map(|m| match m {
                dioxus_core::Mutation::CreateTextNode { value, .. } => Some(value.clone()),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("all active: store broken")),
            "load-error banner missing from rendered output: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("save failed")),
            "persist-error banner missing from rendered output: {texts:?}"
        );
        // 第二次 rebuild：闭包内恢复信号（GlobalSignal 跨测试存活）
        dom.rebuild_in_place();
        assert!(RESTORED.load(Ordering::SeqCst), "signals must be restored");
    }
}
