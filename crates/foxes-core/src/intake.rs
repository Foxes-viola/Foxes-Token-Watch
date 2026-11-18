//! 近一分钟里“记下来”的用量。
//!
//! 来源把一次请求记下来，总比它实际发生晚一截：自己写日志的 agent 晚不到一秒，插件卡片（百事通要等服务端同步、
//! 再由这边轮询）晚几秒到几分钟。请求的时刻照它实际发生的算，等读到时它早就不在“最近这一会儿”里了，
//! 所以按发生时刻算的“当前速率”几乎总是 0，近一分钟的量也总是少一大截。
//!
//! 这里换一个口径：看各分钟的用量比上一轮多出来多少，多出来的就是刚记下的，算在“现在”。隔了很久才补上的旧记录
//! （程序没开、插件掉线期间的）不算刚发生的用量，由 [`GRACE`] 挡在外面：它们照样按发生时刻进各分钟的曲线。

use std::collections::VecDeque;

use serde::Serialize;

use crate::aggregate::RatePoint;
use crate::timeutil::MINUTE;

/// 记录比实际晚这么久以内的，算作刚记下的用量（按请求所在那一分钟的起点算）
const GRACE: i64 = 5 * MINUTE;
/// 刚启动的这段时间里，来源还在把停机期间的记录补回来，只认 [`WARMUP_GRACE`] 以内的
const WARMUP: i64 = MINUTE;
const WARMUP_GRACE: i64 = 2 * MINUTE;
const WINDOW: i64 = MINUTE;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Recorded {
    /// 近一分钟里记下的 token 数（输入 + 缓存 + 输出）
    pub minute: u64,
    /// 最近一次记下用量的时刻；启动以来还没有记到过时为 0
    pub last_at: i64,
}

/// 某一分钟（按请求发生的时刻分）的用量：上一轮的值，和见过的最大值。
#[derive(Clone, Copy, Debug)]
struct Known {
    t: i64,
    last: u64,
    high: u64,
}

#[derive(Debug, Default)]
pub struct Intake {
    known: Vec<Known>,
    /// 近一分钟里记下的：(记下的时刻, token 数)，按时刻先后
    log: VecDeque<(i64, u64)>,
    started: i64,
    last_at: i64,
}

fn total_of(point: &RatePoint) -> u64 {
    point.input + point.cache_read + point.cache_write + point.output
}

impl Intake {
    /// 每汇总一轮调一次。`rate` 是最近一小时每分钟的用量，`per_second` 是最近一分钟每秒的用量，
    /// 都按请求发生的时刻分。
    pub fn update(&mut self, rate: &[RatePoint], per_second: &[RatePoint], now: i64) -> Recorded {
        if self.started == 0 {
            // 第一轮：之前记下的不知道是什么时候记的，照它发生的时刻算
            self.started = now;
            self.log = per_second.iter().map(|point| (point.t, total_of(point))).filter(|(_, tokens)| *tokens > 0).collect();
            self.known = rate.iter().map(|point| Known { t: point.t, last: total_of(point), high: total_of(point) }).collect();
        } else {
            let grace = if now - self.started < WARMUP { WARMUP_GRACE } else { GRACE };
            let mut grown = 0u64;
            // 少掉的：来源把请求的时刻改到了别的分钟，那边多出来的就不是新记下的
            let mut moved = 0u64;
            let mut known = Vec::with_capacity(rate.len());
            for point in rate {
                let total = total_of(point);
                let before = self.known.iter().find(|known| known.t == point.t).copied().unwrap_or(Known { t: point.t, last: 0, high: 0 });
                // 只认超过见过的最大值的部分：来源一时读不到又回来，不算再记一遍
                if total > before.high && now - point.t <= grace {
                    grown += total - before.high;
                }
                moved += before.last.saturating_sub(total);
                known.push(Known { t: point.t, last: total, high: before.high.max(total) });
            }
            self.known = known;
            let fresh = grown.saturating_sub(moved);
            if fresh > 0 {
                self.log.push_back((now, fresh));
                self.last_at = now;
            }
        }
        while self.log.front().is_some_and(|(at, _)| *at <= now - WINDOW) {
            self.log.pop_front();
        }
        Recorded { minute: self.log.iter().map(|(_, tokens)| tokens).sum(), last_at: self.last_at }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeutil::SECOND;

    /// 整分钟
    const NOW: i64 = 1_800_000_000_000;

    /// 以 `now` 所在的那一格结尾的 60 格，`events` 是 (请求发生的时刻, token 数)。
    fn series(now: i64, step: i64, events: &[(i64, u64)]) -> Vec<RatePoint> {
        let end = now.div_euclid(step) * step;
        let from = end - 59 * step;
        let mut points: Vec<RatePoint> = (0..60).map(|index| RatePoint { t: from + index * step, ..Default::default() }).collect();
        for (ts, tokens) in events {
            if *ts >= from && *ts < end + step {
                points[((ts - from) / step) as usize].input += tokens;
            }
        }
        points
    }

    fn update(intake: &mut Intake, now: i64, events: &[(i64, u64)]) -> Recorded {
        intake.update(&series(now, MINUTE, events), &series(now, SECOND, events), now)
    }

    #[test]
    fn first_round_counts_what_happened_in_the_last_minute() {
        let mut intake = Intake::default();
        let events = [(NOW - 20 * SECOND, 300), (NOW - 59 * SECOND, 500), (NOW - 3 * MINUTE, 9000)];
        assert_eq!(update(&mut intake, NOW, &events), Recorded { minute: 800, last_at: 0 });
        // 半分钟后没有新记录：59 秒前的那次已经出了窗口
        assert_eq!(update(&mut intake, NOW + 30 * SECOND, &events).minute, 300);
        assert_eq!(update(&mut intake, NOW + 61 * SECOND, &events), Recorded { minute: 0, last_at: 0 });
    }

    #[test]
    fn a_request_counts_when_it_is_recorded_not_when_it_happened() {
        let mut intake = Intake::default();
        update(&mut intake, NOW, &[]);
        let now = NOW + 2 * MINUTE;
        // 两分半钟前的请求现在才记到
        let late = [(now - 150 * SECOND, 4000)];
        assert_eq!(update(&mut intake, now, &late), Recorded { minute: 4000, last_at: now });
        assert_eq!(update(&mut intake, now + 59 * SECOND, &late).minute, 4000);
        assert_eq!(update(&mut intake, now + 60 * SECOND, &late), Recorded { minute: 0, last_at: now });
    }

    #[test]
    fn records_made_up_long_afterwards_do_not_count_as_fresh() {
        let mut intake = Intake::default();
        update(&mut intake, NOW, &[]);
        let now = NOW + 2 * MINUTE;
        // 插件掉线二十分钟后补回来的记录
        let old = [(now - 20 * MINUTE, 7000), (now - 6 * MINUTE, 500)];
        assert_eq!(update(&mut intake, now, &old), Recorded { minute: 0, last_at: 0 });
        // 之后这几分钟再有新记录也一样，而刚发生的照常算
        let more = [(now - 20 * MINUTE, 7000), (now - 6 * MINUTE, 900), (now - 3 * SECOND, 60)];
        assert_eq!(update(&mut intake, now + SECOND, &more), Recorded { minute: 60, last_at: now + SECOND });
    }

    #[test]
    fn right_after_start_only_the_last_two_minutes_count() {
        let mut intake = Intake::default();
        update(&mut intake, NOW, &[]);
        let now = NOW + 30 * SECOND;
        // 启动半分钟后，来源把停机期间的记录补回来
        let backlog = [(now - 4 * MINUTE, 9000), (now - 70 * SECOND, 600), (now - 5 * SECOND, 200)];
        assert_eq!(update(&mut intake, now, &backlog).minute, 800);
    }

    #[test]
    fn a_request_whose_time_moves_is_not_recorded_twice() {
        let mut intake = Intake::default();
        update(&mut intake, NOW, &[]);
        let now = NOW + 2 * MINUTE;
        assert_eq!(update(&mut intake, now, &[(now - 90 * SECOND, 1000)]).minute, 1000);
        // 来源把这次请求的时刻改到了下一分钟，另外新记了 300
        let moved = [(now - 20 * SECOND, 1000), (now + 2 * SECOND, 300)];
        assert_eq!(update(&mut intake, now + 5 * SECOND, &moved).minute, 1300);
    }

    #[test]
    fn a_source_that_drops_out_and_comes_back_is_not_recorded_again() {
        let mut intake = Intake::default();
        update(&mut intake, NOW, &[]);
        let now = NOW + 2 * MINUTE;
        let events = [(now - 10 * SECOND, 5000)];
        assert_eq!(update(&mut intake, now, &events).minute, 5000);
        assert_eq!(update(&mut intake, now + SECOND, &[]).minute, 5000);
        assert_eq!(update(&mut intake, now + 2 * SECOND, &events), Recorded { minute: 5000, last_at: now });
    }

    #[test]
    fn new_minutes_and_growing_minutes_both_count() {
        let mut intake = Intake::default();
        let before = [(NOW - 30 * SECOND, 100)];
        assert_eq!(update(&mut intake, NOW, &before).minute, 100);
        let now = NOW + 90 * SECOND;
        let after = [(NOW - 30 * SECOND, 100), (NOW - 20 * SECOND, 40), (NOW + 70 * SECOND, 7)];
        assert_eq!(update(&mut intake, now, &after), Recorded { minute: 47, last_at: now });
    }
}
