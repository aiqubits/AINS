//! Native 文件系统工具（对齐 Harness `file_read_tool.py` / `file_write_tool.py`
//! / `file_edit_tool.py` / `glob_tool.py` / `grep_tool.py`）。
//!
//! 仅 Native 编译（WASM 无本地文件系统）。与基线差异（有意）：
//! - 基线 write/edit 内嵌 `edit_approval_prompt` 二次确认；AINS 统一走
//!   ToolRuntime 的三态权限确认回调，工具内不重复确认。
//! - glob/grep 基线优先派生 ripgrep 子进程；AINS 用 `ignore` crate 进程内
//!   遍历（同为 ripgrep 家族实现，尊重 .gitignore），不派生进程。
//! - Docker sandbox 路径校验分支对应 `policy::validate_sandbox_path`，由
//!   Phase 7.1 平台沙箱启用后接入。

use std::{
    fmt,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::error::ToolError;
use crate::policy::permission_engine::sensitive_path_pattern;
use crate::tools::{Tool, ToolCategory, ToolContext, ToolDef, ToolResult};

fn require_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput(format!("missing required string field: {key}")))
}

/// 路径解析（对齐 `_resolve_path`）：`~` 展开 + 相对路径以 cwd 为锚 +
/// 词法规范化（`.`/`..` 消解，不要求路径存在）。todo_write 等其它
/// 落盘工具复用同一入口，保证权限求值与实际访问路径同口径。
pub(crate) fn resolve_path(base: &Path, candidate: &str) -> PathBuf {
    let expanded = if candidate == "~" || candidate.starts_with("~/") {
        match crate::tools::runtime::home_dir() {
            Some(home) => home.join(candidate.trim_start_matches("~/")),
            None => PathBuf::from(candidate),
        }
    } else {
        PathBuf::from(candidate)
    };
    let anchored = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    lexical_normalize(&anchored)
}

/// File access mode for descriptor-relative workspace opens.
enum WorkspaceOpenMode {
    Read,
    Write {
        create_directories: bool,
    },
    ReadWrite {
        create_directories: bool,
        create: bool,
    },
}

/// Failure category for descriptor-relative opens. Expected filesystem
/// failures remain distinguishable from policy rejections, preserving the
/// tools' existing observable error behaviour.
pub(crate) enum WorkspaceOpenError {
    Policy(String),
    NotFound(String),
    IsDirectory(PathBuf),
    Io(String),
}

impl fmt::Display for WorkspaceOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Policy(message) | Self::NotFound(message) | Self::Io(message) => {
                formatter.write_str(message)
            }
            Self::IsDirectory(path) => {
                write!(formatter, "Cannot read directory: {}", path.display())
            }
        }
    }
}

/// Open a file below `cwd` without ever resolving a symlink component.
///
/// Unix implementations walk the path one component at a time from an
/// already-open workspace directory descriptor. Every directory and final
/// file is opened with `O_NOFOLLOW`, so replacing any component after the
/// authorization check cannot redirect the operation outside the workspace.
/// Non-Unix targets fail closed until they have an equivalent handle-relative
/// implementation (Windows reparse-point semantics are not safely covered by
/// `std::fs` path APIs).
fn open_workspace_file(
    cwd: &Path,
    candidate: &str,
    mode: WorkspaceOpenMode,
) -> Result<(File, PathBuf), WorkspaceOpenError> {
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::path::Component;

        use rustix::fs::{Mode, OFlags, mkdirat, openat};
        use rustix::io::Errno;

        let root = std::fs::canonicalize(cwd).map_err(|error| {
            WorkspaceOpenError::Io(format!(
                "cannot resolve workspace {}: {error}",
                cwd.display()
            ))
        })?;
        if !root.is_dir() {
            return Err(WorkspaceOpenError::Policy(format!(
                "workspace {} is not a directory",
                root.display()
            )));
        }
        let path = resolve_path(&root, candidate);
        let relative = path.strip_prefix(&root).map_err(|_| {
            WorkspaceOpenError::Policy(format!(
                "refusing file access outside workspace {}: {}",
                root.display(),
                path.display()
            ))
        })?;
        let parts: Vec<OsString> = relative
            .components()
            .map(|component| match component {
                Component::Normal(name) => Ok(name.to_os_string()),
                _ => Err(WorkspaceOpenError::Policy(
                    "file path must name a file below the workspace".to_string(),
                )),
            })
            .collect::<Result<_, _>>()?;
        let Some((final_name, parents)) = parts.split_last() else {
            return Err(WorkspaceOpenError::IsDirectory(path));
        };

        // `root` was canonical when it was authorized above, but its pathname
        // can be replaced before this open. Refuse to follow a replacement
        // symlink so the descriptor walk cannot become anchored elsewhere.
        let mut parent = open_workspace_root_no_follow(&root)?;
        for name in parents {
            let open_dir = || {
                openat(
                    &parent,
                    Path::new(name),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
            };
            let next = match open_dir() {
                Ok(fd) => fd,
                Err(Errno::NOENT)
                    if matches!(
                        mode,
                        WorkspaceOpenMode::Write {
                            create_directories: true
                        } | WorkspaceOpenMode::ReadWrite {
                            create_directories: true,
                            ..
                        }
                    ) =>
                {
                    match mkdirat(&parent, Path::new(name), Mode::from(0o755)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(error) => {
                            return Err(WorkspaceOpenError::Io(format!(
                                "create workspace directory: {error}"
                            )));
                        }
                    }
                    open_dir().map_err(|error| {
                        WorkspaceOpenError::Policy(format!(
                            "open workspace directory without following symlinks: {error}"
                        ))
                    })?
                }
                Err(Errno::NOENT) => {
                    return Err(WorkspaceOpenError::NotFound(format!(
                        "workspace directory does not exist: {}",
                        path.display()
                    )));
                }
                Err(error) => {
                    return Err(WorkspaceOpenError::Policy(format!(
                        "open workspace directory without following symlinks: {error}"
                    )));
                }
            };
            parent = File::from(next);
        }

        let (flags, create_mode) = match mode {
            WorkspaceOpenMode::Read => (OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty()),
            WorkspaceOpenMode::ReadWrite { create, .. } => (
                OFlags::RDWR
                    | OFlags::NOFOLLOW
                    | if create {
                        OFlags::CREATE
                    } else {
                        OFlags::empty()
                    },
                if create {
                    Mode::from(0o666)
                } else {
                    Mode::empty()
                },
            ),
            // Do not pass O_TRUNC here.  A hard link can point at an inode
            // outside the workspace; validate its link count below before
            // truncating through the returned descriptor.
            WorkspaceOpenMode::Write { .. } => (
                OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW,
                Mode::from(0o666),
            ),
        };
        let file = match openat(&parent, Path::new(final_name), flags, create_mode) {
            Ok(file) => file,
            Err(Errno::NOENT) => {
                return Err(WorkspaceOpenError::NotFound(format!(
                    "workspace file does not exist: {}",
                    path.display()
                )));
            }
            Err(error) => {
                return Err(WorkspaceOpenError::Policy(format!(
                    "open workspace file without following symlinks: {error}"
                )));
            }
        };
        let file = File::from(file);
        use std::os::unix::fs::MetadataExt;
        if file
            .metadata()
            .map_err(|error| WorkspaceOpenError::Io(format!("inspect workspace file: {error}")))?
            .nlink()
            > 1
        {
            return Err(WorkspaceOpenError::Policy(
                "refusing hard-linked file because it may alias data outside the workspace"
                    .to_string(),
            ));
        }
        Ok((file, path))
    }
    #[cfg(not(unix))]
    {
        let _ = (cwd, candidate, mode);
        Err(WorkspaceOpenError::Policy("secure descriptor-relative file operations are unavailable on this platform; refusing access".to_string()))
    }
}

/// Open the authorized workspace root without following a symlink at the root
/// pathname itself. Child components are handled by `open_workspace_file`'s
/// descriptor-relative walk.
#[cfg(unix)]
fn open_workspace_root_no_follow(root: &Path) -> Result<File, WorkspaceOpenError> {
    use rustix::fs::{Mode, OFlags, open};

    let fd = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| {
        WorkspaceOpenError::Policy(format!(
            "open workspace root without following symlinks: {error}"
        ))
    })?;
    Ok(File::from(fd))
}

/// 写文件类工具的并发独占资源键：`file:{规范路径}` 命名空间跨
/// write_file / edit_file / todo_write 共享，同批次内命中同一文件时
/// Runtime 回退顺序执行，避免并发 read-modify-write 丢更新。
pub(crate) fn file_resource_key(cwd: &Path, candidate: &str) -> String {
    format!("file:{}", resolve_path(cwd, candidate).display())
}

fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    normalized.components().next_back(),
                    None | Some(Component::RootDir) | Some(Component::Prefix(_))
                ) {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Resolve a search root to an existing canonical path below the workspace.
///
/// `ignore::WalkBuilder` follows a symlink supplied as its root even when
/// link following is disabled for entries below that root.  Resolve the root
/// before constructing a walker so glob and grep cannot use a workspace
/// symlink as an escape hatch.
fn resolve_workspace_traversal_root(
    workspace: &Path,
    candidate: &Path,
) -> Result<PathBuf, WorkspaceOpenError> {
    let root = lexical_normalize(candidate);
    if !root.starts_with(workspace) {
        return Err(WorkspaceOpenError::Policy(format!(
            "refusing search outside workspace {}: {}",
            workspace.display(),
            root.display()
        )));
    }

    let resolved = std::fs::canonicalize(&root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            WorkspaceOpenError::NotFound(format!("search root does not exist: {}", root.display()))
        } else {
            WorkspaceOpenError::Io(format!(
                "cannot resolve search root {}: {error}",
                root.display()
            ))
        }
    })?;
    if !resolved.starts_with(workspace) {
        return Err(WorkspaceOpenError::Policy(format!(
            "refusing search root that resolves outside workspace {}: {}",
            workspace.display(),
            root.display()
        )));
    }
    Ok(resolved)
}

fn canonical_workspace_root(cwd: &Path) -> Result<PathBuf, WorkspaceOpenError> {
    let workspace = std::fs::canonicalize(cwd).map_err(|error| {
        WorkspaceOpenError::Io(format!(
            "cannot resolve workspace {}: {error}",
            cwd.display()
        ))
    })?;
    if !workspace.is_dir() {
        return Err(WorkspaceOpenError::Policy(format!(
            "workspace {} is not a directory",
            workspace.display()
        )));
    }
    Ok(workspace)
}

// ── read_file ───────────────────────────────────────────────────────────

pub const FILE_READ_DEFAULT_LIMIT: usize = 200;
pub const FILE_READ_MAX_LIMIT: usize = 2000;
/// 单文件输入硬上限：输出行数限制不能替代输入内存限制。
pub const FILE_INPUT_MAX_BYTES: usize = 8 * 1024 * 1024;
/// 单次文件写入的硬上限。在构造 edit 结果前先校验，避免替换放大导致 OOM。
pub const FILE_OUTPUT_MAX_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn read_open_file_bounded(file: File) -> Result<Option<Vec<u8>>, std::io::Error> {
    let mut raw = Vec::new();
    file.take((FILE_INPUT_MAX_BYTES + 1) as u64)
        .read_to_end(&mut raw)?;
    if raw.len() > FILE_INPUT_MAX_BYTES {
        Ok(None)
    } else {
        Ok(Some(raw))
    }
}

/// 读取 UTF-8 文本文件（带行号，`{:>6}\t` 格式，对齐基线）。
pub struct FileReadTool;

#[async_trait::async_trait]
impl Tool for FileReadTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "read_file".into(),
            description: "Read a text file from the local repository.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path of the file to read"},
                    "offset": {"type": "integer", "minimum": 0, "default": 0,
                               "description": "Zero-based starting line"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": FILE_READ_MAX_LIMIT,
                              "default": FILE_READ_DEFAULT_LIMIT,
                              "description": "Number of lines to return"}
                },
                "required": ["path"]
            }),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let (file, path) = match open_workspace_file(
            ctx.cwd,
            require_str(&input, "path")?,
            WorkspaceOpenMode::Read,
        ) {
            Ok(opened) => opened,
            Err(WorkspaceOpenError::NotFound(_)) => {
                return Ok(ToolResult::err(format!(
                    "File not found: {}",
                    resolve_path(ctx.cwd, require_str(&input, "path")?).display()
                )));
            }
            Err(WorkspaceOpenError::IsDirectory(path)) => {
                return Ok(ToolResult::err(format!(
                    "Cannot read directory: {}",
                    path.display()
                )));
            }
            Err(reason) => return Ok(ToolResult::err(reason.to_string())),
        };
        if file
            .metadata()
            .map_err(|error| ToolError::Execution(error.to_string()))?
            .is_dir()
        {
            return Ok(ToolResult::err(format!(
                "Cannot read directory: {}",
                path.display()
            )));
        }
        let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = (input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(FILE_READ_DEFAULT_LIMIT as u64) as usize)
            .clamp(1, FILE_READ_MAX_LIMIT);

        let Some(raw) = read_open_file_bounded(file)
            .map_err(|error| ToolError::Execution(error.to_string()))?
        else {
            return Ok(ToolResult::err(format!(
                "File is larger than the {} byte read limit: {}",
                FILE_INPUT_MAX_BYTES,
                path.display()
            )));
        };
        if raw.contains(&0u8) {
            return Ok(ToolResult::err(format!(
                "Binary file cannot be read as text: {}",
                path.display()
            )));
        }
        let text = String::from_utf8_lossy(&raw);
        let numbered: Vec<String> = text
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(index, line)| format!("{:>6}\t{}", index + 1, line))
            .collect();
        // splitlines 语义：尾部空行不计（split('\n') 会多出末尾空段）
        let numbered = trim_trailing_phantom_line(&text, numbered);
        if numbered.is_empty() {
            return Ok(ToolResult::ok(format!(
                "(no content in selected range for {})",
                path.display()
            )));
        }
        ctx.metadata.record_read_file(path.display().to_string());
        Ok(ToolResult::ok(numbered.join("\n")))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }
}

/// `split('\n')` 相比 Python `splitlines()` 会在文本以 `\n` 结尾时多出一个
/// 空尾段；裁掉该幻影行使行数口径与基线一致。
fn trim_trailing_phantom_line(text: &str, mut numbered: Vec<String>) -> Vec<String> {
    if text.ends_with('\n') {
        let total_lines = text.split('\n').count();
        if let Some(last) = numbered.last()
            && *last == format!("{:>6}\t", total_lines)
        {
            numbered.pop();
        }
    }
    numbered
}

// ── write_file ──────────────────────────────────────────────────────────

/// 创建或整写文本文件。
pub struct FileWriteTool;

#[async_trait::async_trait]
impl Tool for FileWriteTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "write_file".into(),
            description: "Create or overwrite a text file in the local repository.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path of the file to write"},
                    "content": {"type": "string", "description": "Full file contents"},
                    "create_directories": {"type": "boolean", "default": true}
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let content = require_str(&input, "content")?;
        if content.len() > FILE_OUTPUT_MAX_BYTES {
            return Ok(ToolResult::err(format!(
                "Content is larger than the {FILE_OUTPUT_MAX_BYTES} byte write limit"
            )));
        }
        let create_directories = input
            .get("create_directories")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let (mut file, path) = match open_workspace_file(
            ctx.cwd,
            require_str(&input, "path")?,
            WorkspaceOpenMode::Write { create_directories },
        ) {
            Ok(opened) => opened,
            Err(WorkspaceOpenError::NotFound(reason)) => {
                return Err(ToolError::Execution(reason));
            }
            Err(reason) => return Ok(ToolResult::err(reason.to_string())),
        };
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| file.write_all(content.as_bytes()))
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(ToolResult::ok(format!("Wrote {}", path.display())))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn exclusive_execution_key(&self, input: &Value, cwd: &Path) -> Option<String> {
        let path = input.get("path").and_then(Value::as_str)?;
        Some(file_resource_key(cwd, path))
    }
}

// ── edit_file ───────────────────────────────────────────────────────────

/// 字符串替换式文件编辑。
pub struct FileEditTool;

#[async_trait::async_trait]
impl Tool for FileEditTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "edit_file".into(),
            description: "Edit an existing file by replacing a string.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path of the file to edit"},
                    "old_str": {"type": "string", "description": "Existing text to replace"},
                    "new_str": {"type": "string", "description": "Replacement text"},
                    "replace_all": {"type": "boolean", "default": false}
                },
                "required": ["path", "old_str", "new_str"]
            }),
        }
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let (mut file, path) = match open_workspace_file(
            ctx.cwd,
            require_str(&input, "path")?,
            WorkspaceOpenMode::ReadWrite {
                create_directories: false,
                create: false,
            },
        ) {
            Ok(opened) => opened,
            Err(WorkspaceOpenError::NotFound(_)) => {
                return Ok(ToolResult::err(format!(
                    "File not found: {}",
                    resolve_path(ctx.cwd, require_str(&input, "path")?).display()
                )));
            }
            Err(reason) => return Ok(ToolResult::err(reason.to_string())),
        };
        let old_str = require_str(&input, "old_str")?;
        let new_str = require_str(&input, "new_str")?;
        if old_str.is_empty() {
            return Ok(ToolResult::err("old_str must not be empty"));
        }
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let Some(raw) = read_open_file_bounded(
            file.try_clone()
                .map_err(|error| ToolError::Execution(error.to_string()))?,
        )
        .map_err(|error| ToolError::Execution(error.to_string()))?
        else {
            return Ok(ToolResult::err(format!(
                "File is larger than the {} byte edit limit: {}",
                FILE_INPUT_MAX_BYTES,
                path.display()
            )));
        };
        let original =
            String::from_utf8(raw).map_err(|error| ToolError::Execution(error.to_string()))?;
        if !original.contains(old_str) {
            return Ok(ToolResult::err("old_str was not found in the file"));
        }
        let replacements = if replace_all {
            original.matches(old_str).count()
        } else {
            1
        };
        let removed_bytes = replacements
            .checked_mul(old_str.len())
            .ok_or_else(|| ToolError::Execution("edit output size overflow".into()))?;
        let added_bytes = replacements
            .checked_mul(new_str.len())
            .ok_or_else(|| ToolError::Execution("edit output size overflow".into()))?;
        let output_bytes = original
            .len()
            .checked_sub(removed_bytes)
            .and_then(|size| size.checked_add(added_bytes))
            .ok_or_else(|| ToolError::Execution("edit output size overflow".into()))?;
        if output_bytes > FILE_OUTPUT_MAX_BYTES {
            return Ok(ToolResult::err(format!(
                "Edited file would exceed the {FILE_OUTPUT_MAX_BYTES} byte write limit"
            )));
        }
        let updated = if replace_all {
            original.replace(old_str, new_str)
        } else {
            original.replacen(old_str, new_str, 1)
        };
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| file.write_all(updated.as_bytes()))
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(ToolResult::ok(format!("Updated {}", path.display())))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }

    fn exclusive_execution_key(&self, input: &Value, cwd: &Path) -> Option<String> {
        let path = input.get("path").and_then(Value::as_str)?;
        Some(file_resource_key(cwd, path))
    }
}

/// Open a workspace file for a secure read-modify-write update.  The returned
/// descriptor remains anchored below `cwd`, so callers must update through
/// this handle rather than reopening `path` by name.
pub(crate) fn open_workspace_file_for_update(
    cwd: &Path,
    candidate: &str,
    create_directories: bool,
) -> Result<(File, PathBuf), WorkspaceOpenError> {
    open_workspace_file(
        cwd,
        candidate,
        WorkspaceOpenMode::ReadWrite {
            create_directories,
            create: true,
        },
    )
}

// ── glob ────────────────────────────────────────────────────────────────

pub const GLOB_DEFAULT_LIMIT: usize = 200;
pub const GLOB_MAX_LIMIT: usize = 5000;
/// 目录搜索实际访问的条目硬上限。结果 limit 不能约束无匹配或为排序继续
/// 扫描的场景，因此 glob/grep 共享独立的工作量预算。
pub const SEARCH_MAX_VISITED_ENTRIES: usize = 100_000;

fn search_limit_message() -> String {
    format!("Search exceeded the {SEARCH_MAX_VISITED_ENTRIES} entry traversal limit")
}

fn is_sensitive_file(path: &Path) -> bool {
    sensitive_path_pattern(&path.to_string_lossy()).is_some()
}

/// 按 glob 模式列出文件（尊重 .gitignore；git 仓库内包含隐藏路径，
/// 对齐 `_looks_like_git_repo` 启发式）。
pub struct GlobTool;

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "glob".into(),
            description: "List files matching a glob pattern.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string",
                                "description": "Glob pattern relative to the working directory"},
                    "root": {"type": "string", "description": "Optional search root"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": GLOB_MAX_LIMIT,
                              "default": GLOB_DEFAULT_LIMIT}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let pattern = require_str(&input, "pattern")?;
        let root_arg = input.get("root").and_then(Value::as_str);
        let limit = (input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(GLOB_DEFAULT_LIMIT as u64) as usize)
            .clamp(1, GLOB_MAX_LIMIT);

        let workspace = match canonical_workspace_root(ctx.cwd) {
            Ok(workspace) => workspace,
            Err(error) => return Ok(ToolResult::err(error.to_string())),
        };
        let (requested_root, relative_pattern) =
            resolve_glob_request(&workspace, root_arg, pattern);
        let root = match resolve_workspace_traversal_root(&workspace, &requested_root) {
            Ok(root) => root,
            Err(WorkspaceOpenError::NotFound(_)) => return Ok(ToolResult::ok("(no matches)")),
            Err(error) => return Ok(ToolResult::err(error.to_string())),
        };
        let overrides = match build_search_overrides(&root, &relative_pattern) {
            Ok(overrides) => overrides,
            Err(error) => {
                return Ok(ToolResult::ok(format!(
                    "(invalid glob pattern '{relative_pattern}': {error})"
                )));
            }
        };
        let matches = match run_glob(&root, overrides, limit, SEARCH_MAX_VISITED_ENTRIES) {
            Ok(matches) => matches,
            Err(()) => return Ok(ToolResult::err(search_limit_message())),
        };
        if matches.is_empty() {
            return Ok(ToolResult::ok("(no matches)"));
        }
        Ok(ToolResult::ok(matches.join("\n")))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }
}

fn has_glob_magic(value: &str) -> bool {
    value.contains(['*', '?', '['])
}

/// 绝对路径模式拆分为（搜索根，根相对模式）（对齐 `_resolve_glob_request`）。
fn resolve_glob_request(base: &Path, root_arg: Option<&str>, pattern: &str) -> (PathBuf, String) {
    let default_root = || match root_arg {
        Some(root) => resolve_path(base, root),
        None => base.to_path_buf(),
    };
    if pattern.trim().is_empty() {
        return (default_root(), pattern.to_string());
    }
    let candidate = Path::new(pattern);
    if !candidate.is_absolute() {
        return (default_root(), pattern.to_string());
    }
    let parts: Vec<String> = candidate
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    match parts.iter().position(|part| has_glob_magic(part)) {
        None => {
            let parent = candidate
                .parent()
                .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
            let name = candidate
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            (lexical_normalize(&parent), name)
        }
        Some(first_glob_index) => {
            let root: PathBuf = parts[..first_glob_index].iter().collect();
            let root = if root.as_os_str().is_empty() {
                PathBuf::from("/")
            } else {
                root
            };
            let relative: PathBuf = parts[first_glob_index..].iter().collect();
            (
                lexical_normalize(&root),
                relative.to_string_lossy().into_owned(),
            )
        }
    }
}

/// git 仓库启发式（对齐 `_looks_like_git_repo`）：向上最多 6 级找 `.git`。
fn looks_like_git_repo(path: &Path) -> bool {
    let mut current = path.to_path_buf();
    for _ in 0..6 {
        if current.join(".git").exists() {
            return true;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
    false
}

/// 隐藏文件包含启发式：glob 与 grep 必须共用同一口径，否则在
/// 代码库样目录（有 `.git` 祖先或根下有 `.gitignore`）检索时两工具
/// 行为分岔。取基线 `_looks_like_git_repo`（glob_tool）与
/// `.gitignore` 存在性信号（grep_tool）的并集。
fn search_includes_hidden(root: &Path) -> bool {
    looks_like_git_repo(root) || root.join(".gitignore").exists()
}

/// 构建目录遍历的 glob 过滤器。非法模式显式回传错误，由调用方转成
/// 面向模型的提示——静默降级（忽略过滤器或空结果）会让模型误读结果。
fn build_search_overrides(
    root: &Path,
    pattern: &str,
) -> Result<ignore::overrides::Override, ignore::Error> {
    let mut builder = ignore::overrides::OverrideBuilder::new(root);
    builder.add(pattern)?;
    builder.build()
}

/// 进程内 glob：`ignore` walker（尊重 .gitignore）+ `globset` 模式匹配。
/// 结果为 root 相对路径、排序去抖（对齐基线 rg --files --glob 输出口径）。
fn run_glob(
    root: &Path,
    overrides: ignore::overrides::Override,
    limit: usize,
    max_visited_entries: usize,
) -> Result<Vec<String>, ()> {
    if !root.exists() || !root.is_dir() {
        return Ok(Vec::new());
    }
    let include_hidden = search_includes_hidden(root);
    let mut collected = std::collections::BinaryHeap::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(!include_hidden)
        .overrides(overrides)
        .build();
    let mut visited_entries = 0usize;
    for entry in walker {
        visited_entries += 1;
        if visited_entries > max_visited_entries {
            return Err(());
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if is_sensitive_file(entry.path()) {
            continue;
        }
        if let Ok(relative) = entry.path().strip_prefix(root) {
            let relative = relative.to_string_lossy().into_owned();
            if collected.len() < limit {
                collected.push(relative);
            } else if collected
                .peek()
                .is_some_and(|largest| relative.as_str() < largest.as_str())
            {
                collected.pop();
                collected.push(relative);
            }
        }
    }
    let mut collected = collected.into_vec();
    collected.sort();
    Ok(collected)
}

// ── grep ────────────────────────────────────────────────────────────────

pub const GREP_DEFAULT_LIMIT: usize = 200;
pub const GREP_MAX_LIMIT: usize = 2000;

/// 正则内容检索（`path:line_no:line` 输出，对齐 grep_tool 口径）。
pub struct GrepTool;

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "grep".into(),
            description: "Search file contents with a regular expression.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression to search for"},
                    "root": {"type": "string",
                             "description": "Search root directory or file. For multiple roots, call grep separately per root."},
                    "file_glob": {"type": "string", "default": "**/*"},
                    "case_sensitive": {"type": "boolean", "default": true},
                    "limit": {"type": "integer", "minimum": 1, "maximum": GREP_MAX_LIMIT,
                              "default": GREP_DEFAULT_LIMIT}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let pattern = require_str(&input, "pattern")?;
        let case_sensitive = input
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let file_glob = input
            .get("file_glob")
            .and_then(Value::as_str)
            .unwrap_or("**/*");
        let limit = (input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(GREP_DEFAULT_LIMIT as u64) as usize)
            .clamp(1, GREP_MAX_LIMIT);
        let workspace = match canonical_workspace_root(ctx.cwd) {
            Ok(workspace) => workspace,
            Err(error) => return Ok(ToolResult::err(error.to_string())),
        };
        let requested_root = match input.get("root").and_then(Value::as_str) {
            Some(root) => resolve_path(&workspace, root),
            None => workspace.clone(),
        };
        let root = match resolve_workspace_traversal_root(&workspace, &requested_root) {
            Ok(root) => root,
            Err(WorkspaceOpenError::NotFound(_)) => {
                return Ok(ToolResult::err(format!(
                    "Search root does not exist: {}\n\
                 If you intended multiple roots, call grep separately for each root.",
                    requested_root.display()
                )));
            }
            Err(error) => return Ok(ToolResult::err(error.to_string())),
        };

        let compiled = match regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
        {
            Ok(compiled) => compiled,
            Err(error) => {
                return Ok(ToolResult::ok(format!(
                    "(invalid regex pattern '{pattern}': {error})"
                )));
            }
        };

        let overrides = match build_search_overrides(&root, file_glob) {
            Ok(overrides) => overrides,
            Err(error) => {
                return Ok(ToolResult::ok(format!(
                    "(invalid file glob '{file_glob}': {error})"
                )));
            }
        };
        let collected = match run_grep(
            &root,
            &workspace,
            &compiled,
            overrides,
            limit,
            SEARCH_MAX_VISITED_ENTRIES,
        ) {
            Ok(collected) => collected,
            Err(()) => return Ok(ToolResult::err(search_limit_message())),
        };
        if collected.is_empty() {
            return Ok(ToolResult::ok("(no matches)"));
        }
        Ok(ToolResult::ok(collected.join("\n")))
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::FileSystem
    }
}

fn run_grep(
    root: &Path,
    cwd: &Path,
    compiled: &regex::Regex,
    overrides: ignore::overrides::Override,
    limit: usize,
    max_visited_entries: usize,
) -> Result<Vec<String>, ()> {
    let mut collected = Vec::new();
    if root.is_file() {
        // 单文件：display base 为 cwd（root 在 cwd 外时回落父目录）
        let display_base = if root.starts_with(cwd) {
            cwd.to_path_buf()
        } else {
            root.parent()
                .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
        };
        grep_one_file(root, &display_base, compiled, limit, &mut collected);
        return Ok(collected);
    }

    // 与 glob 共用 `search_includes_hidden` 谓词（review 十二/十三轮修复），
    // 保证两工具在同一目录下的隐藏文件包含行为一致。
    let include_hidden = search_includes_hidden(root);
    let mut walker = ignore::WalkBuilder::new(root);
    walker.hidden(!include_hidden);
    walker.overrides(overrides);
    let mut visited_entries = 0usize;
    for entry in walker.build() {
        visited_entries += 1;
        if visited_entries > max_visited_entries {
            return Err(());
        }
        let Ok(entry) = entry else {
            continue;
        };
        if collected.len() >= limit {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        grep_one_file(entry.path(), root, compiled, limit, &mut collected);
    }
    Ok(collected)
}

fn grep_one_file(
    path: &Path,
    display_base: &Path,
    compiled: &regex::Regex,
    limit: usize,
    collected: &mut Vec<String>,
) {
    if is_sensitive_file(path) {
        return;
    }
    let Ok(file) = File::open(path) else {
        return;
    };
    // Match the direct read tool's hard-link policy. A workspace pathname can
    // otherwise alias an inode owned outside the workspace and expose it via
    // content search.
    #[cfg(unix)]
    if file
        .metadata()
        .is_ok_and(|metadata| std::os::unix::fs::MetadataExt::nlink(&metadata) > 1)
    {
        return;
    }
    let Ok(Some(raw)) = read_open_file_bounded(file) else {
        return;
    };
    if raw.contains(&0u8) {
        return; // 二进制跳过
    }
    let text = String::from_utf8_lossy(&raw);
    let display = path
        .strip_prefix(display_base)
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.display().to_string());
    for (line_no, line) in text.lines().enumerate() {
        if collected.len() >= limit {
            return;
        }
        if compiled.is_match(line) {
            collected.push(format!("{display}:{}:{line}", line_no + 1));
        }
    }
}

/// 注册全部文件系统工具（宿主便捷入口）。
pub fn register_filesystem_tools(runtime: &mut crate::tools::ToolRuntime) {
    runtime.register(Box::new(FileReadTool));
    runtime.register(Box::new(FileWriteTool));
    runtime.register(Box::new(FileEditTool));
    runtime.register(Box::new(GlobTool));
    runtime.register(Box::new(GrepTool));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolMetadata;

    async fn run(tool: &dyn Tool, cwd: &Path, input: Value) -> ToolResult {
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd,
            metadata: &mut metadata,
        };
        tool.execute(input, &mut ctx).await.unwrap()
    }

    #[tokio::test]
    async fn read_file_numbers_and_ranges() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let result = run(
            &FileReadTool,
            dir.path(),
            serde_json::json!({"path": "a.txt"}),
        )
        .await;
        assert_eq!(result.output, "     1\talpha\n     2\tbeta\n     3\tgamma");
        // offset/limit
        let result = run(
            &FileReadTool,
            dir.path(),
            serde_json::json!({"path": "a.txt", "offset": 1, "limit": 1}),
        )
        .await;
        assert_eq!(result.output, "     2\tbeta");
        // 范围外
        let result = run(
            &FileReadTool,
            dir.path(),
            serde_json::json!({"path": "a.txt", "offset": 99}),
        )
        .await;
        assert!(result.output.starts_with("(no content in selected range"));
        // 不存在 / 目录 / 二进制
        let result = run(
            &FileReadTool,
            dir.path(),
            serde_json::json!({"path": "nope.txt"}),
        )
        .await;
        assert!(result.is_error && result.output.starts_with("File not found"));
        let result = run(&FileReadTool, dir.path(), serde_json::json!({"path": "."})).await;
        assert!(result.is_error && result.output.starts_with("Cannot read directory"));
        std::fs::write(dir.path().join("bin.dat"), b"ab\x00cd").unwrap();
        let result = run(
            &FileReadTool,
            dir.path(),
            serde_json::json!({"path": "bin.dat"}),
        )
        .await;
        assert!(result.is_error && result.output.contains("Binary file"));
    }

    #[tokio::test]
    async fn read_file_records_metadata_carryover() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let mut metadata = ToolMetadata::new();
        let mut ctx = ToolContext {
            cwd: dir.path(),
            metadata: &mut metadata,
        };
        FileReadTool
            .execute(serde_json::json!({"path": "a.txt"}), &mut ctx)
            .await
            .unwrap();
        assert_eq!(metadata.read_files.len(), 1);
        assert!(metadata.read_files[0].ends_with("a.txt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_tools_reject_linked_components_and_hard_link_aliases() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(outside.path(), dir.path().join("linked")).unwrap();
        let read = run(
            &FileReadTool,
            dir.path(),
            serde_json::json!({"path": "linked/secret.txt"}),
        )
        .await;
        assert!(read.is_error);
        assert!(read.output.contains("symlink"), "{}", read.output);

        let write = run(
            &FileWriteTool,
            dir.path(),
            serde_json::json!({"path": "linked/new.txt", "content": "escaped"}),
        )
        .await;
        assert!(write.is_error, "{write:?}");
        assert!(!outside.path().join("new.txt").exists());

        let edit = run(
            &FileEditTool,
            dir.path(),
            serde_json::json!({"path": "linked/secret.txt", "old_str": "secret", "new_str": "changed"}),
        )
        .await;
        assert!(edit.is_error, "{edit:?}");
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
            "secret"
        );

        // Hard links are not symlinks, but would otherwise let a workspace
        // path alias an outside inode. Reject before any write/truncate.
        std::fs::hard_link(
            outside.path().join("secret.txt"),
            dir.path().join("hard-linked.txt"),
        )
        .unwrap();
        let hard_read = run(
            &FileReadTool,
            dir.path(),
            serde_json::json!({"path": "hard-linked.txt"}),
        )
        .await;
        assert!(hard_read.is_error, "{hard_read:?}");
        let hard_write = run(
            &FileWriteTool,
            dir.path(),
            serde_json::json!({"path": "hard-linked.txt", "content": "overwritten"}),
        )
        .await;
        assert!(hard_write.is_error, "{hard_write:?}");
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
            "secret"
        );

        let hard_grep = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"path": "hard-linked.txt", "pattern": "secret"}),
        )
        .await;
        assert_eq!(hard_grep.output, "(no matches)", "{hard_grep:?}");
    }

    #[tokio::test]
    async fn file_tools_reject_lexical_escape_and_absolute_paths_outside_workspace() {
        // 工具级安全回归：`..` 词法穿越与工作区外绝对路径在
        // descriptor-relative 打开前即被拒绝，绝不落到 `File::open`。
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();

        // read：`..` 词法穿越被拒绝
        let read = run(
            &FileReadTool,
            dir.path(),
            serde_json::json!({"path": "../secret.txt"}),
        )
        .await;
        assert!(read.is_error, "{read:?}");
        assert!(read.output.contains("outside workspace"), "{}", read.output);

        // read：工作区外绝对路径被拒绝
        let read = run(
            &FileReadTool,
            dir.path(),
            serde_json::json!({"path": outside.path().join("secret.txt").display().to_string()}),
        )
        .await;
        assert!(read.is_error, "{read:?}");
        assert!(read.output.contains("outside workspace"), "{}", read.output);

        // write：`..` 词法逃逸不得在工作区外创建文件
        let write = run(
            &FileWriteTool,
            dir.path(),
            serde_json::json!({"path": "../escaped.txt", "content": "x"}),
        )
        .await;
        assert!(write.is_error, "{write:?}");
        assert!(
            !dir.path().parent().unwrap().join("escaped.txt").exists(),
            "lexical escape must not create a file outside the workspace"
        );

        // edit：`..` 词法逃逸同样拒绝（不触碰目标）
        let edit = run(
            &FileEditTool,
            dir.path(),
            serde_json::json!({"path": "../secret.txt", "old_str": "secret", "new_str": "changed"}),
        )
        .await;
        assert!(edit.is_error, "{edit:?}");
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
            "secret"
        );
    }

    #[tokio::test]
    async fn traversal_tools_reject_absolute_root_outside_workspace() {
        // 工具级安全回归：glob/grep 的 root 为工作区外绝对路径时 fail-closed。
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "outside-secret\n").unwrap();
        let outside_root = outside.path().display().to_string();

        let glob = run(
            &GlobTool,
            dir.path(),
            serde_json::json!({"root": outside_root, "pattern": "**/*"}),
        )
        .await;
        assert!(glob.is_error, "{glob:?}");
        assert!(!glob.output.contains("secret.txt"), "{glob:?}");

        let grep = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"root": outside_root, "pattern": "outside-secret"}),
        )
        .await;
        assert!(grep.is_error, "{grep:?}");
        assert!(!grep.output.contains("outside-secret"), "{grep:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn traversal_tools_reject_root_symlink_escape() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "outside-secret\n").unwrap();
        symlink(outside.path(), workspace.path().join("escape")).unwrap();

        let glob = run(
            &GlobTool,
            workspace.path(),
            serde_json::json!({"root": "escape", "pattern": "**/*"}),
        )
        .await;
        assert!(glob.is_error, "{glob:?}");
        assert!(!glob.output.contains("secret.txt"), "{glob:?}");

        let grep = run(
            &GrepTool,
            workspace.path(),
            serde_json::json!({"root": "escape", "pattern": "outside-secret"}),
        )
        .await;
        assert!(grep.is_error, "{grep:?}");
        assert!(!grep.output.contains("outside-secret"), "{grep:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn traversal_tools_allow_root_symlink_to_workspace_descendant() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("inside.txt"), "workspace-needle\n").unwrap();
        symlink(&source, workspace.path().join("linked-source")).unwrap();

        let glob = run(
            &GlobTool,
            workspace.path(),
            serde_json::json!({"root": "linked-source", "pattern": "*.txt"}),
        )
        .await;
        assert_eq!(glob.output, "inside.txt");

        let grep = run(
            &GrepTool,
            workspace.path(),
            serde_json::json!({"root": "linked-source", "pattern": "workspace-needle"}),
        )
        .await;
        assert_eq!(grep.output, "inside.txt:1:workspace-needle");
    }

    #[cfg(unix)]
    #[test]
    fn workspace_root_open_refuses_symlink() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        let workspace_parent = tempfile::tempdir().unwrap();
        let replaced_root = workspace_parent.path().join("workspace");
        symlink(outside.path(), &replaced_root).unwrap();

        assert!(matches!(
            open_workspace_root_no_follow(&replaced_root),
            Err(WorkspaceOpenError::Policy(_))
        ));
    }

    #[tokio::test]
    async fn read_file_rejects_input_over_hard_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((FILE_INPUT_MAX_BYTES + 1) as u64).unwrap();
        let result = run(
            &FileReadTool,
            dir.path(),
            serde_json::json!({"path": "large.txt", "limit": 1}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.output.contains("read limit"), "{}", result.output);
    }

    #[tokio::test]
    async fn edit_file_rejects_input_over_hard_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((FILE_INPUT_MAX_BYTES + 1) as u64).unwrap();
        let result = run(
            &FileEditTool,
            dir.path(),
            serde_json::json!({"path": "large.txt", "old_str": "x", "new_str": "y"}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.output.contains("edit limit"), "{}", result.output);
    }

    #[tokio::test]
    async fn write_file_rejects_output_over_hard_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(
            &FileWriteTool,
            dir.path(),
            serde_json::json!({
                "path": "too-large.txt",
                "content": "x".repeat(FILE_OUTPUT_MAX_BYTES + 1),
            }),
        )
        .await;
        assert!(result.is_error);
        assert!(result.output.contains("write limit"), "{}", result.output);
        assert!(!dir.path().join("too-large.txt").exists());
    }

    #[tokio::test]
    async fn edit_file_rejects_empty_match_and_expanding_output_before_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("expand.txt");
        let original = "a".repeat(1024 * 1024);
        std::fs::write(&path, &original).unwrap();

        let empty_match = run(
            &FileEditTool,
            dir.path(),
            serde_json::json!({"path": "expand.txt", "old_str": "", "new_str": "x", "replace_all": true}),
        )
        .await;
        assert!(empty_match.is_error);
        assert!(empty_match.output.contains("must not be empty"));

        let expansion = run(
            &FileEditTool,
            dir.path(),
            serde_json::json!({
                "path": "expand.txt",
                "old_str": "a",
                "new_str": "123456789",
                "replace_all": true,
            }),
        )
        .await;
        assert!(expansion.is_error);
        assert!(
            expansion.output.contains("write limit"),
            "{}",
            expansion.output
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn write_and_edit_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(
            &FileWriteTool,
            dir.path(),
            serde_json::json!({"path": "sub/dir/new.txt", "content": "hello world"}),
        )
        .await;
        assert!(result.output.starts_with("Wrote "));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/dir/new.txt")).unwrap(),
            "hello world"
        );
        // create_directories=false 且目录不存在 → 执行错误
        let result = FileWriteTool
            .execute(
                serde_json::json!({"path": "no/dir.txt", "content": "x", "create_directories": false}),
                &mut ToolContext {
                    cwd: dir.path(),
                    metadata: &mut ToolMetadata::new(),
                },
            )
            .await;
        assert!(result.is_err());

        // edit：首个替换 / replace_all / 未命中
        let result = run(
            &FileEditTool,
            dir.path(),
            serde_json::json!({"path": "sub/dir/new.txt", "old_str": "world", "new_str": "rust"}),
        )
        .await;
        assert!(result.output.starts_with("Updated "));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/dir/new.txt")).unwrap(),
            "hello rust"
        );
        let result = run(
            &FileEditTool,
            dir.path(),
            serde_json::json!({"path": "sub/dir/new.txt", "old_str": "zzz", "new_str": "y"}),
        )
        .await;
        assert!(result.is_error);
        assert_eq!(result.output, "old_str was not found in the file");
        std::fs::write(dir.path().join("multi.txt"), "a a a").unwrap();
        run(
            &FileEditTool,
            dir.path(),
            serde_json::json!({"path": "multi.txt", "old_str": "a", "new_str": "b", "replace_all": true}),
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("multi.txt")).unwrap(),
            "b b b"
        );
    }

    #[tokio::test]
    async fn glob_lists_matching_files_sorted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/a")).unwrap();
        std::fs::write(dir.path().join("src/a/one.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/two.rs"), "").unwrap();
        std::fs::write(dir.path().join("readme.md"), "").unwrap();
        let result = run(
            &GlobTool,
            dir.path(),
            serde_json::json!({"pattern": "**/*.rs"}),
        )
        .await;
        assert_eq!(result.output, "src/a/one.rs\nsrc/two.rs");
        let result = run(
            &GlobTool,
            dir.path(),
            serde_json::json!({"pattern": "**/*.rs", "limit": 1}),
        )
        .await;
        assert_eq!(result.output, "src/a/one.rs");
        let result = run(
            &GlobTool,
            dir.path(),
            serde_json::json!({"pattern": "*.md"}),
        )
        .await;
        assert_eq!(result.output, "readme.md");
        let result = run(
            &GlobTool,
            dir.path(),
            serde_json::json!({"pattern": "**/*.py"}),
        )
        .await;
        assert_eq!(result.output, "(no matches)");
    }

    #[tokio::test]
    async fn glob_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("ignored")).unwrap();
        std::fs::write(dir.path().join("ignored/skip.rs"), "").unwrap();
        std::fs::write(dir.path().join("keep.rs"), "").unwrap();
        let result = run(
            &GlobTool,
            dir.path(),
            serde_json::json!({"pattern": "**/*.rs"}),
        )
        .await;
        assert_eq!(result.output, "keep.rs");
    }

    #[tokio::test]
    async fn traversal_tools_do_not_expose_sensitive_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let aws = dir.path().join(".aws");
        std::fs::create_dir_all(&aws).unwrap();
        std::fs::write(aws.join("credentials"), "TOP_SECRET=credential\n").unwrap();
        std::fs::write(aws.join("notes.txt"), "safe note\n").unwrap();

        let glob = run(
            &GlobTool,
            dir.path(),
            serde_json::json!({"root": ".aws", "pattern": "*"}),
        )
        .await;
        assert_eq!(glob.output, "notes.txt");

        let grep = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({
                "root": ".aws",
                "file_glob": "*",
                "pattern": "TOP_SECRET"
            }),
        )
        .await;
        assert_eq!(grep.output, "(no matches)");

        let direct = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"root": ".aws/credentials", "pattern": "TOP_SECRET"}),
        )
        .await;
        assert_eq!(direct.output, "(no matches)");
    }

    #[test]
    fn directory_searches_enforce_visited_entry_budget() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.path().join(name), "no match\n").unwrap();
        }

        let overrides = build_search_overrides(dir.path(), "*").unwrap();
        assert!(run_glob(dir.path(), overrides, 10, 2).is_err());
        let regex = regex::Regex::new("never-matches").unwrap();
        let overrides = build_search_overrides(dir.path(), "*").unwrap();
        assert!(run_grep(dir.path(), dir.path(), &regex, overrides, 10, 2).is_err());
    }

    #[tokio::test]
    async fn glob_and_grep_agree_on_hidden_files_in_gitignore_only_root() {
        // review 十三轮修复回归：非 git 目录但根下有 .gitignore 时，
        // glob 与 grep 共用 search_includes_hidden 谓词，隐藏文件包含
        // 行为不得分岔。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(dir.path().join(".env.example"), "needle=1\n").unwrap();
        let glob = run(
            &GlobTool,
            dir.path(),
            serde_json::json!({"pattern": "*.example"}),
        )
        .await;
        assert_eq!(glob.output, ".env.example");
        let grep = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"pattern": "needle", "file_glob": "*.example"}),
        )
        .await;
        assert_eq!(grep.output, ".env.example:1:needle=1");
    }

    #[tokio::test]
    async fn glob_and_grep_report_invalid_glob_patterns_explicitly() {
        // review 十三轮修复回归：非法 glob 不得静默降级（grep 旧行为
        // 是忽略过滤器全量遍历，glob 旧行为是空结果）。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        let glob = run(&GlobTool, dir.path(), serde_json::json!({"pattern": "a["})).await;
        assert!(
            glob.output.starts_with("(invalid glob pattern 'a['"),
            "{}",
            glob.output
        );
        let grep = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"pattern": "needle", "file_glob": "a["}),
        )
        .await;
        assert!(
            grep.output.starts_with("(invalid file glob 'a['"),
            "{}",
            grep.output
        );
    }

    #[test]
    fn write_and_edit_path_aliases_share_one_resource_key() {
        // review 十三轮修复回归：同一文件的相对/绝对/父目录别名必须
        // 归同一独占键，且 write_file 与 edit_file 跨工具共享命名空间。
        let cwd = Path::new("/w/p");
        let write_key = FileWriteTool
            .exclusive_execution_key(&serde_json::json!({"path": "a.txt", "content": ""}), cwd)
            .unwrap();
        assert_eq!(write_key, "file:/w/p/a.txt");
        for alias in ["./a.txt", "sub/../a.txt", "/w/p/a.txt"] {
            let edit_key = FileEditTool
                .exclusive_execution_key(
                    &serde_json::json!({"path": alias, "old_str": "x", "new_str": "y"}),
                    cwd,
                )
                .unwrap();
            assert_eq!(write_key, edit_key, "alias {alias} must share the key");
        }
        // 路径缺失（非法输入）：不产生键，交由 execute 报错
        assert!(
            FileWriteTool
                .exclusive_execution_key(&serde_json::json!({"content": ""}), cwd)
                .is_none()
        );
    }

    #[test]
    fn glob_request_resolution_absolute_patterns() {
        let base = Path::new("/w");
        // 相对模式：root = base
        let (root, pattern) = resolve_glob_request(base, None, "**/*.rs");
        assert_eq!(root, Path::new("/w"));
        assert_eq!(pattern, "**/*.rs");
        // 绝对模式含通配：拆分到首个含魔法字符的组件
        let (root, pattern) = resolve_glob_request(base, None, "/opt/src/**/*.rs");
        assert_eq!(root, Path::new("/opt/src"));
        assert_eq!(pattern, "**/*.rs");
        // 绝对模式无通配：父目录 + 文件名
        let (root, pattern) = resolve_glob_request(base, None, "/opt/src/main.rs");
        assert_eq!(root, Path::new("/opt/src"));
        assert_eq!(pattern, "main.rs");
        // root 参数生效
        let (root, _) = resolve_glob_request(base, Some("sub"), "*.rs");
        assert_eq!(root, Path::new("/w/sub"));
    }

    #[tokio::test]
    async fn grep_matches_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\nHELLO world\nbye\n").unwrap();
        std::fs::write(dir.path().join("b.log"), "hello again\n").unwrap();
        let result = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"pattern": "hello"}),
        )
        .await;
        let mut lines: Vec<&str> = result.output.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, vec!["a.txt:1:hello", "b.log:1:hello again"]);
        // 大小写不敏感
        let result = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"pattern": "hello", "case_sensitive": false, "file_glob": "*.txt"}),
        )
        .await;
        let mut lines: Vec<&str> = result.output.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, vec!["a.txt:1:hello", "a.txt:2:HELLO world"]);
        // 单文件 root
        let result = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"pattern": "again", "root": "b.log"}),
        )
        .await;
        assert_eq!(result.output, "b.log:1:hello again");
        // 无命中 / 非法正则 / root 不存在
        let result = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"pattern": "zzz_none"}),
        )
        .await;
        assert_eq!(result.output, "(no matches)");
        let result = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"pattern": "(unclosed"}),
        )
        .await;
        assert!(result.output.starts_with("(invalid regex pattern"));
        let result = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"pattern": "x", "root": "missing_dir"}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.output.contains("Search root does not exist"));
    }

    #[tokio::test]
    async fn grep_includes_hidden_files_in_git_repo_subdirectory() {
        // review 十二轮修复回归：grep 与 glob 共用 git 仓库启发式（上溯
        // 查找 .git），在仓库子目录检索时隐藏文件的包含行为一致
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.env.example"), "needle=1\n").unwrap();
        let grep = run(
            &GrepTool,
            dir.path(),
            serde_json::json!({"pattern": "needle", "root": "sub"}),
        )
        .await;
        assert_eq!(grep.output, ".env.example:1:needle=1");
        let glob = run(
            &GlobTool,
            dir.path(),
            serde_json::json!({"root": "sub", "pattern": "*"}),
        )
        .await;
        assert_eq!(glob.output, ".env.example");
    }

    #[test]
    fn resolve_path_handles_relative_and_parent() {
        let base = Path::new("/w/p");
        assert_eq!(resolve_path(base, "a/b.txt"), Path::new("/w/p/a/b.txt"));
        assert_eq!(resolve_path(base, "../x.txt"), Path::new("/w/x.txt"));
        assert_eq!(resolve_path(base, "/abs/y.txt"), Path::new("/abs/y.txt"));
        assert_eq!(resolve_path(base, "./z.txt"), Path::new("/w/p/z.txt"));
    }
}
