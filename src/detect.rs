//! Detectors. Each rule id is documented in the README; thresholds are flags.

use std::collections::{BTreeMap, HashMap};

use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::session::Session;

pub const FINDING_SCHEMA: &str = "rf.finding/1";

#[derive(Args, Serialize, Deserialize, Clone, Debug)]
pub struct Thresholds {
    /// file-reread: reads of one file in one session, counting a read only
    /// when it repeats a range (or whole-file read) already made
    #[arg(long, default_value_t = 5)]
    pub reread_min: u64,
    /// repeated-command: runs of one identical shell command in one session
    #[arg(long, default_value_t = 4)]
    pub repeat_cmd_min: u64,
    /// large-tool-output: characters returned by one tool result
    #[arg(long, default_value_t = 100_000)]
    pub large_output_chars: u64,
    /// tool-error-streak: consecutive tool errors
    #[arg(long, default_value_t = 4)]
    pub error_streak_min: u64,
    /// sleep-poll-loop: sleep commands plus polling calls in one session
    #[arg(long, default_value_t = 5)]
    pub sleep_min: u64,
    /// low-cache-read-ratio: minimum cache reads / prompt tokens
    #[arg(long, default_value_t = 0.5)]
    pub cache_read_min_ratio: f64,
    /// cache rules only apply to sessions with at least this many prompt tokens
    #[arg(long, default_value_t = 1_000_000)]
    pub cache_min_prompt_tokens: u64,
    /// cache-churn: cache writes / (cache writes + cache reads)
    #[arg(long, default_value_t = 0.25)]
    pub cache_churn_ratio: f64,
    /// frequent-compaction: compactions in one session
    #[arg(long, default_value_t = 3)]
    pub compactions_min: u64,
    /// near-context-window: prompt tokens / context window of one request
    #[arg(long, default_value_t = 0.9)]
    pub context_ratio: f64,
    /// long-context-tier: share of a session's priced requests billed at the long-context tier
    #[arg(long, default_value_t = 0.1)]
    pub long_context_share: f64,
    /// long-idle-gap: minutes without any record inside a session
    #[arg(long, default_value_t = 30)]
    pub idle_gap_minutes: u64,
    /// subagent-fanout: direct child sessions of one session
    #[arg(long, default_value_t = 5)]
    pub fanout_min: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            reread_min: 5,
            repeat_cmd_min: 4,
            large_output_chars: 100_000,
            error_streak_min: 4,
            sleep_min: 5,
            cache_read_min_ratio: 0.5,
            cache_min_prompt_tokens: 1_000_000,
            cache_churn_ratio: 0.25,
            compactions_min: 3,
            context_ratio: 0.9,
            long_context_share: 0.1,
            idle_gap_minutes: 30,
            fanout_min: 5,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Evidence {
    pub path: String,
    pub locator: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Finding {
    pub schema: String,
    pub tool: String,
    pub rule: String,
    pub severity: String,
    pub subject: String,
    pub message: String,
    pub evidence: Vec<Evidence>,
    pub data: Value,
    pub at: String,
}

/// Reads of one file. A read is keyed by its range (`whole`, or the
/// offset/limit or line range); only a repeat of an earlier key counts as a
/// reread.
#[derive(Default, Debug, Clone)]
pub struct ReadStat {
    pub reads: u64,
    pub rereads: u64,
    pub first_line: u64,
    pub first_reread_line: Option<u64>,
    pub ranges: HashMap<String, u64>,
}

#[derive(Default, Debug, Clone)]
pub struct CmdStat {
    pub count: u64,
    pub first_line: u64,
    pub program: String,
}

/// Per-session event data the detectors need but the session record does not
/// keep. Never serialised: it may hold commands.
#[derive(Default, Debug)]
pub struct Trace {
    /// Transcript path for evidence, home shown as `~`.
    pub evidence: String,
    /// For a transcript read from several files: (lines before the file,
    /// path) in read order, so evidence names the file holding a line.
    pub segments: Vec<(u64, String)>,
    /// Timestamps of the records, in file order.
    pub times: Vec<i64>,
    pub reads: HashMap<String, ReadStat>,
    pub commands: HashMap<String, CmdStat>,
    pub sleeps: u64,
    pub first_sleep_line: Option<u64>,
    /// Polling calls by tool name.
    pub polls: BTreeMap<String, u64>,
    /// Tool results in order: (is_error, line).
    pub outcomes: Vec<(bool, u64)>,
    /// (tool, characters, line).
    pub large_outputs: Vec<(String, u64, u64)>,
    /// Requests near the context window: (prompt tokens, window, line).
    pub near_context: Vec<(u64, u64, u64)>,
    /// Requests priced at the long-context tier: (prompt tokens, line).
    pub long_context: Vec<(u64, u64)>,
    /// Active turn intervals in Unix milliseconds.
    pub intervals: Vec<(i64, i64)>,
    pub rate_limit_lines: Vec<u64>,
    pub api_error_lines: Vec<u64>,
    pub counter_decrease_lines: Vec<u64>,
    /// Codex `token_count` events whose payload did not parse.
    pub unparseable_counter_lines: Vec<u64>,
    pub delta_mismatches: u64,
    pub usage_conflicts: u64,
}

impl Trace {
    /// Records a read of `range` (`whole` for the whole file) of `path`.
    pub fn read(&mut self, path: &str, range: &str, line: u64) {
        let e = self
            .reads
            .entry(path.to_string())
            .or_insert_with(|| ReadStat {
                first_line: line,
                ..ReadStat::default()
            });
        e.reads += 1;
        let n = e.ranges.entry(range.to_string()).or_default();
        *n += 1;
        if *n > 1 {
            e.rereads += 1;
            e.first_reread_line.get_or_insert(line);
        }
    }

    pub fn command(&mut self, cmd: &str, line: u64) {
        let e = self
            .commands
            .entry(cmd.to_string())
            .or_insert_with(|| CmdStat {
                count: 0,
                first_line: line,
                program: crate::util::program_name(cmd),
            });
        e.count += 1;
        if crate::util::is_sleep_command(cmd) {
            self.sleeps += 1;
            self.first_sleep_line.get_or_insert(line);
        }
    }
}

pub struct Ctx<'a> {
    pub th: &'a Thresholds,
    pub at: &'a str,
}

impl Ctx<'_> {
    pub fn finding(
        &self,
        rule: &str,
        severity: &str,
        subject: &str,
        message: String,
        evidence: Vec<Evidence>,
        data: Value,
    ) -> Finding {
        Finding {
            schema: FINDING_SCHEMA.to_string(),
            tool: "agentprof".to_string(),
            rule: rule.to_string(),
            severity: severity.to_string(),
            subject: subject.to_string(),
            message,
            evidence,
            data,
            at: self.at.to_string(),
        }
    }
}

fn ev(trace: &Trace, line: u64) -> Vec<Evidence> {
    if let Some((off, path)) = trace.segments.iter().rev().find(|(off, _)| *off < line) {
        return vec![Evidence {
            path: path.clone(),
            locator: format!("line {}", line - off),
        }];
    }
    if trace.evidence.is_empty() {
        return Vec::new();
    }
    vec![Evidence {
        path: trace.evidence.clone(),
        locator: format!("line {line}"),
    }]
}

/// Statuses that mean the host refused an automated client.
pub fn is_blocked_status(status: &str) -> bool {
    matches!(status, "403" | "418" | "429" | "cloudflare-challenge")
}

/// Statuses that count as a failed web request.
pub fn is_failed_status(status: &str) -> bool {
    match status.parse::<u16>() {
        Ok(code) => code >= 400,
        Err(_) => !matches!(status, "ok" | "ok-no-status" | "no-status"),
    }
}

/// Runs the per-session detectors.
pub fn per_session(s: &Session, t: &Trace, cx: &Ctx) -> Vec<Finding> {
    let th = cx.th;
    let subject = s.key();
    let mut out = Vec::new();

    // file-reread: one read plus at least reread_min - 1 repeats of a
    // range already read.
    let mut rereads: Vec<(&String, &ReadStat)> = t
        .reads
        .iter()
        .filter(|(_, r)| r.rereads + 1 >= th.reread_min.max(2))
        .collect();
    rereads.sort_by(|a, b| b.1.rereads.cmp(&a.1.rereads).then(a.0.cmp(b.0)));
    if let Some((_, top)) = rereads.first() {
        let files: Vec<Value> = rereads
            .iter()
            .take(10)
            .map(|(p, r)| json!({"path": crate::util::tilde(p), "reads": r.reads, "repeat_reads": r.rereads}))
            .collect();
        let extra: u64 = rereads.iter().map(|(_, r)| r.rereads).sum();
        out.push(cx.finding(
            "file-reread",
            "warn",
            &subject,
            format!(
                "{} file(s) had the same range read again at least {} times; the most {} times",
                rereads.len(),
                th.reread_min.max(2) - 1,
                top.rereads
            ),
            ev(t, top.first_reread_line.unwrap_or(top.first_line)),
            json!({"files_over_threshold": rereads.len(), "max_reads": rereads.iter().map(|(_, r)| r.reads).max().unwrap_or(0), "max_repeat_reads": top.rereads, "repeat_reads": extra, "top": files}),
        ));
    }

    // repeated-command
    let mut repeats: Vec<(&String, &CmdStat)> = t
        .commands
        .iter()
        .filter(|(_, c)| c.count >= th.repeat_cmd_min)
        .collect();
    repeats.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
    if let Some((_, top)) = repeats.first() {
        let cmds: Vec<Value> = repeats
            .iter()
            .take(10)
            .map(|(c, s)| json!({"hash": crate::util::fnv64(c), "program": s.program, "runs": s.count}))
            .collect();
        out.push(cx.finding(
            "repeated-command",
            "warn",
            &subject,
            format!(
                "{} identical shell command(s) ran at least {} times; the most repeated {} times",
                repeats.len(),
                th.repeat_cmd_min,
                top.count
            ),
            ev(t, top.first_line),
            json!({"commands_over_threshold": repeats.len(), "max_runs": top.count, "top": cmds}),
        ));
    }

    // large-tool-output
    if !t.large_outputs.is_empty() {
        let mut by_tool: BTreeMap<&str, u64> = BTreeMap::new();
        for (tool, _, _) in &t.large_outputs {
            *by_tool.entry(tool).or_default() += 1;
        }
        let (_, max, line) = t
            .large_outputs
            .iter()
            .max_by_key(|(_, c, _)| *c)
            .expect("non-empty");
        let total: u64 = t.large_outputs.iter().map(|(_, c, _)| c).sum();
        out.push(cx.finding(
            "large-tool-output",
            "warn",
            &subject,
            format!(
                "{} tool result(s) of at least {} characters entered the context; the largest had {} characters",
                t.large_outputs.len(),
                th.large_output_chars,
                max
            ),
            ev(t, *line),
            json!({"count": t.large_outputs.len(), "max_chars": max, "total_chars": total, "by_tool": by_tool}),
        ));
    }

    // tool-error-streak
    let mut best = (0u64, 0u64);
    let mut cur = (0u64, 0u64);
    for (is_err, line) in &t.outcomes {
        if *is_err {
            if cur.0 == 0 {
                cur.1 = *line;
            }
            cur.0 += 1;
            if cur.0 > best.0 {
                best = cur;
            }
        } else {
            cur = (0, 0);
        }
    }
    if best.0 >= th.error_streak_min {
        out.push(cx.finding(
            "tool-error-streak",
            "warn",
            &subject,
            format!("{} consecutive tool calls failed", best.0),
            ev(t, best.1),
            json!({"longest_streak": best.0, "errors": t.outcomes.iter().filter(|o| o.0).count(), "results": t.outcomes.len()}),
        ));
    }

    // web-fetch-failure (one finding per host with failures)
    for (host, statuses) in &s.web {
        let failed: BTreeMap<&String, &u64> = statuses
            .iter()
            .filter(|(st, _)| is_failed_status(st))
            .collect();
        if failed.is_empty() {
            continue;
        }
        let blocked = failed.keys().any(|st| is_blocked_status(st));
        let n: u64 = failed.values().copied().sum();
        let total: u64 = statuses.values().sum();
        out.push(cx.finding(
            "web-fetch-failure",
            if blocked { "warn" } else { "info" },
            &subject,
            format!("{n} of {total} web request(s) to {host} failed"),
            Vec::new(),
            json!({"host": host, "failed": n, "requests": total, "blocked": blocked, "statuses": failed}),
        ));
    }

    // sleep-poll-loop
    let polls: u64 = t.polls.values().sum();
    if t.sleeps + polls >= th.sleep_min {
        out.push(cx.finding(
            "sleep-poll-loop",
            "warn",
            &subject,
            format!(
                "{} sleep command(s) and {} polling call(s) in one session",
                t.sleeps, polls
            ),
            t.first_sleep_line.map(|l| ev(t, l)).unwrap_or_default(),
            json!({"sleep_commands": t.sleeps, "polls": t.polls}),
        ));
    }

    // cache rules
    if let Some(total) = &s.total {
        let tk = &total.tokens;
        let prompt = tk.prompt();
        if prompt >= th.cache_min_prompt_tokens {
            let ratio = tk.cache_read as f64 / prompt as f64;
            if ratio < th.cache_read_min_ratio {
                out.push(cx.finding(
                    "low-cache-read-ratio",
                    "warn",
                    &subject,
                    format!(
                        "only {:.1}% of {} prompt tokens were cache reads",
                        ratio * 100.0,
                        prompt
                    ),
                    Vec::new(),
                    json!({"cache_read_ratio": ratio, "prompt_tokens": prompt, "cache_read": tk.cache_read}),
                ));
            }
            let rw = tk.cache_writes() + tk.cache_read;
            if rw > 0 && tk.cache_writes() > 0 {
                let churn = tk.cache_writes() as f64 / rw as f64;
                if churn >= th.cache_churn_ratio {
                    out.push(cx.finding(
                        "cache-churn",
                        "warn",
                        &subject,
                        format!(
                            "cache writes were {:.1}% of cached prompt tokens",
                            churn * 100.0
                        ),
                        Vec::new(),
                        json!({"cache_write_ratio": churn, "cache_writes": tk.cache_writes(), "cache_read": tk.cache_read}),
                    ));
                }
            }
        }
    }

    // frequent-compaction
    if s.compactions.count >= th.compactions_min {
        out.push(cx.finding(
            "frequent-compaction",
            "warn",
            &subject,
            format!("the context was compacted {} times", s.compactions.count),
            Vec::new(),
            json!({"compactions": s.compactions}),
        ));
    }

    // near-context-window
    if let Some((p, w, line)) = t.near_context.iter().max_by_key(|x| x.0) {
        out.push(cx.finding(
            "near-context-window",
            "warn",
            &subject,
            format!(
                "{} request(s) used at least {:.0}% of the context window; the largest prompt had {} of {} tokens",
                t.near_context.len(),
                th.context_ratio * 100.0,
                p,
                w
            ),
            ev(t, *line),
            json!({"requests": t.near_context.len(), "max_prompt_tokens": p, "context_window": w}),
        ));
    }

    // long-context-tier
    if let Some(total) = &s.total
        && total.long_context_requests > 0
        // A share exactly at the threshold counts, despite float rounding.
        && total.long_context_requests as f64 / total.priced_requests() as f64
            >= th.long_context_share - 1e-9
    {
        let lc = total.long_context_requests;
        let priced = total.priced_requests();
        out.push(cx.finding(
            "long-context-tier",
            "warn",
            &subject,
            format!(
                "{lc} of {priced} priced request(s) ({:.1}%) had prompts over the long-context threshold; the tier added ${:.2} over standard rates",
                100.0 * lc as f64 / priced as f64,
                total.long_context_usd
            ),
            t.long_context
                .iter()
                .max_by_key(|x| x.0)
                .map(|(_, line)| ev(t, *line))
                .unwrap_or_default(),
            json!({"long_context_requests": lc, "priced_requests": priced, "long_context_usd": total.long_context_usd}),
        ));
    }

    // long-idle-gap
    let gap_ms = th.idle_gap_minutes as i64 * 60_000;
    let mut sorted = t.times.clone();
    sorted.sort_unstable();
    let gaps: Vec<i64> = sorted
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|g| *g >= gap_ms)
        .collect();
    if !gaps.is_empty() {
        let total: i64 = gaps.iter().sum();
        let max = *gaps.iter().max().expect("non-empty");
        out.push(cx.finding(
            "long-idle-gap",
            "info",
            &subject,
            format!(
                "{} gap(s) of at least {} minutes without activity; the longest {:.1} hours",
                gaps.len(),
                th.idle_gap_minutes,
                max as f64 / 3_600_000.0
            ),
            Vec::new(),
            json!({"gaps": gaps.len(), "total_ms": total, "max_ms": max}),
        ));
    }

    // rate-limit-saturation
    let rl = &s.rate_limits;
    if rl.saturated_observations > 0 || rl.limit_errors > 0 || rl.limit_reached_observations > 0 {
        out.push(cx.finding(
            "rate-limit-saturation",
            "warn",
            &subject,
            format!(
                "usage limits were reached: {} saturated snapshot(s), {} limit-reached snapshot(s), {} refused request(s)",
                rl.saturated_observations, rl.limit_reached_observations, rl.limit_errors
            ),
            t.rate_limit_lines.first().map(|l| ev(t, *l)).unwrap_or_default(),
            json!({"rate_limits": rl}),
        ));
    }

    // api-error (other than usage limits)
    let other_errors: BTreeMap<&String, &u64> = s
        .api_errors
        .iter()
        .filter(|(k, _)| !k.starts_with("rate_limit"))
        .collect();
    if !other_errors.is_empty() {
        let n: u64 = other_errors.values().copied().sum();
        out.push(
            cx.finding(
                "api-error",
                "warn",
                &subject,
                format!("{n} API error response(s) other than usage limits"),
                t.api_error_lines
                    .first()
                    .map(|l| ev(t, *l))
                    .unwrap_or_default(),
                json!({"api_errors": other_errors}),
            ),
        );
    }

    // turn-aborted
    if s.aborts > 0 && s.source != "t3" {
        out.push(cx.finding(
            "turn-aborted",
            "info",
            &subject,
            format!("{} turn(s) were interrupted or aborted", s.aborts),
            Vec::new(),
            json!({"aborts": s.aborts}),
        ));
    }

    // inherited-counter
    if let Some(inh) = s.inherited_tokens.filter(|v| *v > 0) {
        out.push(cx.finding(
            "inherited-counter",
            "info",
            &subject,
            format!(
                "the thread's cumulative token counter started at {inh} tokens inherited from its base thread; they are excluded here but included in the database's tokens_used"
            ),
            Vec::new(),
            json!({"inherited_tokens": inh, "db_tokens_used": s.db_tokens_used}),
        ));
    }

    // history-gap (data integrity)
    if let Some(u) = s.untracked_tokens.filter(|v| *v > 0) {
        out.push(cx.finding(
            "history-gap",
            "warn",
            &subject,
            format!(
                "the thread was not forked, yet its token counter started at {u} tokens: its own usage in records that are missing or unreadable. Those tokens are not in the totals, so the thread's tokens and cost are a lower bound"
            ),
            Vec::new(),
            json!({"untracked_tokens": u, "db_tokens_used": s.db_tokens_used}),
        ));
    }

    // counter-unparseable (data integrity)
    if let Some(line) = t.unparseable_counter_lines.first() {
        out.push(cx.finding(
            "counter-unparseable",
            "warn",
            &subject,
            format!(
                "{} token counter event(s) did not parse; later counters still cover their tokens, but the requests' split by model, day and tier may be off",
                t.unparseable_counter_lines.len()
            ),
            ev(t, *line),
            json!({"events": t.unparseable_counter_lines.len()}),
        ));
    }

    // counter-decrease (data integrity)
    if let Some(line) = t.counter_decrease_lines.first() {
        out.push(cx.finding(
            "counter-decrease",
            "error",
            &subject,
            format!(
                "the cumulative token counter decreased {} time(s); the session's tokens are unreliable",
                t.counter_decrease_lines.len()
            ),
            ev(t, *line),
            json!({"decreases": t.counter_decrease_lines.len()}),
        ));
    }

    // usage-conflict (data integrity)
    if t.usage_conflicts > 0 {
        out.push(cx.finding(
            "usage-conflict",
            "warn",
            &subject,
            format!(
                "{} request(s) appeared on several lines with different usage; the line with the largest output was kept",
                t.usage_conflicts
            ),
            Vec::new(),
            json!({"requests": t.usage_conflicts}),
        ));
    }

    // transcript-missing
    if let Some(tr) = &s.transcript
        && tr.status == "missing"
    {
        out.push(cx.finding(
            "transcript-missing",
            "warn",
            &subject,
            "the session's transcript file no longer exists; only database totals are available".to_string(),
            Vec::new(),
            json!({"path": tr.path, "db_tokens_used": s.db_tokens_used}),
        ));
    }

    // transcript-damaged
    if let Some(tr) = &s.transcript
        && matches!(tr.status.as_str(), "damaged" | "incomplete")
    {
        let message = if tr.status == "incomplete" {
            "earlier rollout segments exist but the newest one the database records is missing; the thread's tokens are unknown"
        } else {
            "a rollout segment could not be read to the end; the thread's tokens are unknown"
        };
        out.push(cx.finding(
            "transcript-damaged",
            "warn",
            &subject,
            message.to_string(),
            Vec::new(),
            json!({"status": tr.status, "path": tr.path, "error": tr.error, "lines_read": tr.lines, "db_tokens_used": s.db_tokens_used}),
        ));
    }

    // T3 runs
    if let Some(t3) = &s.t3 {
        let failed = t3.runs_by_status.get("failed").copied().unwrap_or(0);
        if failed > 0 {
            out.push(cx.finding(
                "run-failed",
                "warn",
                &subject,
                format!("{failed} run(s) of this T3 thread failed"),
                Vec::new(),
                json!({"runs_by_status": t3.runs_by_status}),
            ));
        }
        let stopped: u64 = ["interrupted", "cancelled", "rolled_back"]
            .iter()
            .filter_map(|k| t3.runs_by_status.get(*k))
            .sum();
        if stopped > 0 {
            out.push(cx.finding(
                "run-interrupted",
                "info",
                &subject,
                format!(
                    "{stopped} run(s) of this T3 thread were interrupted, cancelled or rolled back"
                ),
                Vec::new(),
                json!({"runs_by_status": t3.runs_by_status}),
            ));
        }
        if t3.linked.is_empty() {
            out.push(cx.finding(
                "t3-unlinked",
                "info",
                &subject,
                "the T3 thread could not be linked to a native Claude or Codex session".to_string(),
                Vec::new(),
                json!({"runs": t3.runs_by_status.values().sum::<u64>()}),
            ));
        }
    }

    out
}

/// Cross-session detectors: subagent fan-out and the share of tokens spent
/// in subagents.
pub fn cross_session(sessions: &[Session], cx: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut children: BTreeMap<String, Vec<&Session>> = BTreeMap::new();
    for s in sessions {
        if let Some(p) = &s.parent_id {
            children
                .entry(format!("{}:{p}", s.source))
                .or_default()
                .push(s);
        }
    }
    let by_key: HashMap<String, &Session> = sessions.iter().map(|s| (s.key(), s)).collect();
    for (parent, kids) in &children {
        if (kids.len() as u64) < cx.th.fanout_min {
            continue;
        }
        let own = by_key
            .get(parent)
            .and_then(|s| s.total.as_ref())
            .map(|a| a.tokens.total());
        let kid_tokens: u64 = kids
            .iter()
            .filter_map(|k| k.total.as_ref())
            .map(|a| a.tokens.total())
            .sum();
        let share = own.map(|o| {
            let all = o + kid_tokens;
            if all == 0 {
                0.0
            } else {
                kid_tokens as f64 / all as f64
            }
        });
        out.push(cx.finding(
            "subagent-fanout",
            "info",
            parent,
            format!(
                "the session started {} direct subagent session(s)",
                kids.len()
            ),
            Vec::new(),
            json!({"children": kids.len(), "children_tokens": kid_tokens, "parent_tokens": own, "children_token_share": share}),
        ));
    }

    // Project-level share, per source.
    let mut per_source: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for s in sessions {
        if let Some(a) = &s.total {
            let e = per_source.entry(s.source.as_str()).or_default();
            if s.role == "subagent" {
                e.1 += a.tokens.total();
            } else {
                e.0 += a.tokens.total();
            }
        }
    }
    for (source, (main, sub)) in per_source {
        let all = main + sub;
        if all == 0 {
            continue;
        }
        let share = sub as f64 / all as f64;
        out.push(cx.finding(
            "subagent-token-share",
            "info",
            &format!("project:{source}"),
            format!(
                "{:.1}% of {source} tokens were spent in subagent sessions",
                share * 100.0
            ),
            Vec::new(),
            json!({"subagent_tokens": sub, "main_tokens": main, "share": share}),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_th() -> Thresholds {
        Thresholds::default()
    }

    #[test]
    fn rereads_and_streaks_fire_at_thresholds() {
        let th = ctx_th();
        let cx = Ctx {
            th: &th,
            at: "2026-01-01T00:00:00Z",
        };
        let s = Session::new("claude", "s1");
        let mut t = Trace::default();
        for i in 0..5 {
            t.read("/p/a.md", "whole", i);
        }
        for i in 0..4 {
            t.read("/p/b.md", "whole", i);
        }
        // Different ranges of one file are not rereads.
        for i in 0..8 {
            t.read("/p/c.md", &format!("{}+100", i * 100), i);
        }
        for i in 0..4 {
            t.outcomes.push((true, 10 + i));
        }
        t.outcomes.push((false, 20));
        let f = per_session(&s, &t, &cx);
        let rules: Vec<&str> = f.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&"file-reread"));
        assert!(rules.contains(&"tool-error-streak"));
        let rr = f.iter().find(|f| f.rule == "file-reread").unwrap();
        assert_eq!(rr.data["files_over_threshold"], 1);
        assert_eq!(rr.data["max_reads"], 5);
        assert_eq!(rr.data["max_repeat_reads"], 4);
    }

    #[test]
    fn evidence_names_the_segment_holding_a_line() {
        let t = Trace {
            evidence: "~/s/new.jsonl".into(),
            segments: vec![(0, "~/s/old.jsonl".into()), (10, "~/s/new.jsonl".into())],
            ..Trace::default()
        };
        assert_eq!(ev(&t, 4)[0].path, "~/s/old.jsonl");
        assert_eq!(ev(&t, 10)[0].locator, "line 10");
        assert_eq!(ev(&t, 12)[0].path, "~/s/new.jsonl");
        assert_eq!(ev(&t, 12)[0].locator, "line 2");
    }

    #[test]
    fn data_integrity_rules_fire() {
        let th = ctx_th();
        let cx = Ctx { th: &th, at: "x" };
        let mut s = Session::new("codex", "t");
        s.untracked_tokens = Some(500);
        s.transcript = Some(crate::session::Transcript {
            status: "damaged".into(),
            error: Some("x: read failed".into()),
            ..Default::default()
        });
        let t = Trace {
            unparseable_counter_lines: vec![3],
            ..Trace::default()
        };
        let f = per_session(&s, &t, &cx);
        let rules: Vec<&str> = f.iter().map(|f| f.rule.as_str()).collect();
        for r in ["history-gap", "counter-unparseable", "transcript-damaged"] {
            assert!(rules.contains(&r), "{r}");
        }
        s.untracked_tokens = Some(0);
        s.transcript = None;
        let f = per_session(&s, &Trace::default(), &cx);
        assert!(f.is_empty());
    }

    #[test]
    fn idle_gaps_and_sleep_loops() {
        let th = ctx_th();
        let cx = Ctx { th: &th, at: "x" };
        let s = Session::new("codex", "t");
        let mut t = Trace {
            times: vec![0, 60_000, 60_000 + 31 * 60_000],
            ..Trace::default()
        };
        for i in 0..5 {
            t.command("sleep 10", i);
        }
        let f = per_session(&s, &t, &cx);
        let gap = f.iter().find(|f| f.rule == "long-idle-gap").unwrap();
        assert_eq!(gap.data["gaps"], 1);
        assert!(f.iter().any(|f| f.rule == "sleep-poll-loop"));
        let rep = f.iter().find(|f| f.rule == "repeated-command").unwrap();
        assert_eq!(rep.data["top"][0]["program"], "sleep");
    }

    #[test]
    fn web_failure_marks_blocked_hosts() {
        let th = ctx_th();
        let cx = Ctx { th: &th, at: "x" };
        let mut s = Session::new("claude", "s");
        s.web
            .entry("a.example".into())
            .or_default()
            .insert("403".into(), 2);
        s.web
            .entry("a.example".into())
            .or_default()
            .insert("200".into(), 1);
        s.web
            .entry("b.example".into())
            .or_default()
            .insert("dns".into(), 1);
        let f = per_session(&s, &Trace::default(), &cx);
        let a = f.iter().find(|f| f.data["host"] == "a.example").unwrap();
        assert_eq!(a.severity, "warn");
        assert_eq!(a.data["failed"], 2);
        let b = f.iter().find(|f| f.data["host"] == "b.example").unwrap();
        assert_eq!(b.severity, "info");
    }
}
