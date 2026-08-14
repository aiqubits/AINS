# Phase 7.1/7.2 对齐清单：跨平台 Sandbox + 权限强化 + 二次确认 + Kernel 中断

对齐基线 Harness `sandbox/` + `permissions/`。本轮交付 P0（7.1 平台级
Sandbox + 权限模型强化、7.2 敏感操作二次确认）及 Kernel 真实中断遗留项。
P1（7.3/7.4/7.5）下一批次独立验收。

## 1. 核心重设计：按隔离环境而非"3 个桌面 OS"

Agent 运行在三类**根本不同的隔离环境**，Sandbox 据此重设计：

| 目标 | 构建 | 隔离环境 | 子进程 | Sandbox 策略 | 结论 |
|---|---|---|---|---|---|
| Web | wasm32 | 浏览器（线性内存，无 syscall） | 无 | 环境自带；shell 不注册；网络=fetch 受 CORS+DomainRule+可信代理 | 对齐（环境自带） |
| Desktop Linux | native | 用户进程，全权限 | 有 | 自建：bubblewrap namespace（seccomp profile 待接线） | 部分对齐（真实 namespace 隔离） |
| Desktop macOS | native | 用户进程，全权限 | 有 | 自建：sandbox-exec（Seatbelt/SBPL） | 代码已实现；仅可忠实表示的策略 opt-in，未环境验证 |
| Desktop Windows | native | 用户进程，全权限 | 有 | Job Object（kill-on-close + UI 限制） | 仅无 shell 网络/文件策略时 opt-in；受限令牌/AppContainer 前其余策略拒绝 |
| Mobile Android/iOS | native | OS 应用沙箱（per-UID/SELinux；iOS 禁 exec） | 无 | `MobileSandbox`：环境自带隔离，shell 恒不可用（非 opt-in）；文件/网络由 Layer 1 强制 | 对齐（环境自带） |

关键修正：此前 `service.rs` 仅按 `cfg(wasm32)` vs `not(wasm32)` 二分，Mobile 与
Desktop 同属 native 分支，会错误注册 `ShellCommandTool`（iOS 禁 fork/exec）。本轮
改为 `Platform::current()` 平台驱动：shell + 系统集成仅 Desktop 注册。

## 2. 两层架构

| 层 | 位置 | 作用 | 生效平台 |
|---|---|---|---|
| Layer 1 可移植策略 | `policy/sandbox_policy.rs` | `NetworkPolicy`（域白/黑名单 DomainRule）+ `FilesystemPolicy`（四象限 allow/deny_read/write）；纯 Rust，应用层强制 | 全平台 |
| Layer 2 执行隔离 | `policy/sandbox_{linux,macos,windows}.rs` | `Sandbox::exec_shell` 真实 OS 隔离，把 Layer 1 策略下推进 OS 沙箱 | 仅桌面原生 |

- Layer 1 网络策略接入 `web_fetch`（`tools/network.rs`）：DNS 解析**前**按域名裁决，每一跳重定向复检；判定序 deny 优先→白名单模式默认拒绝→纯黑名单默认放行；`*` 通配全部（全断）。
- Layer 1 四象限接入 `PermissionEngine::evaluate`：敏感路径之后、PathRule 之前；读/写象限由 `is_read_only` 区分；与用户 PathRule 叠加，任一 deny 即拒；**空策略=no-op**（本轮部署默认空，不回归现有行为，限制性策略随操作员配置面注入）。

## 3. Linux bubblewrap（`sandbox_linux.rs`）

| 能力点 | 基线 `sandbox/adapter.py` | AINS | 结论 |
|---|---|---|---|
| 后端 | srt→bwrap 包装 | 直接 bwrap 子进程包装（tokio::process），无 unsafe | 对齐（简化，去 srt 中间层） |
| 探测 | `shutil.which("bwrap")` | `which` crate 查找 bwrap（校验可执行位，跨平台） | 对齐 |
| 隔离 | bwrap namespace | `--unshare-all --die-with-parent --new-session`；`--proc`/`--dev`/`--tmpfs /tmp` | 对齐 |
| 文件系统 | allow/deny_read/write 四象限 | shell 的 cwd 必须同时被 read/write 象限放行，否则拒绝执行；放行后 cwd 绑定可写。绝对非 glob 的 allow_read→`--ro-bind-try`、allow_write→`--bind-try`；glob 与 deny 由 Layer 1 精确执行。运行 shell 所需系统只读路径是明确的运行时例外 | 对齐（shell 侧粗粒度 + Layer 1 精确） |
| 网络 | allowed/denied_domains | 粗粒度：非全断则 `--share-net`；**域名级仅 Layer 1 web_fetch 精确执行**（bwrap 无内建域名过滤） | 偏差（粗粒度，域名级 Layer 1） |
| 输出上限 | — | 读管道时 `take(max).read_to_end`，不先无界收集；外层 `timeout` + `kill_on_drop` 兜底 | 对齐 |
| 降级语义 | `fail_if_unavailable ? raise : 不加包装直跑` | **AINS 默认拒绝**：bwrap 不可用→`capabilities().shell=false`→shell 拒绝执行，**绝不降级直跑** | 有意偏差（更保守，遵循 Phase 3.8 原则） |

## 4. macOS / Windows 平台隔离（代码已实现，opt-in，未在真实环境验证）

**posture**：隔离代码已完整实现，但本机（Linux）无法运行验证。遵循默认拒绝
原则，二者默认 `capabilities().shell=false` 且 `exec_shell` 拒绝（fail-closed，
与原诚实桩行为一致）。macOS 仅在策略可忠实映射到 SBPL 时，才允许运维设置
`AINS_ENABLE_UNVERIFIED_SANDBOX=1` 后启用；含域名 allow/部分 deny 或 glob deny
的 shell 策略会拒绝。Windows 在实现受限令牌/AppContainer 前，任何 shell 网络/
文件策略都拒绝，不能将 Layer 1 的 cwd 求值误当作命令内实际访问的隔离。Linux
(bwrap) 已验证，不受此开关约束。

**编译验证**：全 crate 无法跨目标 `cargo check`（reqwest→rustls→`ring` 的 C
构建需目标 SDK/工具链）。故 FFI 与 API 用法经**隔离 probe** 跨目标校验：
Windows Job Object FFI 经 `cargo check --target x86_64-pc-windows-msvc` 通过；
macOS sandbox-exec 的 which+tokio::process 调用模式经
`cargo check --target x86_64-apple-darwin` 通过；SBPL profile 生成为平台无关
纯函数，在 Linux 单测（3 项）。

| 平台 | 机制 | 实现 | 编译验证 | 运行验证 |
|---|---|---|---|---|
| macOS `sandbox_macos.rs` | `sandbox-exec -p <SBPL>` (Seatbelt) | `which` 探测 + SBPL profile（`macos_sbpl_profile` 由四象限/网络推导）+ tokio::process 包装 + 管道限流 + timeout；无 unsafe | darwin target ✅ | 未验证（无 macOS 环境） |
| Windows `sandbox_windows.rs` | Job Object | `CreateJobObjectW` + `SetInformationJobObject`（KILL_ON_JOB_CLOSE + DIE_ON_UNHANDLED_EXCEPTION + UI 限制拒剪贴板/桌面/句柄/全局原子）+ `AssignProcessToJobObject` + kill-on-close；`windows-sys` FFI | windows-msvc target ✅ | 未验证（无 Windows 环境） |

**降级/未决**：macOS SBPL 若语法错或策略不可忠实表达则 fail-closed（命令不
运行）；Windows 受限令牌（`CreateRestrictedToken`）/AppContainer 是启用配置化
shell 网络/文件策略的前置条件，当前 Job Object 仅提供进程容器 + 资源/UI 限制 +
kill-on-close。Layer 1 仍精确约束文件工具与 `web_fetch`，但不能从任意 shell
文本恢复实际访问，因此不会被当作 shell 隔离的替代品。

## 4b. Mobile Android/iOS 平台（`sandbox_mobile.rs`，FFI-free 语义层）

**与 Desktop 根本不同**：移动 OS 已将整个应用置于强隔离沙箱（Android：
per-UID + SELinux 域 + zygote seccomp；iOS：容器沙箱 + 代码签名，且内核禁
 fork/exec 任意二进制）。因此无可供我们“构建隔离”的子进程——OS 提供的
隔离强于进程内自建。

| 能力点 | AINS | 结论 |
|---|---|---|
| shell | `capabilities().shell=false` **恒不可用**（非 opt-in）；exec_shell 恒返回 `Unavailable` | 与 mac/Win 关键差异：无可启用的沙箱化 shell（iOS 硬失败/Android 无沙箱直跑均违背默认拒绝） |
| 文件/网络 | 由 Layer 1（PermissionEngine 四象限 + web_fetch DomainRule）强制 | 跨平台一致 |
| 实现 | FFI-free（无平台 API，结构等价 NoopSandbox）；`name()` 按 cfg 区分 android/ios | 编译由构造保证 |
| 编译验证 | async_trait + cfg-const 模式经 probe `cargo check --target aarch64-linux-android` + `aarch64-apple-ios` 通过 | ✅ |

**未验证/前瞻**：整体移动 agent 集成（OS 沙箱 + Layer 1 在真机的实际行为）尚未
真机验证，且 `app/mobile` 目前未接入 rust-agent（本适配为前瞻基础设施，接入后即生效）。
Android isolated-process 桥接为可能的后续增强（需 Java/Binder 层，非 Rust 直接可到）。

## 5. 7.2 敏感操作二次确认（`permission_engine.rs`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 分层 | default 模式 requires_confirmation | 敏感操作即使 full_auto / 会话级"总是允许"下亦强制确认（类比 exit_plan_mode 不可绕过） | AINS 扩展 |
| 分类 | — | 隐私工具（clipboard/screenshot）+ 破坏性命令（`rm -rf`/`sudo`/`mkfs`/`dd if=`/`shutdown`/`chmod -r`/`git push --force`/fork bomb/`curl\|sh` 等，fnmatch 小写匹配） | AINS 扩展 |
| 裁决位置 | — | 配置级 allowed_tools（静态授权）之后、会话放行/full_auto 之前；仅提升确认要求，从不放宽 | 对齐（不削弱 plan 只读保证） |
| UI | permission_prompt | 复用 6.11 `PermissionDialog` 桥接，无新组件 | 对齐 |

## 6. Kernel 真实中断（遗留项回收）

| 能力点 | 此前（6.11 偏差） | 本轮 | 结论 |
|---|---|---|---|
| 机制 | on_interrupt 仅复位 UI | `Arc<AtomicBool>` 中断标志 + `AgentKernel::interrupt_handle()`；不经事件通道（避免与 Idle 事件消费竞争） | 落地 |
| 检查点 | 无 | 模型 turn 前（Querying）、工具批分发前（ExecutingTools）协作式 check-and-clear（`swap`） | 落地（边界协作式） |
| FSM | ExecutingTools 仅→Querying/Failed | 新增 `ExecutingTools→Idle` 边（中断出边） | 落地 |
| 落定 | — | 发 `StreamEvent::Status("Query interrupted by user.")`，回 Idle 保活；悬空 tool_use 下轮 sanitize 丢弃（与 UI 镜像 flush_pending_as_interrupted 一致） | 对齐 |
| 防陈旧 | — | on_interrupt 置标志（`store(true)`）；Kernel 在边界 check-and-clear 消费一次；UI on_send 不再清除旧标志，避免 Stop→Send 竞态 | 落地 |

偏差：中断为**边界协作式**（非抢占任意 syscall / 非杀 in-flight 子进程）；
mid-model-stream 与 mid-tool 的即时抢占需并发化改造，留待后续。

## 7. 结构约定

- cfg 门控适配文件清单（AINS_PLAN §"仅允许在以下 cfg 门控适配文件"）新增
  `policy/sandbox_{linux,macos,windows}.rs`；`policy/sandbox.rs` 与
  `policy/sandbox_policy.rs` 保持纯 trait/策略层（无平台 API）。
- `default_sandbox(policy)` 按 `Platform::current()` + `target_os` 选实现：
  Linux→bubblewrap、macOS→sandbox-exec、Windows→Job Object、Android/iOS→`MobileSandbox`（
  环境自带隔离、shell 恒不可用）、Web(WASM)/其它→`NoopSandbox`。

## 8. 里程碑验收

- [x] `cargo clippy -p rust-agent -p ui -p web -p desktop -p mobile -p i18n --all-targets -- -D warnings`
- [x] `cargo clippy -p rust-agent -p web --target wasm32-unknown-unknown -- -D warnings`
- [x] `cargo check -p web -p rust-agent --target wasm32-unknown-unknown`（WASM 兼容）
- [x] `cargo test`：rust-agent lib 302 + kernel_loop 28（含中断回归）+ skills_store 25 +
      新增 sandbox_policy（含 3 项 SBPL profile 单测）/sandbox_linux/permission_engine 单测，零回归
      （全量 725 通过 / 0 失败）
- [x] Linux bwrap 集成测试：安装 bwrap 时运行真实隔离，未安装则 `eprintln!` 跳过
- [x] macOS/Windows 平台隔离代码实现（§4）；FFI/API 用法经隔离 probe 跨目标编译验证
      （`x86_64-pc-windows-msvc` + `x86_64-apple-darwin` 均通过）；默认 opt-in 关、fail-closed
- [x] Mobile Android/iOS 平台隔离代码实现（§4b，`MobileSandbox` FFI-free，shell 恒不可用）；
      async_trait + cfg-const 模式经 probe `cargo check --target aarch64-linux-android` +
      `aarch64-apple-ios` 均通过
- [ ] macOS/Windows/Android/iOS 真实环境运行验证（需对应主机；mac/Win 需
      `AINS_ENABLE_UNVERIFIED_SANDBOX=1`；mobile 需先将 rust-agent 接入 app/mobile）：未验证
- [ ] wasm-pack CI 浏览器测试（web_tools 等 NetworkPolicy 签名更新）：CI-only，推送后确认
- 备注：桌面 shell 隔离按平台不同，**非全平台/全发行版通用**：
  - **Linux**：需安装外部二进制 `bubblewrap`（Debian/Ubuntu：`apt-get install bubblewrap`；
    Fedora/RHEL：`dnf install bubblewrap`；Arch：`pacman -S bubblewrap`；Alpine：`apk add bubblewrap`）；
    缺失则 `capabilities().shell=false`→shell fail-closed 拒绝。
  - **macOS / Windows**：本阶段为诚实桩，shell 一律拒绝，**安装任何东西都不会启用**；
    真实隔离（macOS `sandbox-exec` profile、Windows Job Object + 受限令牌）均为
    **OS 内建机制、无需安装**，实现完成后 `capabilities().shell` 自动转 true，
    留待有对应环境验证时补齐。

## 9. Review 修复记录（2026-08-01）

Code Review（Phase 7 全量 diff）发现的缺陷修复：

1. **Shell 超时/取消后进程树未终止（原 Blocking）**：`sandbox_{linux,macos}.rs` 的
   timeout 触发仅 `kill_on_drop` 杀包装进程（bwrap/sandbox-exec），`sh -c` 及孙进程
   在沙箱内继续运行。修复：`ShellRequest` 新增协作式 `cancel` 标志；Unix 后端 spawn
   时 `process_group(0)`，超时/取消分支统一 `kill(-pgid, SIGKILL)` 杀整个进程组再
   `wait` 收割（Windows 侧 kill-on-close 已覆盖，另补显式终止路径）。hooks 外层重复
   timeout 移除（竞态窗口会绕过后端进程树终止）。新增真实 bwrap 集成测试验证超时与
   取消后 marker 文件不再增长。
2. **文件系统四象限根目录 pattern `"/"` 被静默丢弃（fail-open）**：`trim_end_matches('/')`
   把 `"/"` 变空串导致 `deny_read=["/"]` 失效退化为全放行。修复：全斜杠 pattern 命中
   一切路径，补 fail-closed 回归测试。
3. **`(allow mach-lookup)` 无参写法未经验证（hypothesis）**：列为 macOS 真机验证清单项。

## 10. Review 修复记录（2026-08-01 二轮）

第二轮 Code Review（含首轮修复产物）发现的缺陷修复：

1. **Shell 输出超限后 `exec_shell` 无限挂起（首轮修复引入的回归）**：首轮把整体
   `tokio::time::timeout` 改为读取阶段 select 后，命令输出超过 `max_output_bytes`
   写端阻塞挂起时，`child.wait()` 不再有超时兑底。修复：超时/取消 future 跨阶段
   pin 复用，`wait` 阶段（阶段 2）继续与超时/取消 select；新增真实 bwrap 回归测试
   （`head -c 100000 /dev/zero` 输出超限，超时后必须返回且不挂起）。
2. **tasks 注册插入非原子竞态**：`spawn_shell` 的 tasks/handles/cancels 分两次加锁
   插入，`stop` 在间隙调用会取不到 cancel/handle 而无法取消任务（任务继续运行但
   状态显示 Killed）。修复：单次锁内原子插入全部注册表项。
3. **tasks `stop` 终态覆盖竞态**：stop 在 await 后无条件置 Killed，可能覆盖自然完成
   的 Completed/Failed。修复：任务闭包感知 cancel 标志（取消导致的 timed_out 直接
   写 Killed），stop 仅对仍 Running 的任务置 Killed——两路径无论锁顺序最终态一致。

## 11. Review 修复记录（2026-08-01 三轮）

第三轮 Code Review 发现的缺陷修复：

1. **macOS SBPL profile 把 glob 规则按字面量写入（fail-open 方向）**：SBPL `subpath`
   是字面量前缀匹配，不支持 glob；allow/deny 中的 glob 条目被原样写入 profile 后
   静默失效——限制性规则失效 = 放宽限制。修复：新增 `sbpl_bindable` 过滤（与 Linux
   `bindable_path` 同口径：仅绝对、非 glob 条目进入 profile），glob 规则由 Layer 1
   四象限精确执行；补回归测试验证 glob/相对条目被跳过。
2. 安全提示注释增强（无行为变化）：`AgentDefinition::tools=None` 默认全允许的风险
   提示；`PluginMcpTransport::Stdio` 的来源信任提示；`EncryptedKvStore` 旧明文
   迁移指引。

## 12. Review 修复记录（2026-08-01 四轮，评审修复验收）

第四轮 Code Review（针对前三轮产物 + 全量改动）的修复与验收：

1. **`ShellOutcome` 新增 `cancelled` 字段（超时/取消可区分）**：此前 `timed_out=true`
   同时覆盖超时与协作式取消，调用方无法从结果区分原因（tasks 靠外部 `cancel.load`
   绕过）。修复：三个平台后端在阶段 1/2 的终止分支分别置 `cancelled`（超时=false、
   取消标志=true）；`shell_outcome_to_result` 对取消给出独立消息
   （"Command was cancelled by the user."）并输出 `cancelled` 元数据；tasks 回写
   改用 `outcome.cancelled` 判定 Killed（不再依赖标志轮询）。补取消/超时消息区分
   断言与真实 bwrap 集成测试的 `cancelled` 断言。
2. **输出上限改为共享预算（合并输出 ≤ `max_output_bytes`）**：历史实现 stdout/stderr
   各自 `take(cap)`，合并可达 2×cap，与 `ShellRequest` 文档（"合并捕获的字节上限"）
   不符。修复：新增 `sandbox::read_bounded`（CAS 预留 + EOF/短读/读错误归还），
   三个后端阶段 1 改为共享预算并行读；补双流合并上限、零预算、EOF 归还三组单测。
3. **`sandbox_windows.rs` 进程句柄缺失时 fail-closed**：`raw_handle()` 为 None 时
   原实现静默跳过 Job 分配（子进程无隔离运行，fail-open 方向）。修复：改为
   match 分支，None 时 kill 并拒绝执行（与 AssignProcessToJobObject 失败路径同口径）。
4. **`BackgroundTaskManager::spawn_shell` 显式接收 cwd**：原实现构造时捕获进程
   cwd，与 `shell_command` 的 `ToolContext.cwd`（工作区根）可能分叉。修复：
   `spawn_shell(desc, cmd, cwd)` 由 `BackgroundTaskTool` 透传 `ctx.cwd`（与 shell
   工具同一口径），构造不再捕获 cwd；补 cwd 透传单测。
5. 注释澄清（无行为变化）：`SENSITIVE_COMMAND_PATTERNS` 明确标注启发式可绕过、
   非安全边界（Layer 2 沙箱才是 shell 安全边界）；`HNSW_CACHE_VERSION` 澄清缓存
   仅存元数据、表示层变更不要求 bump。

验收：`cargo check`/`clippy --all-targets -D warnings`（native + wasm32）零告警；
`cargo test -p rust-agent` 全量通过（lib 379 + 集成 169）。

## 13. Review 修复记录（2026-08-01 五轮，复审核）

第五轮复审核（逐条重审第四轮修复 + 全量改动）的补充修复：

1. **`kill_process_group` 无效 pid 防御（新发现，安全性）**：`child_pid==0` 时
   `kill(0, SIGKILL)` 按 POSIX 语义向**调用者所在进程组**发信号（误杀 agent
   自身进程组）；`u32::MAX as i32 = -1` 会回绕命中 PID 1（依赖内核保护）；
   `-(i32::MIN)` 负号溢出 debug panic。修复：`child_pid == 0 ||
   child_pid > i32::MAX` 时 no-op（真实 pid 恒在有效范围）。测试替换为
   `kill_zero_pid_is_noop`（若守卫缺失，kill(0) 会直接杀死测试进程本身，
   cargo test 必然失败）、`kill_oversize_pid_is_noop`（溢出 panic 回归）、
   `kill_unknown_pgid_is_ignored`（ESRCH 容忍）。
2. **`sandbox_windows.rs` 顶层 `use std::path::PathBuf` 丢失（修复引入的
   回归）**：第四轮修改过程中误删该导入（Windows 文件不参与 Linux 编译，
   cargo check 无法发现）；经隔离 probe 跨目标编译（`x86_64-pc-windows-msvc`）
   暴露并恢复。教训：cfg 门控文件的改动必须经隔离 probe 跨目标 check。
3. **隔离 probe 复验**：`sandbox_windows.rs`（raw_handle match / read_abort /
   cancelled / 共享预算）经 `x86_64-pc-windows-msvc` probe 编译通过；
   `sandbox_macos.rs` 同套改动经 `x86_64-apple-darwin` probe 编译通过。

验收：`cargo test -p rust-agent` 全量通过（lib 381 + 集成 169 = 550）；
native/wasm32 clippy 零告警；Windows/macOS 沙箱代码经隔离 probe 跨目标
编译验证。

## 14. Review 修复记录（2026-08-01 六轮，上轮阻塞项验收）

第六轮 Code Review（针对五轮产物 + 全量改动的复审核）的补充修复：

1. **`shell_command` cwd 可写绑定逃逸（上轮 Blocking）**：`cwd` 输入字段
   直接成为沙箱内的**可写绑定点**（bwrap `--bind <cwd> <cwd>` / SBPL 写子树 /
   Windows current_dir）；绝对路径如 `"/"` 会让整个文件系统在沙箱内可写
   （`--bind / /` 后执行覆盖先前只读绑定），"沙箱内 `rm -rf /` 只破坏沙箱
   视图"的假设失效。修复：工具层对 cwd 执行 `validate_sandbox_path`
   （必须落在工作区内，拒绝 `/`、工作区外绝对路径与 `..` 逃逸）；三个平台
   后端追加根目录纵深防御（`cwd.parent() == None` 即根，拒绝执行），保护
   绕过工具层的直接调用方。补工具层回归（拒绝/放行双向断言）与后端
   root-cwd 守卫测试。
2. **只读快路径绕过敏感门控（一致性缺口）**：`evaluate` 步骤 7（只读恒放行）
   无 sensitive 检查——若未来注册 `is_read_only()=true` 的隐私工具将绕过
   强制确认（当前 clipboard/screenshot 恒 false 受测试保护）。修复：只读
   快路径前追加同一敏感门控；补 default/plan 模式只读敏感工具确认回归。
3. **bwrap 存在但无法创建 namespace 的能力探测误导**：非 setuid 安装 +
   系统禁用 unprivileged user namespaces 时，`capabilities().shell=true`
   导致命令运行时才失败（而非明确拒绝）。修复：构造探测追加冒烟验证
   （`bwrap --unshare-all --die-with-parent /bin/true` 需 0 退出），失败视为
   不可用（fail-closed）；补冒烟判定单测（零/非零退出脚本 + 不存在文件）。
4. **`KvMailbox::post` 名称未校验**：sender/recipient 拼入存储 key 前缀，
   含 `/` 会破坏前缀结构（跨收件箱可见）、空名产生悬空前缀。修复：
   post 校验非空且不含 `/`（`MemoryError::Storage`）；补拒绝/放行回归测试。
5. 文档同步：`phase7-perf-privacy.md` 的 Argon2 salt 描述 8 B → 16 B
   （与 `kv_crypto.rs` 的 `MIN_SALT_LEN` 一致）。

**评估为不修项**：Windows `cmd /C` 引号拼接（`/S` 修饰符行为未在真实环境
验证，改动有风险；列入 Windows 真机验证清单）；`supports_shell()` 便捷
方法（现有调用方均正确使用 `capabilities().shell` 单项检查，无实际收益）；
后台任务 per-task timeout（7 天兜底 + stop 取消已覆盖设计意图）。

验收：`cargo test -p rust-agent` 全量通过（lib 386 + 集成 169 = 555）；
native/wasm32 `clippy --all-targets -D warnings` 零告警（含 `--tests`）；
`web`（wasm32）/`desktop`/`client-api` 编译与测试全绿。

## 15. Review 修复记录（2026-08-01 七轮，复审发现）

第七轮复审核（针对六轮产物 + 全量改动的复审）的补充修复：

1. **`validate_sandbox_path` 相对 cwd fail-open（新发现，安全性）**：词法
   规范化后为空（如 `"."` / `".."` 相对路径）的 cwd 使边界退化为空前缀
   （`starts_with("")` 恒 true）——绝对路径逃逸（如 `/etc/passwd`）被放行。
   触发面：`bridge_cwd()` native 分支在 `current_dir()` 失败时回退 `"."`
   （罕见但存在），以及任何未来宿主以相对 cwd 调用。B1 修复（shell cwd 校验）
   在该场景下随之失效。修复：规范化后为空即 fail-closed 拒绝（相对 cwd 的
   调用方应先用工作区锚定绝对化）；补回归测试（`"."`/`".."`/`"./"` 下
   绝对与相对路径均拒绝，绝对 cwd 行为不变）。

**评估为不修项**：`personalization` 的 `fact_patterns()` 每次提取重建 8 个
正则（会话后调用频率低，µs 级开销，LazyLock 优化收益可忽略）；
`HookExecutor` 无宿主注入 `with_sandbox`（command hook 恒 NoopSandbox
拒绝——安全默认，随宿主接线面推进）。

验收：`cargo test -p rust-agent` 全量通过（lib 387 + 集成 169 = 556）；
native/wasm32 `clippy --all-targets -D warnings` 零告警（含 `--tests`）。

## 16. Review 修复记录（2026-08-01 八轮，上轮问题逐项验收）

第八轮（上轮报告逐项审核 + 修复验收）的补充修复：

1. **B2 端到端回归测试补齐**：`ShellCommandTool` 在 `ToolContext.cwd` 为相对
   路径（如 `"."`，`bridge_cwd()` current_dir 失败回退）时 fail-closed 拒绝
   且不触碰沙箱——端到端断言（默认 cwd、显式绝对逃逸、显式相对三种输入均
   拒绝；CwdRecordingSandbox 保持未被调用）。
2. **`BackgroundTaskManager::spawn_shell` cwd 契约防御（新发现）**：
   `background_task` 工具不经 `ShellCommandTool` 的 cwd 校验，相对 cwd 会
   直接传给沙箱后端产生含糊的 bind/chdir 语义（宿主进程 cwd 解析 + 沙箱内
   相对解析）。修复：spawn_shell 拒绝相对或根目录 cwd（`InvalidInput`，与
   empty command 同口径不产生任务记录）；补拒绝/放行回归测试；既有
   `tool_dispatch_run_list_output` 测试的 cwd 同步为绝对路径（契约要求）。
3. **`personalization::fact_patterns` 正则 LazyLock 缓存**：避免每次会话后
   提取重复编译 8 个正则（与 commands 模块同风格）。注意：LazyLock 含内部
   可变性，静态声明必须为数组本体而非借用临时值（E0492）。

**评估为不修项**：`ShellCommandTool` 相对 ctx.cwd 自动绝对化（触发面仅
current_dir 失败回退，B2 修复已 fail-closed 兜底，自动绝对化引入平台分支
复杂度无收益）；`HookExecutor` with_sandbox 宿主注入（安全默认，待接线面）；
`bridge_cwd` 相对回退信号化（B2 已兜底安全）。

验收：`cargo test -p rust-agent` 全量通过（lib 389 + 集成 169 = 558）；
native/wasm32 `clippy --all-targets -D warnings` 零告警（含 `--tests`）；
`web`（wasm32）/`desktop`/`client-api` 编译与测试全绿。

## 17. Review 修复记录（2026-08-01 九轮，Phase 7 全量复核）

本轮逐项复核并修复以下问题：

1. **文件工具越过 Sandbox 四象限**：应用层 native 默认策略锚定到工作区
   cwd；`edit_file` 的隐式读取、`glob`/`grep` 的递归根目录均额外经过
   read quadrant 校验，存在 deny 规则时递归操作 fail-closed。
2. **后台任务绕过 shell 权限**：`background_task/run` 复用
   `shell_command` 的权限身份与 cwd 检查；任务 wait/stop 使用共享完成通知，
   并发调用不再消费同一个 JoinHandle 或提前返回。
3. **中断无法唤醒静默模型流**：模型流增加 100ms 中断轮询；Stop→Send 不再由
   UI 清除 kernel 标志，避免旧查询取消竞态；补永久 pending 模型流回归测试。
4. **敏感环境变量泄露**：Linux/macOS/Windows 沙箱启动前清空宿主环境，仅注入
   最小 PATH/HOME/TMP/LANG，避免 `AINS_API_TOKEN` 进入 shell 或 hook。
5. **传输 URL 校验不一致**：统一使用 `reqwest::Url` 解析，native 重定向逐跳
   复检 scheme/host/userinfo/明文策略；IPv6（含 mapped IPv4）回环识别按解析后
   地址判断。WASM 端由浏览器 Fetch 管理重定向，初始 origin 仍执行同一校验。
6. **默认扩展权限过宽**：插件默认禁用、Swarm 子代理默认空工具白名单；Skill
   命令 frontmatter 的 `allowed-tools` 正确映射为运行时工具门控；偏好单例项
   采用最新值并串行化本进程更新。
7. **工作区根目录误授权**：native 初始化拒绝以文件系统根目录作为 Agent cwd，
   防止默认文件策略将整台主机视为工作区。
8. **中止命令丢失部分输出**：Linux/macOS/Windows 沙箱在 timeout/cancel 路径
   复用有界 stdout/stderr 缓冲，保留 `ShellCommandTool` 的 Partial output 诊断。
9. **Windows 策略大小写旁路**：FilesystemPolicy 在 Windows 上按大小写不敏感
   规则匹配，与实际文件系统语义一致。

仍保留为后续部署/平台验收项：`EncryptedKvStore` 尚未接入应用层密钥生命周期；
Linux shell 的 syscall 级 seccomp 与域名级网络过滤、Windows/macOS 真机隔离及
Windows Job 分配竞态需对应平台验证；后台任务历史记录仍需产品级保留上限设计。

## 18. Review 修复记录（2026-08-02 十轮，Phase 7 合入前复审核）

合入前复审核（针对首轮 Review 报告的 3 项非阻塞 + 1 项观察）的修复与评估：

1. **bwrap 挂载顺序遮蔽 cwd 可写性（修复）**：`build_bwrap_args` 中 cwd 的
   可写 `--bind` 位于 `allow_read` 只读绑定循环**之前**——bwrap 按 argv 顺序
   应用挂载，自定义策略（allow_read 含 cwd 祖先、allow_write 空白名单默认
   放行）下祖先只读绑定会遮蔽 cwd 的可写绑定，沙箱内 cwd 意外只读。修复：
   cwd 可写绑定移至象限循环之后、`--chdir` 之前（恒最后挂载），保证工作
   目录在任何策略下可写；补双向回归测试（祖先只读绑定在 cwd 绑定前 /
   allow_write 显式含 cwd 时最终绑定顺序）。默认部署不受影响（workspace
   同时进 allow_read/allow_write 且 allow_write 绑定本就在后）。
2. **Complete 尾窗中断标志残留污染下一次查询（修复）**：用户在 Complete
   收尾窗口（500ms）内点击停止，回合已自然完成（无工具回答），标志不被
   任何边界消费则残留——下一次查询在 Querying 入口被误中止。修复：回合
   自然结束的回 Idle 路径（无工具回答分支、tool_use 校验错误分支）统一
   消费残留标志（Stop hook 之后消费，覆盖 hook 期间置位）；**不**在 Idle
   入口统一消费——新查询入队后、Querying 检查前置位的标志必须保留（预置
   中断语义，`interrupt_flag_*` 既有回归测试守护）。补 kernel_loop 回归：
   尾窗内置位 → 两轮查询均正常完成、无 QUERY_INTERRUPTED_STATUS。
3. **grep 单文件按路径名打开（评估为不修）**：`grep_one_file` 用
   `File::open(path)` 而非 descriptor-relative 打开。已核实防护链：单文件
   root 经 `resolve_workspace_traversal_root` canonicalize 且验证在工作区
   内；目录遍历 walker 默认不跟随 symlink（条目 `is_file()` 跳过）；另有
   `is_sensitive_file` + `nlink>1` 硬链接防护。剩余 TOCTOU 窗口需本地攻击
   者具备工作区写权限，超出 agent 沙箱威胁模型；openat 重构成本高收益
   极小，不修。
4. **一次不可复现的 lib 测试失败（观察项）**：全量测试首轮 447 中 1 失败，
   连续多轮（含单线程、高负载 4 实例并行 + CPU 压力）无法复现，疑似负载
   下时序敏感测试偶发失败。CI 中若复现需定位具体测试。

验收：`cargo test -p rust-agent` 全量通过（lib 448 + kernel_loop 33 含
尾窗中断回归 + 其余套件）；native/wasm32 `clippy --all-targets -D warnings`
零告警。

## 19. Review 修复记录（2026-08-02 十一轮，逐项审核修复）

本轮对合入前复核报告的逐项审核与修复（含子代理平行审查发现的补充项）：

1. **Native HNSW 同 id 更新无界消耗物理槽位（Blocking，修复）**：注释声称
   「同 id 更新不占用新槽位」，但实现每次更新都 `ids.push` + HNSW 插入新节点
   （旧节点仅墓碑化），`max_slots` 检查对更新豁免导致去重刷新负载下
   `VECTOR_MAX_ENTRIES` 内存上限被击穿、rebuild 永不触发。修复：更新与新增
   同口径——槽位满时无论是否更新均置位 `rebuild_required` 并返回 Err，由
   管理器从 SoT 重建自愈（重建后墓碑清零、槽位回收，不永久失败）；错误消息
   改用 `self.max_slots`（原硬编码常量）。回归测试重写：槽位满时更新必须
   Err + 置位，并验证槽位回收后更新恢复。
2. **swarm `disallowed_tools` 不支持 `*` 通配（Blocking，修复）**：
   `tools:["*"]` + `disallowed_tools:["*"]`（意图全禁）被解释为全部放行
   （fail-open）。修复：disallowed 侧 `*` 按通配处理（=全禁），文档同步；
   补工具门控与 JSON 序列化回归断言。
3. **personalization 自然语言凭据绕过 secret 过滤（接线前提，修复）**：
   `"I prefer my password hunter2"` 等自由文本陈述（无 `=`/`:` 赋值）绕过
   `SECRET_ASSIGNMENT` 进入偏好事实并跨会话持久化。修复：新增宽松模式——
   password/passwd/secret/api key/access token 后跟空格分隔值即拒绝（误杀
   正常陈述可接受）；token 单独出现仅当其值呈密钥样式（≥6 位）时拒绝（避免
   误杀 "token in the file"）。补拒绝/放行双向回归测试。
4. **tasks 无 owner/ACL（评估为接线前提，文档标注）**：`TaskRecord` 无归属
   字段、任务 id 顺序可预测、`list`/`show`/`output` 只读自动放行——当前
   单 agent 进程内使用安全（`ToolContext` 无 agent 身份源，引入 owner 需
   架构级改动），在 `tasks` 模块文档显式标注：swarm 子代理共享同一
   `BackgroundTaskManager` 之前必须补齐归属校验。
5. **`background_task/stop` 权限身份不一致（修复）**：`run` 映射
   `shell_command` 身份而 `stop` 按 `background_task` 身份求值——用户批准
   run（持久化于 shell_command）后 stop 仍需弹窗（"能起不能停"）。修复：
   run + stop 同属 shell_command 授权面；补 stop 复用放行的回归测试。
6. **hnsw_rs `m > 256` 直接 `std::process::exit(1)`（修复）**：配置来自宿主，
   进程不应被第三方库终止。修复：`m` 钳制到 `[4, 256]` 并 `tracing::warn!`
   告警（默认配置 m ≤ 32 不受影响）；补越界构造回归测试。
7. **KvMailbox 消息体无大小上限（修复）**：新增 `MAILBOX_MAX_BODY_BYTES`
   （64 KiB），`post` 超限拒绝（`MemoryError::Storage`）；补超限/边界值测试。
8. **tasks `update` 终态可写（修复）**：Completed/Failed/Killed 后
   progress/note 更新会造成状态回写与展示不一致。修复：终态只读（
   `InvalidInput`）；补 Killed 后拒绝更新的回归测试。
9. **kv_crypto 派生/生成失败路径局部字节未清零（修复）**：getrandom /
   Argon2 失败时显式 `zeroize()` 后丢弃，防部分随机/派生字节残留。
10. **cosine_similarity_i8 无维度校验（修复）**：补 `debug_assert_eq!` 防御
    （zip 静默截断，调用链已有校验，仅防未来误用）。
11. **Web 端排序 `partial_cmp` 确定性（修复）**：改 `total_cmp`（分数经
    ensure_finite 保证无 NaN，total_cmp 让确定性不依赖 NaN 边界语义）。
12. **EncryptedKvStore 使用边界文档化（修复）**：模块文档显式声明**仅限单表
    （单 key 空间）使用**——AAD 不区分表名，多表共享 key 集合时同 key 密文
    可跨表认证通过；补充密文长度泄露（+16B tag）的静态加密已知局限说明。
13. **chacha20poly1305 zeroize 依赖核实（修正）**：确认 zeroize 是 0.10 的
    **硬依赖**（无 feature 开关，`[features]` 为空）而非隐式 feature 合并——
    密钥副本 Drop 清零由库无条件保证，Cargo.toml 注释更正（无行为变化）。
14. **文件工具 descriptor-relative 打开落地（文档同步）**：§18.3 曾评估
    openat 重构"成本高收益极小，不修"；最终实现改为**落地重构**：
    `tools/filesystem.rs` 的 `open_workspace_file` 在 Unix 上以 rustix
    `openat` + `O_NOFOLLOW` 逐段打开（目录描述符相对路径行走，授权检查
    后替换的路径无法经 symlink 逃逸出工作区），配合 `nlink>1` 硬链接
    防护（拒绝可能别名到工作区外的文件）。**Windows 因无等价的
    handle-relative 打开语义（reparse-point 语义无法被 `std::fs` 路径
    API 安全覆盖），文件工具（FileRead/FileWrite/FileEdit/Glob/Grep）
    整体不注册**（`app/web/src/agent/service.rs` `#[cfg(unix)]` 门控，
    fail-closed——不宣传必然失败的操作），非 Unix 的 `open_workspace_file`
    分支同样返回 Policy 拒绝（纵深防御）。这是相对 Phase 6 的 Windows
    功能回归，但符合默认拒绝姿态；Windows 真机验证时需一并确认文件工具
    的"未注册"错误面。**已知例外（§18.3 评估仍成立）**：`grep_one_file`
    无论单文件 root 还是目录遍历条目均仍经 `File::open`（单文件 root 已
    canonicalize + 工作区校验，遍历条目 `is_file()` 已过滤 symlink；
    descriptor-relative 重构成本高，剩余 TOCTOU 需工作区写权限的本地
    攻击者，超出 agent 沙箱威胁模型）；grep 的 nlink>1 硬链接防护独立
    生效，与 read/write/edit 同口径。

**评估为不修项**（各附理由）：tasks 输出 sink 每 chunk spawn（性能，无正确
性影响）；task id 空洞（cosmetic）；stop 超时后 cancel 永置（语义已文档化）；
信箱消息无 TTL（已有 clear_recipient/prune_read 回收）；swarm 跨进程消息 id
碰撞 / 扩展字符 key 注入（hypothesis，当前 KV 后端无路径/URL 语义）；
personalization 注入端 markdown 结构字符（已有 `&<>` 转义 + 定界符外指令
缓解）；env_var 变量名暴露 / IP 永久规则（设计取舍，对齐基线）；commands
expand 输出上限（runtime 输出预算兜底）；`$1` 词字符边界（已知权衡，文档
提示 `${1}` 长形式）；Plugin manifest 大小上限（可信本地输入面）；
InProcessExecutor 无超时（接线时设计，已随 §B3 文档标注强制点）；首次索引
物化 O(N) 次 get（性能优化，留 Phase 8 实测）。

验收：`cargo test -p rust-agent` 全量通过（lib 453 + 集成 177 = 630）；
native/wasm32 `clippy --all-targets -D warnings` 零告警（含 --tests）；
`web`（wasm32）/`desktop` 编译通过。
