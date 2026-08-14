//! Skill package content storage.  Agent Skills packages are files, not KV
//! values: Native uses an OS directory and Web uses the browser's OPFS.  The
//! accompanying KV store is intentionally limited to discovery and runtime
//! metadata.

use std::sync::Arc;

use crate::error::SkillsError;
use crate::marker::MaybeSendSync;
#[cfg(not(target_arch = "wasm32"))]
use crate::skills::store::MAX_SKILL_RESOURCE_BYTES;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait SkillFiles: MaybeSendSync {
    async fn list_packages(&self) -> Result<Vec<String>, SkillsError>;
    /// Checks the presence of a package member without reading its body.  This
    /// keeps startup discovery progressive while ensuring a stale KV index
    /// never advertises a package whose required `SKILL.md` is gone.
    async fn has_file(&self, name: &str, path: &str) -> Result<bool, SkillsError>;
    /// A lightweight change token for a package member.  Discovery uses this
    /// to retain compact metadata without rereading the whole SKILL.md on
    /// every prompt, while still noticing edits made outside AINS.
    async fn file_revision(&self, name: &str, path: &str) -> Result<Option<String>, SkillsError>;
    async fn list_files(&self, name: &str) -> Result<Vec<String>, SkillsError>;
    async fn read_file(&self, name: &str, path: &str) -> Result<Option<Vec<u8>>, SkillsError>;
    async fn write_file(&self, name: &str, path: &str, content: &[u8]) -> Result<(), SkillsError>;
    async fn remove_package(&self, name: &str) -> Result<bool, SkillsError>;
    async fn clear_packages_except(&self, protected: &[String]) -> Result<u64, SkillsError>;
    async fn read_version(&self, name: &str, version: &str) -> Result<Option<String>, SkillsError>;
    async fn write_version(
        &self,
        name: &str,
        version: &str,
        content: &str,
    ) -> Result<(), SkillsError>;
    async fn remove_version(&self, name: &str, version: &str) -> Result<bool, SkillsError>;
    async fn remove_versions(&self, name: &str) -> Result<u64, SkillsError>;
    /// Removes retained workflow snapshots except those owned by protected
    /// system packages. This also cleans snapshots orphaned by an interrupted
    /// package deletion.
    async fn clear_versions_except(&self, protected: &[String]) -> Result<u64, SkillsError>;
}

/// 校验单个路径组件（skill 包名 / 版本号）：非空、不含路径分隔符与控制
/// 字符、非 `.`/`..`。Native 与 Web(OPFS) 共用同一口径，保证两端对非法
/// 入参一致拒绝：Web 端此前把含 `/` 的 version 抛给 JS 的
/// `getFileHandle(path+'.md')` 得到 TypeError、空 version 被静默当作
/// "不存在"，而 Native 端返回 `InvalidFormat`（review P3-3）。
fn validate_component(value: &str, label: &str) -> Result<(), SkillsError> {
    if value.is_empty()
        || value.contains(['/', '\\'])
        || value.chars().any(char::is_control)
        || matches!(value, "." | "..")
    {
        return Err(SkillsError::InvalidFormat(format!("invalid {label}")));
    }
    Ok(())
}

/// 校验 skill 资源相对路径：非空、非绝对路径、无反斜杠与控制字符、
/// 无 `.`/`..` 段。Native 侧 `Path::join` 遇 `/` 开头路径会替换根目录，
/// 必须拒绝；Web(OPFS) 的 JS `dirs()` 会静默过滤 `.`/`..`（不拒绝）。
/// 在此统一校验避免两端行为分叉（review P3-3）。
fn validate_resource_path(path: &str) -> Result<(), SkillsError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path.split('/').any(|segment| matches!(segment, "." | ".."))
    {
        return Err(SkillsError::InvalidFormat(
            "invalid skill resource path".into(),
        ));
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct NativeSkillFiles {
    root: std::path::PathBuf,
}

#[cfg(not(target_arch = "wasm32"))]
impl NativeSkillFiles {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Result<Self, SkillsError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| {
            SkillsError::Storage(format!("create skill root {}: {e}", root.display()))
        })?;
        Ok(Self { root })
    }

    fn component(value: &str, label: &str) -> Result<(), SkillsError> {
        validate_component(value, label)
    }
    fn package(&self, name: &str) -> Result<std::path::PathBuf, SkillsError> {
        Self::component(name, "skill package name")?;
        Ok(self.root.join(name))
    }
    fn resource(&self, name: &str, path: &str) -> Result<std::path::PathBuf, SkillsError> {
        Self::component(name, "skill package name")?;
        validate_resource_path(path)?;
        Ok(self.root.join(name).join(path))
    }
    fn version(&self, name: &str, version: &str) -> Result<std::path::PathBuf, SkillsError> {
        Self::component(name, "skill package name")?;
        Self::component(version, "skill version")?;
        Ok(self
            .root
            .join(".ains-runtime")
            .join("versions")
            .join(name)
            .join(format!("{version}.md")))
    }
    fn err(context: &str, error: std::io::Error) -> SkillsError {
        SkillsError::Storage(format!("{context}: {error}"))
    }

    /// Opens an existing package member from directory descriptors on Unix.
    /// This keeps externally provided standard packages interoperable while
    /// preventing a resource symlink (or a symlinked parent directory) from
    /// making the agent read outside the package root.
    #[cfg(unix)]
    fn open_resource_for_read(
        &self,
        name: &str,
        path: &str,
    ) -> Result<Option<std::fs::File>, SkillsError> {
        use rustix::fs::{Mode, OFlags, open, openat};
        use rustix::io::Errno;

        let root = open(
            &self.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(std::fs::File::from)
        .map_err(|error| {
            SkillsError::Storage(format!(
                "open skill root without following symlinks: {error}"
            ))
        })?;
        let mut parent = root;
        let mut directories = vec![name];
        let mut parts = path.split('/').collect::<Vec<_>>();
        let file = parts.pop().expect("validated non-empty resource path");
        directories.extend(parts);
        for directory in directories {
            parent = match openat(
                &parent,
                std::path::Path::new(directory),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                Mode::empty(),
            ) {
                Ok(next) => std::fs::File::from(next),
                Err(Errno::NOENT) => return Ok(None),
                Err(error) => {
                    return Err(SkillsError::InvalidFormat(format!(
                        "refusing skill resource with unsafe directory component: {error}"
                    )));
                }
            };
        }
        let file = match openat(
            &parent,
            std::path::Path::new(file),
            OFlags::RDONLY | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(file) => std::fs::File::from(file),
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => {
                return Err(SkillsError::InvalidFormat(format!(
                    "refusing unsafe skill resource: {error}"
                )));
            }
        };
        let metadata = file
            .metadata()
            .map_err(|error| Self::err("inspect skill resource", error))?;
        if !metadata.is_file() {
            return Ok(None);
        }
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            return Err(SkillsError::InvalidFormat(
                "refusing hard-linked skill resource".into(),
            ));
        }
        Ok(Some(file))
    }

    #[cfg(unix)]
    fn open_resource_parent_for_write(
        &self,
        name: &str,
        path: &str,
    ) -> Result<(std::fs::File, String), SkillsError> {
        use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
        use rustix::io::Errno;

        let root = open(
            &self.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(std::fs::File::from)
        .map_err(|error| {
            SkillsError::Storage(format!(
                "open skill root without following symlinks: {error}"
            ))
        })?;
        let mut parent = root;
        let mut directories = vec![name];
        let mut parts = path.split('/').collect::<Vec<_>>();
        let file = parts
            .pop()
            .expect("validated non-empty skill file path")
            .to_string();
        directories.extend(parts);
        for directory in directories {
            let open_directory = || {
                openat(
                    &parent,
                    std::path::Path::new(directory),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
            };
            parent = match open_directory() {
                Ok(next) => std::fs::File::from(next),
                Err(Errno::NOENT) => {
                    match mkdirat(&parent, std::path::Path::new(directory), Mode::from(0o755)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(error) => {
                            return Err(SkillsError::Storage(format!(
                                "create skill directory: {error}"
                            )));
                        }
                    }
                    open_directory().map(std::fs::File::from).map_err(|error| {
                        SkillsError::InvalidFormat(format!(
                            "refusing skill directory with unsafe component: {error}"
                        ))
                    })?
                }
                Err(error) => {
                    return Err(SkillsError::InvalidFormat(format!(
                        "refusing skill directory with unsafe component: {error}"
                    )));
                }
            };
        }
        Ok((parent, file))
    }

    #[cfg(unix)]
    fn write_resource(&self, name: &str, path: &str, content: &[u8]) -> Result<(), SkillsError> {
        use std::io::Write;

        use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
        use rustix::io::Errno;

        let (parent, file) = self.open_resource_parent_for_write(name, path)?;
        static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temporary = (0..8)
            .find_map(|_| {
                let candidate = format!(
                    ".{file}.ains-tmp-{}-{}",
                    std::process::id(),
                    TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                );
                match openat(
                    &parent,
                    std::path::Path::new(&candidate),
                    OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                    Mode::from(0o600),
                ) {
                    Ok(fd) => Some(Ok((candidate, std::fs::File::from(fd)))),
                    Err(Errno::EXIST) => None,
                    Err(error) => Some(Err(SkillsError::Storage(format!(
                        "create skill temporary file: {error}"
                    )))),
                }
            })
            .transpose()?
            .ok_or_else(|| SkillsError::Storage("create unique skill temporary file".into()))?;
        let (temporary_name, mut temporary_file) = temporary;
        let write_result = temporary_file
            .write_all(content)
            .and_then(|()| temporary_file.sync_all());
        drop(temporary_file);
        if let Err(error) = write_result {
            let _ = unlinkat(
                &parent,
                std::path::Path::new(&temporary_name),
                AtFlags::empty(),
            );
            return Err(Self::err("write skill temporary file", error));
        }
        renameat(
            &parent,
            std::path::Path::new(&temporary_name),
            &parent,
            std::path::Path::new(&file),
        )
        .map_err(|error| SkillsError::Storage(format!("replace skill file: {error}")))
    }

    /// Non-Unix has no equivalent descriptor-relative API in this crate yet.
    /// Reject every visible symlink instead of following it through `std::fs`.
    #[cfg(not(unix))]
    fn checked_resource_for_read(
        &self,
        name: &str,
        path: &str,
    ) -> Result<Option<std::path::PathBuf>, SkillsError> {
        let mut current = self.root.clone();
        for component in std::iter::once(name).chain(path.split('/')) {
            current.push(component);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(SkillsError::InvalidFormat(
                        "refusing symlinked skill resource".into(),
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(Self::err("inspect skill resource", error)),
            }
        }
        Ok(Some(current))
    }
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<String>) -> Result<(), SkillsError> {
        for entry in std::fs::read_dir(dir).map_err(|e| Self::err("list skill directory", e))? {
            let entry = entry.map_err(|e| Self::err("read skill directory", e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let kind = entry
                .file_type()
                .map_err(|e| Self::err("read skill entry", e))?;
            if kind.is_dir() {
                Self::walk(&entry.path(), &relative, out)?;
            } else if kind.is_file() {
                out.push(relative);
            }
        }
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl SkillFiles for NativeSkillFiles {
    async fn list_packages(&self) -> Result<Vec<String>, SkillsError> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.root).map_err(|e| Self::err("list skill root", e))? {
            let entry = entry.map_err(|e| Self::err("read skill root", e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != ".ains-runtime"
                && entry
                    .file_type()
                    .map_err(|e| Self::err("read skill root entry", e))?
                    .is_dir()
            {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }
    async fn list_files(&self, name: &str) -> Result<Vec<String>, SkillsError> {
        let dir = self.package(name)?;
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut files = Vec::new();
        Self::walk(&dir, "", &mut files)?;
        files.sort();
        Ok(files)
    }
    async fn has_file(&self, name: &str, path: &str) -> Result<bool, SkillsError> {
        let _ = self.resource(name, path)?;
        #[cfg(unix)]
        return Ok(self.open_resource_for_read(name, path)?.is_some());
        #[cfg(not(unix))]
        match self.checked_resource_for_read(name, path)? {
            Some(file) => Ok(std::fs::metadata(file)
                .map_err(|error| Self::err("inspect skill file", error))?
                .is_file()),
            None => Ok(false),
        }
    }
    async fn file_revision(&self, name: &str, path: &str) -> Result<Option<String>, SkillsError> {
        let _ = self.resource(name, path)?;
        #[cfg(unix)]
        let metadata = match self.open_resource_for_read(name, path)? {
            Some(file) => Some(
                file.metadata()
                    .map_err(|error| Self::err("inspect skill file", error))?,
            ),
            None => None,
        };
        #[cfg(not(unix))]
        let metadata = match self.checked_resource_for_read(name, path)? {
            Some(file) => Some(
                std::fs::metadata(file).map_err(|error| Self::err("inspect skill file", error))?,
            ),
            None => None,
        };
        match metadata {
            Some(metadata) if metadata.is_file() => {
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|time| time.as_nanos())
                    .unwrap_or_default();
                Ok(Some(format!("{modified}:{}", metadata.len())))
            }
            _ => Ok(None),
        }
    }
    async fn read_file(&self, name: &str, path: &str) -> Result<Option<Vec<u8>>, SkillsError> {
        use std::io::Read;
        let _ = self.resource(name, path)?;
        let mut value = Vec::new();
        #[cfg(unix)]
        {
            let Some(file) = self.open_resource_for_read(name, path)? else {
                return Ok(None);
            };
            file.take(MAX_SKILL_RESOURCE_BYTES as u64 + 1)
                .read_to_end(&mut value)
                .map_err(|error| Self::err("read skill file", error))?;
        }
        #[cfg(not(unix))]
        match self.checked_resource_for_read(name, path)? {
            Some(file) => {
                std::fs::File::open(&file)
                    .map_err(|error| Self::err("open skill file", error))?
                    .take(MAX_SKILL_RESOURCE_BYTES as u64 + 1)
                    .read_to_end(&mut value)
                    .map_err(|error| Self::err("read skill file", error))?;
            }
            None => return Ok(None),
        }
        // Never materialize an unbounded file in memory: read at most one byte
        // past the resource cap and reject anything larger than the limit.
        if value.len() > MAX_SKILL_RESOURCE_BYTES {
            return Err(SkillsError::InvalidFormat(format!(
                "skill resource exceeds the {MAX_SKILL_RESOURCE_BYTES} byte limit"
            )));
        }
        Ok(Some(value))
    }
    async fn write_file(&self, name: &str, path: &str, content: &[u8]) -> Result<(), SkillsError> {
        #[cfg(unix)]
        {
            let _ = self.resource(name, path)?;
            return self.write_resource(name, path, content);
        }
        #[cfg(not(unix))]
        {
            let file = self.resource(name, path)?;
            let parent = file
                .parent()
                .ok_or_else(|| SkillsError::InvalidFormat("skill file lacks parent".into()))?;
            std::fs::create_dir_all(parent).map_err(|e| Self::err("create skill directory", e))?;
            let temporary = file.with_extension("ains-tmp");
            std::fs::write(&temporary, content)
                .map_err(|e| Self::err("write skill temporary file", e))?;
            std::fs::rename(&temporary, &file).map_err(|e| Self::err("replace skill file", e))
        }
    }
    async fn remove_package(&self, name: &str) -> Result<bool, SkillsError> {
        let package = self.package(name)?;
        if !package.exists() {
            return Ok(false);
        }
        std::fs::remove_dir_all(package).map_err(|e| Self::err("remove skill package", e))?;
        Ok(true)
    }
    async fn clear_packages_except(&self, protected: &[String]) -> Result<u64, SkillsError> {
        let mut removed = 0;
        for name in self.list_packages().await? {
            if !protected.contains(&name) && self.remove_package(&name).await? {
                removed += 1;
            }
        }
        Ok(removed)
    }
    async fn read_version(&self, name: &str, version: &str) -> Result<Option<String>, SkillsError> {
        match std::fs::read_to_string(self.version(name, version)?) {
            Ok(value) => Ok(Some(value)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Self::err("read skill version", e)),
        }
    }
    async fn write_version(
        &self,
        name: &str,
        version: &str,
        content: &str,
    ) -> Result<(), SkillsError> {
        let path = self.version(name, version)?;
        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent)
            .map_err(|e| Self::err("create skill version directory", e))?;
        std::fs::write(path, content).map_err(|e| Self::err("write skill version", e))
    }
    async fn remove_versions(&self, name: &str) -> Result<u64, SkillsError> {
        Self::component(name, "skill package name")?;
        let dir = self.root.join(".ains-runtime").join("versions").join(name);
        if !dir.exists() {
            return Ok(0);
        }
        std::fs::remove_dir_all(dir).map_err(|e| Self::err("remove skill versions", e))?;
        Ok(1)
    }
    async fn remove_version(&self, name: &str, version: &str) -> Result<bool, SkillsError> {
        let path = self.version(name, version)?;
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Self::err("remove skill version", e)),
        }
    }
    async fn clear_versions_except(&self, protected: &[String]) -> Result<u64, SkillsError> {
        let dir = self.root.join(".ains-runtime").join("versions");
        if !dir.exists() {
            return Ok(0);
        }
        let mut removed = 0;
        for entry in std::fs::read_dir(&dir).map_err(|e| Self::err("list skill versions", e))? {
            let entry = entry.map_err(|e| Self::err("read skill version entry", e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if protected.contains(&name)
                || !entry
                    .file_type()
                    .map_err(|e| Self::err("read skill version entry", e))?
                    .is_dir()
            {
                continue;
            }
            std::fs::remove_dir_all(entry.path())
                .map_err(|e| Self::err("remove skill versions", e))?;
            removed += 1;
        }
        Ok(removed)
    }
}

/// Opens the platform's package filesystem.  On Web this is OPFS, not
/// IndexedDB: IndexedDB remains the metadata database only.
#[cfg(not(target_arch = "wasm32"))]
pub async fn open_platform_skill_files(
    root: impl Into<std::path::PathBuf>,
) -> Result<Arc<dyn SkillFiles>, SkillsError> {
    Ok(Arc::new(NativeSkillFiles::new(root)?))
}

/// 无 IO 的 schema 投影桩文件系统：仅用于 `tool_schema_snapshot` 等只投影
/// 注册表、绝不执行工具的路径。任何执行调用都会失败（快照不执行工具），
/// 同步构造。
///
/// native 端使用专用子目录而非裸 `temp_dir()`：桩文件系统不应把整个
/// 系统临时目录当作 skills 根（例如误调用 `list_packages` 时不会把
/// 无关的 temp 条目当成 skill 包）。子目录在构造时创建且保持为空，
/// 除此之外无任何磁盘副作用。
pub fn schema_stub_skill_files() -> Arc<dyn SkillFiles> {
    #[cfg(target_arch = "wasm32")]
    {
        Arc::new(web::OpfsSkillFiles::new("schema-stub".to_string()))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Arc::new(
            NativeSkillFiles::new(std::env::temp_dir().join("ains-schema-stub"))
                .expect("temp dir must be a valid skill stub root"),
        )
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use super::*;
    use js_sys::{Array, Uint8Array};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::JsFuture;

    #[wasm_bindgen(
        inline_js = "export async function ainsSkillOpfs(op, name, path, data, protectedNames) { const root=await navigator.storage.getDirectory(); const base=await root.getDirectoryHandle('ains-skills',{create:true}); const dirs=(p)=>p?p.split('/').filter((s)=>s&&s!=='.'&&s!=='..'):[]; const getdir=async(parts,create)=>{let d=base;for(const part of parts)d=await d.getDirectoryHandle(part,{create});return d}; const read=async()=>{try{let d=await getdir([...dirs(name),...dirs(path).slice(0,-1)],false);let f=await d.getFileHandle(dirs(path).at(-1));return new Uint8Array(await (await f.getFile()).arrayBuffer())}catch(e){if(e&&e.name==='NotFoundError')return null;throw e}}; const has=async()=>{try{let d=await getdir([...dirs(name),...dirs(path).slice(0,-1)],false);await d.getFileHandle(dirs(path).at(-1));return true}catch(e){if(e&&e.name==='NotFoundError')return false;throw e}}; const revision=async()=>{try{let d=await getdir([...dirs(name),...dirs(path).slice(0,-1)],false),h=await d.getFileHandle(dirs(path).at(-1)),f=await h.getFile();return `${f.lastModified}:${f.size}`}catch(e){if(e&&e.name==='NotFoundError')return null;throw e}}; if(op==='read')return read(); if(op==='has')return has(); if(op==='revision')return revision(); if(op==='write'){let ps=dirs(path),d=await getdir([...dirs(name),...ps.slice(0,-1)],true),f=await d.getFileHandle(ps.at(-1),{create:true}),w=await f.createWritable();await w.write(data);await w.close();return null} if(op==='list'){let out=[];async function walk(d,p){for await(const[n,h]of d.entries()){let q=p?p+'/'+n:n;if(h.kind==='directory'&&q!=='.ains-runtime')out.push(q)}}await walk(await getdir(dirs(name),true),'');return out} if(op==='files'){let out=[];async function walk(d,p){for await(const[n,h]of d.entries()){let q=p?p+'/'+n:n;if(h.kind==='file')out.push(q);else await walk(h,q)}}try{await walk(await getdir(dirs(name),false), '')}catch(e){if(!(e&&e.name==='NotFoundError'))throw e}return out} if(op==='remove'){try{let ps=dirs(name),p=await getdir(ps.slice(0,-1),false);await p.removeEntry(ps.at(-1),{recursive:true});return true}catch(e){if(e&&e.name==='NotFoundError')return false;throw e}} if(op==='clear'){try{let d=await getdir(dirs(name),false),n=0;for await(const[k,h]of d.entries())if(h.kind==='directory'&&k!=='.ains-runtime'&&!protectedNames.includes(k)){await d.removeEntry(k,{recursive:true});n++}return n}catch(e){if(e&&e.name==='NotFoundError')return 0;throw e}} if(op==='read-version'){try{let d=await getdir(['.ains-runtime','versions',...dirs(name)],false),f=await d.getFileHandle(path+'.md');return new Uint8Array(await (await f.getFile()).arrayBuffer())}catch(e){if(e&&e.name==='NotFoundError')return null;throw e}} if(op==='write-version'){let d=await getdir(['.ains-runtime','versions',...dirs(name)],true),f=await d.getFileHandle(path+'.md',{create:true}),w=await f.createWritable();await w.write(data);await w.close();return null} if(op==='remove-version'){try{let d=await getdir(['.ains-runtime','versions',...dirs(name)],false);await d.removeEntry(path+'.md');return true}catch(e){if(e&&e.name==='NotFoundError')return false;throw e}} if(op==='remove-versions'){try{let ps=dirs(name),p=await getdir(['.ains-runtime','versions',...ps.slice(0,-1)],false);await p.removeEntry(ps.at(-1),{recursive:true});return 1}catch(e){if(e&&e.name==='NotFoundError')return 0;throw e}} if(op==='clear-versions'){try{let d=await getdir(['.ains-runtime','versions',...dirs(name)],false),n=0;for await(const[k,h]of d.entries())if(h.kind==='directory'&&!protectedNames.includes(k)){await d.removeEntry(k,{recursive:true});n++}return n}catch(e){if(e&&e.name==='NotFoundError')return 0;throw e}} }"
    )]
    extern "C" {
        #[wasm_bindgen(catch)]
        fn ainsSkillOpfs(
            op: &str,
            name: &str,
            path: &str,
            data: &JsValue,
            protected: &Array,
        ) -> Result<js_sys::Promise, JsValue>;
    }
    #[derive(Clone)]
    pub struct OpfsSkillFiles {
        scope: String,
    }
    impl OpfsSkillFiles {
        pub async fn open(scope: String) -> Result<Self, SkillsError> {
            Ok(Self { scope })
        }

        /// 同步构造（无 IO）：`tool_schema_snapshot` 等只投影注册表、绝不
        /// 执行的路径用它 attach 桩存储（`open` 为 async 无法在同步快照中
        /// 使用，且桩存储不需要真实 OPFS 句柄）。
        pub fn new(scope: String) -> Self {
            Self { scope }
        }
        async fn call(
            &self,
            op: &str,
            name: &str,
            path: &str,
            data: JsValue,
            protected: &[String],
        ) -> Result<JsValue, SkillsError> {
            let names = Array::new();
            for n in protected {
                names.push(&JsValue::from_str(n));
            }
            JsFuture::from(
                ainsSkillOpfs(op, name, path, &data, &names)
                    .map_err(|e| SkillsError::Storage(format!("OPFS: {e:?}")))?,
            )
            .await
            .map_err(|e| SkillsError::Storage(format!("OPFS: {e:?}")))
        }
        fn scoped(&self, name: &str) -> String {
            format!("{}/{}", self.scope, name)
        }
    }
    #[async_trait::async_trait(?Send)]
    impl SkillFiles for OpfsSkillFiles {
        async fn list_packages(&self) -> Result<Vec<String>, SkillsError> {
            let a = Array::from(
                &self
                    .call("list", &self.scope, "", JsValue::UNDEFINED, &[])
                    .await?,
            );
            Ok(a.iter().filter_map(|v| v.as_string()).collect())
        }
        async fn has_file(&self, name: &str, path: &str) -> Result<bool, SkillsError> {
            validate_component(name, "skill package name")?;
            validate_resource_path(path)?;
            Ok(self
                .call("has", &self.scoped(name), path, JsValue::UNDEFINED, &[])
                .await?
                .as_bool()
                .unwrap_or(false))
        }
        async fn file_revision(
            &self,
            name: &str,
            path: &str,
        ) -> Result<Option<String>, SkillsError> {
            validate_component(name, "skill package name")?;
            validate_resource_path(path)?;
            let value = self
                .call(
                    "revision",
                    &self.scoped(name),
                    path,
                    JsValue::UNDEFINED,
                    &[],
                )
                .await?;
            Ok(if value.is_null() {
                None
            } else {
                value.as_string()
            })
        }
        async fn list_files(&self, name: &str) -> Result<Vec<String>, SkillsError> {
            validate_component(name, "skill package name")?;
            let a = Array::from(
                &self
                    .call("files", &self.scoped(name), "", JsValue::UNDEFINED, &[])
                    .await?,
            );
            Ok(a.iter().filter_map(|v| v.as_string()).collect())
        }
        async fn read_file(&self, name: &str, path: &str) -> Result<Option<Vec<u8>>, SkillsError> {
            validate_component(name, "skill package name")?;
            validate_resource_path(path)?;
            let v = self
                .call("read", &self.scoped(name), path, JsValue::UNDEFINED, &[])
                .await?;
            if v.is_null() {
                Ok(None)
            } else {
                Ok(Some(Uint8Array::new(&v).to_vec()))
            }
        }
        async fn write_file(&self, name: &str, path: &str, c: &[u8]) -> Result<(), SkillsError> {
            validate_component(name, "skill package name")?;
            validate_resource_path(path)?;
            self.call(
                "write",
                &self.scoped(name),
                path,
                Uint8Array::from(c).into(),
                &[],
            )
            .await?;
            Ok(())
        }
        async fn remove_package(&self, name: &str) -> Result<bool, SkillsError> {
            validate_component(name, "skill package name")?;
            Ok(self
                .call("remove", &self.scoped(name), "", JsValue::UNDEFINED, &[])
                .await?
                .as_bool()
                .unwrap_or(false))
        }
        async fn clear_packages_except(&self, p: &[String]) -> Result<u64, SkillsError> {
            Ok(self
                .call("clear", &self.scope, "", JsValue::UNDEFINED, p)
                .await?
                .as_f64()
                .unwrap_or(0.) as u64)
        }
        async fn read_version(&self, n: &str, v: &str) -> Result<Option<String>, SkillsError> {
            validate_component(n, "skill package name")?;
            validate_component(v, "skill version")?;
            let r = self
                .call("read-version", &self.scoped(n), v, JsValue::UNDEFINED, &[])
                .await?;
            if r.is_null() {
                Ok(None)
            } else {
                String::from_utf8(Uint8Array::new(&r).to_vec())
                    .map(Some)
                    .map_err(|_| SkillsError::InvalidFormat("version is not UTF-8".into()))
            }
        }
        async fn write_version(&self, n: &str, v: &str, c: &str) -> Result<(), SkillsError> {
            validate_component(n, "skill package name")?;
            validate_component(v, "skill version")?;
            self.call(
                "write-version",
                &self.scoped(n),
                v,
                Uint8Array::from(c.as_bytes()).into(),
                &[],
            )
            .await?;
            Ok(())
        }
        async fn remove_version(&self, n: &str, v: &str) -> Result<bool, SkillsError> {
            validate_component(n, "skill package name")?;
            validate_component(v, "skill version")?;
            Ok(self
                .call(
                    "remove-version",
                    &self.scoped(n),
                    v,
                    JsValue::UNDEFINED,
                    &[],
                )
                .await?
                .as_bool()
                .unwrap_or(false))
        }
        async fn remove_versions(&self, n: &str) -> Result<u64, SkillsError> {
            validate_component(n, "skill package name")?;
            Ok(self
                .call(
                    "remove-versions",
                    &self.scoped(n),
                    "",
                    JsValue::UNDEFINED,
                    &[],
                )
                .await?
                .as_f64()
                .unwrap_or(0.) as u64)
        }
        async fn clear_versions_except(&self, protected: &[String]) -> Result<u64, SkillsError> {
            Ok(self
                .call(
                    "clear-versions",
                    &self.scope,
                    "",
                    JsValue::UNDEFINED,
                    protected,
                )
                .await?
                .as_f64()
                .unwrap_or(0.) as u64)
        }
    }
    pub async fn open_platform_skill_files(
        scope: String,
    ) -> Result<Arc<dyn SkillFiles>, SkillsError> {
        Ok(Arc::new(OpfsSkillFiles::open(scope).await?))
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::open_platform_skill_files;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn temp_root() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        // Unique per test: tests run in parallel and each removes its own root.
        std::env::temp_dir().join(format!(
            "ains-skill-files-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn package(root: &std::path::Path) -> NativeSkillFiles {
        let files = NativeSkillFiles::new(root).unwrap();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        files
    }

    #[test]
    fn read_file_accepts_up_to_the_cap_and_rejects_beyond() {
        let root = temp_root();
        let files = package(&root);
        std::fs::write(
            root.join("pkg").join("at-cap.bin"),
            vec![0u8; MAX_SKILL_RESOURCE_BYTES],
        )
        .unwrap();
        std::fs::write(
            root.join("pkg").join("over.bin"),
            vec![0u8; MAX_SKILL_RESOURCE_BYTES + 1],
        )
        .unwrap();

        let at_cap = futures::executor::block_on(files.read_file("pkg", "at-cap.bin")).unwrap();
        assert_eq!(
            at_cap.as_ref().map(Vec::len),
            Some(MAX_SKILL_RESOURCE_BYTES)
        );

        let over = futures::executor::block_on(files.read_file("pkg", "over.bin"));
        assert!(matches!(over, Err(SkillsError::InvalidFormat(_))));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn shared_validation_rejects_the_same_inputs_on_both_platforms() {
        // 共享校验函数与平台无关：Native 与 Web(OPFS) 用同一份代码，
        // 此处验证拒绝口径（review P3-3）。
        for bad in ["", ".", "..", "a/b", "a\\b", "a\x00b"] {
            assert!(
                validate_component(bad, "skill package name").is_err(),
                "component {bad:?} must be rejected"
            );
        }
        for good in ["pkg", "my-skill.v2", "a_b"] {
            assert!(
                validate_component(good, "skill package name").is_ok(),
                "component {good:?} must be accepted"
            );
        }
        for bad in [
            "",
            ".",
            "..",
            "a/../b",
            "../x",
            "a\\b",
            "a\x00b",
            "SKILL.md/..",
            "/etc/passwd",
            "/SKILL.md",
        ] {
            assert!(
                validate_resource_path(bad).is_err(),
                "resource path {bad:?} must be rejected"
            );
        }
        for good in ["SKILL.md", "a/b/c.txt", "a b.txt"] {
            assert!(
                validate_resource_path(good).is_ok(),
                "resource path {good:?} must be accepted"
            );
        }
    }

    #[test]
    fn read_file_round_trips_small_files() {
        let root = temp_root();
        let files = package(&root);
        std::fs::write(root.join("pkg").join("small.txt"), b"ok").unwrap();

        let value = futures::executor::block_on(files.read_file("pkg", "small.txt")).unwrap();
        assert_eq!(value.as_deref(), Some(&b"ok"[..]));
        let missing = futures::executor::block_on(files.read_file("pkg", "nope.txt")).unwrap();
        assert!(missing.is_none());

        std::fs::remove_dir_all(&root).ok();
    }
}
