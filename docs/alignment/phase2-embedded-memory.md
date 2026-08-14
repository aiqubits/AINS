# Phase 2 对齐清单：Embedded Memory

对齐基线：`Harness/src/harness/memory/`（`memdir.py` / `schema.py` /
`manager.py`）与 `services/`（`memory_extract.py` / `session_memory.py`）
（提交时仓库内版本）。基线无本地 KV / 向量索引 / 文档分块对应物，
这三层按 AINS_PLAN 第四章设计实现，逐条记录为 AINS 扩展。
范围：Phase 2.1–2.10（KvStore 双端、TTL、VectorIndex 双端 + 持久化、
DocumentStore + Parser、记忆管理策略、memdir、durable 抽取 + 会话检查点、
双后端集成测试）。

## 1. KvStore 双端（`memory/kv.rs` + `kv_native.rs` / `kv_web.rs`，基线无对应物）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 存储抽象 | 无（基线记忆直接落文件系统） | `KvStore` trait：get/set/delete/list_prefix/sweep_expired；Native=redb，Web=IndexedDB | **AINS 扩展**（计划 4.1 存储结构） |
| 逻辑表 | 无 | 双端同名五表：`kv/memories/embeddings/documents/hnsw_cache`（redb Table ↔ IndexedDB ObjectStore） | AINS 扩展 |
| 落盘格式 | 明文 Markdown 文件 | bincode `Envelope{expires_at_ms, json}`；载荷为 JSON 字符串（`serde_json::Value` 依赖 `deserialize_any`，bincode 不支持，故内嵌字符串） | **偏差（有意）**：双端复用同一（反）序列化逻辑 |
| 前缀扫描 | 目录遍历 | Native=redb range；Web=`IdbKeyRange` 半开区间，上界取前缀的 UTF-16 code-unit 后继（`prefix_successor`，代理区跳跃时循环内再按 `starts_with` 过滤） | AINS 扩展 |
| 批量前缀删除 | 无 | `delete_prefix`：Native 单写事务范围删除；Web 1 ro 键扫描 + 1 rw 批量删除事务（上界过覆盖需 starts_with 过滤，不可整段 range delete）；不解码 Envelope，过期/损坏行一并清除；`clear_namespace` / `remove_index` 走此路径 | AINS 扩展（review 五轮） |
| 写入持久化 | 文件写入 | Web 写路径（set/delete/惰性过期删除/sweep/delete_prefix）统一等待事务 complete/abort：IndexedDB 请求 onsuccess 先于提交，commit 阶段失败（配额等）不得报告成功（引擎回滚逻辑依赖 set 成功即落盘）；Native redb commit 同步返回 | AINS 扩展（review 五轮修复） |

## 2. TTL 过期清理（`memory/ttl.rs`，基线无对应物；计划 4.1）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 读时检查 | memdir 条目 `ttl_days` 在 scan 时过滤 | KvStore `get` 惰性删除过期条目，`list_prefix` 仅跳过（删除交给 sweep）；memdir scan 同样按 `ttl_days` 过滤（见 §6） | 对齐（memdir 语义）+ AINS 扩展（KV 层） |
| 后台定期清理 | 无 | `spawn_ttl_sweeper<R: RuntimeAdapter>`：每 interval 对全部 stores 执行 `sweep_expired`，失败不中断；`SweeperHandle::stop` 下一周期退出 | AINS 扩展（经 RuntimeAdapter 派发，业务逻辑不触碰 tokio / wasm_bindgen） |

## 3. VectorIndex 双端 + 持久化（`vector*.rs`，基线无对应物；计划 4.2）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| Namespace 隔离 | 无 | 5 个 namespace（personal/document/code/enterprise_knowledge/temporary）各持独立索引实例，检索无跨 namespace 入口 | AINS 扩展 |
| Native 索引 | 无 | hnsw_rs HNSW；分数口径统一“越大越相近”（cosine=1-distance，euclidean=-distance）；图层数固定 1（扁平 NSW）——hnsw_rs 的点仅挂随机顶层、不织入 layer-0 邻接表，多层配置有概率性召回缺失（实测双点索引 ~6% 漏点，单层为 0 且耗时无差异） | AINS 扩展 |
| Web 索引 | 无 | 纯 Rust 精确线性余弦 Top-K；容量上限 10_000（附录 B O(N) 天花板），Native 100_000 | AINS 扩展 |
| 删除 | 无 | hnsw_rs 不支持删除 → tombstone 集合 + 检索时补偿 knn 数量；内部槽位不随 tombstone 释放（去重刷新 re-add 亦消耗槽位），槽位耗尽报错，重启从 embeddings 重建即回收 | **偏差（受限）**：库能力限制 |
| 持久化 | 无 | Source Of Truth = `memories`+`embeddings` 表；hnsw_rs 未公开图拓扑序列化 API → `hnsw_cache` 仅存 meta（版本/维度/度量），启动时从 embeddings 全量重建图 | **偏差（受限）**：计划 4.2 "HNSW 图拓扑落盘" 降级为重建；重建成本 O(N log N)，10 万条上限内可接受 |
| write-through | 无 | 双端先落 KvStore 再更新索引；Web 端崩溃后从 embeddings 重载即恢复 | AINS 扩展（计划 4.2 数据流） |

## 4. DocumentStore + Parser（`document.rs` / `parser.rs`，基线无对应物；计划 4.3）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 分块 | 无 | 512 token × 4 chars/token = 2048 字符预算；纯文本按空行段落打包，Markdown 经 pulldown-cmark 标题边界切分 | AINS 扩展 |
| 代码分块 | 无 | 与纯文本同一启发式（段落打包） | **偏差（暂缺）**：tree-sitter 结构化切分延后（体积/双端成本），计划 4.3 允许启发式起步 |
| PDF | 无 | Native=pdf-extract；Web 返回明确错误（`index_content` 为双端入口） | AINS 扩展（平台差异显式化） |
| Embedding 批量 | 无 | `EMBED_BATCH_MAX=20` 常量已定义；`ModelClient::embed` 当前单条接口，传输层批量合并随 Phase 5 远程客户端落地 | **偏差（暂缺）** |
| 去重 | 无 | `source_hash`（sha256 全文）命中直接返回既有 meta | AINS 扩展 |

## 5. 记忆管理策略（`manage.rs` / `engine.rs` ↔ `memory/manager.py` 部分语义）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 签名归一化 | 小写、空白折叠、去 ASCII 标点后哈希 | `normalize_for_signature` 同口径；`content_signature = sha256("{normalized}|{namespace}")` | 对齐 |
| 去重合并 | 同签名更新既有记忆而非新建 | `remember` 同签名 → 刷新（importance 取 max；内容、向量与 metadata 均以新写入为准——metadata 替换对齐 memdir 刷新语义，refreshed_at 由引擎维护，SoT 不与新向量分叉） | 对齐（metadata 替换为 review 五轮修复） |
| 重要性评分 | frontmatter `importance` | `metadata.importance`（默认 1.0），容量满按保留权重（importance × 时间衰减）淘汰最低分 | 对齐 + AINS 扩展（容量淘汰） |
| 时间衰减 | 无显式公式 | `0.5^(age_days/30)`；检索重排 `search_ranked` 按衰减降低排名（正分乘衰减，负分——Euclidean 恒负/cosine 反向——除衰减使其更负，保证衰减只降排名不反向抬升）；按相似度 4 倍过采样后重排再截断 top_k（否则窗口外的新鲜条目永无法反超，review 五轮） | AINS 扩展（计划 4.2） |

## 6. memdir 可读记忆库（`memdir.rs` ↔ `memory/memdir.py` + `schema.py`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 存储介质 | 文件系统目录（`MEMORY.md` + topic `.md` 文件） | KvStore `kv` 表（`memdir/MEMORY.md` / `memdir/entries/{filename}`） | **偏差（有意）**：双端统一存储；Web 无文件系统 |
| 策略文案 | `MEMORY_POLICY_LINES`（8 行） | 逐字相同 | 对齐 |
| prompt 注入 | `load_memory_prompt`：头部 4 行 + 策略 + `## MEMORY.md` 代码块或 "(not created yet)"，恒有输出 | 同结构；目录行为 `kv://memdir`（无文件路径） | 对齐（目录标识为必要偏差） |
| 截断 | 行截断（200）→ 字节截断（25000，回退完整行）→ 尾部换行恢复；字节原因优先；WARNING marker 文案固定 | `truncate_entrypoint` + `append_truncation_marker` 同算法同文案 | 对齐 |
| frontmatter | `FRONTMATTER_FIELDS` 固定顺序，schema_version=1 | `render_entry_file` 同顺序渲染；解析用 serde_yaml 宽松读取，缺字段回落默认 | 对齐 |
| 条目 id | `mem-{紧凑时间戳}-{8 位随机 hex}` | 随机 hex → `sha256(content|now)` 前 8 位（确定性，双端可复现，无需随机源） | **偏差（有意）** |
| slug / 冲突 | 非字母数字→`_`、小写、fallback `memory`；冲突 `{slug}_2.md`… | 同 | 对齐 |
| 去重 | 签名 `sha256("{normalized}|{type}|{category}")`，命中刷新（importance max、updated_at、disabled=false，标题/描述/正文/标签/来源以新写入为准） | 同（category 恒 "knowledge"）；刷新时恢复索引行（幂等）；importance 新建/刷新两路径均钳位 ≥ 1（同基线） | 对齐（索引行恢复为 review 修复） |
| 删除 | 软删除 `disabled: true` + 索引行移除；按 stem/name/title/id 匹配 | 同；索引行锚定 `({filename})` 避免子串误伤（如 `build.md` vs `web_build.md`），review 五轮收紧为行尾 `]({filename})` 锚定（标题文本含 `(build.md)` 字样不再误判）；已禁用条目不遮蔽同名启用条目 | 对齐（匹配口径为 review 修复） |
| scan | 过滤 disabled + TTL 过期（TTL 以 updated_at 为锚、缺失回落 created_at，去重刷新延长寿命），updated_at 降序，cap 200；description 回落正文首个普通行 ≤200 字符 | 同（TTL 锚点为 review 二轮修复：旧实现误锚 created_at，刷新后的过期条目永久不可见） | 对齐 |
| team vault / 文件锁 / secret 扫描 | 有（团队共享记忆库） | 未纳入 | **偏差（暂缺）**：单机嵌入式场景暂无团队共享；随企业知识库需求评估 |
| YAML 转义 | Python yaml 库 | 手写双引号风格转义（`"` `\` 换行/制表/控制字符）；frontmatter 以行首 `---` 结束（值内含 `---` 不截断） | 对齐（实现方式差异，roundtrip 有测试） |
| 时间戳 | ISO-8601 UTC 秒级 | 同格式；手工 civil 历法换算（Hinnant 算法），不引入 chrono | 对齐 |

## 7. durable 抽取 + 会话检查点（`extract.rs` ↔ `services/memory_extract.py` + `session_memory.py`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| system prompt | `EXTRACTION_SYSTEM_PROMPT` | 逐字相同（含 "Harness durable memory" 表述，保留基线名词） | 对齐 |
| gating | <2 条消息跳过；本会话已写记忆跳过 | `maybe_extract(messages, memory_writes_since_last)` 同语义 | 对齐 |
| 清单 | ≤80 文件，`[{type}] {path} ({age}) - {desc}` | `build_manifest` 同格式（age = "today"/"{n}d"） | 对齐 |
| 转写 | 最近 12 条、单条 1200 字符；`{role}: {text}` / `tool calls -> names` / `[non-text content]` | `format_transcript` 同格式 | 对齐 |
| 请求参数 | max_records=3、max_tokens=2048、无工具 | 同（`stream_response` 收 Complete 事件取全文；流在 Complete 前终止——重试耗尽等——返回明确的 skipped 原因，不与“模型判定无可保存”混淆，review 五轮） | 对齐 |
| JSON 解析 | 宽松：首 `{` 到末 `}`；type/scope 宽松映射（note/memory/core/knowledge→默认，personal/user→private，shared→team） | `parse_memory_records` 同语义；解析失败返回空（不报错） | 对齐 |
| 会话检查点 | `# Session Memory` 文档：Current State / Next Step / Verified Work / Active Artifacts（末 10）/ Recent Conversation（`- role: text[:220]`，≤80 行，工具名 ≤6）；12000 字符预算 + 截断 marker | `build_session_memory` 同结构同预算同 marker；存 `kv` 表 `memdir/session_memory.md`；截断切点：基线回退行边界且 marker 附加后可超预算，AINS 按字符预算硬切（含 marker 恒 ≤ 12000，长单行不丢全部内容） | 对齐（存储介质同 §6 偏差；切点为**偏差（有意）**） |

## 8. 测试与验收（Phase 2.10）

- Native：`tests/memory_native.rs` 61 例（KV 读写/TTL 惰性+sweep+后台 sweeper、
  向量检索+重载重建、去重合并/forget、DocumentStore 全生命周期（含
  doc_ids 过滤跨文档检索）、search_ranked 时间衰减重排、memdir
  全流程+截断、抽取 gating+落库、检查点 roundtrip+截断；review 回归 8 例：
  索引行锚定、re-add 恢复索引、同名条目软删、refreshed_at 新鲜度、
  损坏行免疫、frontmatter 特殊字符 / YAML 原生标量与控制字符 roundtrip；
  fix 回归 26 例：索引加载跳过损坏 embedding 行、非有限向量拒绝、forget
  签名守卫、去重刷新替换向量、容量满 remember 淘汰 / insert_with_id 报错、
  clear_namespace 全量清理 + 幂等重复调用、超大 TTL 饱和、文档索引中途
  失败回收孤儿 chunk（含第 i 块部分写入与 meta/hash 写入失败两类故障注入）、
  forget 删除 JSON/Envelope 两级损坏行、索引 add 失败回滚 SoT（新建 +
  去重刷新两路径）、remember 写入中途 KvStore 落盘失败回滚（向量落盘 /
  签名落盘两步故障注入，避免孤儿行占用容量与“有内容无签名”重复建条目）、
  memdir 超大 ttl_days 饱和不溢出、search 回填跳过损坏
  memories 行、惰性过期删除写事务内复核不误删刷新新值、损坏行生命周期
  闭环（去重目标损坏回落新建、淘汰优先回收损坏行、memdir scan /
  文档 meta 损坏免疫 + 重新索引覆写修复）、HNSW 满容量同 id 更新不拒绝
  （去重刷新路径）、去重刷新容忍旧 embedding 快照行损坏、doc meta 损坏
  不阻断 delete（chunk 按实际前缀列举回收 + hash 映射反查清理）、
  Euclidean 分数口径护栏（Native -DistL2 与双端共享 similarity_score
  一致，含 sqrt））；review 二轮回归 4 例：memdir TTL 以 updated_at 为锚 +
  同签名刷新复活过期条目、新建路径 importance 钳位 ≥ 1、容量满淘汰后
  新条目写入失败时恢复被淘汰条目（含签名映射，无净丢失）、doc_ids
  过滤检索固定过采样不足时按 4 倍扩窗重试不欠采样；review 三轮回归
  3 例：TTL 过期边界 >=（恰好到期即过期，同基线）、恢复路径自身
  故障（双故障注入）不破坏引擎可用性且无孤儿行、空 doc_ids 过滤器
  返回空不陷入扩窗循环；review 四轮回归 1 例：doc_ids 过滤检索的
  耗尽判定改按 namespace 总条数上界（回填跳过的损坏行不再误判
  索引耗尽而提前停止扩窗欠取）；另 restore_evicted 向量/索引恢复失败补齐
  warn 日志、条目行快照缺失时跳过向量恢复（避免孤儿 embedding
  随重启存活）；review 五轮回归 5 例：去重刷新 metadata 以新写入为准
  （旧行为静默丢弃新 tags 等字段）、search_ranked 4 倍过采样（窗口外
  新鲜条目可反超）、模型流未完成时抽取报告明确 skipped 原因、memdir
  索引行改行尾 `]({filename})` 锚定（标题含 `(build.md)` 字样不误伤）、
  delete_prefix 单写事务批量删除（含过期/损坏行，幂等）。
  另有 `memory/kv.rs` 内 `prefix_successor` 单元测试 5 例（UTF-16 序上界）、
  `memory/manage.rs` 内单元测试 4 例（签名归一化/namespace 隔离、
  importance 钳位、衰减已知值、**衰减对正负分均单调降排名**（修复
  回归：负分乘衰减被抬向 0，Euclidean 度量下时间衰减整体反向））、
  `memory/memdir.rs` 内单元测试 4 例（手写历法换算已知值对照/roundtrip/
  非法输入拒绝、slugify 非 ASCII 回落）、`memory/parser.rs` 内单元测试
  6 例（扩展名推断、Markdown 标题切分、段落打包预算/内容无丢失、
  超长 CJK 段落 char 边界硬切、空白输入、CRLF 空行边界）。
- Web：`tests/web_memory.rs` 13 例（同契约，IndexedDB 后端 + 线性索引；
  含前缀上界高码元键覆盖、线性索引 remove 压缩 + 加载韧性、
  search 回填跳过损坏行、后台 TTL sweeper 在 WasmRuntimeAdapter 上
  派发/停止、批量 sweep（1 ro 扫描 + 1 rw 删除事务）规模化正确性与
  幂等、memdir TTL 锚定 updated_at + 刷新复活（镜像 Native 用例），
  review 五轮新增：写路径等待事务提交后重开可见、delete_prefix
  不误伤上界过覆盖区间内的非前缀键），
  经 `wasm-pack test --headless --chrome` 执行（本地不假设浏览器存在）。
- 双 target `cargo build` / `cargo clippy --all-targets -- -D warnings` 通过。

## 不复刻清单

1. 基线文件系统 memdir 的目录监视 / 外部编辑器直改语义：AINS 存储在
   KvStore，条目只经 API 读写。
2. team vault、文件锁、secret 正则扫描（见 §6）。
3. 基线 CLI 侧 `/memory` 命令面：属 Phase 6+ 前端范围。

## 遗留偏差汇总（后续 Phase 回收）

| 偏差 | 回收点 |
|---|---|
| HNSW 图拓扑重建替代序列化 | hnsw_rs 上游支持后改 dump/load；或数据量证明重建过慢时换库 |
| tree-sitter 代码分块 | Phase 4+（代码知识库需求落地时） |
| Embedding 批量传输 | Phase 5 远程 ModelClient |
| team vault / 锁 / secret 扫描 | 企业知识库 / 多端同步 Phase |
| WAL（计划 4.4） | 不在 Phase 2 任务表；随崩溃一致性需求评估 |
