//! 命令行入口：`omnicrawl-tts`，对应 `python -m omnicrawl.tts`。
//!
//! ```text
//! omnicrawl-tts --text "欢迎使用 MOSS-TTS-Nano。" --voice Junhao --output out.wav
//! omnicrawl-tts --text "你好" --prompt-audio ref.wav
//! omnicrawl-tts --list-voices
//! omnicrawl-tts --warmup
//! ```

use std::path::PathBuf;

use crate::config::TtsConfig;
use crate::engine::TtsEngine;
use crate::player::play_wav;

const HELP: &str = "\
MOSS-TTS-Nano ONNX 语音合成

用法：omnicrawl-tts [选项]

选项：
  --text <文本>                     要合成的文本
  --text-file <路径>                UTF-8 文本文件路径（与 --text 二选一）
  --voice <音色名>                  内置音色名（未提供参考音频时使用）
  --prompt-audio <路径>             语音克隆参考音频路径（提供时覆盖 --voice）
  --output <路径>                   输出 WAV 路径
  --model-dir <路径>                模型目录；缺省时自动下载到默认目录
  --output-dir <路径>               输出目录（未指定 --output 时）
  --cpu-threads <数量>              ONNX Runtime CPU intra-op 线程数
  --device <auto|cpu|cuda>          推理设备；cuda 尚未接入
  --sample-mode <greedy|fixed|full> 采样模式
  --do-sample <0|1>                 是否采样（0 时强制 greedy）
  --streaming / --no-streaming      codec 流式解码（默认开启）
  --no-play                         合成后不自动播放
  --max-new-frames <数量>
  --voice-clone-max-text-tokens <数量>
  --seed <整数>
  --enable-wetext                   启用 WeTextProcessing 语义归一化（Rust 未实现）
  --warmup                          预热模型后退出（不合成）
  --list-voices                     列出音色后退出
  -v, --verbose                     输出调试日志
  -h, --help                        显示帮助
";

#[derive(Debug, Clone)]
struct Args {
    text: Option<String>,
    text_file: Option<String>,
    voice: Option<String>,
    prompt_audio: Option<String>,
    output: Option<String>,
    model_dir: Option<String>,
    output_dir: String,
    cpu_threads: i64,
    device: String,
    sample_mode: String,
    do_sample: bool,
    streaming: bool,
    no_play: bool,
    max_new_frames: i64,
    voice_clone_max_text_tokens: i64,
    seed: Option<i64>,
    enable_wetext: bool,
    warmup: bool,
    list_voices: bool,
    verbose: bool,
    help: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            text: None,
            text_file: None,
            voice: None,
            prompt_audio: None,
            output: None,
            model_dir: None,
            output_dir: "generated_audio".to_string(),
            cpu_threads: 4,
            device: "auto".to_string(),
            sample_mode: "fixed".to_string(),
            do_sample: true,
            streaming: true,
            no_play: false,
            max_new_frames: 375,
            voice_clone_max_text_tokens: 75,
            seed: None,
            enable_wetext: false,
            warmup: false,
            list_voices: false,
            verbose: false,
            help: false,
        }
    }
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut index = 0usize;
    let next_value = |index: &mut usize, name: &str| -> Result<String, String> {
        *index += 1;
        argv.get(*index)
            .cloned()
            .ok_or_else(|| format!("{name} 缺少取值。"))
    };
    while index < argv.len() {
        let flag = argv[index].as_str();
        match flag {
            "--text" => args.text = Some(next_value(&mut index, flag)?),
            "--text-file" => args.text_file = Some(next_value(&mut index, flag)?),
            "--voice" => args.voice = Some(next_value(&mut index, flag)?),
            "--prompt-audio" | "--reference-audio" => {
                args.prompt_audio = Some(next_value(&mut index, flag)?)
            }
            "--output" => args.output = Some(next_value(&mut index, flag)?),
            "--model-dir" => args.model_dir = Some(next_value(&mut index, flag)?),
            "--output-dir" => args.output_dir = next_value(&mut index, flag)?,
            "--cpu-threads" => {
                args.cpu_threads = next_value(&mut index, flag)?
                    .parse()
                    .map_err(|_| "--cpu-threads 必须是整数。".to_string())?
            }
            "--device" => {
                let value = next_value(&mut index, flag)?;
                if !["auto", "cpu", "cuda"].contains(&value.as_str()) {
                    return Err("--device 必须是 auto、cpu 或 cuda。".to_string());
                }
                args.device = value;
            }
            "--sample-mode" => {
                let value = next_value(&mut index, flag)?;
                if !["greedy", "fixed", "full"].contains(&value.as_str()) {
                    return Err("--sample-mode 必须是 greedy、fixed 或 full。".to_string());
                }
                args.sample_mode = value;
            }
            "--do-sample" => {
                args.do_sample = next_value(&mut index, flag)? != "0";
            }
            "--streaming" => args.streaming = true,
            "--no-streaming" => args.streaming = false,
            "--no-play" => args.no_play = true,
            "--max-new-frames" => {
                args.max_new_frames = next_value(&mut index, flag)?
                    .parse()
                    .map_err(|_| "--max-new-frames 必须是整数。".to_string())?
            }
            "--voice-clone-max-text-tokens" => {
                args.voice_clone_max_text_tokens = next_value(&mut index, flag)?
                    .parse()
                    .map_err(|_| "--voice-clone-max-text-tokens 必须是整数。".to_string())?
            }
            "--seed" => {
                args.seed = Some(
                    next_value(&mut index, flag)?
                        .parse()
                        .map_err(|_| "--seed 必须是整数。".to_string())?,
                )
            }
            "--enable-wetext" => args.enable_wetext = true,
            "--warmup" => args.warmup = true,
            "--list-voices" => args.list_voices = true,
            "-v" | "--verbose" => args.verbose = true,
            "-h" | "--help" => args.help = true,
            other => return Err(format!("未知参数：{other}")),
        }
        index += 1;
    }
    Ok(args)
}

/// 命令行主流程；返回进程退出码。
pub fn main(argv: Vec<String>) -> i32 {
    let args = match parse_args(&argv) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("错误：{message}");
            return 2;
        }
    };
    if args.help {
        print!("{HELP}");
        return 0;
    }

    let config = TtsConfig {
        model_dir: args.model_dir.as_ref().map(PathBuf::from),
        thread_count: args.cpu_threads,
        device: Some(args.device.clone()),
        sample_mode: args.sample_mode.clone(),
        do_sample: args.do_sample,
        max_new_frames: args.max_new_frames,
        voice: args.voice.clone().unwrap_or_else(|| "Junhao".to_string()),
        prompt_audio_path: args.prompt_audio.as_ref().map(PathBuf::from),
        output_dir: PathBuf::from(&args.output_dir),
        streaming: args.streaming,
        voice_clone_max_text_tokens: args.voice_clone_max_text_tokens,
        enable_wetext: args.enable_wetext,
        seed: args.seed,
        ..TtsConfig::default()
    };

    let mut engine = match TtsEngine::new(config.clone()) {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("错误：{error}");
            return 1;
        }
    };

    if args.list_voices {
        println!("内置音色：");
        for row in engine.list_available_voices() {
            let name = row
                .get("voice")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let custom = row.get("group").and_then(|value| value.as_str()) == Some("Custom");
            println!("  - {name}{}", if custom { "（自定义克隆）" } else { "" });
        }
        return 0;
    }
    if args.warmup {
        if let Err(error) = engine.warmup() {
            eprintln!("错误：{error}");
            return 1;
        }
        println!("预热完成。");
        return 0;
    }

    let text = match (&args.text, &args.text_file) {
        (Some(text), _) => text.clone(),
        (None, Some(path)) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("错误：读取 {path} 失败：{error}");
                return 2;
            }
        },
        (None, None) => {
            eprintln!("错误：必须提供 --text 或 --text-file。");
            return 2;
        }
    };

    let result = match engine.synthesize(
        &text,
        Some(config.voice.as_str()),
        config.prompt_audio_path.as_deref(),
        args.output.as_ref().map(PathBuf::from).as_deref(),
        Some(&args.sample_mode),
        Some(args.do_sample),
        Some(args.streaming),
        None,
        None,
        args.seed,
    ) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("错误：{error}");
            return 1;
        }
    };

    if !args.no_play {
        // 同步播放：等播完再退出，否则进程退出会终止异步播放导致无声。
        play_wav(&result.audio_path, true);
    }
    println!(
        "合成完成：{}  采样率={}Hz 时长={:.2}s 帧数={} 分块={} 模式={}",
        result.audio_path.display(),
        result.sample_rate,
        result.duration_seconds,
        result.audio_token_frames,
        result.text_chunks.len(),
        result.sample_mode
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_parsed_like_the_python_cli() {
        let args = parse_args(&[
            "--text".to_string(),
            "你好".to_string(),
            "--no-play".to_string(),
            "--cpu-threads".to_string(),
            "8".to_string(),
            "--reference-audio".to_string(),
            "ref.wav".to_string(),
        ])
        .expect("参数应当合法");
        assert_eq!(args.text.as_deref(), Some("你好"));
        assert_eq!(args.cpu_threads, 8);
        assert_eq!(args.prompt_audio.as_deref(), Some("ref.wav"));
        assert!(args.no_play);
        assert!(args.streaming);
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(parse_args(&["--device".to_string(), "tpu".to_string()]).is_err());
        assert!(parse_args(&["--sample-mode".to_string(), "mixed3".to_string()]).is_err());
        assert!(parse_args(&["--text".to_string()]).is_err());
        assert!(parse_args(&["--unknown".to_string()]).is_err());
    }
}
