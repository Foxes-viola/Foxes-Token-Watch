//! Native Agent Host activity supplements Cursor's sometimes stale composer
//! flags. Read log metadata only; never count log lines as model requests.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{Local, NaiveDateTime};
use serde::Deserialize;

use super::{tail_lines, FileSig};
use crate::model::Phase;
use crate::timeutil::DAY;

#[derive(Clone, Debug)]
pub(super) struct Activity {
    pub id: String,
    pub started: i64,
    pub ts: i64,
    pub active: bool,
    pub phase: Phase,
    pub tool: String,
}

#[derive(Default, Debug)]
struct Log {
    sig: FileSig,
    offset: u64,
    activities: HashMap<String, Activity>,
    pending: HashMap<(String, String), String>,
}

#[derive(Default, Debug)]
pub(super) struct Logs {
    files: HashMap<PathBuf, Log>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    #[serde(default, alias = "conversation_id", alias = "sessionId")]
    conversation_id: String,
    #[serde(default)]
    tool_call_id: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    action_case: String,
}

impl Log {
    fn line(&mut self, line: &[u8]) {
        let Ok(line) = std::str::from_utf8(line) else {
            return;
        };
        let Some((header, body)) = line.split_once("] ") else {
            return;
        };
        let Some((message, json)) = body.split_once(" {") else {
            return;
        };
        let Ok(meta) = serde_json::from_str::<Metadata>(&format!("{{{json}")) else {
            return;
        };
        let Some(date) = header.get(..23) else { return };
        let Some(ts) = NaiveDateTime::parse_from_str(date, "%Y-%m-%d %H:%M:%S%.3f")
            .ok()
            .and_then(|date| date.and_local_timezone(Local).earliest())
            .map(|date| date.timestamp_millis())
        else {
            return;
        };

        let selected = message == "Selected Agent Host turn runtime";
        let terminal = message == "Turn terminal blob reads" || message == "Cancel action received";
        let step = message == "Running step";
        let start = message == "nal.tool_call.start";
        let end = matches!(
            message,
            "nal.tool_call.success" | "nal.tool_call.error" | "nal.tool_call.failure"
        );
        if !(selected || terminal || step || start || end) {
            return;
        }
        // Unlabelled events can be associated only while one turn is active in
        // this host log. Do not guess when a window runs concurrent sessions.
        let id = if !meta.conversation_id.is_empty() {
            meta.conversation_id
        } else {
            let mut active = self.activities.values().filter(|a| a.active);
            let Some(first) = active.next() else { return };
            if active.next().is_some() {
                return;
            }
            first.id.clone()
        };
        if id.len() > 128 || id.chars().any(char::is_control) {
            return;
        }
        if selected {
            self.pending.retain(|(session, _), _| session != &id);
            let started = if meta.action_case == "resumeAction" {
                self.activities.get(&id).map_or(ts, |a| a.started)
            } else {
                ts
            };
            self.activities.insert(
                id.clone(),
                Activity {
                    id,
                    started,
                    ts,
                    active: true,
                    phase: Phase::Requesting,
                    tool: String::new(),
                },
            );
            return;
        }
        let Some(activity) = self.activities.get_mut(&id) else {
            return;
        };
        if ts < activity.ts {
            return;
        }
        activity.ts = ts;
        if terminal {
            activity.active = false;
            activity.phase = Phase::Idle;
            activity.tool.clear();
            self.pending.retain(|(session, _), _| session != &id);
        } else {
            if start {
                self.pending
                    .insert((id.clone(), meta.tool_call_id), meta.tool_name);
            } else if end {
                self.pending.remove(&(id.clone(), meta.tool_call_id));
            }
            activity.active = true;
            if let Some((_, tool)) = self.pending.iter().find(|((session, _), _)| session == &id) {
                activity.phase = Phase::Tool;
                activity.tool.clone_from(tool);
            } else {
                activity.phase = Phase::Generating;
                activity.tool.clear();
            }
        }
    }
}

impl Logs {
    pub fn refresh(&mut self, root: &Path, now: i64) -> io::Result<Vec<Activity>> {
        let mut present = HashSet::new();
        let Ok(startups) = fs::read_dir(root) else {
            return Ok(Vec::new());
        };
        for startup in startups
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        {
            let Ok(windows) = fs::read_dir(startup.path()) else {
                continue;
            };
            for window in windows
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            {
                let dir = window.path().join("exthost/anysphere.cursor-agent-host");
                let Ok(files) = fs::read_dir(dir) else {
                    continue;
                };
                for file in files.flatten() {
                    let name = file.file_name();
                    let name = name.to_string_lossy();
                    if !(name == "Cursor Agent Host.log"
                        || (name.starts_with("Cursor Agent Host.") && name.ends_with(".log")))
                    {
                        continue;
                    }
                    let path = file.path();
                    let Some(sig) = FileSig::of(&path) else {
                        continue;
                    };
                    if now - sig.mtime_ms > 3 * DAY {
                        continue;
                    }
                    present.insert(path.clone());
                    let log = self.files.entry(path).or_default();
                    if sig == log.sig {
                        continue;
                    }
                    if sig.size < log.offset
                        || (sig.size == log.sig.size && sig.mtime_ms != log.sig.mtime_ms)
                    {
                        *log = Log::default();
                    }
                    let mut offset = log.offset;
                    tail_lines(&file.path(), &mut offset, |line| log.line(line))?;
                    log.offset = offset;
                    log.sig = sig;
                }
            }
        }
        self.files.retain(|path, _| present.contains(path));
        let mut latest: HashMap<String, Activity> = HashMap::new();
        for log in self.files.values() {
            for activity in log.activities.values() {
                if latest
                    .get(&activity.id)
                    .is_none_or(|old| old.ts < activity.ts)
                {
                    latest.insert(activity.id.clone(), activity.clone());
                }
            }
        }
        Ok(latest.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_native_turn_through_tools_until_terminal_event() {
        let mut log = Log::default();
        for line in [
            r#"2026-11-11 19:16:54.681 [info] Selected Agent Host turn runtime {"conversationId":"c","actionCase":"userMessageAction"}"#,
            r#"2026-11-11 21:36:48.285 [info] Running step {}"#,
            r#"2026-11-11 21:36:51.150 [info] nal.tool_call.start {"toolCallId":"t","toolName":"await"}"#,
        ] {
            log.line(line.as_bytes());
        }
        let active = &log.activities["c"];
        assert!(active.active);
        assert!(active.ts - active.started > 2 * 60 * 60 * 1000);
        assert_eq!(active.phase, Phase::Tool);
        assert_eq!(active.tool, "await");
        log.line(br#"2026-11-11 21:37:00.000 [info] nal.tool_call.success {"toolCallId":"t"}"#);
        assert_eq!(log.activities["c"].phase, Phase::Generating);
        log.line(br#"2026-11-11 21:38:00.000 [info] Turn terminal blob reads {"sessionId":"c"}"#);
        assert!(!log.activities["c"].active);
    }

    #[test]
    fn does_not_assign_unlabelled_events_to_concurrent_sessions() {
        let mut log = Log::default();
        log.line(br#"2026-11-11 19:00:00.000 [info] Selected Agent Host turn runtime {"conversationId":"a"}"#);
        log.line(br#"2026-11-11 19:00:01.000 [info] Selected Agent Host turn runtime {"conversationId":"b"}"#);
        log.line(br#"2026-11-11 19:00:02.000 [info] nal.tool_call.start {"toolCallId":"x","toolName":"shell"}"#);
        assert!(log.pending.is_empty());
        assert_eq!(log.activities["a"].phase, Phase::Requesting);
        assert_eq!(log.activities["b"].phase, Phase::Requesting);
    }
}
