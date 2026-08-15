//! 持久记忆抽取模型的固定提示词和请求模板。

/// 抽取 system prompt（逐字对齐基线 `EXTRACTION_SYSTEM_PROMPT`，Harness 名称保留）。
pub const EXTRACTION_SYSTEM_PROMPT: &str = "You maintain Harness durable memory.\nSave only stable, future-useful facts that are not derivable from current files,\ngit history, or documentation. Prefer updating existing memories conceptually\nover duplicating them. Do not save secrets. If nothing is worth saving, return\n{\"memories\": []}.\n";

/// 旧 Memdir 抽取器使用的 JSON 请求模板。
pub fn legacy_memory_extraction_request(
    manifest: &str,
    transcript: &str,
    max_records: usize,
) -> String {
    format!(
        "Existing memory files:\n{manifest}\n\nRecent conversation:\n{transcript}\n\nReturn JSON only: {{\"memories\": [{{\"title\": str, \"content\": str, \"description\": str, \"type\": \"user|feedback|project|reference\", \"scope\": \"private|project|team\"}}]}} with at most {max_records} records."
    )
}

/// 生产 `MemoryService` 使用的 JSON 请求模板。
pub fn durable_memory_extraction_request(
    manifest: &str,
    transcript: &str,
    max_records: usize,
) -> String {
    format!(
        "Existing memory files:\n{manifest}\n\nRecent conversation:\n{transcript}\n\nReturn JSON only: {{\"memories\": [{{\"title\": str, \"content\": str, \"description\": str, \"type\": \"user|feedback|project|reference\", \"scope\": \"private|project|team\", \"importance\": float, \"ttl_days\": int, \"tags\": [str]}}]}} with at most {max_records} records."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_request_templates_keep_their_distinct_schemas() {
        let legacy = legacy_memory_extraction_request("manifest", "transcript", 3);
        let durable = durable_memory_extraction_request("manifest", "transcript", 20);
        assert!(legacy.contains("\"scope\": \"private|project|team\""));
        assert!(!legacy.contains("\"importance\": float"));
        assert!(durable.contains("\"importance\": float"));
        assert!(durable.contains("at most 20 records"));
    }
}
