//! 会话压缩模型的固定提示词。

/// 无工具压缩提示词（基线 `get_compact_prompt` 的前导 + 模板 + 尾注）。
pub const COMPACT_PROMPT: &str = "\
CRITICAL: Respond with TEXT ONLY. Do NOT call any tools. You already have all \
the context you need in the conversation above.

Your task is to create a detailed summary of the conversation so far. This \
summary will replace the earlier messages, so it must capture all important \
information.

First, draft your analysis inside <analysis> tags. Then produce a structured \
summary inside <summary> tags with these sections:
1. Primary Request and Intent
2. Key Technical Concepts
3. Files and Code Sections (with paths and line numbers)
4. Errors and Fixes
5. Problem Solving
6. All User Messages
7. Pending Tasks
8. Current Work
9. Optional Next Step

REMINDER: Respond with plain text only — an <analysis> block followed by a \
<summary> block. Tool calls will be rejected.";

/// 摘要请求的 system prompt（与 `COMPACT_PROMPT` 配对使用）。
pub const COMPACTION_SYSTEM_PROMPT: &str = "You are a conversation summarizer.";
