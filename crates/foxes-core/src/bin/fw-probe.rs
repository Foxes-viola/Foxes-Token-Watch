//! 开发用的命令行：扫一遍本机数据，打印各家的汇总数字，用来和参照脚本核对。
//!
//! 默认只输出数字和状态，不输出标题、目录等内容。
//!
//! ```text
//! fw-probe                    汇总
//! fw-probe --files <agent>    某一家逐文件的数字（claude / codex / deepseek / antigravity / cursor）
//! fw-probe --baishitong <秒>  先经百事通插件在本机的转发口读这么久它的对话，再按别的参数输出（会用掉一次自动登录）
//! fw-probe --watch <次数>     每秒刷新一次，看实时状态、刷新耗时、近一分钟记下的用量，以及各分钟的合计比上一轮变了多少
//! fw-probe --json <文件>      把完整快照写成 JSON（含标题和目录，只用于本机调试）
//! fw-probe --history <文件>   像软件那样带上用量历史：读这个文件，把扫到的记进去再写回（拿副本来试，别指向软件正在用的那份）
//! fw-probe --carried <文件>   跟着程序走的那份历史（和 --history 一起用）
//! fw-probe --no-sources       不读本机各家的数据目录，像一台什么都没装的电脑：看历史能顶上多少
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use foxes_core::aggregate::{analyze, Analysis, Totals};
use foxes_core::history::History;
use foxes_core::intake::Intake;
use foxes_core::model::Agent;
use foxes_core::sources::Roots;
use foxes_core::store::Store;
use foxes_core::timeutil::now_ms;

fn group(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn totals_line(label: &str, totals: &Totals) -> String {
    let prompt = totals.input + totals.cache_read + totals.cache_write;
    let hit = if prompt > 0 { totals.cache_read as f64 * 100.0 / prompt as f64 } else { 0.0 };
    format!(
        "{label:<18} total {:>16}  in {:>14}  cache_read {:>16}  cache_write {:>13}  out {:>13}  think {:>12} (est {:>12})  req {:>7}  hit {:>5.1}%",
        group(totals.total),
        group(totals.input),
        group(totals.cache_read),
        group(totals.cache_write),
        group(totals.output),
        group(totals.reasoning),
        group(totals.reasoning_estimated),
        totals.requests,
        hit
    )
}

fn short(text: &str) -> &str {
    let end = text.char_indices().nth(8).map_or(text.len(), |(index, _)| index);
    &text[..end]
}

fn print_summary(store: &Store, analysis: &Analysis, now: i64) {
    let snapshot = &analysis.snapshot;
    let scan = &snapshot.scan;
    println!(
        "scan: files {}  bytes {}  events {}  failing {}  first {} ms  last {} ms (parsed {})",
        scan.files,
        group(scan.bytes),
        scan.events,
        scan.failing,
        scan.first_scan_ms,
        scan.scan_ms,
        scan.parsed
    );
    let processes = store.processes();
    println!(
        "processes: available {}  count {}  claude-registry {} ({} entries, {} alive)  antigravity-hubs {}",
        processes.available(),
        processes.len(),
        store.claude_registry(),
        store.claude_procs().len(),
        store.claude_procs().iter().filter(|proc| proc.alive).count(),
        store.antigravity_hubs().len()
    );
    for (label, needles) in [("claude", &["claude"][..]), ("codex", &["codex"][..]), ("deepseek", &["deepseek", "dsh"][..]), ("antigravity", &["antigravity"][..])] {
        println!("  process names ~{label}: {:?}", processes.names_matching(needles));
    }
    let mut statuses: Vec<String> = store.claude_procs().iter().map(|proc| format!("{}:{}:{}s", proc.status, if proc.alive { "alive" } else { "dead" }, (now - proc.status_updated_ms) / 1000)).collect();
    statuses.sort();
    println!("  claude registry (status:alive:since): {statuses:?}");

    for range in &snapshot.ranges {
        println!();
        println!("== {} ==  sessions {}  series {} x {}", range.id, range.sessions, range.series.len(), range.unit);
        println!("{}", totals_line("ALL", &range.totals));
        if let Some(previous) = &range.previous {
            println!("{}", totals_line("(previous)", previous));
        }
        for agent in &range.by_agent {
            println!("{}  sessions {}", totals_line(agent.agent.label(), &agent.totals), agent.sessions);
        }
        if range.id == "all" {
            println!("-- models");
            for model in &range.by_model {
                let agents: Vec<&str> = model.agents.iter().map(|part| part.agent.id()).collect();
                println!("{}", totals_line(&format!("{}/{}", agents.join("+"), model.model).chars().take(40).collect::<String>(), &model.totals));
            }
            println!("-- projects: {}", range.by_project.len());
        }
    }

    let live = &snapshot.live;
    println!();
    println!("== live ==  running {} (+{} sub-agents)  phases {:?}", live.running, live.running_sub_agents, live.phases);
    for session in &live.sessions {
        println!(
            "  {:<12} {:<9} running={:<5} sub={:<5} phase={:<10?} since {:>5}s  idle {:>6}s  tool={:<14} turn-tokens {:>12}  ctx {}/{}  subs {}/{}",
            session.agent.id(),
            short(&session.id),
            session.running,
            session.sub_agent,
            session.phase,
            (now - session.phase_since) / 1000,
            (now - session.last_active) / 1000,
            session.tool.chars().take(14).collect::<String>(),
            group(session.turn.total),
            session.context_used,
            session.context_window,
            session.running_sub_agents,
            session.sub_agents
        );
    }
    for agent in &live.agents {
        let net = &agent.net;
        println!(
            "  net {:<12} detected={:<5} alive={:<12?} running {}  files {:>5}  status {:?}  latency {} ms  ttft {} ms  samples {}  req {}  retries {}  errors {}  last-code {:?}",
            agent.agent.id(),
            agent.detected,
            agent.process_alive,
            agent.running,
            agent.files,
            net.status,
            net.latency_ms,
            net.ttft_ms,
            net.samples,
            net.requests,
            net.retries,
            net.errors,
            net.last_code
        );
    }
    println!("  net overall: {:?}  events(24h) {}  finished(5m) {}", live.net.status, live.net_events.len(), live.finished.len());
    for limits in &live.rate_limits {
        println!("  rate-limit {}: plan {:?} primary {:?} secondary {:?}", limits.agent.id(), limits.plan, limits.primary, limits.secondary);
    }
    println!("  sessions total {}  (plugin {})", analysis.sessions.len(), analysis.sessions.iter().filter(|session| session.plugin).count());
    if let Some(feed) = &snapshot.plugin.baishitong {
        println!(
            "  baishitong: {:?}  sessions {} (busy {}, unread {})  last ok {}s ago  retry in {}s",
            feed.link,
            feed.sessions,
            feed.busy,
            feed.pending,
            if feed.ok_at > 0 { (now - feed.ok_at) / 1000 } else { -1 },
            if feed.retry_at > now { (feed.retry_at - now) / 1000 } else { 0 }
        );
    }
}

fn print_files(store: &Store, agent: Agent) {
    let mut entries: Vec<_> = store.entries().filter(|entry| entry.agent == agent).collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut sum = Totals::default();
    for entry in entries {
        let mut totals = Totals::default();
        for event in &entry.events {
            totals.add_event(event);
        }
        sum.add(&totals);
        let stem = entry.path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
        // 插件卡片的文件都叫 process.json，用会话 id 区分
        let stem = if agent == Agent::Deepseek || entry.plugin { entry.session.id.clone() } else { stem };
        let tail: String = stem.chars().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
        println!(
            "{tail:<12} sub={:<5} turns {:>4} open={:<5} corrupt={:<5} err {}  {}",
            entry.session.sub_agent,
            entry.live.turns,
            entry.live.turn_open,
            entry.corrupt,
            entry.errors,
            totals_line("", &totals)
        );
    }
    println!("{}", totals_line("SUM (no dedupe)", &sum));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value_of = |flag: &str| args.iter().position(|arg| arg == flag).and_then(|index| args.get(index + 1)).cloned();

    let roots = if args.iter().any(|arg| arg == "--no-sources") { Roots::default() } else { Roots::detect() };
    println!(
        "roots: claude {}  codex {}  deepseek {}  antigravity {}  kiro {}  cursor {}  webmcp {}",
        roots.claude.len(),
        roots.codex.len(),
        roots.deepseek.len(),
        roots.antigravity.len(),
        roots.kiro.len(),
        roots.cursor.len(),
        roots.web_mcp.len()
    );
    let mut store = Store::new(roots);
    // 用量历史要赶在第一次扫描之前读进来：插件卡片以前记下的请求靠它接着算
    let history_paths = [value_of("--history"), value_of("--carried")];
    let mut history = history_paths[0].clone().map(|path| History::new(Some(PathBuf::from(path))).carry(history_paths[1].clone().map(PathBuf::from)));
    if let Some(history) = history.as_mut() {
        let started = Instant::now();
        let primed = history.prime(&mut store);
        println!("history: read {} in {} ms", if primed { "ok" } else { "FAILED" }, started.elapsed().as_millis());
    }
    // 百事通插件的对话要经它在本机的转发口读，默认不读；`--baishitong <秒数>` 读这么久再往下走
    let baishitong = value_of("--baishitong").and_then(|seconds| seconds.parse::<u64>().ok());
    if baishitong.is_some() {
        store.watch_baishitong();
    }
    let started = Instant::now();
    store.refresh();
    println!("first scan: {} ms", started.elapsed().as_millis());
    if let Some(seconds) = baishitong {
        let until = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < until {
            std::thread::sleep(Duration::from_secs(1));
            store.refresh();
        }
    }
    for file in store.source_files() {
        if let Some(error) = &file.last_error {
            eprintln!("{} read failed (retry {}): {error}", file.agent.id(), file.errors);
        }
    }
    if let Some(history) = history.as_mut() {
        // 第一次同步：扫到的记进历史，扫不到的由历史顶上，有改动就写盘
        let started = Instant::now();
        let changed = history.sync(&mut store, now_ms());
        let info = history.info();
        println!(
            "history: sync {} ms (restored changed: {changed})  sessions recorded here {}  other computers {}  sessions restored from them {}  error {:?}",
            started.elapsed().as_millis(),
            info.sessions,
            info.devices,
            info.elsewhere,
            history.error()
        );
        let restored = store.entries().filter(|entry| entry.gone).count();
        println!("history: sessions restored in total {restored}  ({} of them from other computers)", store.entries().filter(|entry| entry.elsewhere).count());
        for path in history_paths.iter().flatten() {
            println!("history file: {} bytes  {path}", group(std::fs::metadata(path).map_or(0, |meta| meta.len())));
        }
    }

    if let Some(name) = value_of("--files") {
        match Agent::ALL.into_iter().find(|agent| agent.id() == name) {
            Some(agent) => print_files(&store, agent),
            None => eprintln!("unknown agent: {name}"),
        }
        return;
    }

    let now = now_ms();
    let started = Instant::now();
    let analysis = analyze(&store, now);
    println!("analyze: {} ms", started.elapsed().as_millis());

    if let Some(path) = value_of("--json") {
        match serde_json::to_vec(&analysis.snapshot) {
            Ok(json) => {
                let size = json.len();
                match std::fs::write(&path, json) {
                    Ok(()) => println!("snapshot json: {size} bytes written"),
                    Err(error) => eprintln!("write failed: {error}"),
                }
            }
            Err(error) => eprintln!("serialize failed: {error}"),
        }
        return;
    }

    print_summary(&store, &analysis, now);

    if let Some(count) = value_of("--watch").and_then(|count| count.parse::<u32>().ok()) {
        println!();
        let mut intake = Intake::default();
        let mut minutes: Vec<(i64, u64)> = Vec::new();
        for _ in 0..count {
            std::thread::sleep(Duration::from_secs(1));
            let started = Instant::now();
            let mut changed = store.refresh();
            if let Some(history) = history.as_mut() {
                changed |= history.sync(&mut store, now_ms());
            }
            let scan_ms = started.elapsed().as_micros() as f64 / 1000.0;
            let started = Instant::now();
            let now = now_ms();
            let analysis = analyze(&store, now);
            let analyze_ms = started.elapsed().as_micros() as f64 / 1000.0;
            let live = &analysis.snapshot.live;
            let recorded = intake.update(&live.rate, &live.per_second, now);
            // 各分钟的合计比上一轮变了多少，写成「几分钟前:变化量」
            let current: Vec<(i64, u64)> = live.rate.iter().map(|point| (point.t, point.input + point.cache_read + point.cache_write + point.output)).collect();
            let moves: Vec<String> = current
                .iter()
                .filter_map(|&(t, total)| {
                    let before = minutes.iter().find(|(known, _)| *known == t).map_or(0, |(_, total)| *total);
                    (!minutes.is_empty() && total != before).then(|| format!("{}m:{:+}", (now - t) / 60_000, total as i64 - before as i64))
                })
                .collect();
            minutes = current;
            let since = if recorded.last_at > 0 { format!("{}s ago", (now - recorded.last_at) / 1000) } else { "never".to_owned() };
            let states: Vec<String> = live.sessions.iter().filter(|session| session.running).map(|session| format!("{}:{:?}", session.agent.id(), session.phase)).collect();
            let feed = analysis.snapshot.plugin.baishitong.as_ref().map_or(String::new(), |feed| format!("  feed {:?} unread {}/{}", feed.link, feed.pending, feed.sessions));
            println!(
                "scan {scan_ms:>7.2} ms  analyze {analyze_ms:>6.2} ms  changed={changed:<5} parsed {}  running {} (+{}){feed}  recorded(1m) {} last {since}  minutes {moves:?}  {states:?}",
                store.stats().parsed,
                live.running,
                live.running_sub_agents,
                group(recorded.minute)
            );
        }
    }
}
