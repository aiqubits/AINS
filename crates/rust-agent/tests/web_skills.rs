//! Phase 6.4 KvSkillStore 契约测试（Web / IndexedDB 后端）。
//!
//! 仅在 CI 中通过 `wasm-pack test --headless --chrome` 执行；
//! 与 `tests/skills_store.rs` 覆盖同一行为契约的核心子集。

#![cfg(target_arch = "wasm32")]

use std::sync::Arc;

use serde_json::json;
use wasm_bindgen_test::*;

use rust_agent::error::SkillsError;
use rust_agent::memory::{IndexedDbBackend, KvStore, TABLE_KV, now_ms};
use rust_agent::platform::Platform;
use rust_agent::skills::{
    KvSkillStore, SkillContext, SkillLoader, SkillManage, SkillMeta, SkillTrust, skill_checksum,
};

wasm_bindgen_test_configure!(run_in_browser);

const SKILL_MD: &str =
    "---\nname: csv-report\ndescription: CSV report workflow\n---\n# CSV Report Workflow\n";

async fn kv(db: &str) -> Arc<dyn KvStore> {
    let backend = IndexedDbBackend::open(db).await.expect("open idb");
    let store = backend.store(TABLE_KV);
    // 每个用例独立 DB 名，仍清空以防重跑残留
    store.delete_prefix("skills").await.expect("clear");
    Arc::new(store)
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
        checksum: String::new(),
    }
}

fn web_ctx() -> SkillContext {
    SkillContext {
        platform: Platform::Web,
        available_tools: vec![],
    }
}

#[wasm_bindgen_test]
async fn put_list_load_roundtrip() {
    let store = KvSkillStore::new(kv("ains-skills-roundtrip").await);
    store.put_skill("csv", SKILL_MD, meta("c")).await.unwrap();

    let entries = store.list_entries().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert!(!entries[0].corrupted);
    assert_eq!(
        entries[0].meta.as_ref().unwrap().checksum,
        skill_checksum(SKILL_MD)
    );

    let content = store.load("csv").await.unwrap();
    assert_eq!(
        content
            .frontmatter
            .get("description")
            .and_then(|v| v.as_str()),
        Some("CSV report workflow")
    );
    assert!(content.body.starts_with("# CSV Report Workflow"));
}

#[wasm_bindgen_test]
async fn checksum_mismatch_flagged_and_excluded_from_loader_list() {
    let backend = kv("ains-skills-corrupt").await;
    let store = KvSkillStore::new(Arc::clone(&backend));
    store.put_skill("drift", SKILL_MD, meta("d")).await.unwrap();
    backend
        .set("skills:drift", &json!("tampered"), None)
        .await
        .unwrap();

    let entries = store.list_entries().await.unwrap();
    assert!(
        entries
            .iter()
            .find(|e| e.name == "drift")
            .unwrap()
            .corrupted
    );
    assert!(store.list(&web_ctx()).await.unwrap().is_empty());
    // 经名称旁路加载同样被完整性门控拒绝
    assert!(store.load("drift").await.is_err());
}

#[wasm_bindgen_test]
async fn delete_removes_both_keys_and_missing_reports_not_found() {
    let backend = kv("ains-skills-delete").await;
    let store = KvSkillStore::new(Arc::clone(&backend));
    store.put_skill("gone", SKILL_MD, meta("g")).await.unwrap();

    store.delete_skill("gone").await.unwrap();
    assert!(backend.get("skills:gone").await.unwrap().is_none());
    assert!(backend.get("skills_meta:gone").await.unwrap().is_none());

    match store.delete_skill("gone").await {
        Err(SkillsError::NotFound(_)) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[wasm_bindgen_test]
async fn platform_gating_hides_desktop_skills_on_web() {
    let store = KvSkillStore::new(kv("ains-skills-gating").await);
    let mut desktop_only = meta("desktop only");
    desktop_only.platforms = vec![Platform::Desktop];
    store
        .put_skill("desk", SKILL_MD, desktop_only)
        .await
        .unwrap();
    store
        .put_skill("anywhere", SKILL_MD, meta("any"))
        .await
        .unwrap();

    let visible = store.list(&web_ctx()).await.unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].name, "anywhere");
}

#[wasm_bindgen_test]
async fn create_update_rollback_and_reference_roundtrip() {
    let store = KvSkillStore::new(kv("ains-skills-manage").await);
    // create v1.0
    store.create_skill("wf", SKILL_MD).await.unwrap();
    assert_eq!(store.list_versions("wf").await.unwrap().len(), 1);
    assert!(store.create_skill("wf", SKILL_MD).await.is_err());
    // update → v1.1
    let md2 = "---\nname: wf\ndescription: wf v2\n---\n# v2\n";
    store.update_skill("wf", md2).await.unwrap();
    let labels: Vec<_> = store
        .list_versions("wf")
        .await
        .unwrap()
        .iter()
        .map(|(v, _)| v.label())
        .collect();
    assert_eq!(labels, ["v1.0", "v1.1"]);
    // rollback v1.0 → v2.0
    store.rollback_skill("wf", "v1.0").await.unwrap();
    assert!(
        store
            .list_versions("wf")
            .await
            .unwrap()
            .iter()
            .any(|(v, _)| v.label() == "v2.0")
    );
    // Level 2 引用文件
    store
        .put_reference("wf", "references/a.md", "hello")
        .await
        .unwrap();
    assert_eq!(
        store.load_reference("wf", "references/a.md").await.unwrap(),
        "hello"
    );
    assert!(
        store
            .load_reference("wf", "references/none.md")
            .await
            .is_err()
    );
}

#[wasm_bindgen_test]
async fn reference_integrity_rejects_tampered_payload() {
    // Level 2 完整性门控与 Native 同契约：篡改/无 checksum 拒绝注入
    let backend = kv("ains-skills-ref-integrity").await;
    let store = KvSkillStore::new(Arc::clone(&backend));
    store.create_skill("wf", SKILL_MD).await.unwrap();
    store
        .put_reference("wf", "references/a.md", "original")
        .await
        .unwrap();
    backend
        .set(
            "skills_ref:wf:references/a.md",
            &json!({"content": "tampered", "checksum": skill_checksum("original")}),
            None,
        )
        .await
        .unwrap();
    match store.load_reference("wf", "references/a.md").await {
        Err(SkillsError::InvalidFormat(_)) => {}
        other => panic!("expected InvalidFormat on tampered reference, got {other:?}"),
    }
}

#[wasm_bindgen_test]
async fn head_orphan_is_listed_corrupted_and_deletable() {
    // create 部分写入中断（有头无镜像）：面板须可见可删
    //（与 Native 套件 orphan 路径同契约）
    let backend = kv("ains-skills-head-orphan").await;
    let store = KvSkillStore::new(Arc::clone(&backend));
    backend
        .set(
            "skills_head:ghost",
            &json!({"active": "v1.0", "meta": {
                "description": "", "category": "general", "requires_tools": [],
                "platforms": [], "trust_level": "generated", "creator": "agent",
                "created_at": 0, "permissions": [], "checksum": "c"
            }}),
            None,
        )
        .await
        .unwrap();

    let entries = store.list_entries().await.unwrap();
    let ghost = entries.iter().find(|e| e.name == "ghost").expect("visible");
    assert!(ghost.corrupted);
    // 损坏条目不进模型可见面
    assert!(store.list(&web_ctx()).await.unwrap().is_empty());

    store.delete_skill("ghost").await.unwrap();
    assert!(backend.get("skills_head:ghost").await.unwrap().is_none());
}
