# rust-agent

AINS Agent Runtime Core：单一 crate、WASM/Native 双编译目标的 Embedded Agent Runtime。

- 设计与分阶段计划见仓库根 `AINS_PLAN.md`；特性语义对齐 `Harness/` 基线，
  对齐清单归档于 `docs/alignment/`。
- 业务逻辑只依赖 trait 抽象（`RuntimeAdapter` / `KvStore` / `VectorIndex` /
  `DocumentStore` / `Tool` / `SkillLoader` / `SkillManage` / `ModelClient`）；
  平台特定实现收敛在 cfg 门控文件（`runtime_native.rs` / `runtime_web.rs`，
  Phase 2 起新增 `memory/kv_native.rs` / `memory/kv_web.rs` 等）。

## 构建与测试

```bash
# Native
cargo build -p rust-agent
cargo test -p rust-agent

# Web (WASM)
cargo build -p rust-agent --target wasm32-unknown-unknown

# 浏览器环境测试（IndexedDB 等，仅 CI 执行）
wasm-pack test --headless --chrome crates/rust-agent
```
