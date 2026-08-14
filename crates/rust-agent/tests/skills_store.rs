//! Native contract: package content lives in an OS file tree, never in KV.
#![cfg(not(target_arch = "wasm32"))]

use rust_agent::memory::{KvStore, RedbBackend, TABLE_KV};
use rust_agent::platform::Platform;
use rust_agent::skills::{
    MAX_SKILL_MD_BYTES, MAX_SKILL_PACKAGE_FILES, MAX_SKILL_RESOURCE_BYTES, SkillContext,
    SkillLoader, SkillManage, SkillPackage, SkillPruner, SkillStore, SkillTrust, SkillVersion,
    open_platform_skill_files, validate_agent_skill_package,
};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct FailFirstSetKv {
    inner: Arc<dyn KvStore>,
    fail_next_set: AtomicBool,
}

struct FailOneSetKv {
    inner: Arc<dyn KvStore>,
    /// One-based `set` invocation to fail. `usize::MAX` disables injection.
    fail_on_set: std::sync::atomic::AtomicUsize,
    set_calls: std::sync::atomic::AtomicUsize,
}

/// Fail only the normal-import → system-trust promotion write. This keeps the
/// test independent of the number of KV writes made by package import itself.
struct FailSystemPromotionKv {
    inner: Arc<dyn KvStore>,
    fail_next_system_promotion: AtomicBool,
}

#[async_trait::async_trait]
impl KvStore for FailFirstSetKv {
    async fn get(
        &self,
        key: &str,
    ) -> Result<Option<serde_json::Value>, rust_agent::error::MemoryError> {
        self.inner.get(key).await
    }

    async fn set(
        &self,
        key: &str,
        value: &serde_json::Value,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), rust_agent::error::MemoryError> {
        if self.fail_next_set.swap(false, Ordering::SeqCst) {
            return Err(rust_agent::error::MemoryError::Storage(
                "injected set failure".into(),
            ));
        }
        self.inner.set(key, value, ttl).await
    }

    async fn delete(&self, key: &str) -> Result<(), rust_agent::error::MemoryError> {
        self.inner.delete(key).await
    }

    async fn list_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<String>, rust_agent::error::MemoryError> {
        self.inner.list_prefix(prefix).await
    }
}

#[async_trait::async_trait]
impl KvStore for FailOneSetKv {
    async fn get(
        &self,
        key: &str,
    ) -> Result<Option<serde_json::Value>, rust_agent::error::MemoryError> {
        self.inner.get(key).await
    }

    async fn set(
        &self,
        key: &str,
        value: &serde_json::Value,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), rust_agent::error::MemoryError> {
        let call = self.set_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.fail_on_set.load(Ordering::SeqCst) {
            return Err(rust_agent::error::MemoryError::Storage(
                "injected set failure".into(),
            ));
        }
        self.inner.set(key, value, ttl).await
    }

    async fn delete(&self, key: &str) -> Result<(), rust_agent::error::MemoryError> {
        self.inner.delete(key).await
    }

    async fn list_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<String>, rust_agent::error::MemoryError> {
        self.inner.list_prefix(prefix).await
    }
}

#[async_trait::async_trait]
impl KvStore for FailSystemPromotionKv {
    async fn get(
        &self,
        key: &str,
    ) -> Result<Option<serde_json::Value>, rust_agent::error::MemoryError> {
        self.inner.get(key).await
    }

    async fn set(
        &self,
        key: &str,
        value: &serde_json::Value,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), rust_agent::error::MemoryError> {
        let is_system_promotion = key.starts_with("skills_runtime:")
            && value.get("trust_level").and_then(serde_json::Value::as_str) == Some("system");
        if is_system_promotion
            && self
                .fail_next_system_promotion
                .swap(false, Ordering::SeqCst)
        {
            return Err(rust_agent::error::MemoryError::Storage(
                "injected system-promotion failure".into(),
            ));
        }
        self.inner.set(key, value, ttl).await
    }

    async fn delete(&self, key: &str) -> Result<(), rust_agent::error::MemoryError> {
        self.inner.delete(key).await
    }

    async fn list_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<String>, rust_agent::error::MemoryError> {
        self.inner.list_prefix(prefix).await
    }
}

fn md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n# Workflow\n\n1. Do the work.\n")
}
async fn store(dir: &tempfile::TempDir) -> SkillStore {
    let kv: Arc<dyn KvStore> = Arc::new(
        RedbBackend::open(dir.path().join("state.redb"))
            .unwrap()
            .table(TABLE_KV),
    );
    SkillStore::new(
        kv,
        open_platform_skill_files(dir.path().join("skills"))
            .await
            .unwrap(),
    )
}
fn context() -> SkillContext {
    SkillContext {
        platform: Platform::Desktop,
        available_tools: vec!["skill".into()],
    }
}

#[tokio::test]
async fn skill_md_is_file_source_of_truth_and_kv_contains_only_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(RedbBackend::open(dir.path().join("state.redb")).unwrap());
    let kv: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_KV));
    let store = SkillStore::new(
        Arc::clone(&kv),
        open_platform_skill_files(dir.path().join("skills"))
            .await
            .unwrap(),
    );
    store
        .create_skill("release-check", &md("release-check", "Run release checks"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("skills/release-check/SKILL.md")).unwrap(),
        md("release-check", "Run release checks")
    );
    assert!(
        kv.get("skills_index:release-check")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        kv.list_prefix("skills/").await.unwrap().is_empty(),
        "KV must not contain SKILL.md or package files"
    );
    assert_eq!(
        store
            .load_raw_for_context("release-check", &context())
            .await
            .unwrap(),
        md("release-check", "Run release checks")
    );
}

#[tokio::test]
async fn ains_platform_metadata_gates_discovery_and_contextual_loading() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let desktop_skill = "---\nname: platform-bound\ndescription: Desktop-only workflow\nmetadata:\n  ains.platforms: desktop\n---\n# Desktop\n";
    store
        .create_skill("platform-bound", desktop_skill)
        .await
        .unwrap();

    let web = SkillContext {
        platform: Platform::Web,
        available_tools: vec!["skill".into()],
    };
    assert!(store.list(&web).await.unwrap().is_empty());
    assert!(
        store
            .load_raw_for_context("platform-bound", &web)
            .await
            .is_err()
    );

    let desktop = context();
    assert_eq!(store.list(&desktop).await.unwrap().len(), 1);
    assert_eq!(
        store
            .load_raw_for_context("platform-bound", &desktop)
            .await
            .unwrap(),
        desktop_skill
    );

    // An update must refresh both metadata and the compact discovery index;
    // otherwise a stale platform gate can keep a valid new workflow hidden.
    let web_skill = "---\nname: platform-bound\ndescription: Web-only workflow\nmetadata:\n  ains.platforms: web\n---\n# Web\n";
    store
        .update_skill("platform-bound", web_skill)
        .await
        .unwrap();
    assert!(store.list(&desktop).await.unwrap().is_empty());
    assert!(
        store
            .load_raw_for_context("platform-bound", &desktop)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .load_raw_for_context("platform-bound", &web)
            .await
            .unwrap(),
        web_skill
    );

    let invalid = "---\nname: invalid-platform\ndescription: Invalid platform declaration\nmetadata:\n  ains.platforms: desktop toaster\n---\n# Invalid\n";
    assert!(
        store
            .create_skill("invalid-platform", invalid)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn failed_create_rolls_back_the_package_so_the_same_name_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let inner: Arc<dyn KvStore> = Arc::new(
        RedbBackend::open(dir.path().join("state.redb"))
            .unwrap()
            .table(TABLE_KV),
    );
    let failing: Arc<dyn KvStore> = Arc::new(FailFirstSetKv {
        inner,
        fail_next_set: AtomicBool::new(true),
    });
    let store = SkillStore::new(
        failing,
        open_platform_skill_files(dir.path().join("skills"))
            .await
            .unwrap(),
    );

    assert!(
        store
            .create_skill("retryable", &md("retryable", "Retry after failure"))
            .await
            .is_err()
    );
    assert!(store.list(&context()).await.unwrap().is_empty());

    store
        .create_skill("retryable", &md("retryable", "Retry after failure"))
        .await
        .unwrap();
    assert_eq!(store.list(&context()).await.unwrap().len(), 1);
}

#[tokio::test]
async fn failed_system_promotion_rolls_back_the_import_and_allows_retry() {
    let dir = tempfile::tempdir().unwrap();
    let inner: Arc<dyn KvStore> = Arc::new(
        RedbBackend::open(dir.path().join("state.redb"))
            .unwrap()
            .table(TABLE_KV),
    );
    let failing = Arc::new(FailSystemPromotionKv {
        inner,
        fail_next_system_promotion: AtomicBool::new(true),
    });
    let store = SkillStore::new(
        failing,
        open_platform_skill_files(dir.path().join("skills"))
            .await
            .unwrap(),
    );
    let package = SkillPackage {
        name: "retry-system".into(),
        files: BTreeMap::from([(
            "SKILL.md".into(),
            md("retry-system", "Host-provided workflow").into_bytes(),
        )]),
    };

    assert!(store.install_system_package(package.clone()).await.is_err());
    assert!(store.list_entries().await.unwrap().is_empty());
    assert!(store.load_raw("retry-system").await.is_err());

    // The injected failure is one-shot. A retry must not be rejected as a
    // duplicate and must restore the System protection boundary.
    store.install_system_package(package).await.unwrap();
    let entry = store.list_entries().await.unwrap().pop().unwrap();
    assert_eq!(entry.name, "retry-system");
    assert_eq!(entry.meta.unwrap().trust_level, SkillTrust::System);
    assert!(store.delete_skill("retry-system").await.is_err());
}

#[tokio::test]
async fn failed_update_restores_the_active_body_and_version_head() {
    let dir = tempfile::tempdir().unwrap();
    let inner: Arc<dyn KvStore> = Arc::new(
        RedbBackend::open(dir.path().join("state.redb"))
            .unwrap()
            .table(TABLE_KV),
    );
    let failing = Arc::new(FailOneSetKv {
        inner,
        fail_on_set: std::sync::atomic::AtomicUsize::new(usize::MAX),
        set_calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let store = SkillStore::new(
        failing.clone(),
        open_platform_skill_files(dir.path().join("skills"))
            .await
            .unwrap(),
    );
    let first = md("atomic-update", "original workflow");
    let second = md("atomic-update", "replacement workflow");
    store.create_skill("atomic-update", &first).await.unwrap();

    // `update_skill` refreshes the index, writes v1.1, deprecates v1.0,
    // replaces the active body/index/meta, then writes the head. Fail the
    // final head write, after the visible SKILL.md has already been replaced.
    let fail_head_write = failing.set_calls.load(Ordering::SeqCst) + 6;
    failing.fail_on_set.store(fail_head_write, Ordering::SeqCst);
    assert!(store.update_skill("atomic-update", &second).await.is_err());

    assert_eq!(store.load_raw("atomic-update").await.unwrap(), first);
    assert_eq!(
        store.active_version("atomic-update").await.unwrap(),
        Some("v1.0".into())
    );
    let versions = store.list_versions("atomic-update").await.unwrap();
    assert_eq!(
        versions
            .into_iter()
            .map(|(version, _)| version.label())
            .collect::<Vec<_>>(),
        ["v1.0"]
    );
    assert_eq!(
        store.list(&context()).await.unwrap()[0].description,
        "original workflow"
    );

    // The injected failure is one-shot. A retry starts from a coherent v1.0
    // state and can publish the replacement normally.
    store.update_skill("atomic-update", &second).await.unwrap();
    assert_eq!(store.load_raw("atomic-update").await.unwrap(), second);
    assert_eq!(
        store.active_version("atomic-update").await.unwrap(),
        Some("v1.1".into())
    );
}

#[tokio::test]
async fn startup_index_then_skill_then_resource_are_progressive() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("report", &md("report", "Build reports when requested"))
        .await
        .unwrap();
    store
        .put_file("report", "references/format.md", b"# Format")
        .await
        .unwrap();
    assert_eq!(
        store.list(&context()).await.unwrap()[0].description,
        "Build reports when requested"
    );
    assert_eq!(
        store.load("report").await.unwrap().body,
        "# Workflow\n\n1. Do the work.\n"
    );
    assert_eq!(
        store
            .load_file("report", "references/format.md")
            .await
            .unwrap(),
        b"# Format"
    );
}

#[tokio::test]
async fn exports_standard_directory_and_binary_asset() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("visual", &md("visual", "Create visual output"))
        .await
        .unwrap();
    store
        .put_file("visual", "assets/a.bin", &[0, 255, 1])
        .await
        .unwrap();
    let package = store.export_package("visual").await.unwrap();
    validate_agent_skill_package(&package).unwrap();
    assert_eq!(package.files.get("assets/a.bin"), Some(&vec![0, 255, 1]));
    let destination = tempfile::tempdir().unwrap();
    let output = package.write_to_directory(destination.path()).unwrap();
    assert_eq!(
        std::fs::read(output.join("assets/a.bin")).unwrap(),
        vec![0, 255, 1]
    );
    assert!(package.write_to_directory(destination.path()).is_err());
}

#[cfg(unix)]
#[test]
fn failed_directory_export_cleans_the_staging_directory_and_can_be_retried() {
    let destination = tempfile::tempdir().unwrap();
    // `valid_path` deliberately permits arbitrary user-facing resource names,
    // so a segment longer than the platform filename limit fails only after
    // the staging directory and SKILL.md have been created.
    let oversized_path = "x".repeat(300);
    let failing = SkillPackage {
        name: "retry-export".into(),
        files: BTreeMap::from([
            (
                "SKILL.md".into(),
                md("retry-export", "Retry a failed export").into_bytes(),
            ),
            (oversized_path, b"will not be written".to_vec()),
        ]),
    };
    assert!(failing.write_to_directory(destination.path()).is_err());
    assert!(!destination.path().join("retry-export").exists());
    assert!(
        std::fs::read_dir(destination.path())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".retry-export.ains-export-")),
        "a failed export must not leave a staging directory that prevents retry"
    );

    let retry = SkillPackage {
        name: "retry-export".into(),
        files: BTreeMap::from([(
            "SKILL.md".into(),
            md("retry-export", "Retry a failed export").into_bytes(),
        )]),
    };
    assert!(retry.write_to_directory(destination.path()).is_ok());
}

#[tokio::test]
async fn import_replaces_an_incomplete_package_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let stale = dir.path().join("skills/retry-import/references/stale.md");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(&stale, "must not survive retry").unwrap();

    store
        .import_package(SkillPackage {
            name: "retry-import".into(),
            files: BTreeMap::from([(
                "SKILL.md".into(),
                md("retry-import", "Clean retry").into_bytes(),
            )]),
        })
        .await
        .unwrap();

    let package = store.export_package("retry-import").await.unwrap();
    assert!(!package.files.contains_key("references/stale.md"));
}

#[test]
fn package_rejects_file_directory_path_conflicts() {
    let package = SkillPackage {
        name: "conflicting-paths".into(),
        files: BTreeMap::from([
            (
                "SKILL.md".into(),
                md("conflicting-paths", "Conflicting resources").into_bytes(),
            ),
            ("references".into(), b"not a directory".to_vec()),
            ("references/guide.md".into(), b"# Guide".to_vec()),
        ]),
    };
    assert!(validate_agent_skill_package(&package).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn resource_loader_refuses_external_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("safe-resource", &md("safe-resource", "Safe resources"))
        .await
        .unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, b"must not be exposed").unwrap();
    let resource = dir
        .path()
        .join("skills/safe-resource/references/external.txt");
    std::fs::create_dir_all(resource.parent().unwrap()).unwrap();
    symlink(&secret, resource).unwrap();

    assert!(
        store
            .load_file("safe-resource", "references/external.txt")
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unsafe_external_skill_does_not_hide_valid_skill_discovery() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("valid-skill", &md("valid-skill", "Usable skill"))
        .await
        .unwrap();
    let unsafe_dir = dir.path().join("skills/unsafe-skill");
    std::fs::create_dir_all(&unsafe_dir).unwrap();
    symlink(
        dir.path().join("skills/valid-skill/SKILL.md"),
        unsafe_dir.join("SKILL.md"),
    )
    .unwrap();

    let skills = store.list(&context()).await.unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "valid-skill");
}

#[tokio::test]
async fn package_and_resource_size_limits_reject_unbounded_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let oversized_body = format!(
        "---\nname: too-large\ndescription: too large\n---\n{}",
        "x".repeat(MAX_SKILL_MD_BYTES)
    );
    assert!(
        store
            .create_skill("too-large", &oversized_body)
            .await
            .is_err()
    );

    let oversized_resource = SkillPackage {
        name: "large-resource".into(),
        files: BTreeMap::from([
            (
                "SKILL.md".into(),
                md("large-resource", "Large resource").into_bytes(),
            ),
            (
                "references/data.bin".into(),
                vec![0; MAX_SKILL_RESOURCE_BYTES + 1],
            ),
        ]),
    };
    assert!(store.import_package(oversized_resource).await.is_err());

    store
        .create_skill(
            "bounded-resource",
            &md("bounded-resource", "Bounded resource"),
        )
        .await
        .unwrap();
    assert!(
        store
            .put_file(
                "bounded-resource",
                "references/large.bin",
                &vec![0; MAX_SKILL_RESOURCE_BYTES + 1],
            )
            .await
            .is_err()
    );

    let too_many = SkillPackage {
        name: "many-files".into(),
        files: std::iter::once((
            "SKILL.md".into(),
            md("many-files", "Many files").into_bytes(),
        ))
        .chain(
            (0..MAX_SKILL_PACKAGE_FILES).map(|index| (format!("references/{index}.txt"), vec![])),
        )
        .collect(),
    };
    assert!(store.import_package(too_many).await.is_err());

    let too_large_total = SkillPackage {
        name: "large-package".into(),
        files: std::iter::once((
            "SKILL.md".into(),
            md("large-package", "Large package").into_bytes(),
        ))
        .chain((0..8).map(|index| {
            (
                format!("assets/{index}.bin"),
                vec![0; MAX_SKILL_RESOURCE_BYTES],
            )
        }))
        .collect(),
    };
    assert!(store.import_package(too_large_total).await.is_err());
}

#[tokio::test]
async fn standard_import_and_system_packages_survive_clear_all() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let imported = SkillPackage {
        name: "imported".into(),
        files: BTreeMap::from([
            (
                "SKILL.md".into(),
                md("imported", "Imported workflow").into_bytes(),
            ),
            ("references/guide.md".into(), b"# Guide".to_vec()),
        ]),
    };
    store.import_package(imported).await.unwrap();
    let duplicate = SkillPackage {
        name: "imported".into(),
        files: BTreeMap::from([
            (
                "SKILL.md".into(),
                md("imported", "Replacement workflow").into_bytes(),
            ),
            ("references/guide.md".into(), b"# Replaced".to_vec()),
        ]),
    };
    assert!(store.import_package(duplicate).await.is_err());
    assert_eq!(
        store
            .load_reference("imported", "references/guide.md")
            .await
            .unwrap(),
        "# Guide"
    );
    let system = SkillPackage {
        name: "builtin".into(),
        files: BTreeMap::from([(
            "SKILL.md".into(),
            md("builtin", "Built-in workflow").into_bytes(),
        )]),
    };
    store.install_system_package(system).await.unwrap();
    assert_eq!(
        store
            .load_reference("imported", "references/guide.md")
            .await
            .unwrap(),
        "# Guide"
    );
    assert_eq!(store.clear_all_skills().await.unwrap(), 1);
    assert!(store.load_raw("imported").await.is_err());
    assert!(store.load_raw("builtin").await.is_ok());
    assert_eq!(store.list_versions("builtin").await.unwrap().len(), 1);
    assert!(store.delete_skill("builtin").await.is_err());
    assert!(store.load_raw("builtin").await.is_ok());
}

#[tokio::test]
async fn concurrent_creation_of_the_same_name_has_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(RedbBackend::open(dir.path().join("state.redb")).unwrap());
    let kv: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_KV));
    let root = dir.path().join("skills");
    let first = SkillStore::new(
        Arc::clone(&kv),
        open_platform_skill_files(&root).await.unwrap(),
    );
    let second = SkillStore::new(kv, open_platform_skill_files(&root).await.unwrap());
    let left_content = md("same", "first");
    let right_content = md("same", "second");
    let (left, right) = tokio::join!(
        first.create_skill("same", &left_content),
        second.create_skill("same", &right_content),
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
}

#[tokio::test]
async fn update_and_rollback_keep_bodies_in_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("review", &md("review", "v1"))
        .await
        .unwrap();
    store
        .update_skill("review", &md("review", "v2"))
        .await
        .unwrap();
    assert_eq!(
        store.active_version("review").await.unwrap().as_deref(),
        Some("v1.1")
    );
    assert!(
        dir.path()
            .join("skills/.ains-runtime/versions/review/v1.0.md")
            .exists()
    );
    store.rollback_skill("review", "v1.0").await.unwrap();
    assert!(
        store
            .load_raw("review")
            .await
            .unwrap()
            .contains("description: v1")
    );
}

#[tokio::test]
async fn update_on_corrupt_active_version_errors_instead_of_silent_reset() {
    // 防御回归（review P4）：head.active 无法解析时，update_skill 不得静默
    // 回退到初始版本（会把损坏状态"修复"成 v1.1，覆盖可能存在的更高版本），
    // 必须显式报错让上层感知内部状态损坏。
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(RedbBackend::open(dir.path().join("state.redb")).unwrap());
    let kv: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_KV));
    let store = SkillStore::new(
        Arc::clone(&kv),
        open_platform_skill_files(dir.path().join("skills"))
            .await
            .unwrap(),
    );
    store
        .create_skill("corrupt-head", &md("corrupt-head", "v1"))
        .await
        .unwrap();

    // 直接损坏 head.active（模拟外部写入 / 内部状态损坏）
    let head_key = "skills_head:corrupt-head";
    let mut head = kv.get(head_key).await.unwrap().unwrap();
    assert_eq!(head["active"], "v1.0");
    head["active"] = serde_json::Value::String("not-a-version".into());
    kv.set(head_key, &head, None).await.unwrap();

    let err = store
        .update_skill("corrupt-head", &md("corrupt-head", "v2"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            rust_agent::error::SkillsError::InvalidFormat(msg)
                if msg.contains("not parseable")
        ),
        "corrupt active must surface InvalidFormat, got {err:?}"
    );
    // 状态未被半途修改：版本文件未被创建，active 仍保持损坏值可被上层诊断
    assert_eq!(
        kv.get(head_key).await.unwrap().unwrap()["active"],
        "not-a-version"
    );
}

#[tokio::test]
async fn command_store_has_no_skill_without_explicit_create() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    assert!(store.list_entries().await.unwrap().is_empty());
    assert!(
        std::fs::read_dir(dir.path().join("skills"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn standard_directory_is_discovered_and_loadable_without_private_index() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let injected = dir.path().join("skills/manual/SKILL.md");
    std::fs::create_dir_all(injected.parent().unwrap()).unwrap();
    std::fs::write(
        &injected,
        md("manual", "This was not created by the command"),
    )
    .unwrap();

    assert_eq!(store.list(&context()).await.unwrap()[0].name, "manual");
    assert_eq!(store.list_entries().await.unwrap()[0].name, "manual");
    assert!(store.load_raw("manual").await.is_ok());

    store.clear_all_skills().await.unwrap();
    assert!(!injected.exists());
}

#[tokio::test]
async fn clear_all_removes_orphaned_workflow_versions_and_runtime_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(RedbBackend::open(dir.path().join("state.redb")).unwrap());
    let kv: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_KV));
    let root = dir.path().join("skills");
    let store = SkillStore::new(
        Arc::clone(&kv),
        open_platform_skill_files(&root).await.unwrap(),
    );
    store
        .create_skill("orphaned", &md("orphaned", "First version"))
        .await
        .unwrap();
    store
        .update_skill("orphaned", &md("orphaned", "Second version"))
        .await
        .unwrap();

    // Simulate an interruption after the package directory was removed but
    // before runtime snapshots and KV metadata could be cleaned up.
    std::fs::remove_dir_all(root.join("orphaned")).unwrap();
    assert!(
        root.join(".ains-runtime/versions/orphaned/v1.0.md")
            .exists()
    );
    assert!(
        !kv.list_prefix("skills_ver:orphaned:")
            .await
            .unwrap()
            .is_empty()
    );

    assert_eq!(store.clear_all_skills().await.unwrap(), 0);
    assert!(!root.join(".ains-runtime/versions/orphaned").exists());
    assert!(
        kv.list_prefix("skills_ver:orphaned:")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(kv.get("skills_index:orphaned").await.unwrap().is_none());
    assert!(kv.get("skills_runtime:orphaned").await.unwrap().is_none());
    assert!(kv.get("skills_head:orphaned").await.unwrap().is_none());
}

#[tokio::test]
async fn discovery_does_not_read_invalid_package_content() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("report", &md("report", "Initial description"))
        .await
        .unwrap();
    std::fs::write(dir.path().join("skills/report/SKILL.md"), "not a skill").unwrap();

    // A standard directory is authoritative, so a malformed replacement is
    // hidden rather than advertised from stale private metadata.
    assert!(store.list(&context()).await.unwrap().is_empty());
    assert!(store.load_raw("report").await.is_err());
}

#[tokio::test]
async fn corrupted_package_is_surfaced_in_list_entries_and_still_deletable() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("report", &md("report", "Build reports"))
        .await
        .unwrap();
    // Overwrite SKILL.md with unparseable content: the directory is present
    // (so the package exists on disk) but its frontmatter is invalid.
    std::fs::write(
        dir.path().join("skills/report/SKILL.md"),
        "# no frontmatter\n\njust a heading\n",
    )
    .unwrap();

    // The management listing must surface the degraded package so the user can
    // act on it, even though discovery (`list`) hides it from agents.
    let entries = store.list_entries().await.unwrap();
    let report = entries.iter().find(|e| e.name == "report").unwrap();
    assert!(report.corrupted, "corrupted package must be flagged");
    assert!(store.list(&context()).await.unwrap().is_empty());

    // It must also be deletable: previously `delete_skill` required a valid
    // frontmatter and would reject a corrupted package, leaving it orphaned.
    store.delete_skill("report").await.unwrap();
    assert!(!dir.path().join("skills/report").exists());
    assert!(store.list_entries().await.unwrap().is_empty());
}

#[tokio::test]
async fn creator_metadata_distinguishes_user_import_and_system_packages() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("by-hand", &md("by-hand", "Created inline"))
        .await
        .unwrap();
    store
        .import_package(SkillPackage {
            name: "imported-one".into(),
            files: BTreeMap::from([(
                "SKILL.md".into(),
                md("imported-one", "Imported from disk").into_bytes(),
            )]),
        })
        .await
        .unwrap();
    store
        .install_system_package(SkillPackage {
            name: "builtin-one".into(),
            files: BTreeMap::from([(
                "SKILL.md".into(),
                md("builtin-one", "Host-provided").into_bytes(),
            )]),
        })
        .await
        .unwrap();

    let entries = store.list_entries().await.unwrap();
    let meta_of = |name: &str| {
        entries
            .iter()
            .find(|e| e.name == name)
            .expect("entry exists")
            .meta
            .as_ref()
            .expect("meta present")
            .clone()
    };
    assert_eq!(meta_of("by-hand").creator, "user");
    assert_eq!(meta_of("imported-one").creator, "imported");
    let builtin = meta_of("builtin-one");
    assert_eq!(builtin.creator, "system");
    assert!(builtin.trust_level == SkillTrust::System);
}

#[tokio::test]
async fn discovery_hides_a_registered_skill_when_its_required_file_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("report", &md("report", "Build reports"))
        .await
        .unwrap();
    std::fs::remove_file(dir.path().join("skills/report/SKILL.md")).unwrap();

    // The metadata index remains compact, but the directory invariant is
    // checked without reading the body before it is exposed to an agent.
    assert!(store.list(&context()).await.unwrap().is_empty());
    assert!(store.list_entries().await.unwrap().is_empty());
    assert!(store.load_raw("report").await.is_err());
}

#[tokio::test]
async fn invalid_names_cannot_escape_the_skill_root() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), "keep").unwrap();

    assert!(store.delete_skill("../outside").await.is_err());
    assert!(outside.join("keep").exists());
}

#[tokio::test]
async fn pruning_and_clearing_remove_workflow_history_files() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(
        RedbBackend::open(dir.path().join("state.redb"))
            .unwrap()
            .table(TABLE_KV),
    );
    let store = SkillStore::with_retention(
        kv,
        open_platform_skill_files(dir.path().join("skills"))
            .await
            .unwrap(),
        1,
    );
    store
        .create_skill("review", &md("review", "v1"))
        .await
        .unwrap();
    store
        .update_skill("review", &md("review", "v2"))
        .await
        .unwrap();
    assert_eq!(SkillPruner.prune(&store, "review").await.unwrap(), 1);
    assert!(
        !dir.path()
            .join("skills/.ains-runtime/versions/review/v1.0.md")
            .exists()
    );

    store.clear_all_skills().await.unwrap();
    assert!(
        !dir.path()
            .join("skills/.ains-runtime/versions/review/v1.1.md")
            .exists()
    );
}

#[test]
fn validator_enforces_the_agent_skills_frontmatter_contract() {
    let valid = "---\nname: déploiement\ndescription: Deploy a service when a release is requested.\nlicense: Apache-2.0\ncompatibility: Requires git and network access\nmetadata:\n  author: example-org\n  version: \"1.0\"\nallowed-tools: Bash(git:*) Read\n---\n# Deploy\n";
    let package = |content: &str| rust_agent::skills::SkillPackage {
        name: "déploiement".into(),
        files: BTreeMap::from([("SKILL.md".into(), content.as_bytes().to_vec())]),
    };
    validate_agent_skill_package(&package(valid)).unwrap();

    // Package names become storage-directory names, so accepting a
    // compatibility spelling would allow two NFKC-equivalent packages to be
    // created side by side. The persisted name itself must already be NFKC.
    let compatibility_name = rust_agent::skills::SkillPackage {
        name: "ｄéploiement".into(),
        files: BTreeMap::from([(
            "SKILL.md".into(),
            "---\nname: déploiement\ndescription: valid\n---\n"
                .as_bytes()
                .to_vec(),
        )]),
    };
    assert!(validate_agent_skill_package(&compatibility_name).is_err());

    // The reference format allows only its six frontmatter fields, requires a
    // matching NFKC name and description, and defines allowed-tools as a
    // space-separated scalar rather than a YAML list.
    assert!(
        validate_agent_skill_package(&package(
            "---\nname: déploiement\ndescription: valid\nextra: no\n---\n"
        ))
        .is_err()
    );
    assert!(
        validate_agent_skill_package(&package(
            "---\nname: déploiement\ndescription: valid\nallowed-tools:\n  - Read\n---\n"
        ))
        .is_err()
    );
    assert!(
        validate_agent_skill_package(&package("---\nname: another\ndescription: valid\n---\n"))
            .is_err()
    );
    for frontmatter in [
        "license: null",
        "compatibility: null",
        "metadata: null",
        "allowed-tools: null",
        "license:\n  - Apache-2.0",
        "compatibility: 1",
        "metadata:\n  author: 1",
    ] {
        assert!(
            validate_agent_skill_package(&package(&format!(
                "---\nname: déploiement\ndescription: valid\n{frontmatter}\n---\n"
            )))
            .is_err(),
            "frontmatter must reject {frontmatter:?}"
        );
    }
}

#[tokio::test]
async fn rollback_missing_target_is_atomic_noop() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("review", &md("review", "v1"))
        .await
        .unwrap();
    assert!(store.rollback_skill("review", "v9.9").await.is_err());
    // A failed rollback must leave the active version, body, and version
    // history untouched.
    assert_eq!(
        store.active_version("review").await.unwrap().as_deref(),
        Some("v1.0")
    );
    assert!(
        store
            .load_raw("review")
            .await
            .unwrap()
            .contains("description: v1")
    );
    assert_eq!(store.list_versions("review").await.unwrap().len(), 1);
}

#[tokio::test]
async fn rollback_then_clear_all_removes_the_rolled_back_skill() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("review", &md("review", "v1"))
        .await
        .unwrap();
    store
        .update_skill("review", &md("review", "v2"))
        .await
        .unwrap();
    store.rollback_skill("review", "v1.0").await.unwrap();
    assert!(store.load_raw("review").await.is_ok());
    // The rollback writes a regular new version, so a subsequent clear_all
    // observes the consistent, single skill and removes it entirely.
    store.clear_all_skills().await.unwrap();
    assert!(store.load_raw("review").await.is_err());
    assert!(!dir.path().join("skills/review").exists());
}

#[tokio::test]
async fn prune_keeps_the_active_version_loadable() {
    let dir = tempfile::tempdir().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(
        RedbBackend::open(dir.path().join("state.redb"))
            .unwrap()
            .table(TABLE_KV),
    );
    let store = SkillStore::with_retention(
        kv,
        open_platform_skill_files(dir.path().join("skills"))
            .await
            .unwrap(),
        1,
    );
    store
        .create_skill("review", &md("review", "v1"))
        .await
        .unwrap();
    store
        .update_skill("review", &md("review", "v2"))
        .await
        .unwrap();
    assert_eq!(SkillPruner.prune(&store, "review").await.unwrap(), 1);
    // The active version survives pruning and stays loadable.
    assert_eq!(
        store.active_version("review").await.unwrap().as_deref(),
        Some("v1.1")
    );
    assert!(
        store
            .load_raw("review")
            .await
            .unwrap()
            .contains("description: v2")
    );
}

#[test]
fn version_bump_at_u32_max_boundary_returns_none() {
    // 回归防御（review）：minor/major 在 u32::MAX 时不得 panic（debug）或回绕（release）。
    let max_minor = SkillVersion {
        major: 1,
        minor: u32::MAX,
    };
    assert!(max_minor.next_minor().is_none());
    assert_eq!(max_minor.next_major().unwrap().label(), "v2.0");
    let max_major = SkillVersion {
        major: u32::MAX,
        minor: 0,
    };
    assert!(max_major.next_major().is_none());
    assert_eq!(max_major.next_minor().unwrap().label(), "v4294967295.1");
    // 常规递增不受影响。
    assert_eq!(SkillVersion::INITIAL.next_minor().unwrap().label(), "v1.1");
}

#[tokio::test]
async fn rollback_writes_an_auditable_new_version() {
    // rollback 是有意的 append-only 审计语义（git revert 而非 reset）：
    // 回滚后 head.active 指向新 minor 版本（内容与目标版本一致），版本列表
    // 单调增长且可观测；回滚候选列表受 max_retained 约束。
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store
        .create_skill("review", &md("review", "v1"))
        .await
        .unwrap();
    store
        .update_skill("review", &md("review", "v2"))
        .await
        .unwrap();
    store
        .update_skill("review", &md("review", "v3"))
        .await
        .unwrap();
    assert_eq!(
        store.active_version("review").await.unwrap().as_deref(),
        Some("v1.2")
    );
    // 回滚到 v1.0：head.active 前进到新版本 v1.3，内容等于 v1.0。
    store.rollback_skill("review", "v1.0").await.unwrap();
    assert_eq!(
        store.active_version("review").await.unwrap().as_deref(),
        Some("v1.3")
    );
    assert!(
        store
            .load_raw("review")
            .await
            .unwrap()
            .contains("description: v1")
    );
    // 版本列表 append-only：v1.0..=v1.3 共 4 个，全部可观测。
    let versions = store.list_versions("review").await.unwrap();
    assert_eq!(versions.len(), 4);
    assert_eq!(versions[0].0.label(), "v1.0");
    assert_eq!(versions[3].0.label(), "v1.3");
    // 再次回滚仍前进（v1.4），不会回退指针。
    store.rollback_skill("review", "v1.1").await.unwrap();
    assert_eq!(
        store.active_version("review").await.unwrap().as_deref(),
        Some("v1.4")
    );
    assert!(
        store
            .load_raw("review")
            .await
            .unwrap()
            .contains("description: v2")
    );
}

#[tokio::test]
async fn concurrent_stores_interleaved_update_and_rollback_stay_consistent() {
    // 两个 SkillStore 共享同一 KV 与目录，交错 update + rollback 时版本链
    // 必须保持单一活跃版本、目录与 KV 一致（目录是权威，load_raw 以
    // head.active 为准），且最终内容可加载。
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(RedbBackend::open(dir.path().join("state.redb")).unwrap());
    let kv: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_KV));
    let root = dir.path().join("skills");
    let first = SkillStore::new(
        Arc::clone(&kv),
        open_platform_skill_files(&root).await.unwrap(),
    );
    let second = SkillStore::new(kv, open_platform_skill_files(&root).await.unwrap());
    first
        .create_skill("inter", &md("inter", "v1"))
        .await
        .unwrap();
    let v2_content = md("inter", "v2");
    let (a, b) = tokio::join!(
        first.update_skill("inter", &v2_content),
        second.rollback_skill("inter", "v1.0"),
    );
    // 两个操作经共享 mutation gate + 跨 tab 锁串行化，任一顺序都必须成功：
    // - update 先：v1.0 → v1.1，rollback(v1.0) → v1.2（内容=v1）
    // - rollback 先：回滚 v1.0 → v1.1，update → v1.2（内容=v2）
    // 无论顺序，最终 head.active 均为 v1.2、版本列表 [v1.0, v1.1, v1.2]。
    assert!(a.is_ok() && b.is_ok());
    let versions = first.list_versions("inter").await.unwrap();
    assert_eq!(
        versions.len(),
        3,
        "interleaved update+rollback must keep exactly 3 versions"
    );
    assert_eq!(
        first.active_version("inter").await.unwrap().as_deref(),
        Some("v1.2")
    );
    assert_eq!(
        second.active_version("inter").await.unwrap().as_deref(),
        Some("v1.2")
    );
    // 内容取决于串行化顺序（v1 或 v2），但都必须可加载且与 head 一致。
    let raw = first.load_raw("inter").await.unwrap();
    assert!(raw.contains("description: v1") || raw.contains("description: v2"));
    assert!(second.load_raw("inter").await.is_ok());
    // 与第三个 store 的只读视图保持一致。
    let third_kv: Arc<dyn KvStore> = Arc::new(backend.table(TABLE_KV));
    let third = SkillStore::new(third_kv, open_platform_skill_files(&root).await.unwrap());
    assert_eq!(
        third.active_version("inter").await.unwrap().as_deref(),
        Some("v1.2")
    );
    assert_eq!(third.list_versions("inter").await.unwrap().len(), 3);
}
