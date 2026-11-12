//! 统一诊断渲染（v1.10 工具链强化）：人读更清楚、机读可解析。
//!
//! 设计约束（不改语义判定、不破既有测试）：
//!   - **人读模式先原样打印原消息**，再追加「文件:行:列 + 稳定错误码 + 源码行 +
//!     caret」。既有回归靠匹配原消息文本，因此原消息一个字都不能动。
//!   - `DIALC_DIAG_FORMAT=json` 时改为输出**一行 JSON**（stderr），供编辑器 / CI
//!     机读；stdout 始终不被诊断污染。
//!   - 错误信息是 `frontend()` 汇总后的 `String`，结构化行列号已编码在文本里，
//!     这里只做解析与渲染，**绝不重新判定错误**。

use std::fs;

pub fn is_json() -> bool {
    std::env::var("DIALC_DIAG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false)
}

/// 从消息里解析「第 N 行 第 M 列」（DialcError 的 Display 格式）。
fn parse_pos(msg: &str) -> (usize, usize) {
    let chars: Vec<char> = msg.chars().collect();
    let mut line = 0usize;
    let mut col = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '第' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let start = j;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                let n: usize = chars[start..j].iter().collect::<String>().parse().unwrap_or(0);
                let mut k = j;
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                if chars.get(k) == Some(&'行') {
                    line = n;
                } else if chars.get(k) == Some(&'列') {
                    col = n;
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    (line, col)
}

/// 稳定错误码：优先取消息里已有的 `dialc.xxx`，否则按阶段推断。
fn infer_code(msg: &str) -> String {
    if let Some(p) = msg.find("dialc.") {
        let rest = &msg[p..];
        let code: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_')
            .collect();
        if code.len() > 6 {
            return code;
        }
    }
    if msg.contains("词法") || msg.contains("lex") {
        return "dialc.lex".to_string();
    }
    if msg.contains("找不到已安装的包") || msg.contains("包缓存") {
        return "dialc.package.missing".to_string();
    }
    if msg.contains("重复定义") || msg.contains("重复声明") {
        return "dialc.duplicate".to_string();
    }
    if msg.contains("语法") || msg.contains("期望") || msg.contains("解析") {
        return "dialc.parse".to_string();
    }
    if msg.contains("类型") || msg.contains("未定义") || msg.contains("不匹配") {
        return "dialc.sema".to_string();
    }
    "dialc.error".to_string()
}

fn source_line(path: &str, line: usize) -> String {
    if line == 0 {
        return String::new();
    }
    match fs::read_to_string(path) {
        Ok(s) => s
            .lines()
            .nth(line.saturating_sub(1))
            .unwrap_or("")
            .trim_end()
            .to_string(),
        Err(_) => String::new(),
    }
}

/// 报告一条前端错误。人读模式保持原消息不变，只追加定位信息。
pub fn report(msg: &str, src_path: &str) {
    let (line, col) = parse_pos(msg);
    let snippet = source_line(src_path, line);
    let code = infer_code(msg);
    if is_json() {
        eprintln!(
            "{}",
            serde_json::json!({
                "tool": "dialc",
                "severity": "error",
                "code": code,
                "message": msg,
                "file": src_path,
                "line": line,
                "col": col,
                "snippet": snippet,
                "caret": if col > 0 { " ".repeat(col.saturating_sub(1)) + "^" } else { String::new() },
            })
        );
        return;
    }
    // 原消息一字不动：既有回归、LSP 之外的 CLI 用户都依赖这个文本。
    eprintln!("{}", msg);
    if line > 0 && !snippet.is_empty() {
        eprintln!("    --> {}:{}:{}  [{}]", src_path, line, col, code);
        eprintln!("     | {}", snippet);
        eprintln!("     | {}^", " ".repeat(col.saturating_sub(1)));
    }
}
