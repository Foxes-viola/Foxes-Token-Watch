//! 各家 agent 共用的数据模型。

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Deepseek,
    Antigravity,
    Kiro,
    Cursor,
    /// 保留旧快照与本机记录的分类标识；Cursor 内的 MCP 对话现在统一归入 Cursor。
    #[serde(rename = "webmcp")]
    WebMcp,
}

impl Agent {
    pub const COUNT: usize = 7;
    pub const ALL: [Agent; Agent::COUNT] =
        [Agent::Claude, Agent::Codex, Agent::Deepseek, Agent::Antigravity, Agent::Kiro, Agent::Cursor, Agent::WebMcp];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Deepseek => "deepseek",
            Agent::Antigravity => "antigravity",
            Agent::Kiro => "kiro",
            Agent::Cursor => "cursor",
            Agent::WebMcp => "webmcp",
        }
    }

    pub fn from_id(id: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|agent| agent.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Deepseek => "Deepseek Harness",
            Agent::Antigravity => "Antigravity",
            Agent::Kiro => "Kiro",
            Agent::Cursor => "Cursor",
            Agent::WebMcp => "网页 MCP",
        }
    }

    /// 来源本身不记录 token 用量，数字由本程序估算。
    pub fn estimates_tokens(self) -> bool {
        matches!(self, Agent::Kiro | Agent::Cursor | Agent::WebMcp)
    }
}

/// 统一口径的 token 用量。
///
/// `input` 只算未命中缓存的输入；`reasoning` 是 `output` 的子集，不重复计入总量。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }

    pub fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }

    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
        self.reasoning += other.reasoning;
    }
}

/// 思考强度（reasoning effort）。前面几档按从低到高排列，`Unknown` 表示来源没有记录。
///
/// 带 `Fast` 的是模型名里强度后面还跟着 `fast` 的那一档（`-max-fast`），和不带的分开算。
/// 用量历史里存的是下标，新的档位只能加在末尾。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    #[default]
    Unknown,
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    #[serde(rename = "minimal-fast")]
    MinimalFast,
    #[serde(rename = "low-fast")]
    LowFast,
    #[serde(rename = "medium-fast")]
    MediumFast,
    #[serde(rename = "high-fast")]
    HighFast,
    #[serde(rename = "xhigh-fast")]
    XHighFast,
    #[serde(rename = "max-fast")]
    MaxFast,
}

/// 名字里的词（连续的 ASCII 字母数字）和它的起始位置，从后往前。
fn words_from_end(name: &str) -> impl Iterator<Item = (usize, &str)> {
    let bytes = name.as_bytes();
    let mut end = bytes.len();
    std::iter::from_fn(move || {
        while end > 0 && !bytes[end - 1].is_ascii_alphanumeric() {
            end -= 1;
        }
        let stop = end;
        while end > 0 && bytes[end - 1].is_ascii_alphanumeric() {
            end -= 1;
        }
        (end < stop).then(|| (end, &name[end..stop]))
    })
}

impl Effort {
    pub const COUNT: usize = 14;
    pub const ALL: [Effort; Effort::COUNT] = [
        Effort::Unknown,
        Effort::Off,
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
        Effort::MinimalFast,
        Effort::LowFast,
        Effort::MediumFast,
        Effort::HighFast,
        Effort::XHighFast,
        Effort::MaxFast,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(index: usize) -> Effort {
        Effort::ALL.get(index).copied().unwrap_or_default()
    }

    pub fn id(self) -> &'static str {
        match self {
            Effort::Unknown => "unknown",
            Effort::Off => "off",
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
            Effort::MinimalFast => "minimal-fast",
            Effort::LowFast => "low-fast",
            Effort::MediumFast => "medium-fast",
            Effort::HighFast => "high-fast",
            Effort::XHighFast => "xhigh-fast",
            Effort::MaxFast => "max-fast",
        }
    }

    /// 同一档带 `fast` 的那一种；没有对应的（没记录、关闭）原样返回。
    pub fn with_fast(self) -> Effort {
        match self {
            Effort::Minimal => Effort::MinimalFast,
            Effort::Low => Effort::LowFast,
            Effort::Medium => Effort::MediumFast,
            Effort::High => Effort::HighFast,
            Effort::XHigh => Effort::XHighFast,
            Effort::Max => Effort::MaxFast,
            other => other,
        }
    }

    /// 去掉 `fast` 之后的那一档。
    pub fn without_fast(self) -> Effort {
        match self {
            Effort::MinimalFast => Effort::Minimal,
            Effort::LowFast => Effort::Low,
            Effort::MediumFast => Effort::Medium,
            Effort::HighFast => Effort::High,
            Effort::XHighFast => Effort::XHigh,
            Effort::MaxFast => Effort::Max,
            other => other,
        }
    }

    /// 认出各家记录里的写法（`xhigh`、`x-high`、`none`、`disabled` …）；认不出来的算没有记录。
    pub fn parse(text: &str) -> Effort {
        let mut word = [0u8; 12];
        let mut len = 0;
        for byte in text.bytes().filter(|byte| byte.is_ascii_alphanumeric()) {
            if len == word.len() {
                return Effort::Unknown;
            }
            word[len] = byte.to_ascii_lowercase();
            len += 1;
        }
        match &word[..len] {
            b"off" | b"none" | b"disabled" | b"false" => Effort::Off,
            b"minimal" | b"min" => Effort::Minimal,
            b"low" => Effort::Low,
            b"medium" | b"med" | b"normal" => Effort::Medium,
            b"high" => Effort::High,
            b"xhigh" | b"extrahigh" | b"veryhigh" => Effort::XHigh,
            b"max" | b"maximum" => Effort::Max,
            b"minimalfast" => Effort::MinimalFast,
            b"lowfast" => Effort::LowFast,
            b"mediumfast" => Effort::MediumFast,
            b"highfast" => Effort::HighFast,
            b"xhighfast" => Effort::XHighFast,
            b"maxfast" => Effort::MaxFast,
            _ => Effort::Unknown,
        }
    }

    /// 把模型名拆成模型本身和末尾带的强度：`claude-opus-5-5-max-fast` → (`claude-opus-5-5`, MaxFast)、
    /// `gemini-3-pro-high` → (`gemini-3-pro`, High)、`claude-fable-5-1 · Max` → (`claude-fable-5-1`, Max)。
    /// `fast` 紧跟在强度后面时和它合成一档，`-max-fast` 和 `-max` 是两档。认不出强度的名字原样返回。
    pub fn split_model_name(name: &str) -> (&str, Effort) {
        let mut words = words_from_end(name).peekable();
        let mut fast = false;
        while let Some((start, word)) = words.next() {
            let lower = word.to_ascii_lowercase();
            match lower.as_str() {
                // 这些词排在强度后面，跳过继续往前找
                "fast" => fast = true,
                "thinking" | "preview" | "latest" | "mode" => {}
                // `gpt-5.1-codex-max` 是模型名，不是强度
                "max" if words.peek().is_some_and(|(_, previous)| previous.eq_ignore_ascii_case("codex")) => break,
                "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => {
                    let base = name[..start].trim_end_matches(|ch: char| !ch.is_alphanumeric());
                    if base.is_empty() {
                        break;
                    }
                    let effort = Effort::parse(&lower);
                    return (base, if fast { effort.with_fast() } else { effort });
                }
                _ => break,
            }
        }
        (name, Effort::Unknown)
    }

    /// 模型名末尾带的强度，见 [`Effort::split_model_name`]。只在来源没有单独记录强度时使用。
    pub fn from_model_name(name: &str) -> Effort {
        Effort::split_model_name(name).1
    }
}

/// 一次模型响应。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageEvent {
    /// 响应结束时刻（epoch 毫秒）
    pub ts: i64,
    pub usage: Usage,
    /// 所属文件 `models` 表里的下标
    pub model: u16,
    /// 跨文件去重键，0 表示不参与去重
    pub key: u64,
    /// 请求发出到响应结束的耗时，0 表示未知
    pub latency_ms: u32,
    /// 首字（首个内容块）延迟，0 表示未知
    pub ttft_ms: u32,
    /// 思考时长，0 表示未知
    pub think_ms: u32,
    /// 思考 token 是按文本占比估算的（来源没有记录精确值）
    pub reasoning_estimated: bool,
    /// 整条用量都是估算的：来源没有记录 token，按上下文读数和文本体量推算
    pub estimated: bool,
    /// 这次计费消耗的额度（Kiro 的 credits），0 表示来源没有记录
    pub credits: f32,
    /// 这次请求设定的思考强度；来源没有单独记录时为 `Unknown`，汇总时再看模型名
    #[serde(default)]
    pub effort: Effort,
}

/// 会话当前在做什么。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Idle,
    /// 请求已发出，还没有任何输出（排队 / 等待响应）
    Requesting,
    Thinking,
    Writing,
    /// 来源分不出思考和写入时统称“生成中”
    Generating,
    Tool,
    Retrying,
    /// 等待用户确认
    Waiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetKind {
    Retry,
    Error,
}

/// 一次网络 / 接口异常（只保留错误码，不保留报错正文）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetEvent {
    pub ts: i64,
    pub kind: NetKind,
    pub code: String,
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub started_ms: i64,
    pub last_active_ms: i64,
    /// 最近一次请求占用的上下文（token）
    pub context_used: u64,
    pub context_window: u64,
    /// 子代理会话：用量照常统计，但不单独计入“对话数”
    pub sub_agent: bool,
    pub parent_id: String,
    pub app_version: String,
}

/// 从记录末尾推断出的实时状态。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveTrack {
    pub turn_open: bool,
    pub turn_started_ms: i64,
    pub phase: Phase,
    pub phase_since_ms: i64,
    pub tool: String,
    pub turns: u32,
    pub last_turn_ms: u64,
    pub last_turn_end_ms: i64,
}

impl LiveTrack {
    pub fn set_phase(&mut self, phase: Phase, ts: i64) {
        if self.phase != phase {
            self.phase = phase;
            self.phase_since_ms = ts;
        }
        if phase != Phase::Tool {
            self.tool.clear();
        }
    }

    pub fn open_turn(&mut self, ts: i64) {
        if !self.turn_open {
            self.turn_open = true;
            self.turn_started_ms = ts;
        }
    }

    pub fn close_turn(&mut self, ts: i64, duration_ms: Option<u64>) {
        if self.turn_open {
            self.turns += 1;
            self.last_turn_ms = duration_ms.unwrap_or_else(|| (ts - self.turn_started_ms).max(0) as u64);
            self.last_turn_end_ms = ts;
        }
        self.turn_open = false;
        self.set_phase(Phase::Idle, ts);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RateWindow {
    pub used_percent: f64,
    pub window_minutes: u64,
    /// 额度重置时刻（epoch 毫秒），0 表示未知
    pub resets_at_ms: i64,
    /// 已用量和上限（按 `unit` 计），上限为 0 表示来源只给了百分比
    pub used: f64,
    pub limit: f64,
    /// 计量单位，比如 `credits`；空表示未知
    pub unit: String,
    /// 额度名称，比如“每月额度”；空时界面按时间窗口命名
    pub label: String,
    /// `resets_at_ms` 是到期时刻（赠送额度到期作废），不是重置时刻
    pub expires: bool,
}

/// 额度使用情况（Codex 的时间窗口额度、Kiro 的每月 credits）。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimits {
    pub ts: i64,
    pub primary: Option<RateWindow>,
    pub secondary: Option<RateWindow>,
    pub plan: String,
}

/// 一次工具调用（详情页的工具统计用）。只保留工具名和结果，不保留参数。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolCall {
    pub ts: i64,
    pub name: String,
    /// MCP 服务名；内置工具为空
    pub server: String,
    pub ok: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_effort_names() {
        for (text, effort) in [
            ("low", Effort::Low),
            ("Medium", Effort::Medium),
            ("HIGH", Effort::High),
            ("xhigh", Effort::XHigh),
            ("x-high", Effort::XHigh),
            ("max", Effort::Max),
            ("minimal", Effort::Minimal),
            ("none", Effort::Off),
            ("disabled", Effort::Off),
            ("", Effort::Unknown),
            ("auto", Effort::Unknown),
            ("something-very-long-and-unrelated", Effort::Unknown),
            ("max-fast", Effort::MaxFast),
            ("Max Fast", Effort::MaxFast),
        ] {
            assert_eq!(Effort::parse(text), effort, "{text}");
        }
        for effort in Effort::ALL {
            assert_eq!(Effort::from_index(effort.index()), effort);
            assert_eq!(serde_json::to_string(&effort).unwrap(), format!("\"{}\"", effort.id()));
            if effort != Effort::Unknown {
                assert_eq!(Effort::parse(effort.id()), effort);
            }
        }
        assert_eq!(Effort::from_index(99), Effort::Unknown);
        assert!(Effort::Low < Effort::High && Effort::High < Effort::XHigh && Effort::XHigh < Effort::Max);
        // 用量历史里存的是下标：原有的几档不能挨位置
        assert_eq!((Effort::Unknown.index(), Effort::High.index(), Effort::Max.index()), (0, 5, 7));
        for effort in Effort::ALL {
            assert_eq!(effort.with_fast().without_fast(), effort.without_fast());
        }
        assert_eq!((Effort::Max.with_fast(), Effort::MaxFast.without_fast()), (Effort::MaxFast, Effort::Max));
        assert_eq!((Effort::Unknown.with_fast(), Effort::Off.with_fast()), (Effort::Unknown, Effort::Off));
    }

    #[test]
    fn reads_effort_from_model_name() {
        for (name, base, effort) in [
            ("claude-opus-5-5-max-fast", "claude-opus-5-5", Effort::MaxFast),
            ("claude-opus-5-5-max", "claude-opus-5-5", Effort::Max),
            ("Claude Fable 5.2 Max Fast", "Claude Fable 5.2", Effort::MaxFast),
            ("claude-opus-5-5-medium", "claude-opus-5-5", Effort::Medium),
            ("claude-4.5-sonnet-high-thinking", "claude-4.5-sonnet", Effort::High),
            ("claude-4.5-sonnet-high-thinking-fast", "claude-4.5-sonnet", Effort::HighFast),
            ("gemini-3-pro-high", "gemini-3-pro", Effort::High),
            ("gemini-3-pro-low", "gemini-3-pro", Effort::Low),
            ("gpt-5.2-xhigh", "gpt-5.2", Effort::XHigh),
            ("gpt-5.2-xhigh-fast", "gpt-5.2", Effort::XHighFast),
            ("claude-fable-5-1 · Max", "claude-fable-5-1", Effort::Max),
            ("gpt-5.1-codex-max-high-fast", "gpt-5.1-codex-max", Effort::HighFast),
            // 认不出强度的不拆：单独的 fast 可能是另一个模型
            ("gpt-5.1-codex-max", "gpt-5.1-codex-max", Effort::Unknown),
            ("gpt-5.1-codex-max-fast", "gpt-5.1-codex-max-fast", Effort::Unknown),
            ("grok-4-fast", "grok-4-fast", Effort::Unknown),
            ("composer-2.5", "composer-2.5", Effort::Unknown),
            ("claude-sonnet-4.5", "claude-sonnet-4.5", Effort::Unknown),
            ("high-flyer-2", "high-flyer-2", Effort::Unknown),
            ("max", "max", Effort::Unknown),
            ("模型 high", "模型", Effort::High),
            ("", "", Effort::Unknown),
        ] {
            assert_eq!(Effort::split_model_name(name), (base, effort), "{name}");
            assert_eq!(Effort::from_model_name(name), effort, "{name}");
        }
    }
}
