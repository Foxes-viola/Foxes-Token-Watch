//! The gateway extension's executor runs the remote assistant's tools on this
//! machine and notes every job in an output channel, which Cursor keeps under
//! `logs/<startup>/window<n>/exthost/output_logging_<time>/<n>-<name>网关 agent.log`:
//! `[10:26:13] 运行 cargo test（337 ms）`. A line names no conversation, but it
//! is the only trace on this machine of a run whose process rows never come back.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Local, NaiveDate, TimeZone};

use super::{list_dirs, tail_lines, FileSig};
use crate::timeutil::{DAY, HOUR};

const CHECK_EVERY_MS: i64 = 3000;
/// Channels untouched for this long are no longer read.
const KEEP_MS: i64 = 3 * DAY;

/// The labels the extension gives the executor's tools (`TOOL_LABELS` in its
/// extension.js, plus `notes`, which it logs by name). Other lines are not tool runs.
const LABELS: [&str; 16] = [
    "查看工作区", "了解项目", "列目录", "读取", "查看图片", "写入", "修改", "删除",
    "搜索", "查诊断", "运行", "启动终端", "读终端", "输入终端", "停止终端", "notes",
];

/// One job the executor ran.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Job {
    pub ts: i64,
    pub ms: i64,
    /// The extension's label for the tool, e.g. `运行`, `读取`.
    pub label: &'static str,
    /// The file, command or query as the line shows it (cut short).
    pub target: String,
    pub ok: bool,
}

#[derive(Debug, Default)]
struct Channel {
    sig: Option<FileSig>,
    offset: u64,
    /// Local midnight of the day the lines are on, and the last time of day read.
    day: i64,
    last: i64,
    jobs: Vec<Job>,
}

#[derive(Debug, Default)]
pub(super) struct Jobs {
    channels: HashMap<PathBuf, Channel>,
    checked: i64,
}

/// Time of day in a channel's `[…]` stamp: `10:26:13`, `10:26:13 PM`, `下午10:26:13`.
fn time_of_day(clock: &str) -> Option<i64> {
    let digits: String = clock.chars().filter(|ch| ch.is_ascii_digit() || *ch == ':').collect();
    let mut parts = digits.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (parts.next()??, parts.next()??, parts.next()??);
    let pm = ["PM", "pm", "下午", "晚上"].iter().any(|mark| clock.contains(mark));
    let am = ["AM", "am", "上午", "凌晨", "早上"].iter().any(|mark| clock.contains(mark));
    let hour = match hour {
        12 if am => 0,
        hour if pm && hour < 12 => hour + 12,
        hour => hour,
    };
    (hour < 24 && minute < 60 && second < 61).then_some(((hour * 60 + minute) * 60 + second) * 1000)
}

/// `运行 cargo test（337 ms）` or `修改 a.rs ✗ not found（3 ms）` → label, target, ms, ok.
fn job(body: &str) -> Option<(&'static str, &str, i64, bool)> {
    let body = body.trim_end().strip_suffix(" ms）")?;
    let open = body.rfind('（')?;
    let ms = body[open + '（'.len_utf8()..].trim().parse().ok()?;
    let body = &body[..open];
    let (label, target) = body.split_once(' ').unwrap_or((body, ""));
    let label = LABELS.iter().copied().find(|known| *known == label)?;
    let (target, ok) = match target.find(" ✗ ") {
        Some(cut) => (&target[..cut], false),
        None => (target, true),
    };
    Some((label, target.trim(), ms, ok))
}

/// Local midnight and time of day of a channel folder (`output_logging_20261113T102608_…`).
fn opened(dir: &Path) -> Option<(i64, i64)> {
    let name = dir.file_name()?.to_str()?;
    let stamp = name.strip_prefix("output_logging_")?.get(..15)?;
    let date = NaiveDate::parse_from_str(stamp.get(..8)?, "%Y%m%d").ok()?;
    let midnight = Local.from_local_datetime(&date.and_hms_opt(0, 0, 0)?).earliest()?.timestamp_millis();
    let clock: i64 = stamp.get(9..)?.parse().ok()?;
    Some((midnight, ((clock / 10_000 * 60 + clock / 100 % 100) * 60 + clock % 100) * 1000))
}

impl Channel {
    fn line(&mut self, line: &[u8]) {
        let Ok(line) = std::str::from_utf8(line) else { return };
        let Some((clock, body)) = line.strip_prefix('[').and_then(|rest| rest.split_once("] ")) else { return };
        let Some(at) = time_of_day(clock) else { return };
        // Stamps carry no date: a time far before the last one is the next day.
        if at + 12 * HOUR < self.last {
            self.day += DAY;
        }
        self.last = at;
        if let Some((label, target, ms, ok)) = job(body) {
            let target = target.chars().filter(|ch| !ch.is_control()).take(160).collect();
            self.jobs.push(Job { ts: self.day + at, ms, label, target, ok });
        }
    }
}

impl Jobs {
    /// Read what the channels under Cursor's `logs` gained; whether anything changed.
    pub(super) fn refresh(&mut self, logs: &Path, now: i64) -> bool {
        // The folders are walked at most every few seconds.
        if (0..CHECK_EVERY_MS).contains(&(now - self.checked)) {
            return false;
        }
        self.checked = now;
        let mut present = HashSet::new();
        let mut changed = false;
        for window in list_dirs(logs).iter().flat_map(|startup| list_dirs(startup)) {
            for output in list_dirs(&window.join("exthost")) {
                let Some((midnight, opened_at)) = opened(&output) else { continue };
                let Ok(files) = fs::read_dir(&output) else { continue };
                for file in files.flatten() {
                    if !file.file_name().to_string_lossy().ends_with("网关 agent.log") {
                        continue;
                    }
                    let path = file.path();
                    let Some(sig) = FileSig::of(&path) else { continue };
                    if now - sig.mtime_ms > KEEP_MS {
                        continue;
                    }
                    present.insert(path.clone());
                    let fresh = || Channel { day: midnight, last: opened_at, ..Channel::default() };
                    let channel = self.channels.entry(path.clone()).or_insert_with(fresh);
                    if channel.sig == Some(sig) {
                        continue;
                    }
                    if channel.sig.is_some_and(|old| sig.size < old.size) {
                        *channel = fresh();
                    }
                    let (before, mut offset) = (channel.jobs.len(), channel.offset);
                    let read = tail_lines(&path, &mut offset, |line| channel.line(line));
                    channel.offset = offset;
                    if read.is_ok() {
                        channel.sig = Some(sig);
                    }
                    changed |= channel.jobs.len() != before;
                }
            }
        }
        let before = self.channels.len();
        self.channels.retain(|path, _| present.contains(path));
        changed || self.channels.len() != before
    }

    pub(super) fn all(&self) -> impl Iterator<Item = &Job> {
        self.channels.values().flat_map(|channel| channel.jobs.iter())
    }
}

#[cfg(test)]
impl Jobs {
    pub(super) fn of(jobs: Vec<Job>) -> Jobs {
        Jobs { channels: HashMap::from([(PathBuf::from("test"), Channel { jobs, ..Channel::default() })]), checked: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tool_runs_and_rolls_over_midnight() {
        let mut channel = Channel { day: 1_000 * DAY, last: 23 * HOUR, ..Channel::default() };
        for line in [
            "[23:59:58] conversationDelta card-1794537665786-6cbc029d（21 ms）",
            "[23:59:59] 运行 Get-ChildItem '迭代2.1'（247 ms）",
            "    at t (vscode-file://vscode-app/c:/x.js:1:2)",
            "[00:00:03] 修改 .claude/worktrees/21-G-gpu/a.gd ✗ old text not found（3 ms）",
            "[00:00:04] MCP 等用户消息 （12 ms）",
            "[12:00:05 AM] 读取 迭代2.1/H-集成与总验收.md（1 ms）",
        ] {
            channel.line(line.as_bytes());
        }
        let jobs: Vec<_> = channel.jobs.iter().map(|job| (job.ts - 1_000 * DAY, job.label, job.target.as_str(), job.ms, job.ok)).collect();
        assert_eq!(
            jobs,
            vec![
                (DAY - 1000, "运行", "Get-ChildItem '迭代2.1'", 247, true),
                (DAY + 3000, "修改", ".claude/worktrees/21-G-gpu/a.gd", 3, false),
                (DAY + 5000, "读取", "迭代2.1/H-集成与总验收.md", 1, true),
            ]
        );
    }

    #[test]
    fn dates_a_channel_by_its_folder() {
        let (midnight, at) = opened(Path::new("logs/x/window2/exthost/output_logging_20261113T102608_6c407712")).unwrap();
        assert_eq!(at, ((10 * 60 + 26) * 60 + 8) * 1000);
        let local = Local.timestamp_millis_opt(midnight).unwrap();
        assert_eq!(local.format("%Y-%m-%d %H:%M").to_string(), "2026-11-13 00:00");
    }
}
