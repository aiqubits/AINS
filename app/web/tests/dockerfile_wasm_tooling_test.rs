//! Dockerfile.web WASM 工具链一致性回归测试（纯 std，无需网络 / Docker）。
//!
//! 背景：`Dockerfile.web` 在 `dx build` 前设置 `NO_DOWNLOADS=1`，使 dioxus-cli 0.7.9
//! 从 PATH 解析本地工具而非联网下载。此时 `dx` 会对 `wasm-bindgen --version` 做
//! **精确字符串匹配**，要求镜像内安装的 `wasm-bindgen-cli` 版本严格等于 `Cargo.lock`
//! 中 `wasm-bindgen` crate 的版本。若二者漂移（例如依赖升级后忘记同步 Dockerfile 里
//! 硬编码的版本号），Docker 构建会在 `dx build` 深处报错、信息晦涩。
//!
//! 本测试在**编译期**通过 `include_str!` 读取仓库根的 `Cargo.lock` 与 `Dockerfile.web`，
//! 静态校验两者版本一致，并防止有人把 `NO_DOWNLOADS` 退回到无效的
//! `WASM_BINDGEN_USE_LOCAL_OPT`。该测试跑在既有的 `cargo test --package web` CI 步骤内，
//! 因此每个 PR 都能拦截漂移。

// 相对本测试文件（app/web/tests/）：`../../../` 即仓库根。
const CARGO_LOCK: &str = include_str!("../../../Cargo.lock");
const DOCKERFILE_WEB: &str = include_str!("../../../Dockerfile.web");

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

/// 从 Dockerfile 提取 `cargo binstall <pkg> --version <X>` 中的版本号 X。
fn dockerfile_binstall_version(df: &str, pkg: &str) -> Option<String> {
    let needle = format!("{pkg} --version ");
    let idx = df.find(&needle)?;
    let rest = &df[idx + needle.len()..];
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

/// Dockerfile.web 里硬编码的 wasm-bindgen-cli 版本必须与 Cargo.lock 的 wasm-bindgen 一致。
#[test]
fn dockerfile_wasm_bindgen_cli_matches_cargo_lock() {
    let lock_ver = cargo_lock_version(CARGO_LOCK, "wasm-bindgen")
        .expect("Cargo.lock 中应存在 wasm-bindgen 包");
    let df_ver = dockerfile_binstall_version(DOCKERFILE_WEB, "wasm-bindgen-cli")
        .expect("Dockerfile.web 中应通过 `cargo binstall wasm-bindgen-cli --version <X>` 固定版本");
    assert_eq!(
        df_ver, lock_ver,
        "Dockerfile.web 固定的 wasm-bindgen-cli 版本为 {df_ver}，但 Cargo.lock 中 \
         wasm-bindgen 为 {lock_ver}；NO_DOWNLOADS=1 下 dx 对 `wasm-bindgen --version` \
         做精确匹配，版本不一致会导致 Docker 构建失败。请同步更新 Dockerfile.web 的版本号。"
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
fn dockerfile_dioxus_cli_pinned_for_esbuild_contract() {
    let df_cli = dockerfile_binstall_version(DOCKERFILE_WEB, "dioxus-cli")
        .expect("Dockerfile.web 应通过 `cargo binstall dioxus-cli --version <X>` 固定版本");
    assert_eq!(
        df_cli, DIOXUS_CLI_VERSION,
        "Dockerfile.web 的 dioxus-cli 版本为 {df_cli}，而 esbuild 同步测试是针对 dioxus-cli \
         {DIOXUS_CLI_VERSION} 的解析契约（NO_DOWNLOADS 下 `which esbuild`、不校验版本，内部固定 \
         esbuild={DIOXUS_CLI_PINNED_ESBUILD}）验证的。升级 dioxus-cli 后请重新确认该契约仍成立。"
    );
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
