//! 自定义音色库与内置音色行：`omnicrawl/tts/custom_voices.py` 的等价实现。
//!
//! 克隆出的音色存到独立的 `~/.omnicrawl/tts/custom_voices.json`（不写进模型 manifest，
//! 避免模型重装时被覆盖）；内置音色则从模型 manifest 的 `builtin_voices` 读取。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use fancy_regex::Regex;
use omnicrawl_controllers::json::python_dumps;
use serde_json::{json, Map, Value};

pub const CUSTOM_VOICES_FILENAME: &str = "custom_voices.json";
pub const CUSTOM_VOICE_GROUP: &str = "Custom";

const MANIFEST_CANDIDATES: [&str; 3] = [
    "browser_poc_manifest.json",
    "MOSS-TTS-Nano-100M-ONNX/browser_poc_manifest.json",
    "MOSS-TTS-Nano-ONNX-CPU/browser_poc_manifest.json",
];

/// 音色库路径：用户级资产固定放在家目录下，不随模型目录变化。
pub fn custom_voices_path() -> PathBuf {
    custom_voices_path_in(&home_directory())
}

/// 可注入根目录的版本（测试与宿主自定义布局用）。
pub fn custom_voices_path_in(root: &Path) -> PathBuf {
    root.join(".omnicrawl")
        .join("tts")
        .join(CUSTOM_VOICES_FILENAME)
}

fn home_directory() -> PathBuf {
    for name in ["USERPROFILE", "HOME"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    PathBuf::from(".")
}

/// 用户级资产所在的家目录根（与 [`custom_voices_path`] 同源）。
pub fn default_root() -> PathBuf {
    home_directory()
}

fn write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn voice_name_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| Regex::new(r"^[\w\u4e00-\u9fff ._-]{1,40}$").expect("音色名正则应当合法"))
}

/// 读取音色库原始结构；文件缺失或损坏时按空库处理。
pub fn read_raw(root: &Path) -> Value {
    let empty = || json!({"version": 1, "voices": []});
    let path = custom_voices_path_in(root);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return empty();
    };
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return empty();
    };
    let Some(map) = data.as_object() else {
        return empty();
    };
    let voices = match map.get("voices") {
        Some(Value::Array(items)) => Value::Array(items.clone()),
        _ => Value::Array(Vec::new()),
    };
    json!({
        "version": map.get("version").cloned().unwrap_or(Value::from(1)),
        "voices": voices,
    })
}

/// 读取全部有效音色条目（缺 voice 或空 prompt_audio_codes 的条目跳过）。
pub fn load_custom_voices(root: &Path) -> Vec<Value> {
    let data = read_raw(root);
    let Some(rows) = data.get("voices").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter(|row| {
            let Some(map) = row.as_object() else {
                return false;
            };
            let voice = map
                .get("voice")
                .map(argument_text)
                .unwrap_or_default();
            let codes_ok = matches!(map.get("prompt_audio_codes"), Some(Value::Array(items)) if !items.is_empty());
            !voice.trim().is_empty() && codes_ok
        })
        .cloned()
        .collect()
}

pub fn list_custom_voice_names(root: &Path) -> Vec<String> {
    load_custom_voices(root)
        .iter()
        .map(|row| {
            row.get("voice")
                .map(argument_text)
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// 校验并规范化音色名；非法时返回错误文案（与 Python `ValueError` 一致）。
pub fn validate_voice_name(voice: &str) -> Result<String, String> {
    let name = voice.trim();
    if name.is_empty() {
        return Err("音色名称不能为空。".to_string());
    }
    if name.chars().count() > 40 {
        return Err("音色名称过长（最多 40 个字符）。".to_string());
    }
    if !voice_name_re().is_match(name).unwrap_or(false) {
        return Err("音色名称只能包含中文、字母、数字、空格、下划线、连字符或点。".to_string());
    }
    Ok(name.to_string())
}

/// 写入（或覆盖同名）一条克隆音色，返回写入的条目。
pub fn add_custom_voice(
    root: &Path,
    voice: &str,
    prompt_audio_codes: &[Vec<i64>],
    display_name: &str,
    audio_file: &str,
    source_audio_path: &str,
) -> Result<Value, String> {
    let name = validate_voice_name(voice)?;
    if prompt_audio_codes.is_empty() {
        return Err("prompt_audio_codes 不能为空。".to_string());
    }
    let codes: Vec<Value> = prompt_audio_codes
        .iter()
        .map(|row| Value::Array(row.iter().map(|code| Value::from(*code)).collect()))
        .collect();
    let resolved_audio_file = if audio_file.trim().is_empty() {
        let source = if source_audio_path.trim().is_empty() {
            name.clone()
        } else {
            source_audio_path.to_string()
        };
        Path::new(&source)
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or(source)
    } else {
        audio_file.to_string()
    };

    let mut entry = Map::new();
    entry.insert("voice".to_string(), Value::from(name.clone()));
    entry.insert(
        "display_name".to_string(),
        Value::from(if display_name.trim().is_empty() {
            name.clone()
        } else {
            display_name.to_string()
        }),
    );
    entry.insert("group".to_string(), Value::from(CUSTOM_VOICE_GROUP));
    entry.insert("audio_file".to_string(), Value::from(resolved_audio_file));
    entry.insert("prompt_audio_codes".to_string(), Value::Array(codes));
    if !source_audio_path.trim().is_empty() {
        entry.insert(
            "source_audio_path".to_string(),
            Value::from(source_audio_path.to_string()),
        );
    }
    let entry = Value::Object(entry);

    let _guard = write_lock().lock().expect("音色库写锁");
    let mut data = read_raw(root);
    let mut voices = data
        .get("voices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut replaced = false;
    for row in voices.iter_mut() {
        let same = row
            .as_object()
            .and_then(|map| map.get("voice"))
            .map(argument_text)
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        if same == name {
            *row = entry.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        voices.push(entry.clone());
    }
    if let Some(map) = data.as_object_mut() {
        map.insert("voices".to_string(), Value::Array(voices));
    }
    write_library(root, &data)?;
    Ok(entry)
}

/// 删除一条自定义音色；不存在返回 false。
pub fn delete_custom_voice(root: &Path, voice: &str) -> Result<bool, String> {
    let name = voice.trim().to_string();
    let _guard = write_lock().lock().expect("音色库写锁");
    let mut data = read_raw(root);
    let rows = data
        .get("voices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut removed = false;
    let mut kept: Vec<Value> = Vec::new();
    for row in rows {
        let same = row
            .as_object()
            .and_then(|map| map.get("voice"))
            .map(argument_text)
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        if same == name {
            removed = true;
        } else {
            kept.push(row);
        }
    }
    if !removed {
        return Ok(false);
    }
    if let Some(map) = data.as_object_mut() {
        map.insert("voices".to_string(), Value::Array(kept));
    }
    write_library(root, &data)?;
    Ok(true)
}

fn write_library(root: &Path, data: &Value) -> Result<(), String> {
    let path = custom_voices_path_in(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("写入自定义音色库失败：{error}"))?;
    }
    std::fs::write(&path, python_dumps(data, 2))
        .map_err(|error| format!("写入自定义音色库失败：{error}"))
}

/// 读取模型 manifest 的内置音色行（不加载 ONNX session）；模型缺失返回空。
pub fn builtin_voice_rows(model_dir: Option<&Path>) -> Vec<Value> {
    let resolved = match model_dir {
        Some(path) => path.to_path_buf(),
        None => crate::config::default_model_dir(),
    };
    let Some(manifest_path) = MANIFEST_CANDIDATES
        .iter()
        .map(|relative| resolved.join(relative))
        .find(|candidate| candidate.is_file())
    else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&manifest_path) else {
        return Vec::new();
    };
    let Ok(manifest) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(rows) = manifest.get("builtin_voices").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter(|row| {
            row.as_object()
                .and_then(|map| map.get("voice"))
                .map(|value| !argument_text(value).trim().is_empty())
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// 全部可用音色名：内置（manifest 顺序）+ 自定义（音色库顺序）。
pub fn all_voice_names(model_dir: Option<&Path>, root: &Path) -> Vec<String> {
    let mut names: Vec<String> = builtin_voice_rows(model_dir)
        .iter()
        .map(|row| {
            row.get("voice")
                .map(argument_text)
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .filter(|name| !name.is_empty())
        .collect();
    names.extend(list_custom_voice_names(root));
    names
}

/// `str(value or "")` 的可用子集。
fn argument_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(flag) => if *flag { "True" } else { "False" }.to_string(),
        Value::Number(number) => number.to_string(),
        other => omnicrawl_controllers::json::python_repr(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-tts-voices-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时目录");
        root
    }

    #[test]
    fn names_are_validated_like_python() {
        assert_eq!(validate_voice_name("  Fairy  ").unwrap(), "Fairy");
        assert_eq!(validate_voice_name("我的音色 1").unwrap(), "我的音色 1");
        assert_eq!(
            validate_voice_name("   ").unwrap_err(),
            "音色名称不能为空。"
        );
        assert_eq!(
            validate_voice_name(&"a".repeat(41)).unwrap_err(),
            "音色名称过长（最多 40 个字符）。"
        );
        assert_eq!(
            validate_voice_name("bad/name").unwrap_err(),
            "音色名称只能包含中文、字母、数字、空格、下划线、连字符或点。"
        );
    }

    #[test]
    fn adding_and_deleting_voices_round_trips_through_the_library() {
        let root = temp_root("library");
        let entry = add_custom_voice(
            &root,
            "Fairy",
            &[vec![1, 2, 3]],
            "CN 我的克隆音色",
            "fairy_ref.wav",
            "source.wav",
        )
        .expect("写入应当成功");
        assert_eq!(entry["group"], CUSTOM_VOICE_GROUP);
        assert_eq!(entry["audio_file"], "fairy_ref.wav");
        assert_eq!(list_custom_voice_names(&root), vec!["Fairy".to_string()]);

        // 同名覆盖：条目数不变。
        add_custom_voice(&root, "Fairy", &[vec![9]], "", "", "").expect("覆盖应当成功");
        assert_eq!(load_custom_voices(&root).len(), 1);
        assert_eq!(entry_of(&root, "Fairy")["audio_file"], "Fairy");

        assert!(delete_custom_voice(&root, "Fairy").expect("删除应当成功"));
        assert!(!delete_custom_voice(&root, "Fairy").expect("再次删除返回 false"));
        assert!(list_custom_voice_names(&root).is_empty());
    }

    fn entry_of(root: &Path, name: &str) -> Value {
        load_custom_voices(root)
            .into_iter()
            .find(|row| row["voice"] == name)
            .expect("条目存在")
    }

    #[test]
    fn builtin_rows_come_from_the_manifest() {
        let root = temp_root("manifest");
        let model_dir = root.join("model");
        std::fs::create_dir_all(&model_dir).expect("创建模型目录");
        std::fs::write(
            model_dir.join("browser_poc_manifest.json"),
            r#"{"builtin_voices":[{"voice":"Junhao"},{"voice":""},{"other":1}]}"#,
        )
        .expect("写 manifest");
        let rows = builtin_voice_rows(Some(&model_dir));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["voice"], "Junhao");
        assert!(all_voice_names(Some(&model_dir), &root) == vec!["Junhao".to_string()]);
    }
}
