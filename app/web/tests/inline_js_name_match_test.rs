//! wasm-bindgen `inline_js` 导出名与 Rust `extern "C"` 函数名一致性回归测试（纯 std，无网络）。
//!
//! 背景：本项目在 wasm 侧用 `#[wasm_bindgen(inline_js = "...")]` 内联了几段 JS
//! （Web Locks、OPFS、目录导入导出）。wasm-bindgen 对 `extern "C"` 导入使用的 JS
//! 绑定名是 **Rust 标识符原样**（不做任何大小写转换）。因此：
//!   - 若 inline_js 导出 `fooBar`，Rust 函数名必须是 `fooBar`（或通过
//!     `js_name = "fooBar"` 显式指定），否则 wasm-bindgen 生成的
//!     `import { foo_bar } from './snippets/...'` 在片段里找不到对应导出；
//!   - 此时 `dx build` 内联阶段的 esbuild 会报
//!     `No matching export in ".../snippets/.../inlineN.js" for import "foo_bar"`，
//!     并**回退为纯拷贝**，导致 `assets/snippets/` 目录缺失；
//!   - 运行时浏览器对 `assets/snippets/*.js` 逐个 404，WASM 引导失败 → **白屏**。
//!
//! 本测试在**编译期**读取相关源文件（`include_str!`），静态校验：inline_js 中每个
//! `export function <X>` 的 `<X>` 必须能在同一 `extern "C"` 块内找到一个同名 Rust
//! 函数（或在对应函数属性里以 `js_name = "<X>"` 显式声明）。这样任何再引入“函数名
//! 大小写与 JS 导出不一致”的改动都会在 CI 编译期被拦截，而非等 Docker 部署后才白屏。
//!
//! 该测试跑在既有的 `cargo test --package web` CI 步骤内（与
//! `dockerfile_wasm_tooling_test.rs` 同一机制）。

// 相对本测试文件（app/web/tests/）：`../../../` 即仓库根。
const RUST_AGENT_STORE: &str = include_str!("../../../crates/rust-agent/src/skills/store.rs");
const RUST_AGENT_FILES: &str = include_str!("../../../crates/rust-agent/src/skills/files.rs");
const WEB_AGENT_SERVICE: &str = include_str!("../../../app/web/src/agent/service.rs");
const WEB_SKILLS_VIEW: &str = include_str!("../../../app/web/src/views/skills.rs");

/// 提取一段源码中所有 inline_js 片段声明的 JS 导出名（`export function X` / `export async function X`）。
/// 返回形如 `(导出名, 出现位置)` 的列表，便于失败时给出可读信息。
fn inline_js_exports(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = source;
    while let Some(idx) = rest.find("export ") {
        let start = idx + "export ".len();
        let after = &rest[start..];
        // 只关心 `export [async ]function <name>`。
        let trimmed = after.trim_start();
        let fn_head = trimmed
            .strip_prefix("async function ")
            .or_else(|| trimmed.strip_prefix("function "));
        if let Some(head) = fn_head {
            let name: String = head
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
                .collect();
            if !name.is_empty() {
                out.push(name);
            }
        }
        rest = &rest[start + after.len()..];
    }
    out
}

/// 收集同一文件里 `extern "C"` 块中被 `#[wasm_bindgen(...)]` 修饰的函数所导入的名字。
/// 对每个函数返回其 Rust 标识符；若属性里带 `js_name = "<X>"`，则用 `<X>`（JS 导出名）。
/// 我们据此得知 wasm-bindgen 最终会用哪个名字去匹配 inline_js 导出。
///
/// 实现：逐 `extern "C" { ... }` 块解析。块内每一对 `#[wasm_bindgen(...)]` 属性 +
/// 紧随其后的 `fn <name>(` 视为一条导入。属性与函数名之间只允许空白/属性行，
/// 故用“最近一个 `#[wasm_bindgen` 之后、`fn ` 之前没有其它 `fn `”来配对即可，
/// 与行数无关，也不受 inline_js 字符串内容干扰。
fn extern_imports(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut search_from = 0;
    // 反复定位 `extern "C" { ... }` 块。
    while let Some(block_start) = source[search_from..].find("extern \"C\" {") {
        let block_start = search_from + block_start + "extern \"C\" {".len();
        // 找到该块的右花括号。注意不能直接 `find('}')`——块内注释里可能含字面 `}`（例如
        // `// import { x }`），会提前截断。Rust 的 extern 块收尾 `}` 一定独占一行（前面只有
        // 空白），故按“行首第一个非空白是 `}`”来定位才是可靠的。
        let rest = &source[block_start..];
        let block_end = rest
            .find('\n')
            .map(|_| {
                let mut idx = 0;
                while let Some(nl_rel) = rest[idx..].find('\n') {
                    let line_start = idx + nl_rel + 1;
                    let line_rest = &rest[line_start..];
                    let trimmed = line_rest.trim_start();
                    if trimmed.starts_with('}') {
                        return line_start + (line_rest.len() - trimmed.len());
                    }
                    idx = line_start;
                }
                rest.len()
            })
            .unwrap_or(rest.len());
        let block_end = block_start + block_end;
        let block = &source[block_start..block_end];

        // 块内按 `fn ` 定位所有函数，向上找最近的 `#[wasm_bindgen` 属性配对。
        let mut idx = 0;
        while let Some(rel) = block[idx..].find("fn ") {
            let fn_pos = idx + rel;
            let before = &block[..fn_pos];
            let attr_start = match before.rfind("#[wasm_bindgen") {
                Some(a) => a,
                None => {
                    // 无 wasm_bindgen 属性，跳过该函数继续。
                    idx = fn_pos + "fn ".len();
                    continue;
                }
            };
            let attr_text = &before[attr_start..];
            // 配对条件：属性到 `fn ` 之间没有其它 `fn `（避免跨函数误配）。
            let following: &str = &block[fn_pos..];
            let between = &before[attr_start + "#[wasm_bindgen".len()..fn_pos];
            if between.contains("fn ") {
                idx = fn_pos + "fn ".len();
                continue;
            }
            let name: String = following[3..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            // 从属性里取 js_name（若有）。
            let js_name = attr_text
                .split_once("js_name = \"")
                .map(|(_, r)| r.chars().take_while(|c| *c != '"').collect::<String>());
            let import_name = js_name.unwrap_or_else(|| name.clone());
            out.push(import_name);
            idx = fn_pos + "fn ".len() + name.len();
        }

        search_from = block_end;
    }
    out
}

/// 每个 inline_js 导出的名字都必须能在这个文件的 extern 导入里找到。
/// 若缺失，说明存在大小写不一致（或漏了 `js_name`），会导致 esbuild 失败 + 白屏。
fn assert_exports_match(source: &str, file_label: &str) {
    let exports = inline_js_exports(source);
    assert!(
        !exports.is_empty(),
        "{file_label}: 未解析到任何 inline_js export function，测试可能失效"
    );
    let imports = extern_imports(source);
    for name in &exports {
        assert!(
            imports.iter().any(|i| i == name),
            "{file_label}: inline_js 导出的 `{name}` 在对应 extern 块中找不到同名导入。\n\
             wasm-bindgen 使用 Rust 函数名原样作为 JS 导入绑定名（不做大小写转换）。\n\
             若 Rust 函数是 snake_case 而 JS 导出是 camelCase，须在函数属性上加 \
             `js_name = \"{name}\"`，否则 dx build 的 esbuild 阶段报 \
             `No matching export ... for import` 并回退为纯拷贝，导致 \
             `assets/snippets/` 缺失、浏览器 404、页面白屏。"
        );
    }
}

/// 每个被 wasm_bindgen 修饰的 extern 函数，其 JS 导入名也必须存在于某个 inline_js 导出。
/// 这是反向校验，防止出现“导出了但没被引用”的死导出（虽不致命，但提示不一致）。
#[test]
fn rust_agent_store_inline_js_exports_match_imports() {
    assert_exports_match(RUST_AGENT_STORE, "crates/rust-agent/src/skills/store.rs");
}

#[test]
fn rust_agent_files_inline_js_exports_match_imports() {
    assert_exports_match(RUST_AGENT_FILES, "crates/rust-agent/src/skills/files.rs");
}

#[test]
fn web_skills_view_inline_js_exports_match_imports() {
    assert_exports_match(WEB_SKILLS_VIEW, "app/web/src/views/skills.rs");
}

#[test]
fn browser_skill_export_serializes_tabs_without_destructive_cleanup() {
    assert!(
        WEB_SKILLS_VIEW
            .contains("navigator.locks.request('ains-skill-export-v1', {mode: 'exclusive'}"),
        "browser exports must serialize the check/create/write sequence across tabs"
    );
    assert!(
        !WEB_SKILLS_VIEW.contains("destination.removeEntry(payload.name, {recursive: true})"),
        "browser exports must not recursively delete a destination whose ownership cannot be proven"
    );
}

#[test]
fn automatic_memory_extraction_setting_uses_the_cross_tab_write_lock() {
    assert!(
        WEB_AGENT_SERVICE
            .contains("with_durable_memory_write_lock(service.set_auto_extract_enabled(enabled))"),
        "the auto-extract preference must share the durable-memory Web Lock with extraction and deletion"
    );
}

/// 回归锚点：`request_ains_skill_mutation_lock` 的导出名必须是 camelCase 的
/// `requestAinsSkillMutationLock`，且该函数必须显式声明 `js_name`。
/// 这正是此前导致生产白屏的根因（无 js_name → esbuild 找不到导出 → snippets 缺失）。
#[test]
fn web_lock_fn_pins_js_name_to_camelcase_export() {
    let store = RUST_AGENT_STORE;
    assert!(
        store.contains("js_name = \"requestAinsSkillMutationLock\""),
        "store.rs 的 request_ains_skill_mutation_lock 必须显式 js_name = \"requestAinsSkillMutationLock\"，\
         否则 wasm-bindgen 生成的 `import {{ request_ains_skill_mutation_lock }}` 在 inline1.js 中\
         找不到导出，esbuild 失败、snippets 缺失、页面白屏。"
    );
    // 确保这个 js_name 是加在锁函数的 `fn` 声明上（位于其后的 `fn request_ains_skill_mutation_lock` 之前）。
    // 源码序列为 `#[wasm_bindgen(catch, js_name = "requestAinsSkillMutationLock")]` + 换行 + `fn ...`。
    let exact =
        "js_name = \"requestAinsSkillMutationLock\")]\n        fn request_ains_skill_mutation_lock";
    assert!(
        store.contains(exact),
        "store.rs 中 js_name = \"requestAinsSkillMutationLock\" 必须紧邻并绑定到 \
         fn request_ains_skill_mutation_lock 声明"
    );
}
