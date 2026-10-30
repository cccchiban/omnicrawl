//! 通用工具：时间戳、事件 ID、稳定哈希、命名空间归一与排序。
//!
//! 从 `models.rs` 按职责拆出。

use super::*;

/// ISO-8601（UTC、秒精度、`Z` 结尾）：对应
/// `datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")`。
pub fn utc_now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// 32 位十六进制事件 id，与 Python 的 `uuid.uuid4().hex` 同形。
///
/// 这里不是密码学随机源：事件 id 只需要在一台机器上不撞车，
/// 所以用「时间戳 + 进程 id + 计数器」做散列（与会话层的 `random_event_id` 同一手法）。
pub fn new_event_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = Sha256::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.update(std::thread::current().name().unwrap_or_default().as_bytes());
    let digest = hasher.finalize();
    hex(&digest[..16])
}

/// 对任意 JSON 可序列化值计算稳定短哈希，用于审计而非完整性锁。
pub fn stable_hash(value: &Value) -> String {
    let encoded = json_dumps_stable(value);
    let digest = Sha256::digest(encoded.as_bytes());
    hex(&digest[..8])
}

/// 对应 `json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))`。
///
/// `serde_json` 保插入序，所以要先把对象键排成字典序再序列化；
/// 转义与分隔符风格与 Python 一致（UTF-8 原样输出、无空格分隔）。
pub fn json_dumps_stable(value: &Value) -> String {
    serde_json::to_string(&sort_value(value)).unwrap_or_default()
}

fn sort_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), sort_value(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

/// `[0-9a-f]` 十六进制小写写法（与 Python 的 `bytes.hex()` 同形）。
pub fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// 把 NPM 包名规范化为自定义 Hook 命名空间片段。
pub fn normalize_plugin_namespace(package_name: &str) -> String {
    let mut text = package_name.trim().to_lowercase();
    if let Some(rest) = text.strip_prefix('@') {
        text = rest.to_string();
    }
    text = text.replace('/', "-");
    let filtered: String = text
        .chars()
        .map(|ch| {
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let mut collapsed = String::with_capacity(filtered.len());
    let mut previous_dash = false;
    for ch in filtered.chars() {
        if ch == '-' {
            if previous_dash {
                continue;
            }
            previous_dash = true;
        } else {
            previous_dash = false;
        }
        collapsed.push(ch);
    }
    let trimmed = collapsed.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "plugin".to_string()
    } else {
        trimmed
    }
}

/// 各模式的默认超时（对应 Python 的 `default_timeout_for_mode`）。
pub fn default_timeout_for_mode(mode: &str) -> i64 {
    match mode {
        HANDLER_MODE_TRANSFORM => DEFAULT_TRANSFORM_TIMEOUT_MS,
        HANDLER_MODE_GUARD => DEFAULT_GUARD_TIMEOUT_MS,
        HANDLER_MODE_NOTIFY => DEFAULT_NOTIFY_TIMEOUT_MS,
        _ => DEFAULT_OBSERVE_TIMEOUT_MS,
    }
}

/// 执行顺序：guard → transform → observe/notify。
pub fn mode_rank(mode: &str) -> i64 {
    match mode {
        HANDLER_MODE_GUARD => 0,
        HANDLER_MODE_TRANSFORM => 1,
        HANDLER_MODE_OBSERVE | HANDLER_MODE_NOTIFY => 2,
        _ => 9,
    }
}

/// project 高于 user：排序时更小的 rank 先执行。
pub fn scope_rank(scope: &str) -> i64 {
    if scope == "project" {
        0
    } else {
        1
    }
}

/// 稳定排序：模式 → 优先级降序 → 作用域 → 插件名 → Handler id。
pub fn sort_handlers(handlers: &[ResolvedHandler]) -> Vec<ResolvedHandler> {
    let mut sorted = handlers.to_vec();
    sorted.sort_by(|left, right| {
        mode_rank(&left.mode)
            .cmp(&mode_rank(&right.mode))
            .then((-left.priority).cmp(&(-right.priority)))
            .then(scope_rank(&left.scope).cmp(&scope_rank(&right.scope)))
            .then(left.plugin_name.cmp(&right.plugin_name))
            .then(left.handler_id.cmp(&right.handler_id))
    });
    sorted
}

fn version_ref_value(reference: Option<&PluginVersionRef>) -> Value {
    match reference {
        Some(item) => Value::Object(item.to_dict()),
        None => Value::Null,
    }
}

fn option_value(value: &Option<String>) -> Value {
    match value {
        Some(text) => Value::from(text.clone()),
        None => Value::Null,
    }
}

/// 与 Python `str()` 的差异见 crate README：非字符串一律取原样文本，`None` 回落空串。
pub(crate) fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => if *flag { "True" } else { "False" }.to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// `data.get(primary, data.get(fallback, ""))`：primary 存在就用它（含 null）。
pub(crate) fn alias_text(map: &Map<String, Value>, primary: &str, fallback: &str) -> String {
    match map.get(primary) {
        Some(value) => text_of(Some(value)),
        None => text_of(map.get(fallback)),
    }
}

pub(crate) fn bool_of(value: Option<&Value>, default: bool) -> bool {
    match value {
        None => default,
        Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

pub(crate) fn bool_alias(
    map: &Map<String, Value>,
    primary: &str,
    fallback: &str,
    default: bool,
) -> bool {
    match map.get(primary) {
        Some(value) => bool_of(Some(value), false),
        None => bool_of(map.get(fallback), default),
    }
}

pub(crate) fn int_of(value: Option<&Value>, default: i64) -> i64 {
    match value {
        None | Some(Value::Null) => default,
        Some(Value::Bool(flag)) => i64::from(*flag),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|item| item as i64))
            .unwrap_or(default),
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(default),
        _ => default,
    }
}

pub(crate) fn int_alias(
    map: &Map<String, Value>,
    primary: &str,
    fallback: &str,
    default: i64,
) -> i64 {
    match map.get(primary) {
        Some(value) => int_of(Some(value), default),
        None => int_of(map.get(fallback), default),
    }
}

/// 字符串数组读取；非数组一律按空数组处理（Python 对字符串会逐字符迭代，这里不复现该怪癖）。
pub(crate) fn string_list_alias(
    map: &Map<String, Value>,
    primary: &str,
    fallback: &str,
) -> Vec<String> {
    let raw = match map.get(primary) {
        Some(value) => value,
        None => match map.get(fallback) {
            Some(value) => value,
            None => return Vec::new(),
        },
    };
    match raw {
        Value::Array(items) => items.iter().map(|item| text_of(Some(item))).collect(),
        _ => Vec::new(),
    }
}

/// Python 的真值测试：空容器、0、空串、null、false 都是假。
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !*flag,
        Value::Number(number) => number.as_f64().map(|item| item == 0.0).unwrap_or(false),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

/// `^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$`
fn is_valid_handler_id(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || chars.len() > 64 {
        return false;
    }
    if !chars[0].is_ascii_alphanumeric() {
        return false;
    }
    chars[1..]
        .iter()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

/// `^plugin\.[a-z0-9][a-z0-9-]{0,62}(?:\.[a-z0-9][a-z0-9-]{0,62})+$`
fn is_valid_custom_event(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() < 3 || parts[0] != "plugin" {
        return false;
    }
    parts[1..].iter().all(|part| {
        let chars: Vec<char> = part.chars().collect();
        if chars.is_empty() || chars.len() > 63 {
            return false;
        }
        let first = chars[0];
        (first.is_ascii_lowercase() || first.is_ascii_digit())
            && chars[1..]
                .iter()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == '-')
    })
}

/// `utf-8-sig`：读到 BOM 就剥掉，其余按 UTF-8 解码（非法字节按替换字符处理）。
pub(crate) fn decode_utf8_sig(bytes: &[u8]) -> String {
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(body).to_string()
}

/// SemVer 2.0.0 的官方正则（Python 侧 `_SEMVER_RE`）的手写等价物。
pub fn is_valid_semver(version: &str) -> bool {
    let text = version.trim();
    let main = match text.split_once('+') {
        Some((left, right)) => {
            if !is_valid_build_metadata(right) {
                return false;
            }
            left
        }
        None => text,
    };
    let core = match main.split_once('-') {
        Some((left, right)) => {
            if !is_valid_prerelease(right) {
                return false;
            }
            left
        }
        None => main,
    };
    is_valid_core_version(core)
}

fn is_valid_core_version(core: &str) -> bool {
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| is_zero_or_positive(part))
}

/// `(0|[1-9]\d*)`
fn is_zero_or_positive(text: &str) -> bool {
    if text == "0" {
        return true;
    }
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_digit() && first != '0' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_digit())
}

/// `(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.…)*`
fn is_valid_prerelease(text: &str) -> bool {
    !text.is_empty() && text.split('.').all(is_valid_prerelease_identifier)
}

fn is_valid_prerelease_identifier(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    if is_zero_or_positive(text) {
        return true;
    }
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index >= bytes.len() {
        return false;
    }
    let marker = bytes[index];
    if !(marker.is_ascii_alphabetic() || marker == b'-') {
        return false;
    }
    bytes[index..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}

/// `(?:[0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*)`
fn is_valid_build_metadata(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        })
}

/// `^(?:@[a-z0-9-~][a-z0-9-._~]*/)?[a-z0-9-~][a-z0-9-._~]*$`
pub fn is_valid_npm_name(name: &str) -> bool {
    let text = name.trim();
    let remainder = match text.strip_prefix('@') {
        Some(after) => match after.split_once('/') {
            Some((scope, rest)) => {
                if !is_valid_npm_segment(scope) {
                    return false;
                }
                rest
            }
            None => return false,
        },
        None => text,
    };
    is_valid_npm_segment(remainder)
}

fn is_valid_npm_segment(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first)
            if first.is_ascii_lowercase()
                || first.is_ascii_digit()
                || first == '-'
                || first == '~' => {}
        _ => return false,
    }
    chars.all(|ch| {
        ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '.' | '_' | '~')
    })
}
