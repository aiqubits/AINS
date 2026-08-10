use std::sync::atomic::{AtomicU8, Ordering};

use dioxus::prelude::*;

use crate::{EN, I18nContext};

/// tool_panel.css 的编译期 asset 句柄（review 低风险 5）：横幅与面板样式
/// 由宿主页面经此常量统一引用一次（/tools 视图经 `ToolPanel` 自带 link、
/// agent 会话视图显式引用），避免每个 `ToolStateBanner` 实例重复插入
/// `<link>` 产生重复 DOM。asset! 在 ui crate 上下文解析（assets 位于本
/// crate），跨 crate 宿主无法直接写同一路径的 asset!。
pub const TOOL_PANEL_CSS: Asset = asset!("/assets/styling/tool_panel.css");

/// 工具状态恢复失败提示（进程级全局信号）：存储不可读/格式损坏时禁用
/// 清单无法恢复，全部工具默认活跃（fail-open 回退）。值为（错误信息，
/// 恢复失败时刻是否已有本地需保留的状态）——横幅文案选择必须用恢复时刻
/// 快照，渲染期重读会随会话期间 dirty/禁用集合变化而漂移。由 /tools
/// 挂载与会话装配（initialize）的加载失败置位、任一加载成功清空；两个
/// 视图共享订阅，替代原 AgentBridge 装配时一次性快照（review Minor 1）。
pub static TOOL_STATE_LOAD_ERROR: GlobalSignal<Option<(String, bool)>> = Signal::global(|| None);

/// 持久化失败提示的进程级全局信号（自 tools.rs 提升，review Minor 1）：
/// spawn_forever 落盘任务挂 ROOT scope、不随组件卸载取消，组件作用域
/// Signal 在卸载后被销毁（copy_value 文档：组件 drop 时值随之 drop），
/// 任务完成时写入已销毁的信号会 panic（Signal::set → try_write().unwrap()；
/// web release panic=abort 直接中止 wasm 实例）。全局信号存储随进程存活：
/// 卸载后写入无害，且重挂载继续显示最新状态。落盘任务失败置位、成功
/// 清空，会话视图与 /tools 共享订阅——会话存活期间实时反映落盘结果，
/// 替代 AgentBridge 装配时一次性快照。
pub static PERSIST_ERROR: GlobalSignal<Option<String>> = Signal::global(|| None);

/// 落盘任务合并状态机（自 tools.rs 提升，review Minor 2 修复）：用单个
/// 原子变量表达"空闲 / 在途 / 在途+挂起"三态，替换原双布尔 Signal。
/// desktop 多线程下，后台任务读取挂起标记为 false 与释放在途标记之间若
/// 发生切换，挂起标记会无人消费——最后一次切换不落盘且无提示（重启后
/// 静默回滚，破坏 fail-closed 语义）；wasm 单线程下无此窗口，但状态机双端
/// 语义一致。`fetch_update` 原子完成"检查+转移"，三态转换无窗口：
/// - 空闲（0）：切换经 0→1 转移并 spawn 落盘任务；
/// - 在途（1）：切换置 2 挂起，由在途任务下一轮消费；
/// - 在途+挂起（2）：任务循环内 2→1 继续补一轮，1→0 结束。
///
/// 进程级 static 而非组件 Signal：视图重挂载不丢挂起标记（在途任务
/// 完成后仍会消费），且 /tools 与会话视图共享同一 in-flight 判定
/// （[`persist_task_in_flight`]），两视图挂载同步逻辑对称。
pub const PERSIST_IDLE: u8 = 0;
/// 落盘任务在途。
pub const PERSIST_RUNNING: u8 = 1;
/// 落盘任务在途且有挂起切换，任务需再跑一轮。
pub const PERSIST_PENDING: u8 = 2;

/// 落盘任务合并状态机（进程级）：spawn_forever 任务与 /tools 切换经
/// [`PERSIST_IDLE`] / [`PERSIST_RUNNING`] / [`PERSIST_PENDING`] 三态原子
/// 转移合并快速连点，避免任务风暴。定义与状态机说明见常量处注释。
pub static PERSIST_STATE: AtomicU8 = AtomicU8::new(PERSIST_IDLE);

/// 是否存在在途落盘任务（挂载同步决策用）：状态机非空闲即视为在途。
/// /tools 与会话视图挂载时共用此判定，保证两视图对"在途任务未收敛"的
/// 感知一致（review Minor 2 修复）。
pub fn persist_task_in_flight() -> bool {
    PERSIST_STATE.load(Ordering::SeqCst) != PERSIST_IDLE
}

/// 切换事件的状态转移（纯函数，review 中等问题 4 修复）：空闲时启动落盘
/// 任务（IDLE→RUNNING），其余状态挂起（→PENDING）由在途任务消费。
/// 与 [`PERSIST_STATE`] 的 `fetch_update` 闭包共用同一实现——把状态机
/// 转移逻辑从 web-only 的 spawn_forever 任务中提取为可测纯函数，回归在
/// ui crate 的表驱动测试暴露而非在视图闭包中潜伏。
pub fn persist_on_toggle(state: u8) -> u8 {
    match state {
        PERSIST_IDLE => PERSIST_RUNNING,
        _ => PERSIST_PENDING,
    }
}

/// 任务单轮结束后的状态转移（纯函数，review 中等问题 4 修复）：有挂起
/// 切换则继续补一轮（PENDING→RUNNING），否则回到空闲（→IDLE）。正常
/// 消费路径与 panic 有界重试路径（见 tools.rs）共用——panic 恢复时消费
/// PENDING 即触发补轮，RUNNING 收敛即放弃。
pub fn persist_on_round_done(state: u8) -> u8 {
    match state {
        PERSIST_PENDING => PERSIST_RUNNING,
        _ => PERSIST_IDLE,
    }
}

/// 挂载时是否同步 PERSIST_ERROR 的决策（自 tools.rs 提升，review Minor
/// 2 修复）：在途落盘任务存在且无失败 marker 时跳过同步——清空会误清
/// 任务即将写入的最新结果（任务可能即将失败置位信号）；有 marker 时设置
/// 方向安全（宁多提示，任务完成后收敛最终状态）。返回 true 表示应调用
/// [`sync_persist_error`]。
pub fn should_sync_persist_error(pending: &Option<String>, in_flight: bool) -> bool {
    !in_flight || pending.is_some()
}

/// 挂载时将跨挂载的持久化失败标记同步到进程级信号 [`PERSIST_ERROR`]
/// （review Minor 2 修复）：无标记时同样显式清空——落盘任务 panic 路径
/// 只置位信号而未写存储 marker，若不清理，视图重挂载后会残留陈旧横幅且
/// 无自愈手段；清空让横幅状态与存储标记始终一致。由 /tools 与 agent
/// 会话视图挂载时共同调用（两视图状态源统一）。
pub fn sync_persist_error(pending: Option<String>, message: &str) {
    match pending {
        Some(e) => *PERSIST_ERROR.write() = Some(format!("{message}: {e}")),
        None => *PERSIST_ERROR.write() = None,
    }
}

/// 工具分类（视图模型，宿主从 rust-agent `ToolCategory` 映射；
/// `Meta` 对应 AgentInternal——权限/交互类元工具，`Mcp` 为远程桥接）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategoryView {
    Compute,
    FileSystem,
    System,
    Network,
    Browser,
    Meta,
    Mcp,
}

impl ToolCategoryView {
    fn class(self) -> &'static str {
        match self {
            Self::Compute => "ains-tools__badge--compute",
            Self::FileSystem => "ains-tools__badge--fs",
            Self::System => "ains-tools__badge--system",
            Self::Network => "ains-tools__badge--network",
            Self::Browser => "ains-tools__badge--browser",
            Self::Meta => "ains-tools__badge--meta",
            Self::Mcp => "ains-tools__badge--mcp",
        }
    }
}

/// 工具卡片视图模型（宿主从 `ToolRuntime::api_schemas()` + `category()` 映射）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCardView {
    pub name: String,
    pub description: String,
    pub category: ToolCategoryView,
    /// 工具活跃状态（默认 true）；非活跃工具不会进入智能体上下文。
    pub enabled: bool,
}

/// 工具状态横幅的严重程度变体：恢复失败（Error）与持久化失败（Warning）
/// 影响范围与自愈性不同——恢复失败意味着全部工具回退活跃（影响面大），
/// 持久化失败只是最近一次切换未落盘（下次成功落盘即自愈）——两类错误
/// 可能同时展示，需用不同色系让用户一眼区分，避免误判严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStateBannerKind {
    /// 恢复失败（默认）：存储不可读，禁用清单无法恢复，全部工具活跃。
    Error,
    /// 持久化失败：切换已生效但未落盘，跨重启可能静默回滚。
    Warning,
}

impl ToolStateBannerKind {
    /// 渲染类名：Warning 复用基础样式并叠加修饰类（BEM，与
    /// `.ains-tools__toggle--off` 同模式），Error 使用基础样式。
    fn class(self) -> &'static str {
        match self {
            Self::Error => "ains-tools__load-error",
            Self::Warning => "ains-tools__load-error ains-tools__load-error--warn",
        }
    }
}

/// 工具状态错误横幅（Phase 6.7 扩展）：展示工具活跃状态的恢复失败 /
/// 持久化失败提示，复用 `.ains-tools__load-error` 样式（Warning 变体叠加
/// 修饰类）。样式由宿主页面入口统一加载 tool_panel.css（/tools 视图经
/// ToolPanel 自带 link、agent 会话视图显式加载），本组件不重复插入
/// `<link>`（review 低风险 5：多个横幅同显时避免重复 DOM）。
#[component]
pub fn ToolStateBanner(message: String, kind: Option<ToolStateBannerKind>) -> Element {
    let class = kind.unwrap_or(ToolStateBannerKind::Error).class();
    rsx! {
        div { class, role: "alert", "{message}" }
    }
}

/// 计算切换目标状态（review 建议补测）：优先基于信号中该工具的最新状态
/// 取反，信号中缺失（如注册表变化）时回退到渲染期捕获值。快速连击在重
/// 渲染前仍持有旧闭包，但每次点击都重新读取信号——两次点击都正确翻转，
/// 不吞"关→开"回弹意图。提取为纯函数：表驱动测试即可锁定该行为，无需
/// 模拟 DOM 事件。
fn toggle_target(list: &[ToolCardView], name: &str, fallback_enabled: bool) -> bool {
    list.iter()
        .find(|card| card.name == name)
        .map(|card| !card.enabled)
        .unwrap_or(!fallback_enabled)
}

/// Tool 执行面板（Phase 6.7）：列出当前运行时可用工具（名称 + 描述 +
/// 分类徽标；元工具高亮区分于普通能力工具）。每个工具卡片带活跃开关，
/// 关闭后该工具不再出现在智能体上下文（`ToolRuntime::api_schemas` 过滤）。
#[component]
pub fn ToolPanel(
    tools: ReadSignal<Vec<ToolCardView>>,
    on_toggle: EventHandler<(String, bool)>,
) -> Element {
    let i18n = try_use_context::<I18nContext>();
    let t = i18n.as_ref().map(|c| c.t()).unwrap_or(&EN);

    let category_label = |category: ToolCategoryView| match category {
        ToolCategoryView::Compute => t.tool_cat_compute,
        ToolCategoryView::FileSystem => t.tool_cat_filesystem,
        ToolCategoryView::System => t.tool_cat_system,
        ToolCategoryView::Network => t.tool_cat_network,
        ToolCategoryView::Browser => t.tool_cat_browser,
        ToolCategoryView::Meta => t.tool_cat_meta,
        ToolCategoryView::Mcp => t.tool_cat_mcp,
    };

    let list = tools.read().clone();
    rsx! {
        // 复用统一句柄（review L3）：与 ToolStateBanner 的宿主引用同一
        // asset 常量，避免路径改动需多处同步。
        document::Link { rel: "stylesheet", href: TOOL_PANEL_CSS }
        section { class: "ains-tools",
            header { class: "ains-tools__header",
                h2 { class: "ains-tools__title", {t.tool_panel_title} }
                p { class: "ains-tools__subtitle", {t.tool_panel_subtitle} }
            }
            if list.is_empty() {
                p { class: "ains-tools__empty", {t.tool_panel_empty} }
            }
            div { class: "ains-tools__grid",
                for tool in list {
                    div { key: "{tool.name}", class: "ains-tools__card",
                        div { class: "ains-tools__card-head",
                            code { class: "ains-tools__card-name", "{tool.name}" }
                            span { class: format!("ains-tools__badge {}", tool.category.class()),
                                {category_label(tool.category)}
                            }
                            button {
                                r#type: "button",
                                class: format!(
                                    "ains-tools__toggle{}",
                                    if tool.enabled { "" } else { " ains-tools__toggle--off" },
                                ),
                                "aria-pressed": "{tool.enabled}",
                                "aria-label": if tool.enabled { t.tool_toggle_on } else { t.tool_toggle_off },
                                title: if tool.enabled { t.tool_toggle_on } else { t.tool_toggle_off },
                                onclick: {
                                    // 闭包仅捕获 name 与渲染期 enabled 回退
                                    // （review Nit 2）：避免整份 ToolCardView
                                    // （含 description/category）被每个按钮的
                                    // 闭包冗余持有。
                                    let tool_name = tool.name.clone();
                                    let tool_enabled = tool.enabled;
                                    move |_| {
                                        // 读取信号中该工具的当前状态取反，而非依赖闭包
                                        // 捕获的渲染期值（review Nit 4）：快速连击在重
                                        // 渲染前仍持有旧闭包，两次都发同一目标状态会吞掉
                                        // "关→开"回弹意图；基于最新状态计算则两次点击
                                        // 都正确翻转。计算逻辑为纯函数（[`toggle_target`]），
                                        // 表驱动测试覆盖。
                                        let target =
                                            toggle_target(&tools.read(), &tool_name, tool_enabled);
                                        on_toggle.call((tool_name.clone(), target));
                                    }
                                },
                                span { class: "ains-tools__toggle-knob" }
                            }
                        }
                        p { class: "ains-tools__card-desc", "{tool.description}" }
                        if tool.category == ToolCategoryView::Meta {
                            p { class: "ains-tools__meta-hint", "{t.tool_meta_disable_hint}" }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_target_uses_latest_list_state_for_rapid_clicks() {
        // 快速连击（review 建议补测）：重渲染前两次点击持有同一旧闭包，但
        // 每次点击都重新读取信号最新状态取反——第一次停用（true→false），
        // 第二次重新启用（false→true），不吞"关→开"回弹意图。
        let card = ToolCardView {
            name: "date".into(),
            description: String::new(),
            category: ToolCategoryView::Compute,
            enabled: true,
        };
        // 初始活跃：点击 → 目标为停用
        assert!(!toggle_target(std::slice::from_ref(&card), "date", true));
        // 信号已更新为停用（重渲染前第二次点击）：目标为重新启用
        let mut updated = card.clone();
        updated.enabled = false;
        assert!(toggle_target(std::slice::from_ref(&updated), "date", true));
        // 信号中工具缺失（注册表变化）：回退渲染期捕获值取反
        assert!(!toggle_target(&[], "ghost", true));
        assert!(toggle_target(&[], "ghost", false));
    }

    #[test]
    fn banner_kind_maps_to_distinct_classes() {
        // 恢复失败（Error）用基础类；持久化失败（Warning）叠加修饰类，
        // 保证两语义共用样式时视觉可区分。
        assert_eq!(ToolStateBannerKind::Error.class(), "ains-tools__load-error");
        assert_eq!(
            ToolStateBannerKind::Warning.class(),
            "ains-tools__load-error ains-tools__load-error--warn"
        );
        // 基础类须为修饰类的子串，避免样式脱离基础布局（间距/圆角等）
        assert!(
            ToolStateBannerKind::Warning
                .class()
                .contains(ToolStateBannerKind::Error.class())
        );
    }

    #[test]
    fn persist_state_machine_transition_table() {
        // 状态机转换表（review 中等问题 4）：切换事件与任务单轮结束事件的
        // 全部转移。与 tools.rs 的 fetch_update 闭包共用同一纯函数——回归
        // 在此暴露，而非在 web-only 的 spawn_forever 任务中潜伏。
        // 切换事件：空闲启动任务，其余状态挂起
        assert_eq!(persist_on_toggle(PERSIST_IDLE), PERSIST_RUNNING);
        assert_eq!(persist_on_toggle(PERSIST_RUNNING), PERSIST_PENDING);
        assert_eq!(persist_on_toggle(PERSIST_PENDING), PERSIST_PENDING);
        // 任务单轮结束：有挂起切换继续补一轮，否则回空闲（含 panic 有界
        // 重试路径的同一转移：PENDING→RUNNING 触发补轮，RUNNING→IDLE 放弃）
        assert_eq!(persist_on_round_done(PERSIST_PENDING), PERSIST_RUNNING);
        assert_eq!(persist_on_round_done(PERSIST_RUNNING), PERSIST_IDLE);
        assert_eq!(persist_on_round_done(PERSIST_IDLE), PERSIST_IDLE);
    }

    #[test]
    fn persist_state_machine_end_to_end_sequence() {
        // 完整序列（review 中等问题 4）：切换→spawn→在途切换挂起→任务消费
        // →再切换→收敛。验证三态转移链路无悬空标记——desktop 多线程下
        // 挂起标记若无人消费，最后一次切换不落盘且无提示（破坏 fail-closed）。
        let state = AtomicU8::new(PERSIST_IDLE);
        // 1. 首次切换：IDLE→RUNNING，spawn 落盘任务
        let prev = state.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |s| {
            Some(persist_on_toggle(s))
        });
        assert_eq!(prev, Ok(PERSIST_IDLE));
        assert_eq!(state.load(Ordering::SeqCst), PERSIST_RUNNING);
        // 2. 在途切换：RUNNING→PENDING，由在途任务下一轮消费
        let prev = state.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |s| {
            Some(persist_on_toggle(s))
        });
        assert_eq!(prev, Ok(PERSIST_RUNNING));
        assert_eq!(state.load(Ordering::SeqCst), PERSIST_PENDING);
        // 3. 任务消费：PENDING→RUNNING，补一轮写最新快照
        let prev = state.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |s| {
            Some(persist_on_round_done(s))
        });
        assert_eq!(prev, Ok(PERSIST_PENDING));
        assert_eq!(state.load(Ordering::SeqCst), PERSIST_RUNNING);
        // 4. 无新切换：RUNNING→IDLE，任务结束（挂起标记被原子消费，无悬空）
        let prev = state.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |s| {
            Some(persist_on_round_done(s))
        });
        assert_eq!(prev, Ok(PERSIST_RUNNING));
        assert_eq!(state.load(Ordering::SeqCst), PERSIST_IDLE);
    }
}
