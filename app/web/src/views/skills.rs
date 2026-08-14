//! Skills 管理视图（Phase 6.4）：浏览 / 详情 / 删除，无导入入口。

use std::sync::Arc;

#[cfg(any(target_arch = "wasm32", feature = "desktop"))]
use std::collections::BTreeMap;

use dioxus::prelude::*;

#[cfg(any(target_arch = "wasm32", feature = "desktop"))]
use rust_agent::skills::SkillPackage;
#[cfg(all(feature = "desktop", not(target_arch = "wasm32")))]
use rust_agent::skills::{
    MAX_SKILL_MD_BYTES, MAX_SKILL_PACKAGE_BYTES, MAX_SKILL_PACKAGE_FILES, MAX_SKILL_RESOURCE_BYTES,
};
use rust_agent::skills::{SkillEntry, SkillLoader, SkillManage, SkillStore, SkillTrust};
use ui::{I18nContext, SkillCard, SkillDetailView, SkillTrustView, SkillsPanel};

use crate::agent::service;

/// Browser-only standard package transfer. The user selects a directory that
/// contains `SKILL.md`; all relative resource paths are transferred verbatim.
/// Export writes a `<skill-name>/` directory into a user-selected destination.
#[cfg(target_arch = "wasm32")]
mod browser_package_transfer {
    use super::*;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::JsFuture;

    #[wasm_bindgen(inline_js = r#"
export function importAinsSkillPackage() {
  if (!window.showDirectoryPicker) throw new Error('Directory import requires the File System Access API');
  return (async () => {
    const root = await window.showDirectoryPicker();
    // Keep in sync with rust_agent::skills::store MAX_SKILL_PACKAGE_FILES /
    // MAX_SKILL_PACKAGE_BYTES / MAX_SKILL_MD_BYTES / MAX_SKILL_RESOURCE_BYTES.
    const maxFiles = 256, maxBytes = 16 * 1024 * 1024, maxSkillMdBytes = 256 * 1024, maxResourceBytes = 2 * 1024 * 1024;
    // Agent Skills permits valid root-level resource names such as
    // `__proto__`.  A normal object would route that assignment through the
    // legacy prototype setter and silently drop the file before JSON encoding.
    const files = Object.create(null);
    let count = 0, total = 0;
    async function walk(dir, prefix) {
      for await (const [name, handle] of dir.entries()) {
        const path = prefix ? `${prefix}/${name}` : name;
        if (handle.kind === 'directory') await walk(handle, path);
        else {
          const file = await handle.getFile();
          if (++count > maxFiles) throw new Error(`Skill package exceeds ${maxFiles} files`);
          if (path === 'SKILL.md' && file.size > maxSkillMdBytes) throw new Error('SKILL.md is too large');
          if (file.size > maxResourceBytes && path !== 'SKILL.md') throw new Error(`Skill resource ${path} is too large`);
          if ((total += file.size) > maxBytes) throw new Error(`Skill package exceeds ${maxBytes} bytes`);
          files[path] = Array.from(new Uint8Array(await file.arrayBuffer()));
        }
      }
    }
    await walk(root, '');
    return JSON.stringify({ name: root.name, files });
  })();
}

export function exportAinsSkillPackage(json) {
  if (!window.showDirectoryPicker) throw new Error('Directory export requires the File System Access API');
  if (!navigator.locks) throw new Error('Directory export requires the Web Locks API');
  // File System Access has no create-if-absent directory primitive which also
  // identifies the creator. Serialize AINS exports across tabs so the check
  // below cannot race another export from this application.
  return navigator.locks.request('ains-skill-export-v1', {mode: 'exclusive'}, async () => {
    const payload = JSON.parse(json);
    const destination = await window.showDirectoryPicker({mode: 'readwrite'});
    try {
      await destination.getDirectoryHandle(payload.name);
      throw new Error(`Destination already contains ${payload.name}; choose an empty destination to avoid stale resources`);
    } catch (error) {
      if (!(error && error.name === 'NotFoundError')) throw error;
    }
    const root = await destination.getDirectoryHandle(payload.name, {create: true});
    try {
      for (const [path, bytes] of Object.entries(payload.files)) {
        const parts = path.split('/');
        const leaf = parts.pop();
        let directory = root;
        for (const part of parts) directory = await directory.getDirectoryHandle(part, {create: true});
        const writable = await (await directory.getFileHandle(leaf, {create: true})).createWritable();
        await writable.write(new Uint8Array(bytes));
        await writable.close();
      }
    } catch (error) {
      // Do not recursively remove `payload.name`: File System Access cannot
      // prove this tab still owns it after creation, and a separate process
      // may have populated it. Keeping an incomplete directory is preferable
      // to deleting another export's files.
      const detail = error && error.message ? error.message : String(error);
      throw new Error(`Skill export failed; ${payload.name} may be incomplete. Remove that directory before retrying. ${detail}`);
    }
  });
}
"#)]
    extern "C" {
        #[wasm_bindgen(catch)]
        fn importAinsSkillPackage() -> Result<js_sys::Promise, JsValue>;
        #[wasm_bindgen(catch)]
        fn exportAinsSkillPackage(json: &str) -> Result<js_sys::Promise, JsValue>;
    }

    #[derive(serde::Deserialize)]
    struct TransferPackage {
        name: String,
        files: BTreeMap<String, Vec<u8>>,
    }

    #[derive(serde::Serialize)]
    struct ExportPackage<'a> {
        name: &'a str,
        files: &'a BTreeMap<String, Vec<u8>>,
    }

    pub async fn import() -> Result<SkillPackage, String> {
        let promise = importAinsSkillPackage().map_err(|error| format!("{error:?}"))?;
        let json = JsFuture::from(promise)
            .await
            .map_err(|error| format!("{error:?}"))?
            .as_string()
            .ok_or_else(|| "directory picker returned invalid package data".to_string())?;
        let package: TransferPackage =
            serde_json::from_str(&json).map_err(|error| error.to_string())?;
        Ok(SkillPackage {
            name: package.name,
            files: package.files,
        })
    }

    pub async fn export(package: SkillPackage) -> Result<(), String> {
        let json = serde_json::to_string(&ExportPackage {
            name: &package.name,
            files: &package.files,
        })
        .map_err(|error| error.to_string())?;
        let promise = exportAinsSkillPackage(&json).map_err(|error| format!("{error:?}"))?;
        JsFuture::from(promise)
            .await
            .map_err(|error| format!("{error:?}"))?;
        Ok(())
    }
}

#[cfg(all(feature = "desktop", not(target_arch = "wasm32")))]
mod desktop_package_transfer {
    use super::*;
    use std::path::Path;

    fn collect(
        root: &Path,
        directory: &Path,
        files: &mut BTreeMap<String, Vec<u8>>,
    ) -> Result<(), String> {
        for entry in std::fs::read_dir(directory).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let metadata =
                std::fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_str()
                .ok_or_else(|| "skill path is not valid UTF-8".to_string())?
                .replace(std::path::MAIN_SEPARATOR, "/");
            if metadata.file_type().is_symlink() {
                return Err(format!("skill package may not contain symlink: {relative}"));
            }
            if metadata.is_dir() {
                collect(root, &entry.path(), files)?;
            } else if metadata.is_file() {
                if files.len() >= MAX_SKILL_PACKAGE_FILES {
                    return Err(format!(
                        "skill package exceeds {MAX_SKILL_PACKAGE_FILES} files"
                    ));
                }
                let size = usize::try_from(metadata.len())
                    .map_err(|_| "skill file is too large".to_string())?;
                let per_file_limit = if relative == "SKILL.md" {
                    MAX_SKILL_MD_BYTES
                } else {
                    MAX_SKILL_RESOURCE_BYTES
                };
                if size > per_file_limit {
                    return Err(format!("skill file {relative} is too large"));
                }
                let total = files
                    .values()
                    .try_fold(size, |total, bytes| {
                        total.checked_add(bytes.len()).ok_or(())
                    })
                    .map_err(|_| "skill package is too large".to_string())?;
                if total > MAX_SKILL_PACKAGE_BYTES {
                    return Err(format!(
                        "skill package exceeds {MAX_SKILL_PACKAGE_BYTES} bytes"
                    ));
                }
                files.insert(
                    relative,
                    std::fs::read(entry.path()).map_err(|error| error.to_string())?,
                );
            }
        }
        Ok(())
    }

    /// rfd 目录选择必须发生在主线程（macOS 约束），因此与阻塞的目录遍历
    /// 拆分：选择返回根路径，遍历交给 [`collect_from_root`] 在后台线程执行。
    pub fn pick_import_root() -> Result<std::path::PathBuf, String> {
        rfd::FileDialog::new()
            .pick_folder()
            .ok_or_else(|| "skill package selection cancelled".to_string())
    }

    /// 阻塞式递归收集：读整个目录树（大包可能耗时），应放到后台线程。
    pub fn collect_from_root(root: &Path) -> Result<SkillPackage, String> {
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "skill directory name is not valid UTF-8".to_string())?
            .to_string();
        let mut files = BTreeMap::new();
        collect(root, root, &mut files)?;
        Ok(SkillPackage { name, files })
    }

    pub fn choose_export_root() -> Result<std::path::PathBuf, String> {
        rfd::FileDialog::new()
            .pick_folder()
            .ok_or_else(|| "skill export selection cancelled".to_string())
    }

    pub fn export(package: SkillPackage, root: &Path) -> Result<(), String> {
        if std::fs::symlink_metadata(root.join(&package.name)).is_ok() {
            return Err(format!(
                "destination already contains {}; choose an empty destination to avoid stale resources",
                package.name
            ));
        }
        package
            .write_to_directory(root)
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

fn to_trust_view(trust: SkillTrust) -> SkillTrustView {
    match trust {
        SkillTrust::System => SkillTrustView::System,
        SkillTrust::Trusted => SkillTrustView::Trusted,
        SkillTrust::Generated => SkillTrustView::Generated,
        SkillTrust::Temporary => SkillTrustView::Temporary,
    }
}

/// Unix 毫秒 → `YYYY-MM-DD` 展示（面板粒度足够，避免时区库依赖）。
fn format_created_at(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn to_card(entry: &SkillEntry) -> SkillCard {
    match &entry.meta {
        Some(meta) => SkillCard {
            name: entry.name.clone(),
            description: entry
                .description
                .clone()
                .unwrap_or_else(|| meta.description.clone()),
            category: meta.category.clone(),
            trust: to_trust_view(meta.trust_level),
            created_at: format_created_at(meta.created_at),
            requires_tools: meta.requires_tools.clone(),
            corrupted: entry.corrupted,
        },
        None => SkillCard {
            name: entry.name.clone(),
            description: entry.description.clone().unwrap_or_default(),
            category: "-".into(),
            trust: SkillTrustView::Generated,
            created_at: "-".into(),
            requires_tools: vec![],
            // Standard imported packages do not carry AINS runtime metadata;
            // a valid SKILL.md is still browseable and usable.
            corrupted: entry.corrupted,
        },
    }
}

#[component]
pub fn Skills() -> Element {
    let i18n = use_context::<I18nContext>();
    let t = i18n.t();
    let client = use_context::<client_api::Client>();

    let mut store_sig = use_signal(|| None::<Arc<SkillStore>>);
    let mut cards = use_signal(Vec::<SkillCard>::new);
    let mut detail = use_signal(|| None::<SkillDetailView>);
    let mut error = use_signal(|| None::<String>);

    // 打开存储并加载列表（共享 KvStore 单例，不拉起 Kernel）
    let initial_client = client.clone();
    use_future(move || {
        let client = initial_client.clone();
        async move {
            match service::open_skill_store(client).await {
                Ok(store) => {
                    match store.list_entries().await {
                        Ok(entries) => {
                            cards.set(entries.iter().map(to_card).collect());
                            // 成功后清除历史错误横幅，避免瞬时故障文案常驻
                            error.set(None);
                        }
                        Err(err) => error.set(Some(format!("{}: {err}", t.skills_load_failed))),
                    }
                    store_sig.set(Some(store));
                }
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_load_failed))),
            }
        }
    });

    let reload = move || async move {
        let store = store_sig.read().clone();
        if let Some(store) = store {
            match store.list_entries().await {
                Ok(entries) => {
                    cards.set(entries.iter().map(to_card).collect());
                    error.set(None);
                }
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_load_failed))),
            }
        }
    };

    let on_open_detail = move |name: String| {
        let Some(store) = store_sig.read().clone() else {
            return;
        };
        spawn(async move {
            match store.load(&name).await {
                Ok(content) => {
                    let frontmatter = match &content.frontmatter {
                        serde_yaml::Value::Null => String::new(),
                        value => serde_yaml::to_string(value).unwrap_or_default(),
                    };
                    // 版本面（6.9）：可回滚候选 + 活跃版本（头指针权威源；
                    // 未版本化旧数据为空）
                    let rollback_versions =
                        store.rollback_candidates(&name).await.unwrap_or_default();
                    let active_version = store
                        .active_version(&name)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    detail.set(Some(SkillDetailView {
                        name,
                        frontmatter,
                        body: content.body,
                        rollback_versions,
                        active_version,
                    }));
                    error.set(None);
                }
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_load_failed))),
            }
        });
    };

    // 回滚（6.9）：目标版本内容提升为新大版本，刷新列表与详情
    let on_rollback = move |(name, version): (String, String)| {
        let Some(store) = store_sig.read().clone() else {
            return;
        };
        spawn(async move {
            match store.rollback_skill(&name, &version).await {
                Ok(_) => {
                    detail.set(None);
                    reload().await;
                }
                // 回滚失败用专属文案（非“加载失败”），候选过期等
                // 存储层错误对用户可归因
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_rollback_failed))),
            }
        });
    };

    let on_delete = move |name: String| {
        let Some(store) = store_sig.read().clone() else {
            return;
        };
        spawn(async move {
            match store.delete_skill(&name).await {
                Ok(()) => {
                    // 详情抽屉若正展示被删条目则一并关闭
                    if detail.read().as_ref().is_some_and(|d| d.name == name) {
                        detail.set(None);
                    }
                    reload().await;
                }
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_delete_failed))),
            }
        });
    };

    let on_clear_all = move |_| {
        let Some(store) = store_sig.read().clone() else {
            return;
        };
        spawn(async move {
            match store.clear_all_skills().await {
                Ok(_) => {
                    detail.set(None);
                    reload().await;
                }
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_clear_all_failed))),
            }
        });
    };

    #[cfg(target_arch = "wasm32")]
    let import_client = client.clone();
    #[cfg(target_arch = "wasm32")]
    let on_import = move |_| {
        let client = import_client.clone();
        spawn(async move {
            match browser_package_transfer::import().await {
                Ok(package) => match service::import_skill_package(client, package).await {
                    Ok(_) => reload().await,
                    Err(err) => error.set(Some(format!("{}: {err}", t.skills_import_failed))),
                },
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_import_failed))),
            }
        });
    };

    #[cfg(target_arch = "wasm32")]
    let export_client = client.clone();
    #[cfg(target_arch = "wasm32")]
    let on_export = move |_| {
        let Some(name) = detail.read().as_ref().map(|detail| detail.name.clone()) else {
            return;
        };
        let client = export_client.clone();
        spawn(async move {
            match service::export_skill_package(client, &name).await {
                Ok(package) => {
                    if let Err(err) = browser_package_transfer::export(package).await {
                        error.set(Some(format!("{}: {err}", t.skills_export_failed)));
                    }
                }
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_export_failed))),
            }
        });
    };

    #[cfg(all(feature = "desktop", not(target_arch = "wasm32")))]
    let desktop_import_client = client.clone();
    #[cfg(all(feature = "desktop", not(target_arch = "wasm32")))]
    let on_import = move |_| {
        // rfd 目录选择必须在主线程完成。
        let root = match desktop_package_transfer::pick_import_root() {
            Ok(root) => root,
            Err(err) => {
                error.set(Some(format!("{}: {err}", t.skills_import_failed)));
                return;
            }
        };
        let client = desktop_import_client.clone();
        spawn(async move {
            // 大目录的递归遍历放到阻塞线程池，避免卡住 UI / 异步执行器；
            // spawn_blocking 让 async 侧用 await 等待，而不是用阻塞的
            // `recv()` 占住执行器线程（macOS 下 rfd 选择须在主线程，
            // 因此收集与选择已经拆分，这里只负责等待收集结果）。
            let root = root.clone();
            let collected = tokio::task::spawn_blocking(move || {
                desktop_package_transfer::collect_from_root(&root)
            })
            .await;
            match collected {
                Ok(Ok(package)) => match service::import_skill_package(client, package).await {
                    Ok(_) => reload().await,
                    Err(err) => error.set(Some(format!("{}: {err}", t.skills_import_failed))),
                },
                Ok(Err(err)) => error.set(Some(format!("{}: {err}", t.skills_import_failed))),
                Err(join_err) => error.set(Some(format!("{}: {join_err}", t.skills_import_failed))),
            }
        });
    };

    #[cfg(all(feature = "desktop", not(target_arch = "wasm32")))]
    let desktop_export_client = client.clone();
    #[cfg(all(feature = "desktop", not(target_arch = "wasm32")))]
    let on_export = move |_| {
        let Some(name) = detail.read().as_ref().map(|detail| detail.name.clone()) else {
            return;
        };
        let root = match desktop_package_transfer::choose_export_root() {
            Ok(root) => root,
            Err(err) => {
                error.set(Some(format!("{}: {err}", t.skills_export_failed)));
                return;
            }
        };
        let client = desktop_export_client.clone();
        spawn(async move {
            match service::export_skill_package(client, &name).await {
                Ok(package) => {
                    if let Err(err) = desktop_package_transfer::export(package, &root) {
                        error.set(Some(format!("{}: {err}", t.skills_export_failed)));
                    }
                }
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_export_failed))),
            }
        });
    };

    #[cfg(all(not(target_arch = "wasm32"), not(feature = "desktop")))]
    let on_import = move |_| {};
    #[cfg(all(not(target_arch = "wasm32"), not(feature = "desktop")))]
    let on_export = move |_| {};

    rsx! {
        div { style: "padding:16px;",
            if cfg!(any(target_arch = "wasm32", feature = "desktop")) {
                div { style: "display:flex;gap:8px;margin-bottom:12px;",
                    button { r#type: "button", onclick: on_import, "{t.skills_import_btn}" }
                    if detail.read().is_some() {
                        button { r#type: "button", onclick: on_export, "{t.skills_export_btn}" }
                    }
                }
            }
            if let Some(err) = error.read().as_ref() {
                div { style: "margin-bottom:12px;color:var(--color-error-text);font-size:13px;",
                    "{err}"
                }
            }
            SkillsPanel {
                skills: cards,
                detail,
                on_open_detail,
                on_close_detail: move |_| detail.set(None),
                on_delete,
                on_clear_all,
                on_rollback,
            }
        }
    }
}
