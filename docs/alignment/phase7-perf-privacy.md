# Phase 7.3/7.4/7.5 对齐清单：内存优化 + 冷启动 + 隐私审计（性能/隐私报告）

对齐基线 AINS_PLAN 第十一章 Phase 7（P1）。本轮交付 7.3 内存占用优化
（HNSW/线性索引 int8 量化）、7.4 冷启动优化（向量索引懒加载）、7.5 隐私审计
（本地数据静态加密 + 网络传输加密加固）。7.1/7.2（P0）见
`phase7-sandbox-security.md`。

> **验收状态**：rust-agent / client-api 双 target（native + wasm32）
> `clippy -D warnings` 全绿；rust-agent 全量 native 测试通过
> （lib 320 + core_traits 28 + memory_native 65 + 其余套件），wasm `--tests`
> 编译通过；desktop crate 编译通过。

---

## 一、7.3 内存占用优化（int8 量化）

### 设计

向量索引 RAM 主要来自驻留内存的向量（1536 维 f32 = 6 KiB/条）。采用
**对称每向量 int8 量化**（`memory/vector.rs`）：

- `quantize_i8(v)`：`scale = max|v| / 127`，`data[i] = round(v[i] / scale)`，
  零向量 `scale = 1`。
- **余弦尺度不变性**：`cosine(k·a, k·b) = cosine(a, b)`（k>0），故量化向量的
  余弦 ≈ 原余弦（仅舍入误差），**无需反量化即可在 i8 空间直接检索**。
- `cosine_similarity_i8` 用 i64 累加防溢出；`quantized_score` 按 Metric 分派
  （Cosine 直算 i8，Euclidean 反量化后算）。

### 内存收益（表示层，精确）

| 维度 | f32 表示 | int8 量化表示 | 压缩比 |
|---|---|---|---|
| 1536（personal/document/knowledge） | 6144 B | 1536 B + 4 B(scale) ≈ 1540 B | ≈ 3.99× |
| 768（code） | 3072 B | 768 B + 4 B ≈ 772 B | ≈ 3.98× |

`embeddings` 表（Source Of Truth）仍存**无损 f32**；量化只作用于内存索引
（重建时从 f32 量化入索引），不牺牲可恢复性。

### 双 target 接线

| 端 | 载体 | 量化点 |
|---|---|---|
| Native | `HnswVectorIndex` | `HnswBackend::Cosine` = `Hnsw<'_, i8, DistCosineI8>`（图元素类型 i8，图内存降 4×）；Euclidean 保留 f32（尺度敏感） |
| Web | `LinearVectorIndex` | 内存表 `Vec<(String, QuantizedVector)>`（i8+scale），检索在 i8 空间评分 |

`DistCosineI8::eval = (1 - cosine_i8).max(0.0)`：余弦浮点累加在自距离 cos≈1
时可能略 >1 触发 hnsw_rs `dist >= 0` 断言，钳到 `[0, ∞)` 兜底。

### 精度权衡（单测验证）

- `i8_cosine_matches_f32_cosine_within_tolerance`：i8 余弦与 f32 余弦差 < 0.02。
- `quantized_cosine_is_scale_invariant`：同方向不同模长量化分量一致。
- `zero_vector_quantization_is_safe`、`quantize_dequantize_roundtrip_within_tolerance`、
  `quantized_score_dispatches_by_metric`。

**偏差**：量化引入 ≤ scale/2 的分量舍入误差，Top-K 近邻在极近打分（差 <0.02）
时可能微调序；个人记忆规模下召回影响可忽略，SoT 无损故可随时以 f32 重建。

---

## 二、7.4 冷启动优化（向量索引懒加载）

### 设计：按需物化（Pending → Loaded）

`DefaultVectorIndexManager`（`memory/vector_manager.rs`）以 `IndexSlot` 两态管理：

- `create_index`：仅登记 `Pending(config)`，**零 I/O、零图重建**（冷启动核心）；
- 首次 `search` 命中该 namespace 才从 `embeddings`（SoT）重建为 `Loaded`（懒加载）；
- `add`/`remove` 对 `Pending` slot 为 **no-op**——引擎遵循「SoT 先行」写入
  （先落盘 `embeddings` 再调用索引），惰性重建时自然纳入 / 排除。

### 冷启动收益

进程启动为全部 5 个 namespace `create_index` 从「整表加载 + HNSW 图重建 ×5」
降为「零 I/O」；只有会话中**实际被检索**的 namespace 才付一次重建成本，
未被检索者永不重建。写入路径（remember）不再触发重建——精确对应计划
「首次检索才重建」。

### 关键实现点

- `search(&self)` 的惰性物化需内部可变性且跨 `.await`（读 KvStore + 建图），
  每个 slot 以 `futures::lock::Mutex` 包裹（双 target 通用，可跨 await 持有，
  不触发 `await_holding_lock`）；`add`/`remove`（`&mut self`）用 `get_mut()` 直取。
- **写时维度校验保留**：`add` 对 `Pending` 仍按 `config.dimension` 校验并在
  不符时返回 `Storage` 错误——避免「错误向量写时静默、物化时才被跳过」，
  使 `MemoryEngine` 能及时回滚 SoT（回归测试 `index_add_failure_rolls_back_sot_rows`、
  `dedupe_refresh_index_failure_restores_previous_state` 验证）。
- Trait 由「返回借用的访问器」(`get_index`/`get_index_mut`) 改为方法式
  (`add`/`remove`/`search`)，因借用返回与内部可变性不可兼得；波及面限于
  `vector.rs` trait、`vector_manager.rs`、`engine.rs` 三处调用点与 `core_traits.rs`
  mock（`MemoryEngine::search` / `DocumentStore::search` 签名保持 `&self` 不变）。

### 单测验证

- `create_index_is_lazy_and_first_search_rebuilds_from_sot`：create 后 `!is_loaded`，
  首查后 `is_loaded` 且命中 SoT 遗留向量；未检索的其他 namespace 保持未物化。
- `pending_index_absorbs_writes_without_rebuild`：写入不物化；no-op add 的条目经
  SoT 懒加载后可命中；未物化时维度不符仍写时报错。

---

## 三、7.5 隐私审计（一）本地数据静态加密

### 设计：`EncryptedKvStore` 透明装饰器（`memory/kv_crypto.rs`，双 target）

value 载荷写入底层前用 **ChaCha20-Poly1305（AEAD）** 加密，读取后解密。

| 维度 | 决策 |
|---|---|
| 算法 | ChaCha20-Poly1305（256-bit key，96-bit nonce），纯 Rust、wasm 兼容 |
| nonce | 每次写入随机（`getrandom` CSPRNG；wasm 启用 `js` 后端 = `crypto.getRandomValues`） |
| AAD | 附加认证数据 = 该条目存储 key → 密文被跨 key 搬运时解密失败（防密文搬运） |
| 完整性 | AEAD tag：错误密钥 / 密文篡改 / nonce 篡改 / AAD 不匹配一律解密失败，不返回错误明文 |
| key 明文 | 仅加密 value；key / TTL 元数据明文（前缀列举、删除、TTL 清理依赖之） |
| 信封 | `{"__ains_sealed_v":1,"n":b64(nonce),"c":b64(ct+tag)}`，与明文可区分 |

### 密钥管理（`EncryptionKey`）

- `generate()`：随机 256-bit（首次运行生成，由调用方安全持久化后复用）；
- `from_bytes([u8;32])`：外部管理密钥导入（如系统钥匙串）；
- `from_passphrase(passphrase, salt)`：口令经 **Argon2id** 派生（salt ≥ 16 B，
  非机密但需持久化以复现同一密钥；与 `kv_crypto.rs` 的 `MIN_SALT_LEN` 一致）；
- `Drop` 时 `zeroize` 清零；`Debug` 输出 `EncryptionKey(***)` 不泄露密钥。

### 单测验证（lib 10 + memory_native 2）

- 单元（`kv_crypto`）：seal/unseal 往返、密文不含明文、错误密钥拒绝、AAD 绑定
  storage key、篡改密文拒绝、明文值不被误当密文、口令派生确定性+salt 敏感、
  短 salt 拒绝、随机密钥可用且互不解密、Debug 不泄露。
- 集成（`memory_native`，包真实 redb 表）：往返 + **密文静态形态**（底层存密文
  信封、不含明文）+ list_prefix（key 明文）+ 错误密钥拒绝 + delete；TTL 透传。

**部署接线（原生端）**：应用层已支持从外部 secrets manager / 系统钥匙串注入
`AINS_STORAGE_KEY_HEX`（恰 64 个 hex 字符 = 32 bytes）。设置后 `open_kv_store` 会用该密钥包裹
`RedbKvStore`；错误格式会使启动失败，而不会静默降级为明文。密钥绝不在 `AINS_DATA_DIR`
内自动生成或落盘，避免密钥与数据库同时失窃时的伪保护。未设置时为兼容保留旧的明文行为。

请勿直接对已含明文的数据库设置此变量：先读出旧值，再以同一 key 经装饰器写回，完成一次性迁移。Web 端缺少密钥输入 / 系统钥库的产品面，仍保留为后续工作。key 明文、value 加密为常规静态加密实践；key 命名不应含敏感信息。

---

## 四、7.5 隐私审计（二）网络传输加密加固

### 设计：默认要求 https / 明文告警（`client-api/src/config.rs`）

`ClientConfig::validate()` 在既有 scheme 校验后新增传输安全裁决：

| 目标 | `allow_insecure_http=false`（默认） | `allow_insecure_http=true` |
|---|---|---|
| 本地绑定地址（localhost / 127.0.0.0/8 / ::1 / 0.0.0.0 / :: / IPv6-mapped loopback）明文 http | 放行 | 放行 |
| 非本地主机明文 http | **拒绝**（`ClientError::Config`，默认要求 https） | 放行 + `tracing::warn!` 告警 |
| 任意 https | 放行 | 放行 |

- URL host 提取处理 userinfo 与 IPv6 字面量（`[::1]:port`）；`host_is_local`
  覆盖 localhost / 127.0.0.0/8 / ::1 / 0.0.0.0 / :: / IPv6-mapped loopback，
  且刻意不把 `*.localhost` 视为本地地址。
- `with_allow_insecure_http(true)` 供受信任内网 / 调试知情放行；desktop 经
  `AINS_ALLOW_INSECURE_HTTP=1` 环境变量接入（`app/desktop/src/main.rs`）。
- 向后兼容：仓内既有 `ClientConfig` 用法均为 loopback（含 wiremock 测试 URL）或
  https，不受影响；desktop 默认 `http://127.0.0.1:8080` 为回环，放行。

### 单测验证（client-api config，2 项）

- `test_transport_security_hardening`：远端明文 http 默认拒绝；https 放行；
  opt-in 后远端明文放行；精确回环地址放行，`*.localhost` 不再绕过 HTTPS；IPv6
  非本地拒绝。
- `test_url_host_extraction`：host/userinfo/IPv6/端口/空串提取正确。

---

## 五、依赖新增

| crate | 位置 | 用途 |
|---|---|---|
| `chacha20poly1305` 0.10（`default-features=false, ["alloc"]`） | rust-agent | AEAD 加解密 |
| `argon2` 0.5（`["alloc"]`） | rust-agent | 口令 → 密钥派生 |
| `zeroize` 1 | rust-agent | 密钥内存清零 |
| `getrandom` 0.2（wasm 侧 `["js"]`） | rust-agent | nonce / 随机密钥 CSPRNG |
| `tracing`（workspace） | client-api | 明文传输安全告警 |

均为纯 Rust、双 target 编译通过（wasm32 依赖 getrandom js 后端）。

---

## 六、遗留 / 后续

- Native 端已支持 `AINS_STORAGE_KEY_HEX` 部署接线；Web 端的密钥 UX / 生命周期仍待产品方案。
- int8 量化仅 Cosine namespace（默认全 Cosine）；Euclidean 仍 f32。
- 传输告警在未装 tracing subscriber 的环境（如 desktop）不显式输出；desktop 经
  显式 env opt-in 表达知情，故静默放行可接受。
