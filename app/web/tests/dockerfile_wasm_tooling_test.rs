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
