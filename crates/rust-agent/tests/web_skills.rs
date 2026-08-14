//! Browser contract is exercised in Chrome: package files reside in OPFS.
#![cfg(target_arch = "wasm32")]
use rust_agent::memory::{IndexedDbBackend, KvStore, TABLE_KV};
use rust_agent::platform::Platform;
use rust_agent::skills::{
    SkillContext, SkillLoader, SkillManage, SkillPackage, SkillStore, open_platform_skill_files,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use wasm_bindgen_test::*;
wasm_bindgen_test_configure!(run_in_browser);
fn md(n: &str) -> String {
    format!("---\nname: {n}\ndescription: Web skill\n---\n# Steps\n")
}
#[wasm_bindgen_test]
async fn opfs_holds_content_and_indexeddb_holds_index() {
    let b = IndexedDbBackend::open("ains-opfs-skills")
        .await
        .unwrap()
        .store(TABLE_KV);
    b.delete_prefix("skills_").await.unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(b);
    let scope = "wasm-contract".to_string();
    let store = SkillStore::new_scoped(
        Arc::clone(&kv),
        open_platform_skill_files(scope.clone()).await.unwrap(),
        scope,
    );
    store.clear_all_skills().await.unwrap();
    store
        .create_skill("web-flow", &md("web-flow"))
        .await
        .unwrap();
    assert!(store.load("web-flow").await.unwrap().body.contains("Steps"));
    assert!(
        kv.list_prefix("owner/wasm-contract/skills/")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        kv.list_prefix("owner/wasm-contract/skills_index:")
            .await
            .unwrap()
            .len()
            == 1
    );
    let c = SkillContext {
        platform: Platform::Web,
        available_tools: vec!["skill".into()],
    };
    assert_eq!(store.list(&c).await.unwrap().len(), 1);
}

#[wasm_bindgen_test]
async fn concurrent_web_imports_share_the_mutation_lock() {
    let backend = IndexedDbBackend::open("ains-opfs-skill-mutation-lock")
        .await
        .unwrap()
        .store(TABLE_KV);
    backend.delete_prefix("skills_").await.unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(backend);
    let scope = "wasm-mutation-lock".to_string();
    let first = SkillStore::new_scoped(
        Arc::clone(&kv),
        open_platform_skill_files(scope.clone()).await.unwrap(),
        scope.clone(),
    );
    let second = SkillStore::new_scoped(
        kv,
        open_platform_skill_files(scope.clone()).await.unwrap(),
        scope,
    );
    first.clear_all_skills().await.unwrap();
    let package = SkillPackage {
        name: "shared-import".into(),
        files: BTreeMap::from([("SKILL.md".into(), md("shared-import").into_bytes())]),
    };
    let (left, right) = futures::join!(
        first.import_package(package.clone()),
        second.import_package(package)
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
}

#[wasm_bindgen_test]
async fn opfs_preserves_a_valid_root_level_proto_named_resource() {
    let backend = IndexedDbBackend::open("ains-opfs-skills-proto-resource")
        .await
        .unwrap()
        .store(TABLE_KV);
    backend.delete_prefix("skills_").await.unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(backend);
    let scope = "wasm-proto-resource".to_string();
    let store = SkillStore::new_scoped(
        kv,
        open_platform_skill_files(scope.clone()).await.unwrap(),
        scope,
    );
    store.clear_all_skills().await.unwrap();
    store
        .import_package(SkillPackage {
            name: "proto-resource".into(),
            files: BTreeMap::from([
                ("SKILL.md".into(), md("proto-resource").into_bytes()),
                ("__proto__".into(), b"root-level resource".to_vec()),
            ]),
        })
        .await
        .unwrap();

    assert_eq!(
        store
            .load_reference("proto-resource", "__proto__")
            .await
            .unwrap(),
        "root-level resource"
    );
}
