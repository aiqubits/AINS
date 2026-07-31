//! Phase 6.4 KvSkillStore 契约测试（Native / redb 后端）。
//!
//! Web 端同一契约见 `tests/web_skills.rs`（CI wasm-pack 浏览器执行）。

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use serde_json::json;

use agent_core::error::SkillsError;
use agent_core::memory::{KvStore, RedbBackend, TABLE_KV, now_ms};
use agent_core::platform::Platform;
use agent_core::skills::{
    AUTO_ROLLBACK_CONSECUTIVE_FAILURES, KvSkillStore, SkillContext, SkillLoader, SkillManage,
    SkillMeta, SkillPruner, SkillTrust, skill_checksum, split_frontmatter,
};

const SKILL_MD: &str = "---\nname: csv-report\ndescription: CSV report workflow\n---\n# CSV Report Workflow\n\n## Procedure\n1. read\n2. write\n";

fn kv(dir: &tempfile::TempDir) -> Arc<dyn KvStore> {
    Arc::new(
        RedbBackend::open(dir.path().join("skills.redb"))
            .expect("open redb")
            .table(TABLE_KV),
    )
}

fn meta(description: &str) -> SkillMeta {
    SkillMeta {
        description: description.to_string(),
        category: "data".into(),
        requires_tools: vec![],
        platforms: vec![],
        trust_level: SkillTrust::Generated,
        creator: "agent".into(),
        created_at: now_ms(),
        permissions: vec![],
        checksum: String::new(), // put_skill 重算
    }
}

fn ctx(platform: Platform, tools: &[&str]) -> SkillContext {
    SkillContext {
        platform,
        available_tools: tools.iter().map(|t| t.to_string()).collect(),
    }
}

#[tokio::test]
async fn list_entries_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));
    assert!(store.list_entries().await.unwrap().is_empty());
}

#[tokio::test]
async fn put_and_list_entries_sorted_with_meta() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));
    store.put_skill("zeta", SKILL_MD, meta("z")).await.unwrap();
    store.put_skill("alpha", SKILL_MD, meta("a")).await.unwrap();

    let entries = store.list_entries().await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "alpha");
    assert_eq!(entries[1].name, "zeta");
    assert!(!entries[0].corrupted);
    let m = entries[0].meta.as_ref().expect("meta present");
    assert_eq!(m.description, "a");
    assert_eq!(m.checksum, skill_checksum(SKILL_MD));
}

#[tokio::test]
async fn corrupted_meta_deserialization_is_flagged_not_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let backend = kv(&dir);
    let store = KvSkillStore::new(Arc::clone(&backend));
    store.put_skill("ok", SKILL_MD, meta("ok")).await.unwrap();
    // 直接向 meta key 写入非法结构
    backend
        .set("skills_meta:bad", &json!({"not": "a meta"}), None)
        .await
        .unwrap();
    backend
        .set("skills:bad", &json!(SKILL_MD), None)
        .await
        .unwrap();

    let entries = store.list_entries().await.unwrap();
    assert_eq!(entries.len(), 2);
    let bad = entries.iter().find(|e| e.name == "bad").unwrap();
    assert!(bad.corrupted);
    assert!(bad.meta.is_none());
    assert!(!entries.iter().find(|e| e.name == "ok").unwrap().corrupted);
}

#[tokio::test]
async fn checksum_mismatch_is_flagged_corrupted() {
    let dir = tempfile::tempdir().unwrap();
    let backend = kv(&dir);
    let store = KvSkillStore::new(Arc::clone(&backend));
    store.put_skill("drift", SKILL_MD, meta("d")).await.unwrap();
    // 篡改原文（模拟存储损坏 / 部分写入）
    backend
        .set("skills:drift", &json!("tampered content"), None)
        .await
        .unwrap();

    let entries = store.list_entries().await.unwrap();
    let drift = entries.iter().find(|e| e.name == "drift").unwrap();
    assert!(drift.corrupted);
    // 元数据本身可读，面板仍能展示名称与删除入口
    assert!(drift.meta.is_some());
}

#[tokio::test]
async fn delete_removes_both_keys_atomically_visible() {
    let dir = tempfile::tempdir().unwrap();
    let backend = kv(&dir);
    let store = KvSkillStore::new(Arc::clone(&backend));
    store.put_skill("gone", SKILL_MD, meta("g")).await.unwrap();

    store.delete_skill("gone").await.unwrap();
    assert!(backend.get("skills:gone").await.unwrap().is_none());
    assert!(backend.get("skills_meta:gone").await.unwrap().is_none());
    assert!(store.list_entries().await.unwrap().is_empty());
}

#[tokio::test]
async fn delete_missing_skill_reports_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));
    match store.delete_skill("ghost").await {
        Err(SkillsError::NotFound(name)) => assert_eq!(name, "ghost"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn orphan_meta_is_listed_corrupted_and_deletable() {
    let dir = tempfile::tempdir().unwrap();
    let backend = kv(&dir);
    let store = KvSkillStore::new(Arc::clone(&backend));
    let meta_value = serde_json::to_value(meta("orphan")).unwrap();
    backend
        .set("skills_meta:orphan", &meta_value, None)
        .await
        .unwrap();

    let entries = store.list_entries().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "orphan");
    assert!(entries[0].corrupted);

    store.delete_skill("orphan").await.unwrap();
    assert!(store.list_entries().await.unwrap().is_empty());
}

#[tokio::test]
async fn load_parses_frontmatter_and_body() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));
    store.put_skill("csv", SKILL_MD, meta("c")).await.unwrap();

    let content = store.load("csv").await.unwrap();
    assert_eq!(
        content
            .frontmatter
            .get("description")
            .and_then(|v| v.as_str()),
        Some("CSV report workflow")
    );
    assert!(content.body.starts_with("# CSV Report Workflow"));

    match store.load("missing").await {
        Err(SkillsError::NotFound(_)) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn load_without_frontmatter_returns_null_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));
    let raw = "# Plain skill\nbody only\n";
    store.put_skill("plain", raw, meta("p")).await.unwrap();

    let content = store.load("plain").await.unwrap();
    assert!(content.frontmatter.is_null());
    assert_eq!(content.body, raw);
}

#[tokio::test]
async fn loader_list_applies_platform_and_tool_gating() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));

    let mut desktop_only = meta("desktop only");
    desktop_only.platforms = vec![Platform::Desktop];
    store
        .put_skill("desk", SKILL_MD, desktop_only)
        .await
        .unwrap();

    let mut needs_shell = meta("needs shell");
    needs_shell.requires_tools = vec!["shell_command".into()];
    store
        .put_skill("shelly", SKILL_MD, needs_shell)
        .await
        .unwrap();

    store
        .put_skill("anywhere", SKILL_MD, meta("any"))
        .await
        .unwrap();

    // Web + 无 shell：只有无门控的 skill 可见
    let visible = store
        .list(&ctx(Platform::Web, &["file_read"]))
        .await
        .unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].name, "anywhere");

    // Desktop + shell：全部可见
    let visible = store
        .list(&ctx(Platform::Desktop, &["file_read", "shell_command"]))
        .await
        .unwrap();
    let names: Vec<_> = visible.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["anywhere", "desk", "shelly"]);
}

#[tokio::test]
async fn loader_list_excludes_corrupted_entries_from_model_surface() {
    let dir = tempfile::tempdir().unwrap();
    let backend = kv(&dir);
    let store = KvSkillStore::new(Arc::clone(&backend));
    store.put_skill("good", SKILL_MD, meta("g")).await.unwrap();
    backend
        .set("skills:broken", &json!(SKILL_MD), None)
        .await
        .unwrap(); // 无 meta → 损坏

    let summaries = store.list(&ctx(Platform::Web, &[])).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].name, "good");
    // 面板面仍可见损坏条目
    assert_eq!(store.list_entries().await.unwrap().len(), 2);
}

#[tokio::test]
async fn create_update_rollback_versioning_and_active_mirror() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));

    // create → v1.0 Active，面板/Loader 可见
    let s = store.create_skill("csv", SKILL_MD).await.unwrap();
    assert_eq!(s.description, "CSV report workflow");
    let vers = store.list_versions("csv").await.unwrap();
    assert_eq!(vers.len(), 1);
    assert_eq!(vers[0].0.label(), "v1.0");
    assert_eq!(vers[0].1.status, agent_core::skills::SkillStatus::Active);
    // 重复创建报错
    assert!(store.create_skill("csv", SKILL_MD).await.is_err());
    // 活跃镜像可被 Loader 读取
    assert!(store.load("csv").await.is_ok());

    // update → v1.1 Active，v1.0 Deprecated
    let md2 = "---\nname: csv\ndescription: CSV v2\n---\n# CSV v2\n";
    store.update_skill("csv", md2).await.unwrap();
    let vers = store.list_versions("csv").await.unwrap();
    let labels: Vec<_> = vers.iter().map(|(v, _)| v.label()).collect();
    assert_eq!(labels, ["v1.0", "v1.1"]);
    let v10 = vers.iter().find(|(v, _)| v.label() == "v1.0").unwrap();
    let v11 = vers.iter().find(|(v, _)| v.label() == "v1.1").unwrap();
    assert_eq!(v10.1.status, agent_core::skills::SkillStatus::Deprecated);
    assert_eq!(v11.1.status, agent_core::skills::SkillStatus::Active);
    // 活跃镜像已切到 v1.1 内容
    assert_eq!(
        store
            .load("csv")
            .await
            .unwrap()
            .frontmatter
            .get("description")
            .and_then(|v| v.as_str()),
        Some("CSV v2")
    );

    // rollback 到 v1.0 → 新大版本 v2.0（内容=v1.0），v1.1 降级
    store.rollback_skill("csv", "v1.0").await.unwrap();
    let vers = store.list_versions("csv").await.unwrap();
    let labels: Vec<_> = vers.iter().map(|(v, _)| v.label()).collect();
    assert_eq!(labels, ["v1.0", "v1.1", "v2.0"]);
    let v20 = vers.iter().find(|(v, _)| v.label() == "v2.0").unwrap();
    assert_eq!(v20.1.status, agent_core::skills::SkillStatus::Active);
    assert_eq!(
        store
            .load("csv")
            .await
            .unwrap()
            .frontmatter
            .get("description")
            .and_then(|v| v.as_str()),
        Some("CSV report workflow")
    );

    // 回滚目标超出保留范围 → 报错
    assert!(store.rollback_skill("csv", "v9.9").await.is_err());

    // delete 清除全部版本 + 头 + 镜像
    store.delete_skill("csv").await.unwrap();
    assert!(store.list_versions("csv").await.unwrap().is_empty());
    assert!(store.list_entries().await.unwrap().is_empty());
}

#[tokio::test]
async fn record_outcome_auto_rollback_after_consecutive_failures() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));
    store.create_skill("wf", SKILL_MD).await.unwrap();
    // 让 v1.0 攒下高成功率
    for _ in 0..8 {
        store.record_outcome("wf", true).await.unwrap();
    }
    // 升级到 v1.1（当前 active），随后 v1.1 连续失败 5 次
    let md2 = "---\nname: wf\ndescription: wf bad\n---\n# bad\n";
    store.update_skill("wf", md2).await.unwrap();
    let mut auto = false;
    for _ in 0..AUTO_ROLLBACK_CONSECUTIVE_FAILURES {
        auto = store.record_outcome("wf", false).await.unwrap();
    }
    assert!(auto, "连续失败达阈值且存在更优版本应触发自动回滚");
    // 触发后应产生新大版本 v2.0（内容=v1.0 高分版）
    let vers = store.list_versions("wf").await.unwrap();
    assert!(
        vers.iter().any(
            |(v, r)| v.label() == "v2.0" && r.status == agent_core::skills::SkillStatus::Active
        )
    );

    // 防抖动（Code Review 修正）：v2.0 内容与 Golden v1.0 字节相同，
    // 再度连续失败不得重升同一内容（否则每 5 次失败无限增长版本号）。
    let before = store.list_versions("wf").await.unwrap().len();
    for _ in 0..(AUTO_ROLLBACK_CONSECUTIVE_FAILURES * 2) {
        let rolled = store.record_outcome("wf", false).await.unwrap();
        assert!(!rolled, "同内容候选不应触发二次自动回滚");
    }
    assert_eq!(
        store.list_versions("wf").await.unwrap().len(),
        before,
        "防抖动：版本数不增长"
    );
}

#[tokio::test]
async fn rollback_with_stale_candidate_after_prune_surfaces_error() {
    // 回滚与清理的竞态：UI 先列出候选 → prune 淘汰目标版本 → 用户点击
    // 回滚。存储层按当前保留集重新校验，过期候选必须报错（UI 据此
    // 渲染错误横幅）而非静默成功或 panic。
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));
    store.create_skill("stale", SKILL_MD).await.unwrap();
    for i in 1..=4 {
        let md = format!("---\nname: stale\ndescription: v{i}\n---\n# v{i}\n");
        store.update_skill("stale", &md).await.unwrap();
    }
    // 此刻候选含 v1.2（最近 3 之内）——模拟 UI 已展示的旧列表
    let candidates = store.rollback_candidates("stale").await.unwrap();
    assert!(candidates.contains(&"v1.2".to_string()), "{candidates:?}");

    // 再升两版并 prune，v1.2 被淘汰出保留集
    for i in 5..=6 {
        let md = format!("---\nname: stale\ndescription: v{i}\n---\n# v{i}\n");
        store.update_skill("stale", &md).await.unwrap();
    }
    SkillPruner.prune(&store, "stale").await.unwrap();
    let remaining: Vec<_> = store
        .list_versions("stale")
        .await
        .unwrap()
        .iter()
        .map(|(v, _)| v.label())
        .collect();
    assert!(!remaining.contains(&"v1.2".to_string()), "{remaining:?}");

    // 过期候选回滚 → 显式错误（保留范围校验先于版本读取，不泄漏
    // 内部状态、不产生新版本）
    match store.rollback_skill("stale", "v1.2").await {
        Err(SkillsError::InvalidFormat(msg)) => {
            assert!(msg.contains("outside the retained range"), "{msg}");
        }
        other => panic!("expected InvalidFormat for pruned candidate, got {other:?}"),
    }
    // 失败的回滚不得产生新版本或改变活跃版
    assert_eq!(
        store.active_version("stale").await.unwrap().as_deref(),
        Some("v1.6")
    );
}

#[tokio::test]
async fn skill_pruner_keeps_recent_and_golden() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));
    store.create_skill("p", SKILL_MD).await.unwrap();
    // v1.0 设为 Golden（高成功率）
    for _ in 0..10 {
        store.record_outcome("p", true).await.unwrap();
    }
    // 连续 update 造出 v1.1..v1.5（低分）
    for i in 1..=5 {
        let md = format!("---\nname: p\ndescription: v{i}\n---\n# v{i}\n");
        store.update_skill("p", &md).await.unwrap();
    }
    let before = store.list_versions("p").await.unwrap().len();
    assert_eq!(before, 6); // v1.0..v1.5
    let pruner = SkillPruner;
    let removed = pruner.prune(&store, "p").await.unwrap();
    assert!(removed >= 1);
    let after: Vec<_> = store
        .list_versions("p")
        .await
        .unwrap()
        .iter()
        .map(|(v, _)| v.label())
        .collect();
    // Golden v1.0 与最近版本（含活跃 v1.5）保留
    assert!(after.contains(&"v1.0".to_string()), "Golden 应保留");
    assert!(after.contains(&"v1.5".to_string()), "活跃版本应保留");
}

#[tokio::test]
async fn pruner_keeps_golden_consistent_with_rollback_candidates() {
    // 回滚候选与清理保留集单一权威：旧高分 Golden 被挤出最近 N
    // 窗口后，清理仍须保留它，且它仍作为可回滚候选（防 UI 列出
    // 却回滚拒用的不一致）。
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir)); // 默认窗口 3
    store.create_skill("g", SKILL_MD).await.unwrap();
    // v1.0 攒高成功率成为 Golden
    for _ in 0..10 {
        store.record_outcome("g", true).await.unwrap();
    }
    // 升至 v1.5，将 v1.0 挤出最近 3（v1.3/v1.4/v1.5）
    for i in 1..=5 {
        let md = format!("---\nname: g\ndescription: v{i}\n---\n# v{i}\n");
        store.update_skill("g", &md).await.unwrap();
    }
    let candidates = store.rollback_candidates("g").await.unwrap();
    assert!(
        candidates.contains(&"v1.0".to_string()),
        "Golden 应作为回滚候选: {candidates:?}"
    );

    // 清理后 Golden 仍在且仍可真实回滚
    SkillPruner.prune(&store, "g").await.unwrap();
    let remaining: Vec<_> = store
        .list_versions("g")
        .await
        .unwrap()
        .iter()
        .map(|(v, _)| v.label())
        .collect();
    assert!(
        remaining.contains(&"v1.0".to_string()),
        "清理后 Golden 必须保留: {remaining:?}"
    );
    for candidate in store.rollback_candidates("g").await.unwrap() {
        assert!(
            remaining.contains(&candidate),
            "候选 {candidate} 必须在剩余版本内"
        );
    }
    store.rollback_skill("g", "v1.0").await.unwrap();
}

#[tokio::test]
async fn version_only_orphan_is_deletable_not_leaked() {
    let dir = tempfile::tempdir().unwrap();
    let backend = kv(&dir);
    let store = KvSkillStore::new(Arc::clone(&backend));
    // 模拟 create 中断：仅残留版本记录（无镜像/头）
    backend
        .set(
            "skills_ver:ghost:v1.0",
            &serde_json::json!({"content":"x","checksum":"c","status":"active","score":{"successes":0,"failures":0,"consecutive_failures":0},"created_at":0}),
            None,
        )
        .await
        .unwrap();
    // 面板不可见（镜像/头均无），但 delete 必须能回收而非 NotFound
    store.delete_skill("ghost").await.unwrap();
    assert!(
        backend
            .get("skills_ver:ghost:v1.0")
            .await
            .unwrap()
            .is_none()
    );
    // 彻底清空后再删才报 NotFound
    assert!(matches!(
        store.delete_skill("ghost").await,
        Err(SkillsError::NotFound(_))
    ));
}

#[tokio::test]
async fn reference_files_put_and_load() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));
    store.create_skill("ref", SKILL_MD).await.unwrap();
    store
        .put_reference("ref", "references/schema.md", "# Schema\ncol,type")
        .await
        .unwrap();
    assert_eq!(
        store
            .load_reference("ref", "references/schema.md")
            .await
            .unwrap(),
        "# Schema\ncol,type"
    );
    match store.load_reference("ref", "references/missing.md").await {
        Err(SkillsError::NotFound(_)) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn load_reference_rejects_tampered_or_checksumless_payload() {
    // Level 2 完整性门控与 load 同口径：篡改/无 checksum 的引用
    // 不可注入模型上下文
    let dir = tempfile::tempdir().unwrap();
    let backend = kv(&dir);
    let store = KvSkillStore::new(Arc::clone(&backend));
    store.create_skill("ref", SKILL_MD).await.unwrap();
    store
        .put_reference("ref", "references/a.md", "original")
        .await
        .unwrap();

    // 篡改 content（checksum 不再匹配）
    backend
        .set(
            "skills_ref:ref:references/a.md",
            &json!({"content": "tampered", "checksum": skill_checksum("original")}),
            None,
        )
        .await
        .unwrap();
    match store.load_reference("ref", "references/a.md").await {
        Err(SkillsError::InvalidFormat(msg)) => {
            assert!(msg.contains("integrity"), "{msg}");
        }
        other => panic!("expected InvalidFormat on tampered reference, got {other:?}"),
    }

    // 裸字符串载荷（无 checksum，无法验证）同样拒绝
    backend
        .set(
            "skills_ref:ref:references/b.md",
            &json!("no checksum"),
            None,
        )
        .await
        .unwrap();
    match store.load_reference("ref", "references/b.md").await {
        Err(SkillsError::InvalidFormat(_)) => {}
        other => panic!("expected InvalidFormat on checksumless payload, got {other:?}"),
    }
}

#[tokio::test]
async fn custom_retention_window_shared_by_rollback_and_pruner() {
    // 保留窗口单一权威源：存储配置的窗口同时约束回滚候选与
    // Pruner 保留集，不得出现“清理保留的版本回滚却拒用”
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::with_retention(kv(&dir), 5);
    store.create_skill("w", SKILL_MD).await.unwrap();
    for i in 1..=5 {
        let md = format!("---\nname: w\ndescription: v{i}\n---\n# v{i}\n");
        store.update_skill("w", &md).await.unwrap();
    }
    // 窗口 5：v1.1..v1.5 均为回滚候选（默认窗口 3 会拒用 v1.1/v1.2）
    let candidates = store.rollback_candidates("w").await.unwrap();
    assert!(candidates.contains(&"v1.1".to_string()), "{candidates:?}");
    store.rollback_skill("w", "v1.1").await.unwrap();

    // Pruner 用同一窗口：清理后剩余版本包含全部回滚候选
    SkillPruner.prune(&store, "w").await.unwrap();
    let remaining: Vec<_> = store
        .list_versions("w")
        .await
        .unwrap()
        .iter()
        .map(|(v, _)| v.label())
        .collect();
    let candidates = store.rollback_candidates("w").await.unwrap();
    for candidate in &candidates {
        assert!(
            remaining.contains(candidate),
            "候选 {candidate} 必须在剩余版本内: {remaining:?}"
        );
    }
    // 全部候选均可真实回滚（窗口一致性的行为面验证）
    let target = candidates.first().cloned().expect("non-empty candidates");
    store.rollback_skill("w", &target).await.unwrap();
}

#[tokio::test]
async fn skill_name_validation_rejects_injection() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvSkillStore::new(kv(&dir));
    for bad in ["", " padded ", "a/b", "a\\b", "a:b", "a\nb"] {
        assert!(
            store.put_skill(bad, SKILL_MD, meta("m")).await.is_err(),
            "name `{bad:?}` should be rejected"
        );
        assert!(store.delete_skill(bad).await.is_err());
    }
}

#[tokio::test]
async fn load_rejects_checksum_mismatch_and_missing_meta() {
    let dir = tempfile::tempdir().unwrap();
    let backend = kv(&dir);
    let store = KvSkillStore::new(Arc::clone(&backend));
    store.put_skill("drift", SKILL_MD, meta("d")).await.unwrap();
    // 篑改原文（checksum 不再匹配 meta）→ load 拒绝注入
    backend
        .set("skills:drift", &json!("tampered content"), None)
        .await
        .unwrap();
    match store.load("drift").await {
        Err(SkillsError::InvalidFormat(_)) => {}
        other => panic!("expected InvalidFormat on checksum mismatch, got {other:?}"),
    }

    // 有原文无 meta（无法验证完整性）→ 同样拒绝
    backend
        .set("skills:orphan", &json!(SKILL_MD), None)
        .await
        .unwrap();
    match store.load("orphan").await {
        Err(SkillsError::InvalidFormat(_)) => {}
        other => panic!("expected InvalidFormat on missing meta, got {other:?}"),
    }
}

#[tokio::test]
async fn promote_preserves_creator_and_trust_across_update_and_rollback() {
    // Code Review 非阻断项 #4：system 来源技能经 update/rollback 后
    // creator/trust 不得被 meta_from_content 的默认值（agent/Generated）改写
    let dir = tempfile::tempdir().unwrap();
    let backend = kv(&dir);
    let store = KvSkillStore::new(Arc::clone(&backend));
    store.create_skill("sys", SKILL_MD).await.unwrap();
    // 模拟系统来源：直接改写头元数据的 creator/trust_level
    let mut head = backend.get("skills_head:sys").await.unwrap().unwrap();
    head["meta"]["creator"] = json!("system");
    head["meta"]["trust_level"] = json!("system");
    backend.set("skills_head:sys", &head, None).await.unwrap();

    let md2 = "---\nname: sys\ndescription: sys v2\n---\n# v2\n";
    store.update_skill("sys", md2).await.unwrap();
    let head = backend.get("skills_head:sys").await.unwrap().unwrap();
    assert_eq!(head["meta"]["creator"], "system", "update 保留 creator");
    assert_eq!(head["meta"]["trust_level"], "system", "update 保留 trust");

    store.rollback_skill("sys", "v1.0").await.unwrap();
    let head = backend.get("skills_head:sys").await.unwrap().unwrap();
    assert_eq!(head["meta"]["creator"], "system", "rollback 保留 creator");
    // 镜像面（面板展示源）同步保留
    let entries = store.list_entries().await.unwrap();
    let meta = entries[0].meta.as_ref().unwrap();
    assert_eq!(meta.creator, "system");
    assert_eq!(meta.trust_level, SkillTrust::System);
}

#[tokio::test]
async fn corrupt_head_active_falls_back_to_max_version_not_overwrite() {
    // Code Review 建议测试 #2：头指针 active 不可解析时，update/rollback
    // 回退到现存最高版本号而非 INITIAL（否则新版写入 v1.1 会覆写
    // 既有记录，静默丢失版本链）
    let dir = tempfile::tempdir().unwrap();
    let backend = kv(&dir);
    let store = KvSkillStore::new(Arc::clone(&backend));
    store.create_skill("ch", SKILL_MD).await.unwrap(); // v1.0
    let md2 = "---\nname: ch\ndescription: ch v2\n---\n# v2\n";
    store.update_skill("ch", md2).await.unwrap(); // v1.1

    // 损坏头指针（不可解析的版本号）
    let mut head = backend.get("skills_head:ch").await.unwrap().unwrap();
    head["active"] = json!("garbage");
    backend.set("skills_head:ch", &head, None).await.unwrap();

    let md3 = "---\nname: ch\ndescription: ch v3\n---\n# v3\n";
    store.update_skill("ch", md3).await.unwrap();
    let vers = store.list_versions("ch").await.unwrap();
    let labels: Vec<_> = vers.iter().map(|(v, _)| v.label()).collect();
    assert_eq!(labels, ["v1.0", "v1.1", "v1.2"], "不覆写既有 v1.1");
    // v1.1 内容未被 v3 覆写，新内容落在 v1.2
    let v11 = vers.iter().find(|(v, _)| v.label() == "v1.1").unwrap();
    assert!(v11.1.content.contains("ch v2"), "v1.1 内容保持原样");
    let v12 = vers.iter().find(|(v, _)| v.label() == "v1.2").unwrap();
    assert!(v12.1.content.contains("ch v3"));
    assert_eq!(
        store.active_version("ch").await.unwrap().as_deref(),
        Some("v1.2"),
        "头指针修复为新版本"
    );
}

#[test]
fn split_frontmatter_edge_cases() {
    // 未闭合 frontmatter → 全文视为 body
    let (fm, body) = split_frontmatter("---\nname: x\nno close");
    assert!(fm.is_empty());
    assert!(body.starts_with("---\n"));
    // 行中出现 --- 不误判为关闭栏
    let (fm, body) = split_frontmatter("---\nname: a --- b\n---\nbody");
    assert_eq!(fm, "name: a --- b");
    assert_eq!(body, "body");
    // 关闭栏在文件末尾（无换行）
    let (fm, body) = split_frontmatter("---\nname: x\n---");
    assert_eq!(fm, "name: x");
    assert!(body.is_empty());
}
