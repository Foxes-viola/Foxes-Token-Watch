//! 文本体量折算 token 的估算口径（Kiro / Cursor 的记录里没有 token 数时用）。
//!
//! 口径用本机真实数据校准过：
//! - Kiro 的 `contextUsage` 百分比 × 100 万是精确的提示词 token 数，相邻两次请求之间
//!   上下文的增量约等于新增内容体量 × 0.46（中位数 0.49）。
//! - Cursor 状态块里的精确上下文大小与按体量累计的估算同量级。
//!
//! 体量的算法见 [`crate::lenient::text_weight`]：ASCII 每字节 1，其他字符每个 5。

use crate::model::Usage;

/// 体量 → token 的比例（千分数）。
///
/// 实测：中文约 1.5 token / 字（体量 5）、英文和代码约 0.3 token / 字节，合在一起取 0.3。
/// 上下文的增量比这个大（工具调用本身的封装、系统提示重发等），用 [`CONTEXT_PER_WEIGHT_MILLI`]。
const TOKENS_PER_WEIGHT_MILLI: u64 = 300;
/// 上下文增量与新增内容体量的比例（千分数），用 Kiro 的精确读数校准。
const CONTEXT_PER_WEIGHT_MILLI: u64 = 460;
/// 第一次请求前已经在上下文里的部分（系统提示、工具定义、规则等）。
/// Kiro 实测约 3.2 万，Cursor 实测约 1.6~2.6 万，取中间值。
pub const BASE_CONTEXT: u64 = 26_000;
/// 只有工具调用、没有正文时，输出至少有这么多（工具名、参数的封装）。
const MIN_OUTPUT: u64 = 24;

/// 文本体量折成的 token 数。
pub fn tokens_of(weight: u64) -> u64 {
    weight.saturating_mul(TOKENS_PER_WEIGHT_MILLI) / 1000
}

/// 新增内容让上下文长了多少 token。
pub fn context_growth(weight: u64) -> u64 {
    weight.saturating_mul(CONTEXT_PER_WEIGHT_MILLI) / 1000
}

/// 一次模型调用的输出：正文和工具参数的 token 数加上思考部分。
/// 返回 `(输出, 其中的思考)`。
pub fn output_of(text_weight: u64, thinking_weight: u64) -> (u64, u64) {
    let reasoning = tokens_of(thinking_weight);
    let output = (tokens_of(text_weight) + reasoning).max(MIN_OUTPUT);
    (output, reasoning.min(output))
}

/// 把一次请求的提示词划分成缓存命中和未命中。
///
/// 两家的服务端都有提示词缓存：连续的多步请求里，上一次已经发过的前缀会命中缓存。
/// 记录里没有命中数，按“上一次的提示词全部命中，新增部分未命中”估算；
/// 距上一次太久（缓存已过期）时整段算未命中。
pub fn split_prompt(prompt: u64, previous_prompt: u64, cache_alive: bool) -> Usage {
    let cache_read = if cache_alive { previous_prompt.min(prompt) } else { 0 };
    Usage { input: prompt - cache_read, cache_read, cache_write: 0, output: 0, reasoning: 0 }
}

/// 提示词缓存的有效期：超过这么久没有新请求就当作已过期。
pub const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// 上下文涨到窗口的这个比例（千分数）时，当作客户端已把历史压缩成摘要。
/// Cursor 两个长对话结束时的上下文读数（窗口 100 万）是 30 万和 54 万，与这个比例的模拟结果同量级。
const COMPACT_AT_MILLI: u64 = 700;
/// 压缩后的摘要占窗口的比例（千分数）。
const SUMMARY_MILLI: u64 = 50;

/// 记录里只有对话内容、没有 token 数时，按内容累计上下文来估算每次模型调用的提示词。
///
/// 每次调用都会把当前整个上下文重新发一遍；模型输出、工具结果、用户消息按 [`context_growth`] 加进上下文，
/// 涨到窗口的一定比例时视为已压缩。命中和未命中的划分见 [`split_prompt`]。
#[derive(Clone, Debug)]
pub struct ContextModel {
    window: u64,
    context: u64,
    last_prompt: u64,
    last_request_ms: i64,
}

impl ContextModel {
    pub fn new(window: u64) -> ContextModel {
        ContextModel { window: window.max(4 * BASE_CONTEXT), context: BASE_CONTEXT, last_prompt: 0, last_request_ms: 0 }
    }

    /// 接着一段已经算过的历史往下算：那段历史的最后一次请求在 `ts` 发出，提示词是 `prompt`。
    /// 来源只留着对话的后半段时用，免得后半段从空上下文重新算起。
    pub fn resume(&mut self, prompt: u64, ts: i64) {
        self.context = prompt.clamp(BASE_CONTEXT, self.window);
        self.last_prompt = self.context;
        self.last_request_ms = ts.max(1);
    }

    /// 新内容（体量）进入上下文。
    pub fn add(&mut self, weight: u64) {
        self.context = self.context.saturating_add(context_growth(weight));
    }

    /// 在 `ts` 发出一次请求：返回这次提示词的用量（输出为 0，由调用方按输出体量补上）。
    pub fn request(&mut self, ts: i64) -> Usage {
        if self.context > self.window / 1000 * COMPACT_AT_MILLI {
            self.context = BASE_CONTEXT + self.window / 1000 * SUMMARY_MILLI;
            // 摘要是新内容，只有系统提示和工具定义还在缓存里
            self.last_prompt = self.last_prompt.min(BASE_CONTEXT);
        }
        let cache_alive = self.last_request_ms > 0 && ts - self.last_request_ms < CACHE_TTL_MS;
        let usage = split_prompt(self.context, self.last_prompt, cache_alive);
        self.last_prompt = self.context;
        self.last_request_ms = ts.max(1);
        usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_prompt_into_cached_and_new() {
        let usage = split_prompt(50_000, 40_000, true);
        assert_eq!((usage.input, usage.cache_read), (10_000, 40_000));
        let cold = split_prompt(50_000, 40_000, false);
        assert_eq!((cold.input, cold.cache_read), (50_000, 0));
        // 上下文被压缩后变小：命中部分不会超过这次的提示词
        let shrunk = split_prompt(20_000, 90_000, true);
        assert_eq!((shrunk.input, shrunk.cache_read), (0, 20_000));
    }

    #[test]
    fn context_model_resends_history_and_compacts_near_the_window() {
        let mut model = ContextModel::new(200_000);
        let first = model.request(1_000);
        assert_eq!((first.input, first.cache_read), (BASE_CONTEXT, 0));
        model.add(100_000);
        let second = model.request(2_000);
        assert_eq!((second.input, second.cache_read), (context_growth(100_000), BASE_CONTEXT));
        // 超过窗口的 70%：压缩成基础上下文加摘要，摘要部分不命中缓存
        model.add(300_000);
        let compacted = model.request(3_000);
        assert_eq!((compacted.input, compacted.cache_read), (10_000, BASE_CONTEXT));
        let cold = model.request(3_000 + CACHE_TTL_MS);
        assert_eq!(cold.cache_read, 0);
    }

    #[test]
    fn resumed_model_continues_from_the_recorded_prompt() {
        let mut model = ContextModel::new(1_000_000);
        model.resume(180_000, 5_000);
        model.add(10_000);
        let next = model.request(6_000);
        assert_eq!((next.input, next.cache_read), (context_growth(10_000), 180_000));
        // 记下的提示词不会比基础上下文小，也不会超过窗口
        let mut small = ContextModel::new(200_000);
        small.resume(10, 1_000);
        assert_eq!(small.request(2_000).cache_read, BASE_CONTEXT);
        small.resume(u64::MAX, 3_000);
        let compacted = small.request(4_000);
        assert_eq!(compacted.input + compacted.cache_read, BASE_CONTEXT + 10_000);
    }

    #[test]
    fn output_has_a_floor_and_keeps_reasoning_inside() {
        assert_eq!(output_of(0, 0), (MIN_OUTPUT, 0));
        let (output, reasoning) = output_of(1000, 2000);
        assert_eq!(reasoning, 600);
        assert_eq!(output, 900);
    }
}
