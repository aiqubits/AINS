//! Agent Skills storage: package files are the source of truth; KV contains
//! only the progressive-discovery index and AINS runtime metadata.

use std::collections::BTreeMap;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::error::{MemoryError, SkillsError};
use crate::memory::{KvStore, now_ms};
use crate::platform::Platform;
use crate::skills::files::SkillFiles;
use crate::skills::{SkillContent, SkillContext, SkillLoader, SkillManage, SkillSummary};

pub const SKILL_INDEX_KEY_PREFIX: &str = "skills_index:";
pub const SKILL_RUNTIME_KEY_PREFIX: &str = "skills_runtime:";
pub const SKILL_META_KEY_PREFIX: &str = SKILL_RUNTIME_KEY_PREFIX;
pub const SKILL_SYSTEM_KEY_PREFIX: &str = "skills_system:";
pub const SKILL_VER_KEY_PREFIX: &str = "skills_ver:";
pub const SKILL_HEAD_KEY_PREFIX: &str = "skills_head:";
pub const SKILL_MD_FILE: &str = "SKILL.md";
pub const MAX_SKILL_NAME_CHARS: usize = 64;
/// Limits apply equally to browser directory imports and native stores. They
/// bound untrusted package transfer before resource text is ever injected into
/// an Agent tool result.
pub const MAX_SKILL_MD_BYTES: usize = 256 * 1024;
pub const MAX_SKILL_RESOURCE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SKILL_PACKAGE_FILES: usize = 256;
pub const MAX_SKILL_PACKAGE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_RETAINED_VERSIONS: usize = 3;
pub const AUTO_ROLLBACK_CONSECUTIVE_FAILURES: u32 = 5;

/// Serializes package creation across all store handles in this runtime. The
/// directory and KV index are a compound commit; without this gate two views
/// can both observe a missing name and overwrite one another.
///
/// Scope note (deliberate non-goal): this gate is process-local on native
/// builds. Sharing one skills directory across *multiple OS processes* is not
/// synchronized — currently `SkillStore` is only instantiated in the wasm web
/// app (where `with_cross_tab_mutation_lock` additionally takes an origin-wide
/// browser lease) and in-process tests. A desktop/server multi-process
/// deployment must add an OS-level file lock before sharing a directory.
static SKILL_MUTATION_GATE: OnceLock<Arc<futures::lock::Mutex<()>>> = OnceLock::new();

fn skill_mutation_gate() -> Arc<futures::lock::Mutex<()>> {
    Arc::clone(SKILL_MUTATION_GATE.get_or_init(|| Arc::new(futures::lock::Mutex::new(()))))
}

/// Browser tabs do not share Rust's process-local gate. Every package mutation
/// therefore also takes this origin-wide lease on Web, including imports and
/// clear/delete operations initiated directly from the management UI.
#[cfg(target_arch = "wasm32")]
mod browser_mutation_lock {
    use futures::channel::oneshot;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::JsFuture;

    use crate::error::SkillsError;

    #[wasm_bindgen(
        inline_js = "export function requestAinsSkillMutationLock(callback){if(!navigator.locks)throw new Error('Web Locks unavailable');return navigator.locks.request('ains-skill-mutation-v1',callback)}"
    )]
    extern "C" {
        // The JS export in the inline_js snippet is `requestAinsSkillMutationLock`
        // (camelCase). wasm-bindgen uses the Rust identifier verbatim as the JS
        // import binding — with no case conversion — so we must pin js_name to
        // the camelCase export. Otherwise wasm-bindgen emits
        // `import { request_ains_skill_mutation_lock }`, which does not exist in
        // the snippet, esbuild fails, and dx falls back to a copy that omits the
        // snippets dir → runtime 404 / white screen.
        #[wasm_bindgen(catch, js_name = "requestAinsSkillMutationLock")]
        fn request_ains_skill_mutation_lock(
            callback: &js_sys::Function,
        ) -> Result<js_sys::Promise, JsValue>;
    }

    pub async fn with_lock<T>(
        operation: impl std::future::Future<Output = Result<T, SkillsError>>,
    ) -> Result<T, SkillsError> {
        use wasm_bindgen::{JsCast, closure::Closure};
        use wasm_bindgen_futures::future_to_promise;

        let (acquired_tx, acquired_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let callback = Closure::once_into_js(move |_lock: JsValue| -> js_sys::Promise {
            let _ = acquired_tx.send(());
            future_to_promise(async move {
                let _ = release_rx.await;
                Ok(JsValue::UNDEFINED)
            })
        })
        .dyn_into::<js_sys::Function>()
        .map_err(|_| SkillsError::Storage("skill mutation lock callback unavailable".into()))?;
        let request = request_ains_skill_mutation_lock(&callback)
            .map_err(|error| SkillsError::Storage(format!("skill mutation lock: {error:?}")))?;
        acquired_rx
            .await
            .map_err(|_| SkillsError::Storage("skill mutation lock acquisition failed".into()))?;
        let result = operation.await;
        let _ = release_tx.send(());
        if JsFuture::from(request).await.is_err() && result.is_ok() {
            return Err(SkillsError::Storage(
                "skill mutation lock release failed".into(),
            ));
        }
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillStatus {
    Active,
    Deprecated,
    Expired,
    Revoked,
}
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SkillScore {
    pub successes: u32,
    pub failures: u32,
    pub consecutive_failures: u32,
}
impl SkillScore {
    pub fn success_rate(&self) -> f32 {
        let n = self.successes + self.failures;
        if n == 0 {
            0.
        } else {
            self.successes as f32 / n as f32
        }
    }
    pub fn record(&mut self, ok: bool) {
        if ok {
            self.successes += 1;
            self.consecutive_failures = 0
        } else {
            self.failures += 1;
            self.consecutive_failures += 1
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SkillVersion {
    pub major: u32,
    pub minor: u32,
}
impl SkillVersion {
    pub const INITIAL: Self = Self { major: 1, minor: 0 };
    pub fn parse(s: &str) -> Option<Self> {
        let (a, b) = s.strip_prefix('v')?.split_once('.')?;
        Some(Self {
            major: a.parse().ok()?,
            minor: b.parse().ok()?,
        })
    }
    pub fn label(self) -> String {
        format!("v{}.{}", self.major, self.minor)
    }
    /// Next minor version, or `None` when `minor == u32::MAX` (overflow guard).
    pub fn next_minor(self) -> Option<Self> {
        Some(Self {
            major: self.major,
            minor: self.minor.checked_add(1)?,
        })
    }
    /// Next major version, or `None` when `major == u32::MAX` (overflow guard).
    pub fn next_major(self) -> Option<Self> {
        Some(Self {
            major: self.major.checked_add(1)?,
            minor: 0,
        })
    }
}
/// KV version metadata deliberately excludes the workflow body; the body is
/// in `.ains-runtime/versions/<name>/<version>.md` in the same package FS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionRecord {
    pub checksum: String,
    pub status: SkillStatus,
    pub score: SkillScore,
    pub created_at: i64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillHead {
    pub active: String,
    pub meta: SkillMeta,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillTrust {
    System,
    Trusted,
    Generated,
    Temporary,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillMeta {
    pub description: String,
    pub category: String,
    pub requires_tools: Vec<String>,
    pub platforms: Vec<Platform>,
    pub trust_level: SkillTrust,
    pub creator: String,
    pub created_at: i64,
    pub permissions: Vec<String>,
    pub checksum: String,
}
#[derive(Debug, Clone, PartialEq)]
pub struct SkillEntry {
    pub name: String,
    pub description: Option<String>,
    pub meta: Option<SkillMeta>,
    pub corrupted: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillIndex {
    pub name: String,
    pub description: String,
    /// Optional AINS runtime platform gate. An empty list means every
    /// platform. It is derived from the portable `metadata.ains.platforms`
    /// frontmatter extension, so discovery does not need to reread every
    /// SKILL.md body on each prompt turn.
    #[serde(default)]
    pub platforms: Vec<Platform>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPackage {
    pub name: String,
    pub files: BTreeMap<String, Vec<u8>>,
}
impl SkillPackage {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn write_to_directory(
        &self,
        root: &std::path::Path,
    ) -> Result<std::path::PathBuf, SkillsError> {
        validate_agent_skill_package(self)?;
        let d = root.join(&self.name);
        match std::fs::symlink_metadata(&d) {
            Ok(_) => {
                return Err(SkillsError::InvalidFormat(format!(
                    "export destination already contains {}",
                    d.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
        // Build in a sibling staging directory and publish with one rename.
        // A full package export can fail after writing a few files (for
        // example disk-full); writing directly to `d` would then leave a
        // destination that the next export correctly refuses to overwrite.
        static EXPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let staging = (0..8)
            .find_map(|_| {
                let candidate = root.join(format!(
                    ".{}.ains-export-{}-{}",
                    self.name,
                    std::process::id(),
                    EXPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::create_dir(&candidate) {
                    Ok(()) => Some(Ok(candidate)),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => Some(Err(io(error))),
                }
            })
            .transpose()?
            .ok_or_else(|| {
                SkillsError::Storage("create unique skill export staging directory".into())
            })?;
        let write_result = (|| {
            for (p, b) in &self.files {
                let f = staging.join(p);
                std::fs::create_dir_all(f.parent().expect("validated resource has a parent"))
                    .map_err(io)?;
                std::fs::write(f, b).map_err(io)?;
            }
            publish_staged_directory_without_replacing(&staging, &d)
        })();
        if let Err(error) = write_result {
            // Only the private staging path is removed; never delete `d`,
            // which may have been created concurrently by another exporter.
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        Ok(d)
    }
}

/// Publish a completed directory without ever replacing a destination that
/// appeared after the initial absence check.  `std::fs::rename` replaces an
/// existing empty directory on Unix, so it cannot implement the export API's
/// no-overwrite promise by itself.
#[cfg(not(target_arch = "wasm32"))]
fn publish_staged_directory_without_replacing(
    staging: &std::path::Path,
    destination: &std::path::Path,
) -> Result<(), SkillsError> {
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
        target_os = "redox",
    ))]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        use rustix::io::Errno;

        renameat_with(CWD, staging, CWD, destination, RenameFlags::NOREPLACE).map_err(|error| {
            if error == Errno::EXIST {
                SkillsError::InvalidFormat(format!(
                    "export destination already contains {}",
                    destination.display()
                ))
            } else {
                SkillsError::Storage(format!(
                    "publish skill export without replacing destination: {error}"
                ))
            }
        })
    }

    // Windows `std::fs::rename` fails when the destination exists, unlike its
    // Unix counterpart.  On unsupported Unix targets, fail closed rather than
    // silently falling back to a replace-capable rename.
    #[cfg(target_os = "windows")]
    {
        return std::fs::rename(staging, destination).map_err(io);
    }
    #[cfg(all(
        unix,
        not(any(
            target_os = "android",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "visionos",
            target_os = "watchos",
            target_os = "redox",
        ))
    ))]
    {
        let _ = (staging, destination);
        Err(SkillsError::Storage(
            "atomic no-replace directory export is unsupported on this platform".into(),
        ))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(default)]
    metadata: Option<BTreeMap<String, String>>,
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: Option<String>,
}

/// AINS-owned metadata namespace for a portable, machine-readable platform
/// gate. Agent Skills `compatibility` remains free-form prose and must not be
/// guessed at runtime. Values are a comma- or whitespace-separated subset of
/// `web`, `desktop`, and `mobile`; omitting the key means all platforms.
const AINS_PLATFORM_METADATA_KEY: &str = "ains.platforms";

#[cfg(not(target_arch = "wasm32"))]
fn io(e: std::io::Error) -> SkillsError {
    SkillsError::Storage(e.to_string())
}
fn serr(e: MemoryError) -> SkillsError {
    SkillsError::Storage(e.to_string())
}
pub fn skill_checksum(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    format!("{:x}", h.finalize())
}
pub fn split_frontmatter(raw: &str) -> (String, String) {
    let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return (String::new(), raw.into());
    };
    for (i, _) in rest.match_indices("---") {
        let after = &rest[i + 3..];
        if (i == 0 || rest.as_bytes().get(i - 1) == Some(&b'\n'))
            && (after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n"))
        {
            return (
                rest[..i].trim_end().into(),
                after.trim_start_matches(['\r', '\n']).into(),
            );
        }
    }
    (String::new(), raw.into())
}
fn normalize(name: &str) -> String {
    name.nfkc().collect()
}
fn valid_name(name: &str) -> bool {
    let n = normalize(name);
    // Persisted package names are directory names and KV key components. Do
    // not merely validate their NFKC form: accepting a compatibility spelling
    // would allow it to coexist with its normalized equivalent as a second
    // package that users and frontmatter comparisons cannot distinguish.
    name == n
        && !n.is_empty()
        && n.chars().count() <= 64
        && n == n.to_lowercase()
        && !n.starts_with('-')
        && !n.ends_with('-')
        && !n.contains("--")
        && n.chars().all(|c| c.is_alphanumeric() || c == '-')
}
fn require_valid_name(name: &str) -> Result<(), SkillsError> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(SkillsError::InvalidFormat("invalid skill name".into()))
    }
}
fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path != SKILL_MD_FILE
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path
            .split('/')
            .any(|x| x.is_empty() || x == "." || x == "..")
        && !path.chars().any(char::is_control)
}
fn frontmatter_string<'a>(
    mapping: &'a serde_yaml::Mapping,
    field: &str,
    required: bool,
) -> Result<Option<&'a str>, SkillsError> {
    let key = serde_yaml::Value::String(field.into());
    match mapping.get(&key) {
        Some(serde_yaml::Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(SkillsError::InvalidFormat(format!(
            "frontmatter {field} must be a string"
        ))),
        None if required => Err(SkillsError::InvalidFormat(format!(
            "frontmatter {field} is required"
        ))),
        None => Ok(None),
    }
}
/// Enforce the Agent Skills frontmatter value types before deserializing it.
/// In particular, serde otherwise treats an explicit YAML `null` for an
/// optional field as if that field had been omitted.
fn validate_frontmatter_shape(document: &serde_yaml::Value) -> Result<(), SkillsError> {
    let mapping = document
        .as_mapping()
        .ok_or_else(|| SkillsError::InvalidFormat("frontmatter must be a mapping".into()))?;
    for field in ["name", "description"] {
        frontmatter_string(mapping, field, true)?;
    }
    for field in ["license", "compatibility", "allowed-tools"] {
        frontmatter_string(mapping, field, false)?;
    }
    if let Some(value) = mapping.get(serde_yaml::Value::String("metadata".into())) {
        let metadata = value.as_mapping().ok_or_else(|| {
            SkillsError::InvalidFormat("frontmatter metadata must be a string map".into())
        })?;
        for (key, value) in metadata {
            if !matches!(key, serde_yaml::Value::String(_))
                || !matches!(value, serde_yaml::Value::String(_))
            {
                return Err(SkillsError::InvalidFormat(
                    "frontmatter metadata must be a string map".into(),
                ));
            }
        }
    }
    Ok(())
}
/// 供 `SkillCreateTool` 在消耗一次性授权之前做与 `create_skill` 同源的完整
/// 输入预检（name 格式、frontmatter 一致性、体积上限）。与 `validate_content`
/// 的区别是这里不暴露内部 `Frontmatter` 解析产物。
pub(crate) fn validate_skill_create_input(name: &str, raw: &str) -> Result<(), SkillsError> {
    validate_content(name, raw).map(|_| ())
}

fn validate_content(name: &str, raw: &str) -> Result<Frontmatter, SkillsError> {
    if !valid_name(name) {
        return Err(SkillsError::InvalidFormat("invalid skill name".into()));
    }
    if raw.len() > MAX_SKILL_MD_BYTES {
        return Err(SkillsError::InvalidFormat(format!(
            "SKILL.md exceeds {MAX_SKILL_MD_BYTES} bytes"
        )));
    }
    let (fm, _) = split_frontmatter(raw);
    if fm.is_empty() {
        return Err(SkillsError::InvalidFormat(
            "SKILL.md requires YAML frontmatter".into(),
        ));
    }
    let document: serde_yaml::Value = serde_yaml::from_str(&fm)
        .map_err(|e| SkillsError::InvalidFormat(format!("frontmatter: {e}")))?;
    validate_frontmatter_shape(&document)?;
    let fm: Frontmatter = serde_yaml::from_value(document)
        .map_err(|e| SkillsError::InvalidFormat(format!("frontmatter: {e}")))?;
    if normalize(&fm.name) != normalize(name) || !valid_name(&fm.name) {
        return Err(SkillsError::InvalidFormat(
            "frontmatter name must match directory".into(),
        ));
    }
    if fm.description.trim().is_empty() || fm.description.chars().count() > 1024 {
        return Err(SkillsError::InvalidFormat(
            "description must be 1-1024 characters".into(),
        ));
    }
    if fm
        .compatibility
        .as_ref()
        .is_some_and(|x| x.is_empty() || x.chars().count() > 500)
    {
        return Err(SkillsError::InvalidFormat(
            "compatibility must be 1-500 characters".into(),
        ));
    }
    let _ = (&fm.license, &fm.allowed_tools);
    platforms_from_frontmatter(&fm)?;
    Ok(fm)
}

fn platforms_from_frontmatter(frontmatter: &Frontmatter) -> Result<Vec<Platform>, SkillsError> {
    let Some(value) = frontmatter
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(AINS_PLATFORM_METADATA_KEY))
    else {
        return Ok(vec![]);
    };

    let mut platforms = Vec::new();
    for name in value.split(|character: char| character == ',' || character.is_ascii_whitespace()) {
        if name.is_empty() {
            continue;
        }
        let platform = match name {
            "web" => Platform::Web,
            "desktop" => Platform::Desktop,
            "mobile" => Platform::Mobile,
            _ => {
                return Err(SkillsError::InvalidFormat(format!(
                    "metadata.{AINS_PLATFORM_METADATA_KEY} contains an unknown platform: {name}"
                )));
            }
        };
        if !platforms.contains(&platform) {
            platforms.push(platform);
        }
    }
    if platforms.is_empty() {
        return Err(SkillsError::InvalidFormat(format!(
            "metadata.{AINS_PLATFORM_METADATA_KEY} must name at least one platform"
        )));
    }
    Ok(platforms)
}

fn index_from_frontmatter(
    name: &str,
    frontmatter: &Frontmatter,
) -> Result<SkillIndex, SkillsError> {
    Ok(SkillIndex {
        name: name.into(),
        description: frontmatter.description.clone(),
        platforms: platforms_from_frontmatter(frontmatter)?,
    })
}

fn index_matches_context(index: &SkillIndex, context: &SkillContext) -> bool {
    index.platforms.is_empty() || index.platforms.contains(&context.platform)
}
pub fn validate_agent_skill_package(p: &SkillPackage) -> Result<(), SkillsError> {
    if p.files.len() > MAX_SKILL_PACKAGE_FILES {
        return Err(SkillsError::InvalidFormat(format!(
            "skill package exceeds {MAX_SKILL_PACKAGE_FILES} files"
        )));
    }
    let package_bytes = p.files.values().try_fold(0usize, |total, file| {
        total
            .checked_add(file.len())
            .ok_or_else(|| SkillsError::InvalidFormat("skill package is too large".into()))
    })?;
    if package_bytes > MAX_SKILL_PACKAGE_BYTES {
        return Err(SkillsError::InvalidFormat(format!(
            "skill package exceeds {MAX_SKILL_PACKAGE_BYTES} bytes"
        )));
    }
    let raw = std::str::from_utf8(
        p.files
            .get(SKILL_MD_FILE)
            .ok_or_else(|| SkillsError::InvalidFormat("missing SKILL.md".into()))?,
    )
    .map_err(|_| SkillsError::InvalidFormat("SKILL.md must be UTF-8".into()))?;
    validate_content(&p.name, raw)?;
    if p.files.keys().any(|x| x != SKILL_MD_FILE && !valid_path(x)) {
        return Err(SkillsError::InvalidFormat(
            "invalid package resource path".into(),
        ));
    }
    for path in p.files.keys().filter(|path| path.as_str() != SKILL_MD_FILE) {
        let mut ancestor = path.as_str();
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            if p.files.contains_key(parent) {
                return Err(SkillsError::InvalidFormat(format!(
                    "skill package file conflicts with resource directory: {parent}"
                )));
            }
            ancestor = parent;
        }
    }
    if p.files
        .iter()
        .any(|(path, bytes)| path != SKILL_MD_FILE && bytes.len() > MAX_SKILL_RESOURCE_BYTES)
    {
        return Err(SkillsError::InvalidFormat(format!(
            "skill resource exceeds {MAX_SKILL_RESOURCE_BYTES} bytes"
        )));
    }
    Ok(())
}

/// File-backed standard package store. `kv` is intentionally metadata-only.
pub struct SkillStore {
    kv: Arc<dyn KvStore>,
    files: Arc<dyn SkillFiles>,
    scope: Option<String>,
    max_retained: usize,
    mutation_gate: Arc<futures::lock::Mutex<()>>,
    /// Compact, process-local discovery metadata. Package directories remain
    /// authoritative; this cache prevents every prompt turn from rereading
    /// every SKILL.md body after the initial discovery pass.
    discovery: Arc<RwLock<BTreeMap<String, (SkillIndex, String)>>>,
}

struct UpdateRollbackState {
    content: String,
    index: Option<SkillIndex>,
    meta: Option<SkillMeta>,
    head: Option<SkillHead>,
    old_version: (String, Option<String>, Option<VersionRecord>),
    new_version: (String, Option<String>, Option<VersionRecord>),
}

impl SkillStore {
    pub fn new(kv: Arc<dyn KvStore>, files: Arc<dyn SkillFiles>) -> Self {
        Self {
            kv,
            files,
            scope: None,
            max_retained: 3,
            mutation_gate: skill_mutation_gate(),
            discovery: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }
    pub fn new_scoped(
        kv: Arc<dyn KvStore>,
        files: Arc<dyn SkillFiles>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            kv,
            files,
            scope: Some(scope.into()),
            max_retained: 3,
            mutation_gate: skill_mutation_gate(),
            discovery: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }
    pub fn with_retention(kv: Arc<dyn KvStore>, files: Arc<dyn SkillFiles>, n: usize) -> Self {
        Self {
            kv,
            files,
            scope: None,
            max_retained: n.max(1),
            mutation_gate: skill_mutation_gate(),
            discovery: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }
    fn key(&self, p: &str, n: &str) -> String {
        match &self.scope {
            Some(s) => format!("owner/{s}/{p}{n}"),
            None => format!("{p}{n}"),
        }
    }
    async fn with_cross_tab_mutation_lock<T>(
        &self,
        operation: impl std::future::Future<Output = Result<T, SkillsError>>,
    ) -> Result<T, SkillsError> {
        #[cfg(target_arch = "wasm32")]
        {
            browser_mutation_lock::with_lock(operation).await
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            operation.await
        }
    }
    fn index(&self, n: &str) -> String {
        self.key(SKILL_INDEX_KEY_PREFIX, n)
    }
    fn meta(&self, n: &str) -> String {
        self.key(SKILL_RUNTIME_KEY_PREFIX, n)
    }
    fn head(&self, n: &str) -> String {
        self.key(SKILL_HEAD_KEY_PREFIX, n)
    }
    fn ver(&self, n: &str, v: &str) -> String {
        self.key(SKILL_VER_KEY_PREFIX, &format!("{n}:{v}"))
    }
    fn ver_prefix(&self, n: &str) -> String {
        self.key(SKILL_VER_KEY_PREFIX, &format!("{n}:"))
    }
    /// Remove all runtime metadata that is not attached to a protected system
    /// package.  Package directories are not the sole source for this sweep:
    /// a crash or external deletion can leave versions/index records behind
    /// after the package itself has gone away.
    async fn clear_runtime_metadata_except(&self, protected: &[String]) -> Result<(), SkillsError> {
        let families = [
            (self.key(SKILL_INDEX_KEY_PREFIX, ""), false),
            (self.key(SKILL_RUNTIME_KEY_PREFIX, ""), false),
            (self.key(SKILL_HEAD_KEY_PREFIX, ""), false),
            (self.key(SKILL_VER_KEY_PREFIX, ""), true),
        ];
        for (prefix, versioned) in families {
            for key in self.kv.list_prefix(&prefix).await.map_err(serr)? {
                let Some(suffix) = key.strip_prefix(&prefix) else {
                    continue;
                };
                let name = if versioned {
                    suffix
                        .split_once(':')
                        .map(|(name, _)| name)
                        .unwrap_or(suffix)
                } else {
                    suffix
                };
                if protected
                    .iter()
                    .any(|protected_name| protected_name == name)
                {
                    continue;
                }
                self.kv.delete(&key).await.map_err(serr)?;
            }
        }
        Ok(())
    }
    async fn put_json<T: Serialize>(&self, k: &str, v: &T) -> Result<(), SkillsError> {
        self.kv
            .set(
                k,
                &serde_json::to_value(v).map_err(|e| SkillsError::Storage(e.to_string()))?,
                None,
            )
            .await
            .map_err(serr)
    }
    async fn get_json<T: for<'a> Deserialize<'a>>(
        &self,
        k: &str,
    ) -> Result<Option<T>, SkillsError> {
        match self.kv.get(k).await.map_err(serr)? {
            Some(v) => serde_json::from_value(v)
                .map(Some)
                .map_err(|e| SkillsError::Storage(e.to_string())),
            None => Ok(None),
        }
    }
    async fn raw(&self, n: &str) -> Result<Option<String>, SkillsError> {
        require_valid_name(n)?;
        match self.files.read_file(n, SKILL_MD_FILE).await? {
            Some(v) => String::from_utf8(v)
                .map(Some)
                .map_err(|_| SkillsError::InvalidFormat("SKILL.md must be UTF-8".into())),
            None => Ok(None),
        }
    }
    async fn write_active(&self, n: &str, c: &str, meta: SkillMeta) -> Result<(), SkillsError> {
        self.files
            .write_file(n, SKILL_MD_FILE, c.as_bytes())
            .await?;
        let index = SkillIndex {
            name: n.into(),
            description: meta.description.clone(),
            platforms: meta.platforms.clone(),
        };
        self.put_json(&self.index(n), &index).await?;
        self.cache_index(index).await;
        self.put_json(&self.meta(n), &meta).await
    }
    async fn cache_index(&self, index: SkillIndex) {
        let revision = self
            .files
            .file_revision(&index.name, SKILL_MD_FILE)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        self.discovery
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(index.name.clone(), (index, revision));
    }
    fn remove_cached_index(&self, name: &str) {
        self.discovery
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(name);
    }
    /// A valid standard package is discoverable directly from its directory.
    /// The KV index is only a cache/runtime aid, never an authority boundary.
    async fn registered_index(&self, n: &str) -> Result<SkillIndex, SkillsError> {
        require_valid_name(n)?;
        let content = self
            .raw(n)
            .await?
            .ok_or_else(|| SkillsError::NotFound(n.into()))?;
        let frontmatter = validate_content(n, &content)?;
        let index = index_from_frontmatter(n, &frontmatter)?;
        // Best-effort cache refresh. A read-only/imported package must remain
        // usable even when runtime metadata storage is unavailable.
        let _ = self.put_json(&self.index(n), &index).await;
        self.cache_index(index.clone()).await;
        Ok(index)
    }
    async fn registered_indexes(&self) -> Result<Vec<SkillIndex>, SkillsError> {
        let names = self.files.list_packages().await?;
        let names: Vec<_> = names.into_iter().filter(|name| valid_name(name)).collect();
        let known = self
            .discovery
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let mut indexes = Vec::new();
        for name in &names {
            // A manually added malformed or unsafe package must not make the
            // whole library undiscoverable. `SKILL.md` is untrusted until its
            // path and frontmatter have both passed validation.
            let revision = match self.files.file_revision(name, SKILL_MD_FILE).await {
                Ok(revision) => revision,
                Err(_) => continue,
            };
            if let (Some((index, cached_revision)), Some(revision)) = (known.get(name), revision)
                && cached_revision == &revision
            {
                indexes.push(index.clone());
            } else if let Ok(index) = self.registered_index(name).await {
                indexes.push(index);
            }
        }
        self.discovery
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|name, _| names.iter().any(|current| current == name));
        indexes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(indexes)
    }
    /// `SKILL.md` remains the authoritative content.  Once a package is
    /// registered, a successful activation refreshes only its compact KV
    /// discovery summary; it cannot register a new on-disk directory.
    async fn refresh_registered_index(
        &self,
        n: &str,
        content: &str,
    ) -> Result<SkillIndex, SkillsError> {
        let frontmatter = validate_content(n, content)?;
        let index = index_from_frontmatter(n, &frontmatter)?;
        let current = self
            .discovery
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(n)
            .map(|(index, _)| index.clone());
        if current.as_ref() != Some(&index) {
            self.put_json(&self.index(n), &index).await?;
        }
        self.cache_index(index.clone()).await;
        Ok(index)
    }
    async fn summary(&self, n: &str) -> Result<SkillSummary, SkillsError> {
        let content = self
            .raw(n)
            .await?
            .ok_or_else(|| SkillsError::NotFound(n.into()))?;
        let i = self.refresh_registered_index(n, &content).await?;
        Ok(SkillSummary {
            name: i.name,
            description: i.description,
            category: "general".into(),
            requires_tools: vec![],
        })
    }
    /// Best-effort rollback for a package that was never successfully
    /// committed.  Creation is a file + KV compound operation; leaving its
    /// `SKILL.md` behind after a metadata failure makes the incomplete package
    /// discoverable and prevents the user from retrying the same import.
    async fn discard_uncommitted_package(&self, n: &str) {
        let _ = self.files.remove_package(n).await;
        let _ = self.files.remove_versions(n).await;
        for key in [self.index(n), self.meta(n), self.head(n)] {
            let _ = self.kv.delete(&key).await;
        }
        let _ = self.kv.delete_prefix(&self.ver_prefix(n)).await;
        self.remove_cached_index(n);
    }
    async fn restore_version_state(
        &self,
        n: &str,
        version: &str,
        body: Option<String>,
        record: Option<VersionRecord>,
    ) {
        match body {
            Some(body) => {
                let _ = self.files.write_version(n, version, &body).await;
            }
            None => {
                let _ = self.files.remove_version(n, version).await;
            }
        }
        match record {
            Some(record) => {
                let _ = self.put_json(&self.ver(n, version), &record).await;
            }
            None => {
                let _ = self.kv.delete(&self.ver(n, version)).await;
            }
        }
    }

    /// Restore the source-of-truth workflow and runtime records after an
    /// existing package update fails part way through. Updating a package is a
    /// file + several KV writes; without this rollback, a failed head write can
    /// leave the new `SKILL.md` visible while `active_version` identifies the
    /// old snapshot.
    async fn restore_failed_update(&self, n: &str, state: &UpdateRollbackState) {
        let _ = self
            .files
            .write_file(n, SKILL_MD_FILE, state.content.as_bytes())
            .await;
        self.restore_version_state(
            n,
            &state.old_version.0,
            state.old_version.1.clone(),
            state.old_version.2.clone(),
        )
        .await;
        self.restore_version_state(
            n,
            &state.new_version.0,
            state.new_version.1.clone(),
            state.new_version.2.clone(),
        )
        .await;

        match state.index.clone() {
            Some(index) => {
                let _ = self.put_json(&self.index(n), &index).await;
                self.cache_index(index).await;
            }
            None => {
                let _ = self.kv.delete(&self.index(n)).await;
                self.remove_cached_index(n);
            }
        }
        match state.meta.clone() {
            Some(meta) => {
                let _ = self.put_json(&self.meta(n), &meta).await;
            }
            None => {
                let _ = self.kv.delete(&self.meta(n)).await;
            }
        }
        match state.head.clone() {
            Some(head) => {
                let _ = self.put_json(&self.head(n), &head).await;
            }
            None => {
                let _ = self.kv.delete(&self.head(n)).await;
            }
        }
    }
    /// Caller holds `mutation_gate`. Keeping the package body, references and
    /// metadata in one transaction prevents a concurrent clear/import from
    /// observing a half-imported directory.
    async fn create_skill_unlocked(
        &self,
        n: &str,
        c: &str,
        creator: &str,
    ) -> Result<SkillSummary, SkillsError> {
        let fm = validate_content(n, c)?;
        let platforms = platforms_from_frontmatter(&fm)?;
        if self.get_json::<SkillIndex>(&self.index(n)).await?.is_some()
            || self.raw(n).await?.is_some()
        {
            return Err(SkillsError::InvalidFormat("skill already exists".into()));
        }
        let meta = SkillMeta {
            description: fm.description,
            category: "general".into(),
            requires_tools: vec![],
            platforms,
            trust_level: SkillTrust::Trusted,
            creator: creator.into(),
            created_at: now_ms(),
            permissions: vec![],
            checksum: skill_checksum(c),
        };
        let result = async {
            self.files
                .write_file(n, SKILL_MD_FILE, c.as_bytes())
                .await?;
            self.create_version(n, SkillVersion::INITIAL, c, SkillStatus::Active)
                .await?;
            self.put_json(&self.meta(n), &meta).await?;
            self.put_json(
                &self.head(n),
                &SkillHead {
                    active: SkillVersion::INITIAL.label(),
                    meta: meta.clone(),
                },
            )
            .await?;
            self.put_json(
                &self.index(n),
                &SkillIndex {
                    name: n.into(),
                    description: meta.description.clone(),
                    platforms: meta.platforms.clone(),
                },
            )
            .await?;
            self.summary(n).await
        }
        .await;
        if result.is_err() {
            self.discard_uncommitted_package(n).await;
        }
        result
    }
    pub async fn export_package(&self, n: &str) -> Result<SkillPackage, SkillsError> {
        self.registered_index(n).await?;
        let mut files = BTreeMap::new();
        for p in self.files.list_files(n).await? {
            if let Some(b) = self.files.read_file(n, &p).await? {
                files.insert(p, b);
            }
        }
        let p = SkillPackage {
            name: n.into(),
            files,
        };
        validate_agent_skill_package(&p)?;
        Ok(p)
    }
    /// Import a portable Agent Skills directory package. A package needs no
    /// AINS-specific index; only its standard `SKILL.md` is authoritative.
    /// Caller holds `mutation_gate`.
    async fn import_package_unlocked(
        &self,
        package: SkillPackage,
    ) -> Result<SkillSummary, SkillsError> {
        validate_agent_skill_package(&package)?;
        let name = package.name.clone();
        if self
            .get_json::<SkillIndex>(&self.index(&name))
            .await?
            .is_some()
            || self.raw(&name).await?.is_some()
        {
            return Err(SkillsError::InvalidFormat("skill already exists".into()));
        }
        let content = String::from_utf8(
            package
                .files
                .get(SKILL_MD_FILE)
                .cloned()
                .expect("validated package has SKILL.md"),
        )
        .map_err(|_| SkillsError::InvalidFormat("SKILL.md must be UTF-8".into()))?;
        // A prior interrupted import can leave resources without a SKILL.md.
        // They are not a package yet, so remove them before starting instead
        // of allowing a retry to inherit files absent from the new package.
        self.files.remove_package(&name).await?;
        // Resources are written before the final SKILL.md visibility commit.
        // A failed transfer therefore leaves, at most, an undiscoverable
        // directory instead of advertising a partially imported skill.
        let result = async {
            for (path, bytes) in package.files {
                if path != SKILL_MD_FILE {
                    self.files.write_file(&name, &path, &bytes).await?;
                }
            }
            self.create_skill_unlocked(&name, &content, "imported")
                .await
        }
        .await;
        if result.is_err() {
            self.discard_uncommitted_package(&name).await;
        }
        result
    }
    pub async fn import_package(&self, package: SkillPackage) -> Result<SkillSummary, SkillsError> {
        self.with_cross_tab_mutation_lock(async {
            let _gate = self.mutation_gate.lock().await;
            self.import_package_unlocked(package).await
        })
        .await
    }

    /// Register a host-provided built-in package. It remains a normal standard
    /// directory package, while AINS runtime metadata prevents user clear/delete
    /// controls from removing it.
    pub async fn install_system_package(
        &self,
        package: SkillPackage,
    ) -> Result<SkillSummary, SkillsError> {
        self.with_cross_tab_mutation_lock(async {
            let _gate = self.mutation_gate.lock().await;
            let summary = self.import_package_unlocked(package).await?;
            // Import first writes a regular package, then promotes its runtime
            // metadata to System. Those are separate stores, so a failed
            // promotion must not leave a successfully imported but deletable
            // package behind: retries would otherwise reject the duplicate and
            // the host's built-in could be removed by user clear/delete paths.
            let promotion = async {
                let mut meta = self
                    .get_json::<SkillMeta>(&self.meta(&summary.name))
                    .await?
                    .ok_or_else(|| SkillsError::NotFound(summary.name.clone()))?;
                meta.trust_level = SkillTrust::System;
                meta.creator = "system".into();
                self.put_json(&self.meta(&summary.name), &meta).await?;
                if let Some(mut head) = self
                    .get_json::<SkillHead>(&self.head(&summary.name))
                    .await?
                {
                    head.meta = meta;
                    self.put_json(&self.head(&summary.name), &head).await?;
                }
                Ok(())
            }
            .await;
            if promotion.is_err() {
                self.discard_uncommitted_package(&summary.name).await;
            }
            promotion.map(|()| summary)
        })
        .await
    }
    pub async fn load_raw_for_context(
        &self,
        n: &str,
        ctx: &SkillContext,
    ) -> Result<String, SkillsError> {
        if !ctx.available_tools.iter().any(|x| x == "skill") {
            return Err(SkillsError::InvalidFormat("skill unavailable".into()));
        }
        let index = self.registered_index(n).await?;
        if !index_matches_context(&index, ctx) {
            return Err(SkillsError::InvalidFormat(
                "skill is unavailable on this platform".into(),
            ));
        }
        self.load_raw(n).await
    }
    pub async fn load_raw(&self, n: &str) -> Result<String, SkillsError> {
        let c = self
            .raw(n)
            .await?
            .ok_or_else(|| SkillsError::NotFound(n.into()))?;
        self.refresh_registered_index(n, &c).await?;
        Ok(c)
    }
    pub async fn load_file(&self, n: &str, p: &str) -> Result<Vec<u8>, SkillsError> {
        require_valid_name(n)?;
        if !valid_path(p) {
            return Err(SkillsError::InvalidFormat("invalid resource path".into()));
        }
        self.load_raw(n).await?;
        let file = self
            .files
            .read_file(n, p)
            .await?
            .ok_or_else(|| SkillsError::NotFound(format!("{n}/{p}")))?;
        if file.len() > MAX_SKILL_RESOURCE_BYTES {
            return Err(SkillsError::InvalidFormat(format!(
                "skill resource exceeds {MAX_SKILL_RESOURCE_BYTES} bytes"
            )));
        }
        Ok(file)
    }
    pub async fn put_file(&self, n: &str, p: &str, b: &[u8]) -> Result<(), SkillsError> {
        require_valid_name(n)?;
        if !valid_path(p) {
            return Err(SkillsError::InvalidFormat("invalid resource path".into()));
        }
        if b.len() > MAX_SKILL_RESOURCE_BYTES {
            return Err(SkillsError::InvalidFormat(format!(
                "skill resource exceeds {MAX_SKILL_RESOURCE_BYTES} bytes"
            )));
        }
        self.load_raw(n).await?;
        self.files.write_file(n, p, b).await
    }
    pub async fn put_reference(&self, n: &str, p: &str, c: &str) -> Result<(), SkillsError> {
        self.put_file(n, p, c.as_bytes()).await
    }
    pub async fn list_entries(&self) -> Result<Vec<SkillEntry>, SkillsError> {
        let mut out = Vec::new();
        // Iterate the on-disk packages directly so that a package with a valid
        // name but a corrupted `SKILL.md` (present yet unparseable) is still
        // surfaced (marked `corrupted`) and thus remains manageable through the
        // management UI. Invalid-named directories are never surfaced, and a
        // package whose `SKILL.md` is missing stays hidden (it is still
        // deletable via `delete_skill`). Both degraded cases are surfaced by
        // `delete_skill`'s name-only gate.
        for n in self.files.list_packages().await? {
            if !valid_name(&n) {
                continue;
            }
            let raw = self.raw(&n).await?;
            let Some(content) = raw.as_deref() else {
                continue;
            };
            let index = self.refresh_registered_index(&n, content).await.ok();
            let meta = self
                .get_json::<SkillMeta>(&self.meta(&n))
                .await
                .ok()
                .flatten();
            let corrupted = validate_content(&n, content).is_err() || index.is_none();
            out.push(SkillEntry {
                name: n,
                description: index.map(|x| x.description),
                meta,
                corrupted,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
    pub async fn list_versions(
        &self,
        n: &str,
    ) -> Result<Vec<(SkillVersion, VersionRecord)>, SkillsError> {
        self.registered_index(n).await?;
        let mut out = vec![];
        for k in self
            .kv
            .list_prefix(&self.ver_prefix(n))
            .await
            .map_err(serr)?
        {
            let Some(v) = k
                .strip_prefix(&self.ver_prefix(n))
                .and_then(SkillVersion::parse)
            else {
                continue;
            };
            if let Some(r) = self.get_json(&k).await? {
                out.push((v, r));
            }
        }
        out.sort_by_key(|x| x.0);
        Ok(out)
    }
    pub async fn active_version(&self, n: &str) -> Result<Option<String>, SkillsError> {
        self.registered_index(n).await?;
        Ok(self
            .get_json::<SkillHead>(&self.head(n))
            .await?
            .map(|x| x.active))
    }
    pub async fn rollback_candidates(&self, n: &str) -> Result<Vec<String>, SkillsError> {
        Ok(self
            .list_versions(n)
            .await?
            .into_iter()
            .rev()
            .take(self.max_retained)
            .map(|x| x.0.label())
            .collect())
    }
    async fn create_version(
        &self,
        n: &str,
        v: SkillVersion,
        c: &str,
        status: SkillStatus,
    ) -> Result<(), SkillsError> {
        require_valid_name(n)?;
        self.files.write_version(n, &v.label(), c).await?;
        self.put_json(
            &self.ver(n, &v.label()),
            &VersionRecord {
                checksum: skill_checksum(c),
                status,
                score: SkillScore::default(),
                created_at: now_ms(),
            },
        )
        .await
    }

    /// Write a new version with the cross-tab mutation lock and the in-process
    /// mutation gate already held. `update_skill` and `rollback_skill` share
    /// this so a rollback's version read and write form one atomic mutation.
    async fn update_skill_locked(&self, n: &str, c: &str) -> Result<SkillSummary, SkillsError> {
        {
            self.registered_index(n).await?;
            let old_content = self
                .raw(n)
                .await?
                .ok_or_else(|| SkillsError::NotFound(n.into()))?;
            let old_index = self.get_json::<SkillIndex>(&self.index(n)).await?;
            let old_meta = self.get_json::<SkillMeta>(&self.meta(n)).await?;
            let mut head = self
                .get_json::<SkillHead>(&self.head(n))
                .await?
                .ok_or_else(|| SkillsError::NotFound(n.into()))?;
            let old_head = head.clone();
            let fm = validate_content(n, c)?;
            let platforms = platforms_from_frontmatter(&fm)?;
            // 防御内部状态损坏：head.active 无法解析时不再静默回退到初始
            // 版本（会把损坏状态"修复"成 v1.1 覆盖可能存在的更高版本），
            // 而是显式报错，让上层感知并处理（review P4）。
            let old = SkillVersion::parse(&head.active).ok_or_else(|| {
                SkillsError::InvalidFormat(format!(
                    "skill {n} active version {:?} is not parseable",
                    head.active
                ))
            })?;
            // 版本号溢出（u32::MAX）理论上不可达，但绝不 panic 或回绕：显式报错。
            let next = old.next_minor().ok_or_else(|| {
                SkillsError::InvalidFormat(format!("skill {n} version overflow at {}", old.label()))
            })?;
            let old_label = old.label();
            let next_label = next.label();
            let old_version_body = self.files.read_version(n, &old_label).await?;
            let old_version_record = self
                .get_json::<VersionRecord>(&self.ver(n, &old_label))
                .await?;
            let next_version_body = self.files.read_version(n, &next_label).await?;
            let next_version_record = self
                .get_json::<VersionRecord>(&self.ver(n, &next_label))
                .await?;
            let rollback_state = UpdateRollbackState {
                content: old_content,
                index: old_index,
                meta: old_meta,
                head: Some(old_head),
                old_version: (old_label.clone(), old_version_body, old_version_record),
                new_version: (next_label.clone(), next_version_body, next_version_record),
            };

            let result = async {
                self.create_version(n, next, c, SkillStatus::Active).await?;
                if let Some(mut record) = self
                    .get_json::<VersionRecord>(&self.ver(n, &old_label))
                    .await?
                {
                    record.status = SkillStatus::Deprecated;
                    self.put_json(&self.ver(n, &old_label), &record).await?
                }
                head.active = next_label.clone();
                head.meta.description = fm.description;
                head.meta.platforms = platforms;
                head.meta.checksum = skill_checksum(c);
                self.write_active(n, c, head.meta.clone()).await?;
                self.put_json(&self.head(n), &head).await?;
                self.summary(n).await
            }
            .await;
            if result.is_err() {
                self.restore_failed_update(n, &rollback_state).await;
            }
            result
        }
    }
}
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
impl SkillLoader for SkillStore {
    async fn list(&self, ctx: &SkillContext) -> Result<Vec<SkillSummary>, SkillsError> {
        if !ctx.available_tools.iter().any(|x| x == "skill") {
            return Ok(vec![]);
        }
        Ok(self
            .registered_indexes()
            .await?
            .into_iter()
            .filter(|index| index_matches_context(index, ctx))
            .map(|i| SkillSummary {
                name: i.name,
                description: i.description,
                category: "general".into(),
                requires_tools: vec![],
            })
            .collect())
    }
    async fn load(&self, n: &str) -> Result<SkillContent, SkillsError> {
        let raw = self.load_raw(n).await?;
        let (f, b) = split_frontmatter(&raw);
        Ok(SkillContent {
            frontmatter: serde_yaml::from_str(&f)
                .map_err(|e| SkillsError::InvalidFormat(e.to_string()))?,
            body: b,
        })
    }
    async fn load_reference(&self, n: &str, p: &str) -> Result<String, SkillsError> {
        String::from_utf8(self.load_file(n, p).await?)
            .map_err(|_| SkillsError::InvalidFormat("resource is not UTF-8".into()))
    }
}
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
impl SkillManage for SkillStore {
    async fn create_skill(&self, n: &str, c: &str) -> Result<SkillSummary, SkillsError> {
        self.with_cross_tab_mutation_lock(async {
            let _gate = self.mutation_gate.lock().await;
            self.create_skill_unlocked(n, c, "user").await
        })
        .await
    }
    async fn update_skill(&self, n: &str, c: &str) -> Result<SkillSummary, SkillsError> {
        self.with_cross_tab_mutation_lock(async {
            let _gate = self.mutation_gate.lock().await;
            self.update_skill_locked(n, c).await
        })
        .await
    }
    /// Rolls back to a historical version.
    ///
    /// Audit semantics: a rollback does **not** rewind `head.active` to the
    /// target label; instead it writes a regular new minor version whose body
    /// equals the target's (like `git revert`, not `git reset`). Every mutation
    /// therefore stays append-only and observable via `list_versions`; the
    /// version list is bounded by `max_retained` + `SkillPruner`. Callers
    /// picking targets from `rollback_candidates` are unaffected because a
    /// rolled-back snapshot and its source share identical content.
    async fn rollback_skill(&self, n: &str, target: &str) -> Result<SkillSummary, SkillsError> {
        self.with_cross_tab_mutation_lock(async {
            let _gate = self.mutation_gate.lock().await;
            self.registered_index(n).await?;
            if SkillVersion::parse(target).is_none() {
                return Err(SkillsError::InvalidFormat("invalid skill version".into()));
            }
            let c = self
                .files
                .read_version(n, target)
                .await?
                .ok_or_else(|| SkillsError::NotFound(format!("{n}:{target}")))?;
            self.update_skill_locked(n, &c).await
        })
        .await
    }
    async fn delete_skill(&self, n: &str) -> Result<(), SkillsError> {
        self.with_cross_tab_mutation_lock(async {
            let _gate = self.mutation_gate.lock().await;
            // A corrupt `SKILL.md` must still be deletable: gate on the name
            // alone (unsafe names are rejected) rather than requiring a valid
            // frontmatter.
            require_valid_name(n)?;
            if self
                .get_json::<SkillMeta>(&self.meta(n))
                .await?
                .is_some_and(|meta| meta.trust_level == SkillTrust::System)
            {
                return Err(SkillsError::InvalidFormat(
                    "system skills cannot be deleted".into(),
                ));
            }
            self.files.remove_package(n).await?;
            self.files.remove_versions(n).await?;
            let keys = [self.index(n), self.meta(n), self.head(n)];
            for k in keys {
                self.kv.delete(&k).await.map_err(serr)?
            }
            self.kv
                .delete_prefix(&self.ver_prefix(n))
                .await
                .map_err(serr)?;
            self.remove_cached_index(n);
            Ok(())
        })
        .await
    }
    async fn clear_all_skills(&self) -> Result<u64, SkillsError> {
        self.with_cross_tab_mutation_lock(async {
            let _gate = self.mutation_gate.lock().await;
            let packages = self.files.list_packages().await?;
            let mut protected = Vec::new();
            for name in &packages {
                if self
                    .get_json::<SkillMeta>(&self.meta(name))
                    .await?
                    .is_some_and(|meta| meta.trust_level == SkillTrust::System)
                {
                    protected.push(name.clone());
                }
            }
            let mut removed = 0;
            for name in packages {
                if protected.contains(&name) {
                    continue;
                }
                if self.files.remove_package(&name).await? {
                    removed += 1;
                }
                self.remove_cached_index(&name);
            }
            self.files.clear_versions_except(&protected).await?;
            self.clear_runtime_metadata_except(&protected).await?;
            self.discovery
                .write()
                .unwrap_or_else(|poison| poison.into_inner())
                .retain(|name, _| protected.contains(name));
            Ok(removed)
        })
        .await
    }
}
pub struct SkillPruner;
impl SkillPruner {
    pub async fn prune(&self, store: &SkillStore, name: &str) -> Result<usize, SkillsError> {
        // 版本裁剪与 rollback/update/delete 同为包级变更，必须持有
        // 跨 tab 锁 + 进程内门闩，否则并发更新可能裁剪掉正在提升的版本。
        store
            .with_cross_tab_mutation_lock(async {
                let _gate = store.mutation_gate.lock().await;
                let versions = store.list_versions(name).await?;
                let keep = store.max_retained;
                let mut n = 0;
                for (v, _) in versions.into_iter().rev().skip(keep) {
                    store.files.remove_version(name, &v.label()).await?;
                    store
                        .kv
                        .delete(&store.ver(name, &v.label()))
                        .await
                        .map_err(serr)?;
                    n += 1;
                }
                Ok(n)
            })
            .await
    }
}

#[cfg(all(
    test,
    not(target_arch = "wasm32"),
    any(
        target_os = "android",
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
        target_os = "redox",
    )
))]
mod export_publish_tests {
    use super::publish_staged_directory_without_replacing;

    #[test]
    fn publish_refuses_a_destination_created_after_staging() {
        let root = tempfile::tempdir().expect("temporary export root");
        let staging = root.path().join(".skill.ains-export-staging");
        let destination = root.path().join("skill");
        std::fs::create_dir(&staging).expect("create staging directory");
        std::fs::write(staging.join("SKILL.md"), "staged package").expect("write staging file");

        // This models a second exporter creating the final name after the
        // first exporter completed staging but before it tried to publish.
        std::fs::create_dir(&destination).expect("concurrent destination");
        assert!(publish_staged_directory_without_replacing(&staging, &destination).is_err());
        assert!(
            destination.is_dir(),
            "the concurrent destination must survive"
        );
        assert!(
            staging.is_dir(),
            "a failed publish must leave staging for caller cleanup"
        );
    }
}
