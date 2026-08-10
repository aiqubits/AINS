<div align="center">

# AINS

<p align="center">
    <strong>AI 原生系统（AI Native System）</strong><br/>
    Rust Native + WASM 双执行环境嵌入式 Agent Runtime · AI 网关 · 全端 UI
</p>

<p align="center">
  <a href="https://github.com/aiqubits/ains/stargazers"><img src="https://img.shields.io/github/stars/aiqubits/ains" alt="Stars"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License MIT"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.92+-orange.svg" alt="Rust 1.92+"></a>
  <img src="https://img.shields.io/badge/Dual%20Runtime-Native%20%2B%20WASM-8A2BE2" alt="Dual Runtime">
  <img src="https://img.shields.io/badge/Safety-Fail--Closed-2EA44F" alt="Fail-Closed">
</p>

<p align="center">
  <a href="#核心特性">核心特性</a> ·
  <a href="#架构总览">架构总览</a> ·
  <a href="#模块地图">模块地图</a> ·
  <a href="#技术栈">技术栈</a> ·
  <a href="#快速开始">快速开始</a> ·
  <a href="#测试与质量">测试与质量</a> ·
  <a href="#文档">文档</a> ·
  <a href="#许可证">许可证</a>
</p>

</div>

---

## 项目简介

AINS 是一个面向企业级场景需求的 AI 原生系统框架，由 **客户端 Agent Runtime**、**服务端 AI 网关与业务后台**、**全端 UI** 三大部分组成：

- **客户端**：`crates/rust-agent` 是 **Rust Native + WASM 双执行环境的 Embedded Agent Runtime**——以库的形式嵌入 Dioxus 应用，单进程运行，不依赖任何系统级服务（无 Server Middleware、无 Daemon）。
- **服务端**：`server/` 提供 AI 能力代理（Chat / Vision / Web Search / Embedding / STT / TTS）、身份认证、租户计费与后台管理，**不修改用户会话与记忆**。
- **全端**：基于 Dioxus 的 Web（WASM）/ Desktop / Mobile 三端应用与共享 UI 组件库、类型化客户端 SDK。

> AINS 不是传统桌面 Agent（Electron/Python），不是依赖系统级服务的 AIOS，也不是固定 DAG 的工作流引擎或纯 Chat Bot——它是完整的 **Agent Loop + 记忆系统 + 工具运行时 + 感知系统** 的嵌入式智能体运行时。

## 核心特性

### 🤖 客户端 Agent Runtime（`crates/rust-agent`）

| 能力域 | 特性 |
|--------|------|
| **Agent Kernel** | 事件驱动 FSM 主循环（delta → tool_use → 权限 → 执行 → tool_result 续轮）、流式工具调用、`max_turns` 上限、中断恢复（`continue_pending`）、协作式中断机制（Kernel 在模型 turn / 工具批边界 check-and-clear） |
| **三层嵌入式记忆** | KV（Native 用 redb / Web 用 IndexedDB，TTL 自动过期）、Vector（Native 集成 HNSW 近似检索，int8 对称量化使索引 RAM ≈ 1/4；Web 纯 Rust 精确余弦 Top-K）、Document（PDF / Text / Code 解析）；另含 memdir 可读记忆库（MEMORY.md）、LLM 对话记忆提取、重要性评分 / 去重合并 / 时间衰减 |
| **Tool Runtime** | 本地工具（文件系统 / Shell / 计算 / 网络 / 剪贴板 / 截图 / 通知）+ **MCP 远程工具**（stdio 与 streamable-http 双传输，单 server 失败不阻断启动）；工具输出 inline/preview 字符预算；Shell 执行路径**必经沙箱层** |
| **权限引擎** | 三态权限（允许 / 询问 / 拒绝）+ PermissionMode（default / plan / full_auto）+ PathRule glob 规则 + 内置敏感路径黑名单（不可覆盖）+ 敏感操作二次确认（破坏性命令 / 隐私工具即使 full_auto 也强制确认） |
| **平台沙箱** | 按平台选实现的真实执行隔离：Linux **bubblewrap**、macOS sandbox-exec、Windows Job Object、Mobile（OS 应用沙箱，shell 恒不可用）；**fail-closed 降级**——沙箱不可用时拒绝执行而非降级直跑 |
| **Skills 系统** | 渐进式加载 + 完整性门控（checksum）、版本化存储（保留 ≤3 + Golden）、评分淘汰、自动回滚与 Agent 主动回滚 |
| **感知系统（Perception）** | Vision（Camera / Screenshot）、Voice（麦克风采集 + Server STT）、File（拖拽解析）三通道，统一产出 `PerceptionOutcome` 注入上下文，与 Kernel 零耦合 |
| **上下文管线** | 分段系统提示（base + Environment + AGENTS.md 逐级发现 + 记忆段 + 技能索引 + 权限模式，各段可开关）、会话持久化（快照原子双写 + sanitize）、**四级上下文压缩**（microcompact → 文本折叠 → 会话记忆压缩 → LLM 摘要，逐级复查阈值 + 熔断） |
| **扩展能力（Phase 7+）** | Slash Commands 命令模板、插件系统（skills / commands / tools / hooks / MCP 五注册面统一注入）、子代理 swarm（进程内 TeammateExecutor + KV 信箱 IPC + 权限上收 lead）、后台任务管理（tokio 后台进程 + 有界输出）、会话个性化（偏好提取 → 规则注入 System Prompt） |
| **安全与隐私** | `EncryptedKvStore`（ChaCha20-Poly1305 AEAD，AAD 绑存储 key，Argon2id 口令派生，Drop 清零）、SSRF 防护（公网校验 + 每跳重定向复检，封堵内网 / 环回 / IPv6 特殊段）、传输加固（非本地主机默认强制 HTTPS）、向量索引懒加载（`create_index` 零 I/O，首次检索才重建） |

### 🏢 服务端 AI 网关与业务后台（`server/`）

| 能力域 | 特性 |
|--------|------|
| **双框架运行时抽象** | `Runtime` trait 统一 Web 框架接口，**Axum / Salvo 之间切换业务代码零修改**（routes / handlers / services 框架无关）；统一请求 / 响应 / 错误模型，中间件抽象（JWT 校验、限流）跨框架复用 |
| **AI Gateway** | Chat / Vision / Web Search / Embedding / STT / TTS 能力代理，SSE 流式转发（带背压与 300s 上游超时）、AES-256-GCM 加密、渠道管理（多模型渠道 + 能力声明） |
| **租户计费体系** | 租户（Tenant）、套餐（Plan）、用户套餐（UserPlan）、计量（Metering）、配额（Quota）、Token 用量（TokenUsage）、支付订单（PaymentOrder）全链路，配合响应模板与多渠道分发（Dispatch） |
| **AutoRouter 读写分离** | 对业务代码**完全透明**的 SeaORM 连接层：`query_*` 路由读库、`execute` 路由写库、`SELECT FOR UPDATE` 强制主库；轮询 / 随机 / 加权负载均衡 + **熔断器**（30s 冷却 + 15s 健康检查自动恢复）+ 降级回写 + 三阶段重试 |
| **Redis 三件套** | 统一缓存（`get_or_insert_with_lock` 防击穿、负缓存防穿透、不可用时**优雅降级 no-op**）、分布式限流（`distributed-ratelimit`，固定窗口，IP + 邮箱双维度、每端点独立配额）、分布式锁（Lua 原子释放防误删、Drop 自动解锁、fail-open / fail-close 双策略） |
| **认证与账号安全** | Argon2id 密码哈希、JWT（HS256，`token_version` 使密码变更后旧令牌立即失效）、Refresh Token 轮转（90 天，每次刷新作废旧 token）、邮箱验证（SMTP）、微信验证码登录（公众号关注者二因子）、密码重置、HTTP 安全头（HSTS / CSP / X-Frame-Options 等） |
| **分布式 ID** | Snowflake 雪花算法（`AtomicI64` + CAS 无锁），worker 经数据库自动注册 / 心跳 / 注销（多节点不重复），JSON 序列化为字符串避免 WASM 端 JS Number 精度丢失 |
| **可靠性** | Panic 捕获返回 500 不崩溃、SIGTERM/SIGINT 优雅关闭、PG + Redis 双连接池、Liveness / Readiness 健康检查、Nginx 反代层限流兜底（认证端点 5 req/min） |

### 🖥️ 全端 UI（`app/`）

- **Dioxus 跨端**：Web（WASM，`app/web`）、桌面（`app/desktop`，经 `#[path]` 复用 Web 桥接与视图）、移动（`app/mobile`），响应式布局适配移动端。
- **共享组件库（`app/ui`）**：Chat 对话视图、权限弹窗 / 权限控制（异步三态询问）、Skills 管理面板、Memory 浏览器、Tool 执行面板、Agent 状态指示器、Slash Command 下拉、Todo 列表、数据表格、代码控制台等。
- **业务视图（`app/web`）**：认证 / Dashboard / 用户管理 / 租户管理 / 套餐 / 订单 / 计量 / 渠道 / Agent 对话 / Skills / Tools / Memory / 个人中心。
- **类型化 SDK（`app/client-api`）**：统一传输层封装（envelope + SSE 流式），提供 chat（vision 图片消息经 chat 携带）/ embed / stt / tts 类型化方法，配套完整集成测试。
- **i18n 国际化**（`crates/i18n`，过程宏翻译，UI 内置语言切换器）。

### 🧩 可插拔 crate 生态

| Crate | 职责 |
|-------|------|
| `ains-runtime` | `Runtime` trait + 统一 Request / Response / HttpError / JWT 校验 / 限流守卫 / 优雅关闭 |
| `ains-axum` / `ains-salvo` | Axum / Salvo 运行时适配器（`AxumRuntime<S>` / `SalvoRuntime<S>`） |
| `distributed-ratelimit` | Redis 分布式限流（固定窗口，`SET NX EX` + `INCR`） |
| `emailserver` | SMTP 邮件发送 |
| `wechat-api` | 微信公众号 SDK（验证码登录 / 消息处理器链 / 加解密 / 平台 API） |
| `i18n` | 国际化过程宏 |

## 架构总览

```
┌─────────────────────────────────────────────────────────────────┐
│  Frontend — Dioxus 多端                                         │
│  app/web (WASM) · app/desktop · app/mobile                      │
│  ├─ app/ui        共享组件库 (chat / 权限弹窗 / skills / memory) │
│  └─ app/client-api 类型化 SDK (chat/embed/stt/tts)             │
├─────────────────────────────────────────────────────────────────┤
│  客户端 Agent Runtime — crates/rust-agent (Native + WASM)        │
│  ├─ Agent Kernel (FSM)      ├─ Memory (KV/Vector/Document)      │
│  ├─ Tool Runtime (本地+MCP) ├─ Skills System                    │
│  ├─ Permission Engine 三态  ├─ Platform Sandbox (4 平台)         │
│  ├─ Perception (Vision/Voice/File) └─ 上下文管线 + 四级压缩      │
│  └─ 扩展: Slash/Plugins/Swarm/Tasks/Personalization             │
├─────────────────────────────────────────────────────────────────┤
│  Nginx 反向代理 (限流 / 安全头 / TLS / 静态资源)                 │
├─────────────────────────────────────────────────────────────────┤
│  Backend — Runtime 抽象层 (Axum ⇄ Salvo 零修改切换)              │
│  ├─ AI Gateway (Chat/Vision/WebSearch/STT/TTS/Embedding)        │
│  ├─ 业务服务: Auth / User / Cache / Lock / Wechat / 验证         │
│  ├─ 计费: Tenant / Plan / Metering / Quota / PaymentOrder       │
│  └─ AutoRouter 主从读写分离 (负载均衡 + 熔断 + 降级回写)          │
├─────────────────────────────────────────────────────────────────┤
│  Persistent: PostgreSQL (主库 / CNP 1主2从) · Redis (缓存+限流+锁)│
└─────────────────────────────────────────────────────────────────┘
```

## 模块地图

```
ains/
├── app/                        # 前端多端应用 (Dioxus)
│   ├── ui/                     #   共享 UI 组件库
│   ├── web/                    #   Web 端 (WASM，含 Agent 装配桥接)
│   ├── desktop/                #   桌面端 (Native，复用 Web 视图)
│   ├── mobile/                 #   移动端
│   └── client-api/             #   类型化客户端 API SDK
├── server/                     # 服务端业务 (框架无关)
│   ├── migrations/             #   SQL 迁移 (001_init.sql)
│   ├── bootstrap/              #   启动引导 (axum.rs / salvo.rs)
│   ├── handlers/               #   HTTP 处理器 (认证/用户/网关/计费/租户)
│   ├── middlewares/            #   认证 / 限流 / Panic 捕获
│   ├── repositories/           #   SeaORM 仓储 (user/tenant/plan/channel...)
│   ├── services/               #   业务服务 (gateway/metering/quota/dispatch...)
│   └── utils/                  #   配置 / JWT / 密码 / Snowflake / AutoRouter
├── crates/                     # 可复用 crate 生态
│   ├── rust-agent/             #   客户端 Agent Runtime 核心 (Native + WASM)
│   ├── ains-runtime/           #   Web 框架运行时抽象 (Runtime trait)
│   ├── ains-axum/  ains-salvo/ #   Axum / Salvo 适配器
│   ├── distributed-ratelimit/  #   Redis 分布式限流
│   ├── emailserver/            #   SMTP 邮件发送
│   ├── wechat-api/             #   微信公众号 SDK
│   └── i18n/                   #   国际化过程宏
├── docs/                       # 架构 / 部署 / 开发文档 + 对齐清单
├── k8s/                        # K8s 部署清单 (CNP 1主2从 PG 集群 / Ingress)
├── .github/workflows/          # CI: 双目标构建 + wasm 浏览器测试 + 安全审计
├── docker-compose.yml          # 全栈编排 (server / web / PG / Redis)
└── config.toml.example         # 配置示例
```

## 技术栈

| 领域 | 选型 |
|------|------|
| 语言 | Rust 1.92+（edition 2024），单一 workspace 14 个成员 crate |
| 后端框架 | **Axum 0.8**（默认）/ **Salvo 0.93**（可选），经 `Runtime` trait 抽象可切换 |
| 前端框架 | **Dioxus 0.7**（Web WASM / Desktop / Mobile 三端复用） |
| 数据库 | PostgreSQL 16（SeaORM；K8s 生产环境为 CloudNativePG 1 主 2 从） |
| 缓存 / 限流 / 锁 | Redis 7 |
| 异步运行时 | Tokio（Native）· wasm-bindgen-futures（Web） |
| 本地存储 | redb（Native）· IndexedDB（Web），经 trait 双后端互斥编译 |
| AI 能力 | Chat / Vision / Web Search / Embedding / STT / TTS（服务端 AI Gateway 代理） |
| 部署 | Docker Compose · Kubernetes（3 副本 + CNP 集群）· Nginx |

## 快速开始

### 环境要求

| 依赖 | 版本要求 | 用途 |
|------|----------|------|
| Rust | 1.92+ | 全仓构建（含 wasm32 目标需 `rustup target add wasm32-unknown-unknown`） |
| Docker | 24.0+ | 本地启动 PostgreSQL / Redis（可选） |
| Dioxus CLI | 0.7.9+ | 前端开发（可选） |

### 构建

```bash
# 全 workspace 构建（native）
cargo build --release

# 双目标验证（native + wasm32，含 WASM 前端依赖）
cargo build --release --target wasm32-unknown-unknown -p rust-agent
```

### 运行服务端（Docker Compose 全栈）

```bash
docker compose up -d          # 启动 postgres + redis + ains-server + ains-web
```

或本地直跑（先按 `config.toml.example` 配置数据库与 Redis 连接）：

```bash
cargo run -p ains-server
```

### 运行前端（开发模式）

```bash
cd app/web && dx serve        # 默认代理后端 http://127.0.0.1:3000/api/
```

### 测试

```bash
cargo test --workspace        # 全仓测试
```

## 测试与质量

- **双目标 CI**（`.github/workflows/ains.yml`）：native + wasm32 双 target 构建，`clippy -D warnings` 零告警，`cargo audit` 安全审计（依赖漏洞阻断），逐 crate 并行流水线。
- **1000+ 自动化测试**：rust-agent 覆盖 Kernel FSM / 记忆 / 工具 / 权限 / 沙箱 / 压缩等数百项单测与集成测试；WASM 端浏览器契约测试（`wasm-pack` + headless Chrome，覆盖 IndexedDB / 记忆 / 工具 / Skills）。
- **双框架集成测试**：Axum 与 Salvo 两套集成测试并行验证（认证 / 限流 / 读写分离熔断 / 计费 / 网关 / 微信回调）。
- **多轮安全回归**：Phase 3 工具系统经 14 轮 code review 修复（SSRF 封堵、路径旁路、敏感信息掩码、资源预算上限等），遵循 **fail-closed 默认拒绝** 原则。

## 文档

| 文档 | 说明 |
|------|------|
| [架构设计](docs/architecture.md) | 运行时抽象 / 读写分离 / 限流 / 中间件 / API 参考 |
| [部署指南](docs/deployment.md) | 本地 / Docker Compose / K8s 三级部署方案 |
| [开发指南](docs/development.md) | 开发环境与工程实践 |

## 许可证

MIT License © 2026 AINS
