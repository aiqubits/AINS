//! Python `fnmatch.fnmatch` 语义的纯 Rust 实现（双 target，无依赖）。
//!
//! 供权限引擎 PathRule / 敏感路径黑名单与 Hook matcher 使用，与
//! OpenHarness `permissions/checker.py` + `hooks/executor.py` 的 fnmatch
//! 口径逐字对齐：
//! - `*` 匹配任意字符序列（**包括 `/`**，非路径感知——与 glob 不同）；
//! - `?` 匹配任意单字符；
//! - `[seq]` 字符类（支持 `a-z` 区间），`[!seq]` 取反；
//! - `[` 无闭合 `]` 时按字面量处理；`[]]` / `[!]]` 中首位 `]` 为字面量；
//! - 全串匹配（Python `re.match` + `\Z`）。

/// 判断 `name` 是否整体匹配 `pattern`（大小写敏感，等价 posix fnmatchcase）。
pub fn fnmatch(name: &str, pattern: &str) -> bool {
    let name: Vec<char> = name.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    match_here(&name, &pattern)
}

fn match_here(name: &[char], pattern: &[char]) -> bool {
    let mut n = 0usize;
    let mut p = 0usize;
    // `*` 回溯点：记录最近一个 `*` 的 pattern 位置与当时的 name 位置
    let mut star_p: Option<usize> = None;
    let mut star_n = 0usize;

    while n < name.len() {
        if p < pattern.len() {
            match pattern[p] {
                '*' => {
                    star_p = Some(p);
                    star_n = n;
                    p += 1;
                    continue;
                }
                '?' => {
                    n += 1;
                    p += 1;
                    continue;
                }
                '[' => {
                    if let Some((matched, next_p)) = match_class(name[n], pattern, p) {
                        if matched {
                            n += 1;
                            p = next_p;
                            continue;
                        }
                    } else if pattern[p] == name[n] {
                        // 无闭合 `]`：`[` 按字面量
                        n += 1;
                        p += 1;
                        continue;
                    }
                }
                literal => {
                    if literal == name[n] {
                        n += 1;
                        p += 1;
                        continue;
                    }
                }
            }
        }
        // 当前位置匹配失败：回溯到最近的 `*`，让它多吞一个字符
        match star_p {
            Some(sp) => {
                star_n += 1;
                n = star_n;
                p = sp + 1;
            }
            None => return false,
        }
    }
    // name 耗尽：pattern 余下必须全为 `*`
    pattern[p..].iter().all(|&c| c == '*')
}

/// 尝试在 `pattern[start]`（值为 `[`）处解析字符类并匹配 `ch`。
/// 返回 `Some((是否匹配, 类结束后的 pattern 下标))`；无闭合 `]` 返回 `None`。
fn match_class(ch: char, pattern: &[char], start: usize) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negated = pattern.get(i) == Some(&'!');
    if negated {
        i += 1;
    }
    let class_start = i;
    // 首位 `]` 为字面量成员
    let mut end = i;
    loop {
        match pattern.get(end) {
            None => return None,
            Some(']') if end > class_start => break,
            _ => end += 1,
        }
    }

    let members = &pattern[class_start..end];
    let mut matched = false;
    let mut j = 0usize;
    while j < members.len() {
        // 区间 `a-z`（`-` 在首/尾位时为字面量）
        if j + 2 < members.len() && members[j + 1] == '-' {
            if members[j] <= ch && ch <= members[j + 2] {
                matched = true;
            }
            j += 3;
        } else {
            if members[j] == ch {
                matched = true;
            }
            j += 1;
        }
    }
    Some((matched != negated, end + 1))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::fnmatch;

    #[test]
    fn star_crosses_path_separators() {
        // 与 Python fnmatch 一致：`*` 匹配 `/`（权限黑名单依赖此语义）
        assert!(fnmatch("/home/user/.ssh/id_rsa", "*/.ssh/*"));
        assert!(fnmatch("/root/.aws/credentials", "*/.aws/credentials"));
        assert!(!fnmatch("/home/user/ssh/id_rsa", "*/.ssh/*"));
    }

    #[test]
    fn question_mark_and_literal() {
        assert!(fnmatch("a.rs", "?.rs"));
        assert!(!fnmatch("ab.rs", "?.rs"));
        assert!(fnmatch("exact", "exact"));
        assert!(!fnmatch("exact2", "exact"));
    }

    #[test]
    fn char_class_and_negation() {
        assert!(fnmatch("file1.txt", "file[0-9].txt"));
        assert!(!fnmatch("filex.txt", "file[0-9].txt"));
        assert!(fnmatch("filex.txt", "file[!0-9].txt"));
        assert!(fnmatch("a]b", "a[]]b")); // 首位 `]` 字面量
    }

    #[test]
    fn unclosed_bracket_is_literal() {
        assert!(fnmatch("a[b", "a[b"));
        assert!(!fnmatch("ab", "a[b"));
    }

    #[test]
    fn full_match_semantics() {
        // 全串匹配：前缀命中不算
        assert!(!fnmatch("abc.txt", "abc"));
        assert!(fnmatch("abc", "abc*"));
        assert!(fnmatch("abc", "***"));
        assert!(!fnmatch("abc", ""));
        assert!(fnmatch("", ""));
        assert!(fnmatch("", "*"));
    }

    #[test]
    fn directory_root_with_trailing_slash() {
        // 目录根尾随 `/` 参与匹配（对齐 _policy_match_paths 语义）
        assert!(fnmatch("/home/user/.ssh/", "*/.ssh/*"));
        assert!(!fnmatch("/home/user/.ssh", "*/.ssh/*"));
    }
}
