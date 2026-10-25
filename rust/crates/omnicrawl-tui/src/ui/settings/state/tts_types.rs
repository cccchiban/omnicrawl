//! 由 `settings/state.rs` 按页面拆出：行定义与常量，语义未变。

use super::*;

// ---------- TTS 页（云服务通道） ----------

/// TTS 页的行号。前 4 行与通道无关，随后 6 行属于当前服务商。
pub const TTS_ROW_ENABLED: usize = 0;

pub const TTS_ROW_PROVIDER: usize = 1;

pub const TTS_ROW_AUTO_PLAY: usize = 2;

/// 回合结束后朗读助手回复（不经工具面，因此不触发审批）。
pub const TTS_ROW_SPEAK_REPLIES: usize = 3;

/// 通道字段区起点：以下行按当前服务商取值，含义随 `provider` 变。
pub const TTS_ROW_BASE_URL: usize = 4;

pub const TTS_ROW_MODEL: usize = 5;

pub const TTS_ROW_VOICE: usize = 6;

/// OpenAI 的 `instructions` / Gemini 的语气指令（作为正文前缀）。
pub const TTS_ROW_INSTRUCTIONS: usize = 7;

/// OpenAI 的 `speed`；Gemini 没有该参数，该行在 Gemini 下不显示。
pub const TTS_ROW_SPEED: usize = 8;

/// 当前服务商的 API Key：直接填明文，保存时写进对应段的 `api_key`。
pub const TTS_ROW_API_KEY: usize = 9;

pub const TTS_ROW_SAVE: usize = 10;

/// TTS 页的行数。
pub const TTS_ROW_COUNT: usize = 11;

/// 与通道无关的行标签（顺序与行号一致）。
pub const TTS_ROW_LABELS: [&str; TTS_ROW_COUNT] = [
    "启用 TTS",
    "服务商 provider",
    "合成完成后自动播放",
    "自动朗读回复",
    "接口地址 base_url",
    "模型 model",
    "音色 voice",
    "语气指令 instructions",
    "语速 speed",
    "API Key",
    "保存",
];

/// 服务商候选（取值即配置值，与 `[tts_cloud].provider` 一一对应）。
pub const TTS_PROVIDER_OPTIONS: [&str; 2] = ["openai", "gemini"];

/// 语速候选（OpenAI 的取值域是 0.25~4.0，常用的几档够用）。
pub const TTS_SPEED_OPTIONS: [&str; 5] = ["0.75", "1", "1.25", "1.5", "2"];

/// OpenAI 内置音色（13 个，逐项取自官方 TTS 文档的 voice 列表）。
pub const TTS_OPENAI_VOICES: [&str; 13] = [
    "alloy", "ash", "ballad", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer", "verse",
    "marin", "cedar",
];

/// Gemini TTS 预置音色（官方语音列表，取自 Gemini-TTS 文档）。
pub const TTS_GEMINI_VOICES: [&str; 30] = [
    "Zephyr",
    "Puck",
    "Charon",
    "Kore",
    "Fenrir",
    "Leda",
    "Orus",
    "Aoede",
    "Callirrhoe",
    "Autonoe",
    "Enceladus",
    "Iapetus",
    "Umbriel",
    "Algieba",
    "Despina",
    "Erinome",
    "Algenib",
    "Rasalgethi",
    "Laomedeia",
    "Achernar",
    "Alnilam",
    "Schedar",
    "Gacrux",
    "Pulcherrima",
    "Achird",
    "Zubenelgenubi",
    "Vindemiatrix",
    "Sadachbia",
    "Sadaltager",
    "Sulafat",
];

/// TTS 页要宿主做的一件事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsChange {
    /// 保存整页草稿（写 `[tts]` / `[tts_cloud]` 与当前服务商段，并让工具表跟着变）。
    Save(TtsDraft),
}

/// TTS 页保存时的草稿：通用键 + 当前服务商那一段的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsDraft {
    pub enabled: bool,
    pub auto_play: bool,
    pub speak_replies: bool,
    pub provider: String,
    /// 当前服务商的字段：`[tts_api]` 或 `[tts_gemini]` 其一。
    pub channel: TtsChannelDraft,
}

/// 当前服务商的通道字段（两家的字段集不同，因此分开表达）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsChannelDraft {
    OpenAi {
        base_url: String,
        model: String,
        voice: String,
        instructions: String,
        speed: String,
        /// 明文密钥；空串表示「不改动磁盘原值」。
        api_key: String,
    },
    Gemini {
        base_url: String,
        model: String,
        voice: String,
        instructions: String,
        /// 明文密钥；空串表示「不改动磁盘原值」。
        api_key: String,
    },
}

/// TTS 页的初值（宿主在打开面板时读配置给出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsValues {
    pub enabled: bool,
    pub auto_play: bool,
    pub speak_replies: bool,
    pub provider: String,
    /// 两家的配置都带上：切换服务商行时界面立刻显示另一家的值。
    pub openai: TtsChannelValues,
    pub gemini: TtsChannelValues,
}

impl Default for TtsValues {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_play: true,
            speak_replies: false,
            provider: "openai".to_string(),
            openai: TtsChannelValues::openai_default(),
            gemini: TtsChannelValues::gemini_default(),
        }
    }
}

/// 一家的通道初值（字段并集：按服务商取用其中一部分）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TtsChannelValues {
    pub base_url: String,
    pub model: String,
    pub voice: String,
    pub instructions: String,
    /// OpenAI 的语速（文本形式，便于编辑往返）。
    pub speed: String,
    /// 已配置的密钥在界面上只显示掩码占位；输入态从空起步。
    pub api_key: String,
}

impl TtsChannelValues {
    fn openai_default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o-mini-tts".to_string(),
            voice: "alloy".to_string(),
            speed: "1".to_string(),
            ..Self::default()
        }
    }

    fn gemini_default() -> Self {
        Self {
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            model: "gemini-2.5-flash-preview-tts".to_string(),
            voice: "Kore".to_string(),
            ..Self::default()
        }
    }
}

/// TTS 页一行在渲染层的视图。
#[derive(Debug, Clone, PartialEq)]
pub struct TtsRowView {
    pub label: &'static str,
    pub value: String,
    /// 该行是可执行的动作用（渲染成按钮样式）。
    pub action: bool,
    /// 该行在**当前可见行**里的下标（屏幕行号）；选中判据与它比，不与配置行号比。
    pub index: usize,
    /// 该行是否是当前选中行。
    pub focused: bool,
}

/// TTS 页的编辑草稿。
struct TtsState {
    enabled: bool,
    auto_play: bool,
    speak_replies: bool,
    provider: String,
    openai: TtsChannelValues,
    gemini: TtsChannelValues,
    focused: usize,
    /// 文本输入态：`(行号, 缓冲)`，对映 Python 的 `Input` 获得焦点。
    input: Option<(usize, Composer)>,
    status: String,
}

impl TtsState {
    fn new(values: TtsValues) -> Self {
        Self {
            enabled: values.enabled,
            auto_play: values.auto_play,
            speak_replies: values.speak_replies,
            provider: values.provider,
            openai: values.openai,
            gemini: values.gemini,
            focused: TTS_ROW_ENABLED,
            input: None,
            status: String::new(),
        }
    }

    /// 当前服务商的那一份通道草稿。
    fn channel(&self) -> &TtsChannelValues {
        if self.is_gemini() {
            &self.gemini
        } else {
            &self.openai
        }
    }

    /// 当前服务商那一份通道草稿的可变引用。
    fn channel_mut(&mut self) -> &mut TtsChannelValues {
        if self.is_gemini() {
            &mut self.gemini
        } else {
            &mut self.openai
        }
    }

    fn is_gemini(&self) -> bool {
        self.provider == "gemini"
    }

    /// 当前服务商可见的行（Gemini 没有语速）。
    fn visible_rows(&self) -> Vec<usize> {
        (0..TTS_ROW_COUNT)
            .filter(|row| *row != TTS_ROW_SPEED || !self.is_gemini())
            .collect()
    }

    fn current_voices(&self) -> Vec<String> {
        if self.is_gemini() {
            TTS_GEMINI_VOICES
                .iter()
                .map(|name| name.to_string())
                .collect()
        } else {
            TTS_OPENAI_VOICES
                .iter()
                .map(|name| name.to_string())
                .collect()
        }
    }

    fn draft(&self) -> TtsDraft {
        let channel = self.channel();
        let channel = if self.is_gemini() {
            TtsChannelDraft::Gemini {
                base_url: channel.base_url.clone(),
                model: channel.model.clone(),
                voice: channel.voice.clone(),
                instructions: channel.instructions.clone(),
                api_key: secret_draft(&channel.api_key),
            }
        } else {
            TtsChannelDraft::OpenAi {
                base_url: channel.base_url.clone(),
                model: channel.model.clone(),
                voice: channel.voice.clone(),
                instructions: channel.instructions.clone(),
                speed: channel.speed.clone(),
                api_key: secret_draft(&channel.api_key),
            }
        };
        TtsDraft {
            enabled: self.enabled,
            auto_play: self.auto_play,
            speak_replies: self.speak_replies,
            provider: self.provider.clone(),
            channel,
        }
    }

    fn row_text(&self, row: usize) -> String {
        // 输入态：该行显示缓冲文本与光标。
        if let Some((input_row, composer)) = &self.input {
            if *input_row == row {
                return format!("{}▌", composer.text());
            }
        }
        let channel = self.channel();
        match row {
            TTS_ROW_ENABLED => if self.enabled { "启用" } else { "停用" }.to_string(),
            TTS_ROW_PROVIDER => self.provider.clone(),
            TTS_ROW_AUTO_PLAY => if self.auto_play { "开启" } else { "关闭" }.to_string(),
            TTS_ROW_SPEAK_REPLIES => if self.speak_replies {
                "开启"
            } else {
                "关闭"
            }
            .to_string(),
            TTS_ROW_BASE_URL => channel.base_url.clone(),
            TTS_ROW_MODEL => channel.model.clone(),
            TTS_ROW_VOICE => channel.voice.clone(),
            TTS_ROW_INSTRUCTIONS => channel.instructions.clone(),
            TTS_ROW_SPEED => channel.speed.clone(),
            // 明文永不回显：已配置的密钥只显示掩码占位。
            TTS_ROW_API_KEY => match channel.api_key.trim() {
                "" => "（未配置）".to_string(),
                _ => MASKED_SECRET.to_string(),
            },
            _ => String::new(),
        }
    }

    fn rows(&self) -> Vec<TtsRowView> {
        self.visible_rows()
            .into_iter()
            .enumerate()
            .map(|(index, row)| TtsRowView {
                label: TTS_ROW_LABELS[row],
                value: self.row_text(row),
                action: row == TTS_ROW_SAVE || row == TTS_ROW_PROVIDER,
                index,
                focused: row == self.focused,
            })
            .collect()
    }

    /// 把选中行挪到相邻的可见行（隐藏行的位置会被跳过）。
    fn move_focus(&mut self, delta: isize) {
        let rows = self.visible_rows();
        let Some(index) = rows.iter().position(|row| *row == self.focused) else {
            self.focused = rows.first().copied().unwrap_or(TTS_ROW_ENABLED);
            return;
        };
        let next = (index as isize + delta).rem_euclid(rows.len() as isize) as usize;
        self.focused = rows[next];
    }

    fn cycle(&mut self, row: usize, direction: isize) {
        // 切换服务商后原来选中的行可能不再可见，落回第一行。
        let gemini = self.is_gemini();
        match row {
            TTS_ROW_ENABLED => self.enabled = !self.enabled,
            TTS_ROW_AUTO_PLAY => self.auto_play = !self.auto_play,
            TTS_ROW_SPEAK_REPLIES => self.speak_replies = !self.speak_replies,
            TTS_ROW_PROVIDER => {
                let options: Vec<String> =
                    TTS_PROVIDER_OPTIONS.iter().map(|v| v.to_string()).collect();
                self.provider = cycle_text(&options, &self.provider, direction);
                if !self.visible_rows().contains(&self.focused) {
                    self.focused = TTS_ROW_ENABLED;
                }
            }
            TTS_ROW_VOICE => {
                let voices = self.current_voices();
                let channel = if gemini {
                    &mut self.gemini
                } else {
                    &mut self.openai
                };
                channel.voice = cycle_text(&voices, &channel.voice, direction);
            }
            TTS_ROW_SPEED => {
                let options: Vec<String> =
                    TTS_SPEED_OPTIONS.iter().map(|v| v.to_string()).collect();
                self.openai.speed = cycle_text(&options, &self.openai.speed, direction);
            }
            _ => {}
        }
    }

    /// 保存成功：把草稿回落到新配置（界面显示值跟着更新）。
    fn apply_saved(&mut self, draft: &TtsDraft) {
        self.enabled = draft.enabled;
        self.auto_play = draft.auto_play;
        self.speak_replies = draft.speak_replies;
        self.provider = draft.provider.clone();
        match &draft.channel {
            TtsChannelDraft::OpenAi {
                base_url,
                model,
                voice,
                instructions,
                speed,
                api_key,
            } => {
                self.openai = TtsChannelValues {
                    base_url: base_url.clone(),
                    model: model.clone(),
                    voice: voice.clone(),
                    instructions: instructions.clone(),
                    speed: speed.clone(),
                    // 草稿里的密钥就是刚写盘的明文；界面仍只显示掩码。
                    api_key: api_key.clone(),
                };
            }
            TtsChannelDraft::Gemini {
                base_url,
                model,
                voice,
                instructions,
                api_key,
            } => {
                self.gemini = TtsChannelValues {
                    base_url: base_url.clone(),
                    model: model.clone(),
                    voice: voice.clone(),
                    instructions: instructions.clone(),
                    speed: self.gemini.speed.clone(),
                    api_key: api_key.clone(),
                };
            }
        }
    }

    /// 该行是否有输入态（只有文本行才有；开关候选类行就地切档）。
    fn editable(&self, row: usize) -> bool {
        matches!(
            row,
            TTS_ROW_BASE_URL | TTS_ROW_MODEL | TTS_ROW_INSTRUCTIONS | TTS_ROW_API_KEY
        )
    }

    fn begin_input(&mut self, row: usize) {
        if !self.editable(row) {
            return;
        }
        let mut composer = Composer::default();
        // 密钥不回显：进输入态也从空起步，留空即保持磁盘原值。
        if row != TTS_ROW_API_KEY {
            composer.set_text(&self.row_text(row));
        }
        self.input = Some((row, composer));
    }

    fn commit_input(&mut self) {
        let Some((row, composer)) = self.input.take() else {
            return;
        };
        let text = composer.text().to_string();
        let channel = self.channel_mut();
        match row {
            TTS_ROW_BASE_URL => channel.base_url = text,
            TTS_ROW_MODEL => channel.model = text,
            TTS_ROW_INSTRUCTIONS => channel.instructions = text,
            // 留空不改动磁盘原值（对映渠道表单的 API Key 行）。
            TTS_ROW_API_KEY => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    channel.api_key = trimmed.to_string();
                }
            }
            _ => {}
        }
    }

    /// 粘贴：文本行直接落到输入态（没有输入态就先按当前行进；动作行不接收）。
    fn paste(&mut self, text: &str) -> bool {
        if self.input.is_none() {
            self.begin_input(self.focused);
            if self.input.is_none() {
                return false;
            }
        }
        match self.input.as_mut() {
            Some((_, composer)) => paste_into(Some(composer), text),
            None => false,
        }
    }
}

const TTS_HINT: &str = "↑↓ 选择  ←→/Enter/空格 切换  Enter 执行  Ctrl+S 保存  Esc 返回";

