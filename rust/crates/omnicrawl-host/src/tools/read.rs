//! `read` 工具：行窗口、超长行截断、行号与续读 footer，以及定位读取。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 的 `read_file_result` /
//! `_read_file_window` / `_format_read_lines` / `_find_function_range`。三种读取形态：
//! 普通行窗口（流式逐行）、`text` 片段定位、`function_name` 函数定位。此外
//! `omnicrawl://docs/` 内置文档 URI 走 `omnicrawl-mcp` 的编译期内嵌文档表。

use std::path::Path;

use omnicrawl_mcp::bundled::{read_bundled_doc, BUNDLED_DOC_URI_PREFIX};
use serde_json::{Map, Value};

use super::arguments::{limited_int, optional_text};
use super::error::{ToolError, ToolOutcome};
use super::paths::WorkspacePaths;
use super::sample::split_lines_keepends;

pub const READ_MAX_LINES: i64 = 500;
pub const READ_MAX_LINE_LENGTH: usize = 2_000;
/// MCP Server 路径使用的整读上限；主 Agent 路径不设上限（与 Python 一致）。
pub const MAX_FILE_READ_CHARS: usize = 200_000;
const READ_START_LINE_MAX: i64 = 100_000;
const READ_CONTEXT_LINES_MAX: i64 = 200;
/// 大括号扫描回退最多向下看多少行（与 Python 的 `start_index + 500` 一致）。
const BRACED_SCAN_LINES_MAX: usize = 500;

/// 函数定位结果：`(起始行, 结束行, 解析出的限定名)`，行号从 1 起。
type FunctionRange = (i64, i64, String);

pub fn read(paths: &WorkspacePaths, arguments: &Map<String, Value>) -> ToolOutcome {
    let raw_path = optional_text(arguments, "path");
    if raw_path.starts_with(BUNDLED_DOC_URI_PREFIX) {
        return read_bundled(&raw_path, arguments);
    }
    let path = paths.safe_path(&raw_path)?;
    let function_name = optional_text(arguments, "function_name");
    let snippet = optional_text(arguments, "text");
    if !function_name.is_empty() && !snippet.is_empty() {
        return Err(ToolError::new("function_name 和 text 不能同时指定。"));
    }
    let max_lines = limited_int(arguments, "max_lines", READ_MAX_LINES, 1, READ_MAX_LINES);
    let display = paths.relative(&path);
    if !path.is_file() {
        return Err(ToolError::new(format!("不是文件：{display}")));
    }
    if !function_name.is_empty() {
        return read_function(paths, &path, &function_name, max_lines);
    }
    if !snippet.is_empty() {
        return read_snippet(paths, &path, &snippet, arguments, max_lines);
    }
    let start_line = limited_int(arguments, "start_line", 1, 1, READ_START_LINE_MAX);
    read_window(paths, &path, start_line, max_lines)
}

/// 片段定位读取（普通文件）：整读后交给共用的片段渲染。
fn read_snippet(
    paths: &WorkspacePaths,
    path: &Path,
    snippet: &str,
    arguments: &Map<String, Value>,
    max_lines: i64,
) -> ToolOutcome {
    let display = paths.relative(path);
    let text = read_text(path, None)?;
    let lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
    snippet_window(&lines, &display, snippet, arguments, max_lines)
}

/// `omnicrawl://docs/<name>.md`：内置文档不进工作区，直接读编译期内嵌文本。
///
/// 内嵌文本与 Python 安装包里的 `docs/*.md` 逐字相同（由 `omnicrawl-mcp` 的对照
/// 测试钉住内容哈希），因此这里只做定位与行窗口渲染，语义与读普通文件一致。
fn read_bundled(uri: &str, arguments: &Map<String, Value>) -> ToolOutcome {
    let text = read_bundled_doc(uri).map_err(|error| ToolError::new(error.message()))?;
    let max_lines = limited_int(arguments, "max_lines", READ_MAX_LINES, 1, READ_MAX_LINES);
    let snippet = optional_text(arguments, "text");
    let function_name = optional_text(arguments, "function_name");
    if !function_name.is_empty() && !snippet.is_empty() {
        return Err(ToolError::new("function_name 和 text 不能同时指定。"));
    }
    let lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
    // 内置 Markdown 没有函数声明，函数定位必然落空；与 Python 同文案报错。
    if !function_name.is_empty() {
        return Err(missing_function_error(&function_name, uri));
    }
    if !snippet.is_empty() {
        return snippet_window(&lines, uri, &snippet, arguments, max_lines);
    }
    let start_line = limited_int(arguments, "start_line", 1, 1, READ_START_LINE_MAX);
    render_window(&lines, uri, start_line, max_lines)
}

/// 整文件读取（片段定位与其它工具共用）；`max_chars` 非空时超限截断。
pub fn read_text(path: &Path, max_chars: Option<usize>) -> ToolOutcome {
    let display = path.to_string_lossy().to_string();
    let bytes = std::fs::read(path)
        .map_err(|error| ToolError::new(format!("读取文件失败：{display}，{error}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::new(format!("文件不是 UTF-8 文本或包含二进制内容：{display}")))?;
    match max_chars {
        Some(limit) if text.chars().count() > limit => {
            let kept: String = text.chars().take(limit).collect();
            Ok(format!("{kept}\n... 文件内容已截断。"))
        }
        _ => Ok(text),
    }
}

fn read_window(
    paths: &WorkspacePaths,
    path: &Path,
    start_line: i64,
    max_lines: i64,
) -> ToolOutcome {
    let display = paths.relative(path);
    let bytes = std::fs::read(path)
        .map_err(|error| ToolError::new(format!("读取文件失败：{display}，{error}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::new(format!("文件不是 UTF-8 文本或包含二进制内容：{display}")))?;

    // Python 走逐行流式读取；这里已整读为字符串，换行拆分保持同一语义：
    // 末尾没有换行的最后一行也要算一行，`total_lines` 与实际行数一致。
    let raw_lines = split_lines_keepends(&text);
    let total_lines = raw_lines.len();
    let mut selected: Vec<(usize, String)> = Vec::new();
    for (index, raw_line) in raw_lines.into_iter().enumerate() {
        let line_no = index + 1;
        if (line_no as i64) < start_line {
            continue;
        }
        if selected.len() as i64 >= max_lines {
            continue;
        }
        let line = raw_line.trim_end_matches(['\r', '\n']);
        selected.push((line_no, truncate_read_line(line)));
    }

    // 起始行超出文件末尾时直接报错，而不是返回无意义的空窗口。
    if start_line > total_lines as i64 && total_lines > 0 {
        return Err(out_of_range_error(&display, start_line, total_lines));
    }
    if total_lines == 0 && start_line > 1 {
        return Err(out_of_range_error(&display, start_line, 0));
    }

    let end_line = selected
        .last()
        .map(|(line_no, _)| *line_no as i64)
        .unwrap_or(start_line - 1);
    let truncated = end_line < total_lines as i64;
    let footer = format_read_footer(start_line, end_line, total_lines, truncated);
    let mut numbered: Vec<String> = selected
        .iter()
        .map(|(line_no, line)| format!("{line_no}: {line}"))
        .collect();
    if numbered.is_empty() {
        numbered.push("文件为空，或指定范围没有内容。".to_string());
    }
    Ok(format!("{}\n{footer}", numbered.join("\n")))
}

/// 函数定位读取（对应 Python `read_file_result` 的 `function_name` 分支）。
///
/// 定位需要完整文本，因此这里走整读而不是行窗口；渲染仍复用 `format_read_lines`。
fn read_function(
    paths: &WorkspacePaths,
    path: &Path,
    function_name: &str,
    max_lines: i64,
) -> ToolOutcome {
    let display = paths.relative(path);
    let text = read_text(path, None)?;
    let range = find_function_range(path, &text, function_name, &display)?;
    let Some((start_line, end_line, resolved_name)) = range else {
        return Err(missing_function_error(function_name, &display));
    };
    let lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
    let header = format!("定位：函数 {resolved_name}（第 {start_line}-{end_line} 行）");
    Ok(format_read_lines(
        &lines,
        start_line,
        end_line,
        max_lines,
        &header,
        "函数内容超过 max_lines，可提高 max_lines 继续读取。",
    ))
}

/// 未找到函数或方法的统一文案（普通文件与内置文档共用）。
fn missing_function_error(function_name: &str, display: &str) -> ToolError {
    ToolError::new(format!(
        "未找到函数或方法：{function_name}（文件：{display}）。"
    ))
}

/// 定位函数范围：`.py` 优先 AST，失败或未命中时退回大括号扫描（与 Python 一致）。
fn find_function_range(
    path: &Path,
    text: &str,
    target_name: &str,
    display: &str,
) -> Result<Option<FunctionRange>, ToolError> {
    let is_python = path
        .extension()
        .map(|extension| extension.eq_ignore_ascii_case("py"))
        .unwrap_or(false);
    if is_python {
        if let Some(range) = python_function_range(text, target_name)? {
            return Ok(Some(range));
        }
    }
    braced_function_range(text, target_name, display)
}

/// Python 源码中的一个函数/方法定义（限定名 + 起止行）。
struct PythonFunction {
    qualified: String,
    start_line: i64,
    end_line: i64,
}

/// Python AST 定位（对应 `_find_python_function_range`）。
///
/// `ast.parse` 语法错误时退回 `None`，让调用方走大括号扫描；命中多个候选时优先
/// 精确限定名，仍有多个按名字列表报歧义错误。本实现是缩进敏感的手写解析器，只
/// 维护「行首缩进 → 作用域栈」，不复现完整 Python 语法。
fn python_function_range(
    text: &str,
    target_name: &str,
) -> Result<Option<FunctionRange>, ToolError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut matches: Vec<PythonFunction> = Vec::new();
    if !collect_python_functions(text, &mut matches) {
        // 解析失败等价于 `ast.parse` 抛语法错误：退回大括号扫描。
        return Ok(None);
    }
    let candidates: Vec<&PythonFunction> = matches
        .iter()
        .filter(|node| node.qualified == target_name || leaf_name(&node.qualified) == target_name)
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    let exact: Vec<&&PythonFunction> = candidates
        .iter()
        .filter(|node| node.qualified == target_name)
        .collect();
    let chosen: Vec<&&PythonFunction> = if exact.is_empty() {
        candidates.iter().collect()
    } else {
        exact
    };
    if chosen.len() == 1 {
        let node = chosen[0];
        return Ok(Some((
            node.start_line,
            node.end_line,
            node.qualified.clone(),
        )));
    }
    let names = chosen
        .iter()
        .map(|node| node.qualified.clone())
        .collect::<Vec<_>>()
        .join(", ");
    Err(ToolError::new(format!(
        "函数名 {target_name} 存在多个匹配，请使用限定名：{names}。"
    )))
}

/// 限定名的叶子部分（`A.B.deep` → `deep`）。
fn leaf_name(qualified: &str) -> &str {
    qualified.rsplit('.').next().unwrap_or(qualified)
}

/// 扫描 Python 缩进块，收集所有函数/方法定义；解析明显不成立时返回 `false`。
///
/// 只跟踪 `class` / `def` / `async def` 三类语句与两件对定位有影响的事实：
/// 装饰器行（起始行取装饰器行）与函数体结束行（返回 `end_lineno` 语义）。嵌套定义
/// 通过作用域栈累积限定名，与 Python `VisitChain` 的 `scopes` 行为一致。
fn collect_python_functions(text: &str, matches: &mut Vec<PythonFunction>) -> bool {
    let mut decorators: Vec<i64> = Vec::new();
    let mut parentheses = 0i64;
    let mut bracket = 0i64;
    let mut brace = 0i64;
    let mut triple: Option<&str> = None;
    // 仍在解析中的定义块栈；块一旦结算（`end_line` 有值）就移出并写进 `matches`。
    let mut blocks: Vec<PythonBlock> = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line_no = index as i64 + 1;
        // 制表符按 8 列展开：Python 词法分析同样处理，缩进比较才与 `ast` 一致。
        let expanded;
        let line = if raw.contains('\u{9}') {
            expanded = raw.replace('\u{9}', "        ");
            expanded.as_str()
        } else {
            raw
        };
        let indent = line.chars().take_while(|c| *c == ' ').count();
        let trimmed = line.trim();

        // 三引号字符串内部不参与缩进结算（否则文档字符串正文会被当成块体正文行）。
        if let Some(marker) = triple {
            if trimmed.matches(marker).count() % 2 == 1 {
                triple = None;
            }
            continue;
        }
        let blank_or_comment = trimmed.is_empty() || trimmed.starts_with('#');
        if !blank_or_comment {
            // 定义行本身（`def`/`class`/装饰器/续行）不是块体的第一条正文行。
            let is_header = trimmed.starts_with('@')
                || python_statement(trimmed).is_some()
                || parentheses > 0
                || bracket > 0
                || brace > 0;
            if !is_header {
                // 首次遇到块体正文行时把块体缩进定为该行缩进（Python 隐含缩进规则）。
                if let Some(block) = blocks.last_mut() {
                    if block.body_indent.is_none() && indent > block.def_indent {
                        block.body_indent = Some(indent);
                    }
                }
            }
            // 缩进回退的块到此结束，`end_lineno` 语义即「上一行」。
            for block in blocks.iter_mut() {
                if block.end_line.is_some() {
                    continue;
                }
                let closed = match block.body_indent {
                    Some(body) => indent <= block.def_indent || (indent < body && !is_header),
                    None => indent <= block.def_indent,
                };
                if closed {
                    block.end_line = Some((line_no - 1).max(block.start_line));
                }
            }
        }
        if let Some(marker) = detect_triple_quote(trimmed) {
            triple = Some(marker);
            continue;
        }

        // 已结算的块按「先内后外」移出栈并落进结果，保证限定名归属正确。
        settle_blocks(&mut blocks, matches);

        let continuation = parentheses > 0 || bracket > 0 || brace > 0;
        if !continuation {
            // 未结算但缩进已回退的残块（块体缩进未确定就结束）同样按上一行收尾。
            while let Some(block) = blocks.last() {
                if block.def_indent < indent {
                    break;
                }
                let block = blocks.pop().expect("栈顶存在");
                let end_line = block
                    .end_line
                    .unwrap_or((line_no - 1).max(block.start_line));
                matches.push(PythonFunction {
                    qualified: block.qualified,
                    start_line: block.start_line,
                    end_line,
                });
            }
            let statement = python_statement(trimmed);
            // 装饰器只在「既不是装饰器也不是被装饰定义」的语句上作废；空行与注释
            // 不打断 `@deco` 与 `def` 的配对（Python 允许二者之间有空行）。
            if !blank_or_comment && !trimmed.starts_with('@') && statement.is_none() {
                decorators.clear();
            }
            if let Some(name) = statement {
                let start = decorators.first().copied().unwrap_or(line_no);
                blocks.push(PythonBlock {
                    def_indent: indent,
                    body_indent: None,
                    start_line: start,
                    qualified: qualified_name(&blocks, &name),
                    end_line: None,
                });
            }
        }
        update_depth(trimmed, &mut parentheses, &mut bracket, &mut brace);
        if trimmed.starts_with('@') {
            decorators.push(line_no);
        }
    }

    // 收尾：体直到文件末尾的块，以及始终没有正文行的单行体定义。
    let last_line = text.lines().count() as i64;
    for block in blocks {
        let end_line = block.end_line.unwrap_or(if block.body_indent.is_none() {
            block.start_line
        } else {
            last_line
        });
        matches.push(PythonFunction {
            qualified: block.qualified,
            start_line: block.start_line,
            end_line: end_line.max(block.start_line),
        });
    }
    // 语法明显不成立的信号：括号/引号从未闭合，此时退回大括号扫描。
    parentheses == 0 && bracket == 0 && brace == 0 && triple.is_none()
}

/// 把栈顶已结算的块依次落进结果（先内后外）。
fn settle_blocks(blocks: &mut Vec<PythonBlock>, matches: &mut Vec<PythonFunction>) {
    while let Some(block) = blocks.last() {
        let Some(end_line) = block.end_line else {
            break;
        };
        let block = blocks.pop().expect("栈顶存在");
        matches.push(PythonFunction {
            qualified: block.qualified,
            start_line: block.start_line,
            end_line,
        });
    }
}

/// 一个正在解析的 Python 缩进块（class / def / async def）。
struct PythonBlock {
    /// 定义行自身的缩进。
    def_indent: usize,
    /// 块体第一条正文行的缩进（首行到达前为 `None`）。
    body_indent: Option<usize>,
    start_line: i64,
    qualified: String,
    end_line: Option<i64>,
}

/// 按当前块栈拼出限定名（`A.B.deep`）。
fn qualified_name(blocks: &[PythonBlock], name: &str) -> String {
    if blocks.is_empty() {
        name.to_string()
    } else {
        let prefix = blocks
            .iter()
            .map(|block| leaf_name(&block.qualified).to_string())
            .collect::<Vec<_>>()
            .join(".");
        format!("{prefix}.{name}")
    }
}

/// 识别一条 `class` / `def` / `async def` 语句，返回其名字。
///
/// `class` 与 `def` 在缩进块语义上没有区别（都开场一个作用域），因此不区分种类；
/// 判据只要「名字后面紧跟 `(`、`:` 或行尾」，避免把 `def_x = 1` 误判成定义。
fn python_statement(trimmed: &str) -> Option<String> {
    let rest = if let Some(rest) = trimmed.strip_prefix("class ") {
        rest
    } else {
        let without_async = trimmed.strip_prefix("async ").unwrap_or(trimmed);
        without_async.strip_prefix("def ")?
    };
    let name = identifier_prefix(rest);
    if name.is_empty() {
        return None;
    }
    match rest[name.len()..].trim_start().chars().next() {
        Some('(') | Some(':') | None => Some(name),
        _ => None,
    }
}

/// 取行首标识符（字母/数字/下划线，首字符非数字）。
fn identifier_prefix(text: &str) -> String {
    text.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// 识别该行是否（新）开启一个三引号字符串，返回引号标记。
fn detect_triple_quote(trimmed: &str) -> Option<&'static str> {
    ["\"\"\"", "'''"]
        .into_iter()
        .find(|marker| trimmed.matches(marker).count() % 2 == 1)
}

/// 累计一行里的括号深度（三引号内部内容已在上游排除）。
fn update_depth(line: &str, parentheses: &mut i64, bracket: &mut i64, brace: &mut i64) {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for character in line.chars() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '#' => break,
            '(' => *parentheses += 1,
            ')' => *parentheses -= 1,
            '[' => *bracket += 1,
            ']' => *bracket -= 1,
            '{' => *brace += 1,
            '}' => *brace -= 1,
            _ => {}
        }
    }
}

/// 大括号扫描回退（对应 `_find_braced_function_range`）。
fn braced_function_range(
    text: &str,
    target_name: &str,
    _display: &str,
) -> Result<Option<FunctionRange>, ToolError> {
    let leaf = leaf_name(target_name).to_string();
    let lines: Vec<&str> = text.lines().collect();
    let mut matches: Vec<FunctionRange> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !is_declaration_line(line, &leaf) {
            continue;
        }
        if let Some(end_index) = braced_block_end(&lines, index) {
            let resolved = if target_name.contains('.') {
                target_name.to_string()
            } else {
                leaf.clone()
            };
            matches.push((index as i64 + 1, end_index as i64 + 1, resolved));
        }
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches.remove(0))),
        _ => Err(ToolError::new(format!(
            "函数名 {target_name} 存在多个文本匹配，请改用更具体的文件或函数名。"
        ))),
    }
}

/// 声明行判定：四类写法对应 Python `declaration_patterns` 的四条正则。
///
/// 四条正则的差异必须照搬，尤其是「修饰符前缀只在前两条允许」这一点：`export const
/// add = ...` 在 Python 里并不算声明（第三条正则不带修饰符前缀），因此本函数也在
/// 变量/箭头分支上使用未经修饰符剥离的原始行。
fn is_declaration_line(line: &str, leaf: &str) -> bool {
    let trimmed = line.trim_start();
    // 第 1、2 条正则共享「零到多个修饰符」前缀。
    let mut rest = trimmed;
    loop {
        let before = rest;
        for modifier in DECLARATION_MODIFIERS {
            if let Some(after) = strip_word(rest, modifier) {
                rest = after.trim_start();
            }
        }
        if rest == before {
            break;
        }
    }
    // 第 1 条：`function hello(` / `func f(` / `fn f(` / `def f(`。
    for keyword in ["function", "func", "fn", "def"] {
        if let Some(after) = strip_word(rest, keyword) {
            return name_followed_by_paren(after.trim_start(), leaf);
        }
    }
    // 第 2 条：类型返回式 `void* hello(` / `std::string hello(`。
    if typed_declaration(rest, leaf) {
        return true;
    }
    // 第 3 条：`const add = (a, b) =>`（不带修饰符前缀）。
    if variable_arrow_declaration(trimmed, leaf) {
        return true;
    }
    // 第 4 条：裸名字式 `hello(...) {` / `hello(...) =>`（不带修饰符前缀）。
    bare_arrow_declaration(trimmed, leaf)
}

/// 第 3 条正则：`const|let|var NAME = (async )?(...)=>`。
fn variable_arrow_declaration(trimmed: &str, leaf: &str) -> bool {
    for keyword in ["const", "let", "var"] {
        let Some(after) = strip_word(trimmed, keyword) else {
            continue;
        };
        let after = after.trim_start();
        let Some(after_name) = after.strip_prefix(leaf) else {
            return false;
        };
        let after_name = after_name.trim_start();
        if !after_name.starts_with('=') {
            return false;
        }
        let body = after_name[1..].trim_start();
        let body = body.strip_prefix("async").unwrap_or(body).trim_start();
        return body.contains("=>");
    }
    false
}

/// 第 4 条正则：`NAME\([^;]*\)\s*(?:=>|\{)`。
fn bare_arrow_declaration(trimmed: &str, leaf: &str) -> bool {
    let Some(after) = trimmed.strip_prefix(leaf) else {
        return false;
    };
    let after = after.trim_start();
    let Some(inner) = after.strip_prefix('(') else {
        return false;
    };
    // `[^;]*` 表示参数里不能出现分号；超出即视为不是声明。
    let Some(close) = inner.find(')') else {
        return false;
    };
    if inner[..close].contains(';') {
        return false;
    }
    let tail = inner[close + 1..].trim_start();
    tail.starts_with("=>") || tail.starts_with('{')
}

/// 声明前的可选修饰符（与 Python 正则里的集合一致）。
const DECLARATION_MODIFIERS: [&str; 14] = [
    "export",
    "default",
    "public",
    "private",
    "protected",
    "static",
    "async",
    "final",
    "virtual",
    "override",
    "inline",
    "extern",
    "unsafe",
    "pub",
];

/// 剥掉行首整词（后面必须是空白或行尾），命中返回剩余部分。
fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(word)?;
    if rest.is_empty() {
        return Some(rest);
    }
    match rest.chars().next() {
        Some(next) if next.is_whitespace() => Some(rest),
        _ => None,
    }
}

/// `leaf(...)`：名字之后只允许空白再遇到 `(`。
fn name_followed_by_paren(text: &str, leaf: &str) -> bool {
    match text.strip_prefix(leaf) {
        Some(rest) => rest.trim_start().starts_with('('),
        None => false,
    }
}

/// 类型前缀式：名字左侧是类型词 + 空白，右侧紧跟 `(`。
fn typed_declaration(text: &str, leaf: &str) -> bool {
    let Some(position) = text.find(leaf) else {
        return false;
    };
    let Some(rest) = text.get(position + leaf.len()..) else {
        return false;
    };
    if !rest.trim_start().starts_with('(') {
        return false;
    }
    let head = text[..position].trim_end();
    if head.is_empty() || !head.ends_with(char::is_whitespace) {
        return false;
    }
    let type_part = head.trim_end();
    let Some(first) = type_part.chars().next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    type_part
        .chars()
        .all(|character| character.is_alphanumeric() || "_<>,.?[]*&: ".contains(character))
}

/// 从声明行向下找大括号块结束行；未收全时返回 `None`（对应 `_find_braced_block_end`）。
fn braced_block_end(lines: &[&str], start_index: usize) -> Option<usize> {
    let mut depth = 0i64;
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let end = (start_index + BRACED_SCAN_LINES_MAX).min(lines.len());
    for (index, line) in lines.iter().enumerate().take(end).skip(start_index) {
        let mut characters = line.chars().peekable();
        while let Some(character) = characters.next() {
            if let Some(active) = quote {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == active {
                    quote = None;
                }
                continue;
            }
            match character {
                '\'' | '"' | '`' => quote = Some(character),
                '/' if characters.peek() == Some(&'/') => break,
                '{' => {
                    depth += 1;
                    started = true;
                }
                '}' if started => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(index);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// 片段定位的行窗口渲染（普通文件与内置文档共用）。
fn snippet_window(
    lines: &[String],
    display: &str,
    snippet: &str,
    arguments: &Map<String, Value>,
    max_lines: i64,
) -> ToolOutcome {
    let text = lines.join("\n");
    let Some(offset) = text.find(snippet) else {
        return Err(ToolError::new(format!(
            "未找到指定文字片段（文件：{display}）。"
        )));
    };
    let context_lines = limited_int(arguments, "context_lines", 20, 0, READ_CONTEXT_LINES_MAX);
    let anchor_start_line = text[..offset].matches('\n').count() as i64 + 1;
    let last_char_start = offset
        + snippet
            .char_indices()
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0);
    let anchor_end_line = text[..last_char_start].matches('\n').count() as i64 + 1;
    let start_line = (anchor_start_line - context_lines).max(1);
    let end_line = (anchor_end_line + context_lines).min(lines.len() as i64);
    let header = format!(
        "定位：文字片段首次匹配（第 {anchor_start_line}-{anchor_end_line} 行，上下文 {context_lines} 行）"
    );
    Ok(format_read_lines(
        lines,
        start_line,
        end_line,
        max_lines,
        &header,
        "文字片段上下文超过 max_lines，可提高 max_lines 继续读取。",
    ))
}

/// 已切好行的普通行窗口渲染（内置文档用：内容已内嵌，无文件可读）。
fn render_window(lines: &[String], display: &str, start_line: i64, max_lines: i64) -> ToolOutcome {
    let total_lines = lines.len();
    if start_line > total_lines as i64 && total_lines > 0 {
        return Err(out_of_range_error(display, start_line, total_lines));
    }
    if total_lines == 0 && start_line > 1 {
        return Err(out_of_range_error(display, start_line, 0));
    }
    let selected: Vec<(i64, String)> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| (index as i64 + 1, line.clone()))
        .filter(|(line_no, _)| *line_no >= start_line)
        .take(max_lines as usize)
        .collect();
    let end_line = selected
        .last()
        .map(|(line_no, _)| *line_no)
        .unwrap_or(start_line - 1);
    let truncated = end_line < total_lines as i64;
    let footer = format_read_footer(start_line, end_line, total_lines, truncated);
    let mut numbered: Vec<String> = selected
        .iter()
        .map(|(line_no, line)| format!("{line_no}: {line}"))
        .collect();
    if numbered.is_empty() {
        numbered.push("文件为空，或指定范围没有内容。".to_string());
    }
    Ok(format!("{}\n{footer}", numbered.join("\n")))
}

/// 定位类读取的行号窗口格式化（Python 侧 `_format_read_lines`）。
fn format_read_lines(
    lines: &[String],
    start_line: i64,
    end_line: i64,
    max_lines: i64,
    header: &str,
    truncation_hint: &str,
) -> String {
    let total_lines = lines.len() as i64;
    let selected_start = start_line.max(1).min(total_lines + 1);
    let selected_end = end_line.max(selected_start - 1).min(total_lines);
    let take = (selected_end.min(selected_start - 1 + max_lines) - (selected_start - 1)).max(0);
    let selected = lines
        .get((selected_start - 1).max(0) as usize..)
        .unwrap_or_default()
        .iter()
        .take(take as usize);

    let mut numbered: Vec<String> = selected
        .enumerate()
        .map(|(index, line)| format!("{}: {}", selected_start + index as i64, line))
        .collect();
    if selected_start <= selected_end && selected_start - 1 + max_lines < selected_end {
        numbered.push(format!("... {truncation_hint}"));
    }
    if numbered.is_empty() {
        numbered.push("文件为空，或指定范围没有内容。".to_string());
    }
    if header.is_empty() {
        numbered.join("\n")
    } else {
        format!("{header}\n{}", numbered.join("\n"))
    }
}

fn truncate_read_line(line: &str) -> String {
    if line.chars().count() <= READ_MAX_LINE_LENGTH {
        return line.to_string();
    }
    let head: String = line.chars().take(READ_MAX_LINE_LENGTH).collect();
    format!("{head}... (line truncated to {READ_MAX_LINE_LENGTH} chars)")
}

fn format_read_footer(
    start_line: i64,
    end_line: i64,
    total_lines: usize,
    truncated: bool,
) -> String {
    if !truncated {
        return format!("(End of file - total {total_lines} lines)");
    }
    format!(
        "(Showing lines {start_line}-{end_line} of {total_lines}. Use start_line={} to continue.)",
        end_line + 1
    )
}

fn out_of_range_error(display: &str, start_line: i64, total_lines: usize) -> ToolError {
    ToolError::new(format!(
        "start_line {start_line} 超出文件范围：{display}（共 {total_lines} 行）。"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace(name: &str) -> (WorkspacePaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-read-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        (WorkspacePaths::new(&root), root)
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn window_reports_line_numbers_and_end_of_file() {
        let (paths, root) = workspace("window");
        std::fs::write(root.join("a.txt"), "第一行\n第二行\n第三行\n").expect("写测试文件");
        let text = read(&paths, &args(json!({"path": "a.txt"}))).expect("读取应成功");
        assert_eq!(
            text,
            "1: 第一行\n2: 第二行\n3: 第三行\n(End of file - total 3 lines)"
        );
    }

    #[test]
    fn window_truncates_and_points_at_the_next_start_line() {
        let (paths, root) = workspace("truncate");
        std::fs::write(root.join("a.txt"), "1\n2\n3\n4\n5\n").expect("写测试文件");
        let text = read(
            &paths,
            &args(json!({"path": "a.txt", "start_line": 2, "max_lines": 2})),
        )
        .expect("读取应成功");
        assert_eq!(
            text,
            "2: 2\n3: 3\n(Showing lines 2-3 of 5. Use start_line=4 to continue.)"
        );
    }

    #[test]
    fn long_lines_are_marked_truncated() {
        let (paths, root) = workspace("long-line");
        let long = "x".repeat(READ_MAX_LINE_LENGTH + 50);
        std::fs::write(root.join("a.txt"), format!("{long}\n")).expect("写测试文件");
        let text = read(&paths, &args(json!({"path": "a.txt"}))).expect("读取应成功");
        assert!(text.contains("(line truncated to 2000 chars)"), "{text}");
        assert!(text.starts_with(&format!("1: {}", "x".repeat(READ_MAX_LINE_LENGTH))));
    }

    #[test]
    fn errors_match_python_texts() {
        let (paths, root) = workspace("errors");
        std::fs::write(root.join("a.txt"), "one\n").expect("写测试文件");

        let not_a_file = read(&paths, &args(json!({"path": "missing.txt"}))).unwrap_err();
        assert!(
            not_a_file.message.starts_with("不是文件："),
            "{}",
            not_a_file.message
        );

        let out_of_range =
            read(&paths, &args(json!({"path": "a.txt", "start_line": 9}))).unwrap_err();
        assert_eq!(
            out_of_range.message,
            "start_line 9 超出文件范围：a.txt（共 1 行）。"
        );

        let empty_path = read(&paths, &args(json!({"path": " "}))).unwrap_err();
        assert_eq!(empty_path.message, "路径不能为空。");

        let both_modes = read(
            &paths,
            &args(json!({"path": "a.txt", "function_name": "f", "text": "o"})),
        )
        .unwrap_err();
        assert_eq!(both_modes.message, "function_name 和 text 不能同时指定。");

        // 未命中的函数定位是普通错误（不再是 `FS_UNSUPPORTED_FEATURE`）。
        let missing = read(
            &paths,
            &args(json!({"path": "a.txt", "function_name": "f"})),
        )
        .unwrap_err();
        assert_eq!(missing.message, "未找到函数或方法：f（文件：a.txt）。");
    }

    #[test]
    fn function_mode_locates_python_declarations() {
        let (paths, root) = workspace("locate-py");
        std::fs::write(
            root.join("m.py"),
            "class Outer:\n    def run(self):\n        return 1\n\ndef run():\n    return 2\n",
        )
        .expect("写测试文件");

        let qualified = read(
            &paths,
            &args(json!({"path": "m.py", "function_name": "Outer.run"})),
        )
        .expect("限定名定位应成功");
        assert_eq!(
            qualified,
            "定位：函数 Outer.run（第 2-3 行）\n2:     def run(self):\n3:         return 1"
        );

        // 装饰器行计入起始行，截断提示保留 header。
        std::fs::write(
            root.join("deco.py"),
            "@decorator\n@another(1)\ndef handled():\n    value = 1\n    return value\n",
        )
        .expect("写测试文件");
        let decorated = read(
            &paths,
            &args(json!({"path": "deco.py", "function_name": "handled", "max_lines": 2})),
        )
        .expect("装饰器定位应成功");
        assert!(
            decorated
                .starts_with("定位：函数 handled（第 1-5 行）\n1: @decorator\n2: @another(1)\n..."),
            "{decorated}"
        );

        // 同名多命中要求限定名。
        std::fs::write(
            root.join("dup.py"),
            "def dup():\n    return 1\n\ndef dup():\n    return 2\n",
        )
        .expect("写测试文件");
        let ambiguous = read(
            &paths,
            &args(json!({"path": "dup.py", "function_name": "dup"})),
        )
        .unwrap_err();
        assert_eq!(
            ambiguous.message,
            "函数名 dup 存在多个匹配，请使用限定名：dup, dup。"
        );
    }

    #[test]
    fn function_mode_falls_back_to_braced_scan() {
        let (paths, root) = workspace("locate-braced");
        std::fs::write(
            root.join("a.js"),
            "export function hello(name) {\n  console.log(name);\n}\n",
        )
        .expect("写测试文件");
        let located = read(
            &paths,
            &args(json!({"path": "a.js", "function_name": "hello"})),
        )
        .expect("JS 声明定位应成功");
        assert_eq!(
            located,
            "定位：函数 hello（第 1-3 行）\n1: export function hello(name) {\n2:   console.log(name);\n3: }"
        );
    }

    #[test]
    fn bundled_docs_are_readable() {
        let (paths, _root) = workspace("bundled");
        let text = read(
            &paths,
            &args(json!({"path": "omnicrawl://docs/API.md", "max_lines": 3})),
        )
        .expect("内置文档应可读");
        assert!(text.starts_with("1: "), "{text}");
        assert!(text.contains("Use start_line=4 to continue.)"), "{text}");

        let missing = read(&paths, &args(json!({"path": "omnicrawl://docs/NOPE.md"}))).unwrap_err();
        assert_eq!(missing.message, "内置文档不存在：omnicrawl://docs/NOPE.md");

        let traversal = read(
            &paths,
            &args(json!({"path": "omnicrawl://docs/../config.toml"})),
        )
        .unwrap_err();
        assert!(
            traversal
                .message
                .starts_with("内置文档 URI 仅允许单个 Markdown 文件名："),
            "{}",
            traversal.message
        );
    }

    #[test]
    fn snippet_mode_shows_context_lines() {
        let (paths, root) = workspace("snippet");
        let body: String = (1..=10).map(|index| format!("行{index}\n")).collect();
        std::fs::write(root.join("a.txt"), body).expect("写测试文件");
        let text = read(
            &paths,
            &args(json!({"path": "a.txt", "text": "行5", "context_lines": 1, "max_lines": 10})),
        )
        .expect("片段定位应成功");
        assert!(
            text.starts_with("定位：文字片段首次匹配（第 5-5 行，上下文 1 行）"),
            "{text}"
        );
        assert!(text.contains("4: 行4"), "{text}");
        assert!(text.contains("6: 行6"), "{text}");

        let missing = read(&paths, &args(json!({"path": "a.txt", "text": "不存在"}))).unwrap_err();
        assert_eq!(missing.message, "未找到指定文字片段（文件：a.txt）。");
    }

    #[test]
    fn empty_file_renders_placeholder_and_footer() {
        let (paths, root) = workspace("empty");
        std::fs::write(root.join("a.txt"), "").expect("写空文件");
        let text = read(&paths, &args(json!({"path": "a.txt"}))).expect("读取应成功");
        assert_eq!(
            text,
            "文件为空，或指定范围没有内容。\n(End of file - total 0 lines)"
        );
    }
}
