//! Dockerfile.web WASM 工具链一致性回归测试（纯 std，无需网络 / Docker）。
//!
//! 背景：`Dockerfile.web` 在 `dx build` 前设置 `NO_DOWNLOADS=1`，使 dioxus-cli 0.7.9
//! 从 PATH 解析本地工具而非联网下载。此时 `dx` 会对 `wasm-bindgen --version` 做
//! **精确字符串匹配**，要求镜像内安装的 `wasm-bindgen-cli` 版本严格等于 `Cargo.lock`
//! 中 `wasm-bindgen` crate 的版本。若二者漂移（例如依赖升级后忘记同步 Dockerfile 里
//! 硬编码的版本号），Docker 构建会在 `dx build` 深处报错、信息晦涩。
//!
//! 本测试在**编译期**通过 `include_str!` 读取仓库根的 `Cargo.lock`、`Dockerfile.web`
//! 与 `.github/workflows/ains.yml`，静态校验三处工具链版本一致（Dockerfile 与 CI
//! workflow 的 wasm-bindgen-cli / esbuild 均须与 Cargo.lock 对齐），并校验 CI 与
//! Dockerfile 两侧的 esbuild SHA256 哈希一致、防止有人把 `NO_DOWNLOADS` 退回到
//! 无效的 `WASM_BINDGEN_USE_LOCAL_OPT`。该测试跑在既有的
//! `cargo test --package web` CI 步骤内，因此每个 PR 都能拦截漂移。对于裁剪后的生产
//! workspace，测试还会在临时目录复刻 Docker 的文件布局，通过 Cargo metadata
//! 语义比较依赖契约，并运行 `cargo tree --locked --offline --depth 0` 验证 lockfile。
//! 相关生产输入变化时，CI 还会构建完整 Docker runtime 镜像。

// 相对本测试文件（app/web/tests/）：`../../../` 即仓库根。
const CARGO_LOCK: &str = include_str!("../../../Cargo.lock");
const PROD_WEB_CARGO_TOML: &str = include_str!("../../../prod-build/web/Cargo.toml");
const PROD_WEB_CARGO_LOCK: &str = include_str!("../../../prod-build/web/Cargo.lock");
const PROD_WEB_MANIFEST: &str = include_str!("../../../prod-build/web/manifests/web.toml");
const PROD_UI_MANIFEST: &str = include_str!("../../../prod-build/web/manifests/ui.toml");
const PROD_CLIENT_API_MANIFEST: &str =
    include_str!("../../../prod-build/web/manifests/client-api.toml");
const PROD_RUST_AGENT_MANIFEST: &str =
    include_str!("../../../prod-build/web/manifests/rust-agent.toml");
const PROD_I18N_MANIFEST: &str = include_str!("../../../prod-build/web/manifests/i18n.toml");
const DOCKERFILE_WEB: &str = include_str!("../../../Dockerfile.web");
const DOCKERFILE_WEB_DOCKERIGNORE: &str = include_str!("../../../Dockerfile.web.dockerignore");
const CI_WORKFLOW: &str = include_str!("../../../.github/workflows/ains.yml");
const FAVICON_ICO: &[u8] = include_bytes!("../assets/favicon.ico");

const PROD_MANIFESTS: [&str; 5] = [
    PROD_WEB_MANIFEST,
    PROD_UI_MANIFEST,
    PROD_CLIENT_API_MANIFEST,
    PROD_RUST_AGENT_MANIFEST,
    PROD_I18N_MANIFEST,
];

const PROD_PACKAGE_NAMES: [&str; 5] = ["web", "ui", "client-api", "rust-agent", "i18n"];

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DependencyContract {
    name: String,
    rename: Option<String>,
    requirement: String,
    kind: Option<String>,
    optional: bool,
    uses_default_features: bool,
    features: Vec<String>,
    target: Option<String>,
    source: Option<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LockPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
}

struct StagedWorkspace(std::path::PathBuf);

impl StagedWorkspace {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ains-prod-web-metadata-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("应创建临时 Web 生产 workspace");
        Self(root)
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().expect("staged file should have a parent"))
            .expect("应创建临时 Web 生产 workspace 的父目录");
        std::fs::write(path, content).expect("应写入临时 Web 生产 workspace 文件");
    }
}

impl Drop for StagedWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 浏览器会在 WASM 注入带指纹的 favicon link 之前请求约定俗成的
/// `/favicon.ico`；运行镜像必须保留一个不带指纹的根路径副本。
#[test]
fn dockerfile_copies_favicon_to_conventional_root_path() {
    assert!(
        !FAVICON_ICO.is_empty(),
        "app/web/assets/favicon.ico 不应为空"
    );
    assert!(
        DOCKERFILE_WEB
            .contains("COPY app/web/assets/favicon.ico /usr/share/nginx/html/favicon.ico"),
        "Dockerfile.web 必须把 favicon.ico 复制到 nginx 根目录，避免 /favicon.ico 返回 404"
    );
}

// ── esbuild 版本同步契约 ──
//
// esbuild 与 wasm-bindgen 的校验方式**不同**：dioxus-cli 0.7.9 在 NO_DOWNLOADS=1 下通过
// `which::which("esbuild")` 从 PATH 解析 esbuild，且**不做任何版本比对**（见 dioxus 仓库
// packages/cli/src/esbuild.rs 的 `prefer_no_downloads()` 分支）。因此只要 PATH 上存在 esbuild
// 即可满足构建，Dockerfile 里 esbuild 的具体版本无需与 dioxus-cli 精确对齐。
//
// 我们据此只做**宽松校验**：确保 esbuild 已固定版本并安装到 PATH，并把上述“无版本比对”契约
// 绑定到具体的 dioxus-cli 版本上——一旦有人升级 dioxus-cli，测试会提醒重新确认该契约是否仍成立。

/// esbuild 解析契约所对应的 dioxus-cli 版本。该版本下 dx 用 `which esbuild` 解析、不校验版本。
const DIOXUS_CLI_VERSION: &str = "0.7.9";
/// dioxus-cli 0.7.9 内部固定（联网时会自动下载）的 esbuild 版本，仅作参考：
/// 因离线模式不校验版本，Dockerfile 可安装其它版本（当前为 0.28.1）而不冲突。
const DIOXUS_CLI_PINNED_ESBUILD: &str = "0.27.3";

/// 从 `Cargo.lock` 提取指定 crate 的版本。
/// 精确匹配 `name = "<crate>"` 整行，避免命中 `wasm-bindgen-futures` 等同前缀包。
fn cargo_lock_version(lock: &str, crate_name: &str) -> Option<String> {
    let name_line = format!("name = \"{crate_name}\"");
    for block in lock.split("[[package]]") {
        if !block.lines().any(|l| l.trim() == name_line) {
            continue;
        }
        for l in block.lines() {
            if let Some(rest) = l.trim().strip_prefix("version = \"")
                && let Some(end) = rest.find('"')
            {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

fn cargo_lock_versions(lock: &str, crate_name: &str) -> Vec<String> {
    let name_line = format!("name = \"{crate_name}\"");
    let mut versions = lock
        .split("[[package]]")
        .filter(|block| block.lines().any(|line| line.trim() == name_line))
        .filter_map(|block| {
            block.lines().find_map(|line| {
                let rest = line.trim().strip_prefix("version = \"")?;
                let end = rest.find('"')?;
                Some(rest[..end].to_string())
            })
        })
        .collect::<Vec<_>>();
    versions.sort();
    versions.dedup();
    versions
}

fn quoted_lock_field(block: &str, field: &str) -> Option<String> {
    let prefix = format!("{field} = \"");
    block.lines().find_map(|line| {
        let value = line.trim().strip_prefix(&prefix)?;
        Some(value.strip_suffix('"').unwrap_or(value).to_owned())
    })
}

fn cargo_lock_packages(lock: &str) -> std::collections::BTreeSet<LockPackage> {
    lock.split("[[package]]")
        .skip(1)
        .map(|block| LockPackage {
            name: quoted_lock_field(block, "name").expect("lock package 应包含 name"),
            version: quoted_lock_field(block, "version").expect("lock package 应包含 version"),
            source: quoted_lock_field(block, "source"),
            checksum: quoted_lock_field(block, "checksum"),
        })
        .collect()
}

fn cargo_metadata(manifest_path: &std::path::Path) -> serde_json::Value {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(manifest_path)
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .expect("应能执行 cargo metadata 校验 Web workspace");
    assert!(
        output.status.success(),
        "cargo metadata 必须成功。\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata 应输出合法 JSON")
}

fn metadata_package<'a>(
    metadata: &'a serde_json::Value,
    package_name: &str,
) -> &'a serde_json::Value {
    metadata["packages"]
        .as_array()
        .expect("cargo metadata packages 应为数组")
        .iter()
        .find(|package| package["name"].as_str() == Some(package_name))
        .unwrap_or_else(|| panic!("cargo metadata 缺少 {package_name} 包"))
}

fn dependency_contracts(
    metadata: &serde_json::Value,
    package_name: &str,
) -> Vec<DependencyContract> {
    let package = metadata_package(metadata, package_name);
    let mut contracts = package["dependencies"]
        .as_array()
        .expect("cargo metadata dependencies 应为数组")
        .iter()
        .filter_map(|dependency| {
            let kind = dependency["kind"].as_str().map(str::to_owned);
            if kind.as_deref() == Some("dev") {
                return None;
            }

            let target = dependency["target"].as_str().map(str::to_owned);
            if target
                .as_deref()
                .is_some_and(|value| value != "cfg(target_arch = \"wasm32\")")
            {
                return None;
            }

            let name = dependency["name"]
                .as_str()
                .expect("dependency name 应为字符串")
                .to_owned();
            let mut features = dependency["features"]
                .as_array()
                .expect("dependency features 应为数组")
                .iter()
                .map(|feature| {
                    feature
                        .as_str()
                        .expect("dependency feature 应为字符串")
                        .to_owned()
                })
                .collect::<Vec<_>>();
            features.sort();

            Some(DependencyContract {
                rename: dependency["rename"].as_str().map(str::to_owned),
                requirement: if name == "dioxus" {
                    // 根 workspace 使用兼容范围，生产 workspace 则与 CLI 精确锁定；
                    // 两份 lockfile 的唯一解析版本由独立断言保证一致。
                    "<dioxus-cli-pinned>".to_owned()
                } else {
                    dependency["req"]
                        .as_str()
                        .expect("dependency req 应为字符串")
                        .to_owned()
                },
                kind,
                optional: dependency["optional"].as_bool().unwrap_or(false),
                uses_default_features: dependency["uses_default_features"]
                    .as_bool()
                    .unwrap_or(true),
                features,
                target,
                source: dependency["source"].as_str().map(str::to_owned),
                name,
            })
        })
        .collect::<Vec<_>>();
    contracts.sort();
    contracts
}

fn package_features(
    metadata: &serde_json::Value,
    package_name: &str,
) -> std::collections::BTreeMap<String, Vec<String>> {
    metadata_package(metadata, package_name)["features"]
        .as_object()
        .expect("cargo metadata features 应为对象")
        .iter()
        .filter(|(feature, _)| !(package_name == "web" && feature.as_str() == "desktop"))
        .map(|(feature, members)| {
            let mut members = members
                .as_array()
                .expect("feature members 应为数组")
                .iter()
                .map(|member| {
                    member
                        .as_str()
                        .expect("feature member 应为字符串")
                        .to_owned()
                })
                .collect::<Vec<_>>();
            members.sort();
            (feature.clone(), members)
        })
        .collect()
}

fn source_workspace_metadata() -> serde_json::Value {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("Cargo.toml");
    cargo_metadata(&manifest)
}

/// Docker 构建必须使用独立的 Web-only workspace，且不能重新复制完整
/// workspace 中的 desktop/mobile/server manifests。
#[test]
fn dockerfile_uses_pruned_web_only_workspace() {
    assert!(
        DOCKERFILE_WEB.contains("COPY prod-build/web/Cargo.toml prod-build/web/Cargo.lock ./"),
        "Dockerfile.web 应使用 prod-build/web 下的独立 workspace 与锁文件"
    );
    for forbidden in [
        "COPY Cargo.toml Cargo.lock ./",
        "COPY app/desktop/",
        "COPY app/mobile/",
        "COPY server/",
    ] {
        assert!(
            !DOCKERFILE_WEB.contains(forbidden),
            "Dockerfile.web 不应包含完整 workspace 输入：{forbidden}"
        );
    }
    assert!(
        DOCKERFILE_WEB.contains("cargo fetch --locked")
            && DOCKERFILE_WEB.contains("dx build --locked --release --package web --platform web")
            && DOCKERFILE_WEB.contains("target=/app/target,sharing=locked")
            && DOCKERFILE_WEB.contains("/app/web-public")
            && DOCKERFILE_WEB.contains("CARGO_NET_OFFLINE=true"),
        "Web Docker 构建应锁定精简依赖、缓存编译产物，并让唯一一次 dx 编译离线运行"
    );
    assert!(
        !DOCKERFILE_WEB.contains("cargo check ") && !DOCKERFILE_WEB.contains("cargo build "),
        "部署构建不应在 dx build 前重复编译 Web 依赖"
    );
}

/// `/app/target` 是跨构建持久化的 BuildKit cache。Dioxus 0.7.9 不保证删除
/// 上一次 public 输出中已经从源码移除的资源，因此必须在 `dx build` 前只清理
/// 可重新生成的 public 子目录，不能为了干净输出而丢弃整个 Cargo 编译缓存。
#[test]
fn dockerfile_clears_cached_dioxus_public_output_before_build() {
    const BUILD_STEP: &str =
        "RUN --mount=type=cache,id=ains-web-target,target=/app/target,sharing=locked";
    const PUBLIC_OUTPUT: &str = "/app/target/dx/web/release/web/public";
    const CLEANUP: &str = "rm -rf -- /app/target/dx/web/release/web/public";
    const BUILD: &str = "dx build --locked --release --package web --platform web";

    let step = DOCKERFILE_WEB
        .split_once(BUILD_STEP)
        .expect("Dockerfile.web 应通过 BuildKit cache mount 构建 Web")
        .1
        .split_once("\n\n# ── Runtime stage")
        .expect("Web 构建步骤后应进入 runtime stage")
        .0;
    let cleanup_position = step
        .find(CLEANUP)
        .expect("Web 构建前应清理缓存中的旧 Dioxus public 输出");
    let build_position = step.find(BUILD).expect("Web 构建步骤应执行 dx build");

    assert!(
        cleanup_position < build_position,
        "旧 public 输出必须在 dx build 前清理，避免已删除资源进入新镜像"
    );
    assert!(
        step.contains(&format!("cp -a {PUBLIC_OUTPUT}/. /app/web-public/")),
        "dx build 的全新 public 输出应复制到 cache mount 外再打包"
    );
    for forbidden in [
        "rm -rf -- /app/target &&",
        "rm -rf /app/target &&",
        "rm -rf -- /app/target/*",
        "rm -rf /app/target/*",
    ] {
        assert!(
            !step.contains(forbidden),
            "不应清空整个 Cargo target cache：{forbidden}"
        );
    }
}

/// GHA 的 external layer cache 不会自动携带 BuildKit cache mount 内容。
/// 若 `cargo fetch` 只写 registry/git mount，新 runner 可能命中并跳过 fetch，
/// 随后的源码变更却会让离线 dx build 在空 mount 上失败。生产依赖因此必须进入
/// 普通镜像层；仅可把可丢弃、可重建的编译产物放入 cache mount。
#[test]
fn gha_layer_cache_preserves_dependencies_needed_by_the_offline_build() {
    assert!(
        CI_WORKFLOW.contains("cache-from: type=gha,scope=ains-web")
            && CI_WORKFLOW.contains("cache-to: type=gha,mode=max,scope=ains-web"),
        "生产镜像应继续使用 GHA external layer cache"
    );
    for forbidden_mount in [
        "target=/usr/local/cargo/registry",
        "target=/usr/local/cargo/git",
    ] {
        assert!(
            !DOCKERFILE_WEB.contains(forbidden_mount),
            "GHA layer cache 不保存 {forbidden_mount} 的内容；离线构建依赖必须留在普通镜像层"
        );
    }
    assert!(
        DOCKERFILE_WEB.contains("RUN cargo fetch --locked")
            && DOCKERFILE_WEB.contains("target=/app/target,sharing=locked")
            && DOCKERFILE_WEB.contains("CARGO_NET_OFFLINE=true"),
        "生产构建应将锁定依赖持久化到镜像层，仅缓存可重建的 target 产物，再离线编译"
    );
}

/// Registry stalls must fail within a practical CI window. Cargo's per-request
/// timeout/retry settings are bounded in the image, and the complete Web job has
/// a hard deadline covering tool installation, native/WASM checks, and Docker.
#[test]
fn docker_and_ci_bound_dependency_download_stalls() {
    for required in ["ENV CARGO_HTTP_TIMEOUT=120", "ENV CARGO_NET_RETRY=3"] {
        assert!(
            DOCKERFILE_WEB.contains(required),
            "Docker 依赖下载应使用有界网络策略：缺少 {required}"
        );
    }

    let web_job = CI_WORKFLOW
        .split_once("\n  web:\n")
        .expect("CI 应包含 Web job")
        .1
        .split_once("\n  client-api:\n")
        .expect("Web job 后应包含 client-api job")
        .0;
    assert!(
        web_job.contains("\n    timeout-minutes: 120\n"),
        "完整 Web job 应设置 120 分钟硬超时，避免 registry/build stall 占满 runner"
    );
}

/// 构建上下文也使用 allowlist，避免未被 COPY 的 Desktop/Mobile/Server 源码
/// 仍被发送给 Docker builder。
#[test]
fn dockerfile_context_excludes_non_web_projects() {
    let rules = DOCKERFILE_WEB_DOCKERIGNORE.lines().collect::<Vec<_>>();
    assert!(
        rules.contains(&"**"),
        "Dockerfile.web.dockerignore 应以全量排除作为 allowlist 基线"
    );
    for required_exception in [
        "!Dockerfile.web",
        "!prod-build/",
        "!prod-build/web/",
        "!prod-build/web/**",
        "!app/",
        "!app/web/",
        "!app/web/Dioxus.toml",
        "!app/web/src/**",
        "!app/web/assets/**",
        "!app/ui/",
        "!app/ui/src/**",
        "!app/ui/assets/**",
        "!app/client-api/",
        "!app/client-api/src/**",
        "!crates/",
        "!crates/rust-agent/",
        "!crates/rust-agent/src/**",
        "!crates/i18n/",
        "!crates/i18n/src/**",
        "!nginx/",
        "!nginx/default.conf",
    ] {
        assert!(
            rules.contains(&required_exception),
            "Web 构建上下文缺少必要 allowlist 规则：{required_exception}"
        );
    }
    for forbidden_prefix in ["!app/desktop", "!app/mobile", "!server"] {
        assert!(
            !rules.iter().any(|line| line.starts_with(forbidden_prefix)),
            "Web 构建上下文不应重新包含 {forbidden_prefix}"
        );
    }
}

/// Web-only workspace 的成员和锁文件都不得包含 AINS 的非 Web 包，以及从
/// 本地双平台 manifest 中剔除的 Native/测试专属依赖。
#[test]
fn production_web_workspace_excludes_native_and_test_packages() {
    for member in [
        "app/web",
        "app/ui",
        "app/client-api",
        "crates/rust-agent",
        "crates/i18n",
    ] {
        assert!(
            PROD_WEB_CARGO_TOML.contains(&format!("\"{member}\"")),
            "Web-only workspace 缺少必要成员 {member}"
        );
    }
    for forbidden_member in ["server", "app/desktop", "app/mobile"] {
        assert!(
            !PROD_WEB_CARGO_TOML.contains(&format!("\"{forbidden_member}\"")),
            "Web-only workspace 不应包含 {forbidden_member}"
        );
    }
    for forbidden_package in [
        "ains-server",
        "desktop",
        "mobile",
        "rfd",
        "redb",
        "hnsw_rs",
        "pdf-extract",
        "wiremock",
        "which",
    ] {
        assert!(
            cargo_lock_version(PROD_WEB_CARGO_LOCK, forbidden_package).is_none(),
            "Web 生产锁文件不应包含 Native/测试专属包 {forbidden_package}"
        );
    }
}

/// 定时安全审计只需扫描根 Cargo.lock，前提是生产镜像锁定的每个外部包
/// （含 source/checksum）都由根锁文件中的同一身份覆盖。这样不会重复运行
/// audit-check，也不会因两份锁文件以后独立解析版本而漏扫生产依赖。
#[test]
fn production_lockfile_is_covered_by_root_security_audit() {
    assert!(
        CI_WORKFLOW.contains("uses: rustsec/audit-check@v2"),
        "CI 应保留根 Cargo.lock 的定期 RustSec 审计"
    );

    let root_packages = cargo_lock_packages(CARGO_LOCK);
    let missing = cargo_lock_packages(PROD_WEB_CARGO_LOCK)
        .difference(&root_packages)
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "Web 生产锁文件包含根 Cargo.lock 未以相同版本/source/checksum 覆盖的包，\
         现有 RustSec 审计会漏扫这些生产依赖：{missing:#?}"
    );
}

/// Docker 专用 manifests 只允许生产依赖。通过 Cargo 自身解析两套 manifests，
/// 语义比较 Web/WASM 依赖、features 和包元数据，避免格式变化影响测试，也避免
/// `[workspace.dependencies]` 的版本/features 漂移绕过逐行字符串检查。
#[test]
fn production_manifests_match_source_web_contracts_semantically() {
    for production in PROD_MANIFESTS {
        for forbidden_section in [
            "[dev-dependencies]",
            ".dev-dependencies]",
            "cfg(not(target_arch = \"wasm32\"))",
            "cfg(target_os =",
        ] {
            assert!(
                !production.contains(forbidden_section),
                "Web 生产 manifest 不应包含 {forbidden_section}"
            );
        }
    }

    let workspace = stage_production_web_workspace();
    let source_metadata = source_workspace_metadata();
    let production_metadata = cargo_metadata(&workspace.0.join("Cargo.toml"));

    for package_name in PROD_PACKAGE_NAMES {
        for field in ["version", "edition", "rust_version"] {
            assert!(
                metadata_package(&source_metadata, package_name)[field]
                    == metadata_package(&production_metadata, package_name)[field],
                "Web 生产包 {package_name} 的 {field} 与源 manifest 不一致"
            );
        }
        assert_eq!(
            dependency_contracts(&production_metadata, package_name),
            dependency_contracts(&source_metadata, package_name),
            "Web 生产包 {package_name} 的生产依赖契约与源 manifest 不一致"
        );
        assert_eq!(
            package_features(&production_metadata, package_name),
            package_features(&source_metadata, package_name),
            "Web 生产包 {package_name} 的 feature 契约与源 manifest 不一致"
        );
    }
}

/// 证明语义比较会深入解析 workspace 继承的 features，而不只是比较成员
/// manifest 中相同的 `serde = { workspace = true }` 文本。
#[test]
fn production_manifest_semantic_check_rejects_workspace_feature_drift() {
    let workspace = stage_production_web_workspace();
    let drifted = PROD_WEB_CARGO_TOML.replacen(
        "serde = { version = \"1\", features = [\"derive\"] }",
        "serde = { version = \"1\" }",
        1,
    );
    assert_ne!(
        drifted, PROD_WEB_CARGO_TOML,
        "测试必须实际移除 serde feature"
    );
    workspace.write("Cargo.toml", &drifted);

    let source_metadata = source_workspace_metadata();
    let production_metadata = cargo_metadata(&workspace.0.join("Cargo.toml"));
    assert_ne!(
        dependency_contracts(&production_metadata, "web"),
        dependency_contracts(&source_metadata, "web"),
        "workspace 依赖 feature 漂移必须被语义契约检查发现"
    );
}

fn stage_production_web_workspace() -> StagedWorkspace {
    let workspace = StagedWorkspace::new();
    for (relative, content) in [
        ("Cargo.toml", PROD_WEB_CARGO_TOML),
        ("Cargo.lock", PROD_WEB_CARGO_LOCK),
        ("app/web/Cargo.toml", PROD_WEB_MANIFEST),
        ("app/ui/Cargo.toml", PROD_UI_MANIFEST),
        ("app/client-api/Cargo.toml", PROD_CLIENT_API_MANIFEST),
        ("crates/rust-agent/Cargo.toml", PROD_RUST_AGENT_MANIFEST),
        ("crates/i18n/Cargo.toml", PROD_I18N_MANIFEST),
        ("app/web/src/main.rs", "fn main() {}\n"),
        ("app/ui/src/lib.rs", "pub fn placeholder() {}\n"),
        ("app/client-api/src/lib.rs", "pub fn placeholder() {}\n"),
        ("crates/rust-agent/src/lib.rs", "pub fn placeholder() {}\n"),
        ("crates/i18n/src/lib.rs", "pub fn placeholder() {}\n"),
    ] {
        workspace.write(relative, content);
    }
    workspace
}

fn production_workspace_tree(workspace: &StagedWorkspace) -> std::process::Output {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    std::process::Command::new(cargo)
        .args([
            "tree",
            "--locked",
            "--offline",
            "--depth",
            "0",
            "--manifest-path",
        ])
        .arg(workspace.0.join("Cargo.toml"))
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .expect("应能执行 cargo tree 校验 Web 生产 workspace")
}

/// 静态字符串断言无法证明 Docker 专用 lockfile 仍可解析。这里复刻
/// Dockerfile 的 manifest 布局，并用 `cargo tree --locked --offline --depth 0`
/// 解析锁定依赖图而不下载 crate 源码；依赖编译由既有 native/wasm CI 步骤覆盖。
#[test]
fn production_web_workspace_lockfile_is_resolvable_offline() {
    let workspace = stage_production_web_workspace();
    let output = production_workspace_tree(&workspace);

    assert!(
        output.status.success(),
        "Web 生产 workspace 的 manifests/Cargo.lock 必须可离线锁定解析。\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 证明上述真实解析不是空断言：只要生产 manifest 与锁文件发生漂移，
/// 同一条 `cargo tree --locked` 命令必须失败。
#[test]
fn production_web_workspace_tree_rejects_lockfile_drift() {
    let workspace = stage_production_web_workspace();
    let drifted = PROD_WEB_MANIFEST.replacen("version = \"0.1.0\"", "version = \"0.1.1\"", 1);
    assert_ne!(drifted, PROD_WEB_MANIFEST, "测试必须实际修改包版本");
    workspace.write("app/web/Cargo.toml", &drifted);

    let output = production_workspace_tree(&workspace);
    assert!(
        !output.status.success(),
        "manifest 与 Cargo.lock 漂移时，cargo tree --locked 必须失败"
    );
}

#[test]
fn ci_web_release_build_uses_the_committed_lockfile() {
    assert!(
        CI_WORKFLOW.contains("dx build --locked --release --package web --platform web"),
        "CI Web release 构建必须使用 --locked，避免验证与提交锁文件不同的依赖图"
    );
}

#[test]
fn ci_builds_the_production_web_image_when_its_inputs_change() {
    for required in [
        "fetch-depth: 0",
        "id: prod_web_changes",
        ".github/workflows/ains.yml Dockerfile.web",
        "Dockerfile.web.dockerignore prod-build/web",
        "app/web/Cargo.toml app/web/Dioxus.toml",
        "crates/rust-agent/Cargo.toml crates/i18n/Cargo.toml",
        "app/web/src app/web/assets app/ui/src app/ui/assets",
        "app/client-api/src crates/rust-agent/src crates/i18n/src",
        "if: steps.prod_web_changes.outputs.changed == 'true'",
        "uses: docker/setup-buildx-action@v4",
        "uses: docker/build-push-action@v7",
        "file: Dockerfile.web",
        "target: runtime",
        "load: true",
        "cache-from: type=gha,scope=ains-web",
        "cache-to: type=gha,mode=max,scope=ains-web,ignore-error=true",
    ] {
        assert!(
            CI_WORKFLOW.contains(required),
            "CI 应在生产 Web 构建输入变化时执行真实镜像构建：缺少 {required}"
        );
    }
}

/// 构建步骤通过 load 导入的镜像必须被实际执行：校验 Nginx 配置，并确认
/// Dioxus 入口、favicon、WASM 与 JS 产物都非空，避免只验证“Docker 层可导出”。
#[test]
fn ci_smoke_tests_the_loaded_production_image() {
    for required in [
        "Smoke test production Web runtime image",
        "WEB_IMAGE: ains-web-ci:${{ github.sha }}",
        "docker run --rm --add-host ains-server:127.0.0.1",
        "--entrypoint sh \"${WEB_IMAGE}\" -ec",
        "nginx -t",
        "test -s /usr/share/nginx/html/index.html",
        "test -s /usr/share/nginx/html/favicon.ico",
        "set -- /usr/share/nginx/html/assets/*.wasm",
        "set -- /usr/share/nginx/html/assets/*.js",
    ] {
        assert!(
            CI_WORKFLOW.contains(required),
            "CI 应运行并检查生产 Web runtime 镜像：缺少 {required}"
        );
    }
}

fn production_change_pathspecs() -> Vec<&'static str> {
    let diff_block = CI_WORKFLOW
        .split_once("elif git diff --quiet")
        .expect("CI 应包含生产 Web 变更检测命令")
        .1
        .split_once("; then")
        .expect("生产 Web 变更检测命令应有 then 分支")
        .0;
    diff_block
        .split_once("-- \\")
        .expect("git diff 应使用 -- 分隔 revision 与 pathspec")
        .1
        .split_whitespace()
        .filter(|token| *token != "\\")
        .collect()
}

fn docker_context_copy_sources() -> Vec<&'static str> {
    DOCKERFILE_WEB
        .lines()
        .filter_map(|line| line.strip_prefix("COPY "))
        .filter(|copy| !copy.starts_with("--from="))
        .flat_map(|copy| {
            let fields = copy.split_whitespace().collect::<Vec<_>>();
            fields[..fields.len() - 1].to_vec()
        })
        .collect()
}

/// 变更检测必须覆盖 Dockerfile 从仓库上下文 COPY 的所有输入。由 Dockerfile
/// 动态推导来源，新增源码目录或配置 COPY 时无需再靠人工记住同步一份测试清单。
#[test]
fn production_image_change_detection_covers_every_docker_copy_input() {
    let pathspecs = production_change_pathspecs();
    let uncovered = docker_context_copy_sources()
        .into_iter()
        .filter(|source| {
            !pathspecs.iter().any(|pathspec| {
                *source == *pathspec
                    || source
                        .strip_prefix(pathspec)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
        })
        .collect::<Vec<_>>();

    assert!(
        uncovered.is_empty(),
        "生产镜像变更检测遗漏 Docker COPY 输入：{uncovered:?}"
    );
    assert!(
        !pathspecs
            .iter()
            .any(|pathspec| pathspec.starts_with("server")),
        "纯服务端源码不应触发 Web runtime 镜像构建"
    );
}

/// 从 Dockerfile / CI workflow 提取 `cargo binstall <pkg> --version <X>` 中的版本号 X。
fn binstall_version(text: &str, pkg: &str) -> Option<String> {
    let needle = format!("{pkg} --version ");
    let idx = text.find(&needle)?;
    let rest = &text[idx + needle.len()..];
    let ver: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!ver.is_empty()).then_some(ver)
}

/// 从 Dockerfile 提取 `ENV <VAR>=<X>` 中的版本号 X。
fn dockerfile_env_version(df: &str, var: &str) -> Option<String> {
    let needle = format!("ENV {var}=");
    let idx = df.find(&needle)?;
    let rest = &df[idx + needle.len()..];
    let ver: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!ver.is_empty()).then_some(ver)
}

/// 从 CI workflow 提取 esbuild tarball URL（`linux-${ARCH}-<X>.tgz`）中的版本号 X。
/// 该 URL 中的版本为字面量（非变量），漂移时直接与 Dockerfile 的 `ESBUILD_VERSION` 对齐。
/// 版本号后紧跟 `.tgz` 后缀，须在此处截断（否则 `take_while` 会把 `.tgz` 的前导点吃掉）。
fn workflow_esbuild_url_version(yml: &str) -> Option<String> {
    let needle = "linux-${ARCH}-";
    let idx = yml.find(needle)?;
    let rest = &yml[idx + needle.len()..];
    let end = rest.find(".tgz")?;
    let ver = &rest[..end];
    (!ver.is_empty()).then(|| ver.to_string())
}

/// 从 Dockerfile / CI workflow 提取指定架构分支的 esbuild SHA256 哈希。
/// 两种文件均在 `case` 分支内以 `echo "<sha256>  /tmp/esbuild.tgz" | sha256sum -c -`
/// 校验 tarball；`echo "` 与哈希可同行（Dockerfile）或换行（CI workflow），
/// 因此先定位分支起始 `{arch})`，取分支体（至 `;;`）内第一个 `"` 后的 64 位十六进制串。
fn esbuild_sha256(text: &str, arch: &str) -> Option<String> {
    let branch = format!("{arch})");
    let start = text.find(&branch)?;
    let rest = &text[start + branch.len()..];
    let end = rest.find(";;").unwrap_or(rest.len());
    let body = &rest[..end];
    let quote = body.find('"')?;
    let sha: String = body[quote + 1..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    (sha.len() == 64).then_some(sha)
}

/// Dockerfile.web 里硬编码的 wasm-bindgen-cli 版本必须与 Cargo.lock 的 wasm-bindgen 一致。
#[test]
fn dockerfile_wasm_bindgen_cli_matches_cargo_lock() {
    let lock_ver = cargo_lock_version(CARGO_LOCK, "wasm-bindgen")
        .expect("Cargo.lock 中应存在 wasm-bindgen 包");
    let df_ver = binstall_version(DOCKERFILE_WEB, "wasm-bindgen-cli")
        .expect("Dockerfile.web 中应通过 `cargo binstall wasm-bindgen-cli --version <X>` 固定版本");
    assert_eq!(
        df_ver, lock_ver,
        "Dockerfile.web 固定的 wasm-bindgen-cli 版本为 {df_ver}，但 Cargo.lock 中 \
         wasm-bindgen 为 {lock_ver}；NO_DOWNLOADS=1 下 dx 对 `wasm-bindgen --version` \
         做精确匹配，版本不一致会导致 Docker 构建失败。请同步更新 Dockerfile.web 的版本号。"
    );
    let prod_lock_ver = cargo_lock_version(PROD_WEB_CARGO_LOCK, "wasm-bindgen")
        .expect("prod-build/web/Cargo.lock 中应存在 wasm-bindgen 包");
    assert_eq!(
        prod_lock_ver, lock_ver,
        "Web 生产锁文件的 wasm-bindgen 为 {prod_lock_ver}，但根锁文件为 {lock_ver}；\
         Dockerfile.web 的 wasm-bindgen-cli 必须同时匹配两份锁文件"
    );
}

/// 防止回退到 dioxus-cli 0.7.9 不识别的 `WASM_BINDGEN_USE_LOCAL_OPT`（无效开关）。
#[test]
fn dockerfile_uses_no_downloads_not_inert_flag() {
    assert!(
        DOCKERFILE_WEB.contains("NO_DOWNLOADS=1"),
        "Dockerfile.web 必须通过 NO_DOWNLOADS=1 让 dx 使用本地工具（离线构建）"
    );
    // 仅禁止把它作为生效的 ENV 设置；注释里为解释历史原因而提及该名字是允许的。
    assert!(
        !DOCKERFILE_WEB.contains("ENV WASM_BINDGEN_USE_LOCAL_OPT"),
        "WASM_BINDGEN_USE_LOCAL_OPT 不被 dioxus-cli 0.7.9 识别（无效），应使用 NO_DOWNLOADS"
    );
}

/// esbuild 必须在 Dockerfile.web 中固定版本，并把二进制安装到 PATH 上。
/// 背景：dioxus-cli 0.7.9 在 NO_DOWNLOADS=1 下通过 `which esbuild` 解析（不校验版本），
/// 若镜像未预装 esbuild，`dx build` 会报 “esbuild not found on PATH and downloads are disabled”。
#[test]
fn dockerfile_esbuild_version_is_pinned() {
    let ver = dockerfile_env_version(DOCKERFILE_WEB, "ESBUILD_VERSION")
        .expect("Dockerfile.web 应通过 `ENV ESBUILD_VERSION=<X>` 固定 esbuild 版本");
    // 宽松校验：dx 不比对 esbuild 版本，这里只要求版本号形态合法（如 0.28.1）。
    assert!(
        ver.split('.')
            .all(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit())),
        "ESBUILD_VERSION={ver} 不是合法的版本号"
    );
    // 安装步骤必须复用 ${ESBUILD_VERSION}，避免版本号与实际下载地址脱节。
    assert!(
        DOCKERFILE_WEB.contains("registry.npmjs.org/@esbuild/linux-${ARCH}")
            && DOCKERFILE_WEB.contains("${ESBUILD_VERSION}.tgz"),
        "esbuild 安装步骤应从 npm registry 使用 ${{ESBUILD_VERSION}} 下载对应平台包"
    );
    // 二进制必须落在 PATH 上（/usr/local/bin），否则 NO_DOWNLOADS 下 `which` 找不到。
    assert!(
        DOCKERFILE_WEB.contains("/usr/local/bin/esbuild"),
        "esbuild 二进制应安装到 PATH 上的 /usr/local/bin/esbuild"
    );
}

/// dioxus-cli 必须仍固定在我们验证过 esbuild 解析契约的版本（0.7.9）。
/// 该版本下 dx 用 `which esbuild` 解析、不校验版本；其内部固定 esbuild=0.27.3 仅作参考，
/// 故 Dockerfile 安装的 esbuild（当前 0.28.1）不会与之冲突。若升级 dioxus-cli，
/// 请重新核对其 esbuild 解析逻辑是否仍是“PATH 存在即可”，并按需同步版本。
#[test]
fn dioxus_toolchain_versions_stay_aligned() {
    let df_cli = binstall_version(DOCKERFILE_WEB, "dioxus-cli")
        .expect("Dockerfile.web 应通过 `cargo binstall dioxus-cli --version <X>` 固定版本");
    assert_eq!(
        df_cli, DIOXUS_CLI_VERSION,
        "Dockerfile.web 的 dioxus-cli 版本为 {df_cli}，而 esbuild 同步测试是针对 dioxus-cli \
         {DIOXUS_CLI_VERSION} 的解析契约（NO_DOWNLOADS 下 `which esbuild`、不校验版本，内部固定 \
         esbuild={DIOXUS_CLI_PINNED_ESBUILD}）验证的。升级 dioxus-cli 后请重新确认该契约仍成立。"
    );
    let ci_cli = binstall_version(CI_WORKFLOW, "dioxus-cli")
        .expect("CI 应通过 `cargo binstall dioxus-cli --version <X>` 固定版本");
    assert_eq!(
        ci_cli, df_cli,
        "CI 与 Dockerfile.web 必须使用同一 dioxus-cli 版本"
    );
    for (label, lock) in [("根", CARGO_LOCK), ("生产", PROD_WEB_CARGO_LOCK)] {
        assert_eq!(
            cargo_lock_versions(lock, "dioxus"),
            vec![DIOXUS_CLI_VERSION.to_owned()],
            "{label} Cargo.lock 必须只解析到与 dioxus-cli 一致的 dioxus 版本"
        );
    }
}

/// CI workflow 硬编码的 wasm-bindgen-cli 版本必须与 Cargo.lock 一致。
/// 审查修复：web job 的 `dx build` 同样在 NO_DOWNLOADS=1 下对
/// `wasm-bindgen --version` 做精确匹配；Dockerfile 与 workflow 两侧版本任何
/// 一侧漂移都会导致构建失败（Docker 构建或 CI 深处报错、信息晦涩），
/// 本测试把 workflow 侧也纳入编译期防护。
#[test]
fn ci_workflow_wasm_bindgen_cli_matches_cargo_lock() {
    let lock_ver = cargo_lock_version(CARGO_LOCK, "wasm-bindgen")
        .expect("Cargo.lock 中应存在 wasm-bindgen 包");
    let wf_ver = binstall_version(CI_WORKFLOW, "wasm-bindgen-cli")
        .expect(".github/workflows/ains.yml 中应通过 `cargo binstall wasm-bindgen-cli --version <X>` 固定版本");
    assert_eq!(
        wf_ver, lock_ver,
        "CI workflow 固定的 wasm-bindgen-cli 版本为 {wf_ver}，但 Cargo.lock 中 \
         wasm-bindgen 为 {lock_ver}；NO_DOWNLOADS=1 下 dx 对 `wasm-bindgen --version` \
         做精确匹配，版本不一致会导致 CI 的 dx build 步骤失败。请同步更新 ains.yml 的版本号。"
    );
}

/// CI workflow 的 esbuild 版本必须与 Dockerfile.web 的 `ESBUILD_VERSION` 一致。
/// 两条链路各自从 npm 下载 tarball 并做 SHA256 校验，版本漂移会让 CI 验证的
/// 产物与生产镜像不一致（版本号同步由本测试约束，SHA256 同步由
/// `ci_workflow_esbuild_sha256_matches_dockerfile` 约束）。
#[test]
fn ci_workflow_esbuild_version_matches_dockerfile() {
    let df_ver = dockerfile_env_version(DOCKERFILE_WEB, "ESBUILD_VERSION")
        .expect("Dockerfile.web 应通过 `ENV ESBUILD_VERSION=<X>` 固定 esbuild 版本");
    let wf_ver = workflow_esbuild_url_version(CI_WORKFLOW)
        .expect(".github/workflows/ains.yml 的 esbuild 下载 URL 应形如 `linux-${ARCH}-<X>.tgz`");
    assert_eq!(
        wf_ver, df_ver,
        "CI workflow 的 esbuild 版本为 {wf_ver}，但 Dockerfile.web 为 {df_ver}；\
         CI 与生产两条构建链路应使用同一 esbuild 版本。"
    );
}

/// CI workflow 与 Dockerfile.web 的 esbuild SHA256 哈希必须一致。
/// 两侧各自硬编码 x64/arm64 哈希校验 npm tarball；版本号同步由
/// `ci_workflow_esbuild_version_matches_dockerfile` 约束，但升级 esbuild 时
/// 若只更新一侧哈希（或改版本而漏改哈希），会造成“CI 绿、生产红”或反之，
/// 本测试把两侧哈希也纳入编译期同步。
#[test]
fn ci_workflow_esbuild_sha256_matches_dockerfile() {
    for arch in ["x64", "arm64"] {
        let df_sha = esbuild_sha256(DOCKERFILE_WEB, arch).unwrap_or_else(|| {
            panic!("Dockerfile.web 应包含 `{arch}) echo \"<sha256>...` 形式的 esbuild 哈希校验")
        });
        let wf_sha = esbuild_sha256(CI_WORKFLOW, arch).unwrap_or_else(|| {
            panic!("ains.yml 应包含 `{arch}) echo \"<sha256>...` 形式的 esbuild 哈希校验")
        });
        assert_eq!(
            df_sha, wf_sha,
            "CI workflow 的 esbuild {arch} SHA256 为 {wf_sha}，但 Dockerfile.web 为 {df_sha}；\
             CI 与生产两条构建链路必须使用同一 tarball 校验哈希，请同步两侧哈希。"
        );
    }
}

/// 校验解析辅助函数：不应把 `wasm-bindgen-futures` 误判为 `wasm-bindgen`。
#[test]
fn cargo_lock_version_matches_exact_name_only() {
    let sample = "\
[[package]]
name = \"wasm-bindgen-futures\"
version = \"0.4.99\"

[[package]]
name = \"wasm-bindgen\"
version = \"0.2.126\"
";
    assert_eq!(
        cargo_lock_version(sample, "wasm-bindgen").as_deref(),
        Some("0.2.126")
    );
}

/// 校验 `ENV <VAR>=<X>` 版本解析：正确取到版本号，缺失时返回 None。
#[test]
fn dockerfile_env_version_parses_pinned_value() {
    let sample = "ENV ESBUILD_VERSION=0.28.1\nRUN echo hi\n";
    assert_eq!(
        dockerfile_env_version(sample, "ESBUILD_VERSION").as_deref(),
        Some("0.28.1")
    );
    assert_eq!(dockerfile_env_version(sample, "MISSING"), None);
}

/// 校验 workflow 中 esbuild tarball URL 的版本解析：正确取到版本号，缺失时返回 None。
#[test]
fn workflow_esbuild_url_version_parses_pinned_value() {
    let sample = "https://registry.npmjs.org/@esbuild/linux-${ARCH}/-/linux-${ARCH}-0.28.1.tgz";
    assert_eq!(
        workflow_esbuild_url_version(sample).as_deref(),
        Some("0.28.1")
    );
    assert_eq!(workflow_esbuild_url_version("no esbuild url"), None);
}

/// 校验架构分支的 esbuild SHA256 解析：正确取到各架构哈希，缺失架构返回 None。
/// 样本哈希使用占位值——解析函数与真实哈希值无关，真实值同步由
/// `ci_workflow_esbuild_sha256_matches_dockerfile` 约束。
#[test]
fn esbuild_sha256_parses_pinned_value() {
    let sample = "case \"$ARCH\" in \
      x64) echo \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  /tmp/esbuild.tgz\" | sha256sum -c - ;; \
      arm64) echo \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  /tmp/esbuild.tgz\" | sha256sum -c - ;; \
      *) echo \"Unsupported architecture: $ARCH\"; exit 1 ;; \
    esac";
    assert_eq!(
        esbuild_sha256(sample, "x64").as_deref(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    assert_eq!(
        esbuild_sha256(sample, "arm64").as_deref(),
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    );
    assert_eq!(esbuild_sha256(sample, "riscv64"), None);
}
