//! Codex: the `threads` table of `state_5.sqlite` and the rollout JSONL files
//! it points to (plain or `.zst`).
//!
//! Rollouts record cumulative token counters (`total_token_usage`). A
//! thread's own usage is the sum of the positive differences between
//! consecutive counters; the first counter's `last_token_usage` is its first
//! request, and anything the counter held before that (a fork's base thread)
//! is reported as `inherited_tokens` and never counted.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jiff::tz::TimeZone;
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;

use crate::detect::Trace;
use crate::pricing::Pricing;
use crate::session::{Acc, OpStat, Session, Spawn, Tokens, Transcript};
use crate::util::{day, display_path, host_of, parse_ts, tilde};

/// Opens a SQLite file read-only. `mode=ro` needs to create the `-shm` file
/// when the database is in WAL mode; when that fails and no `-wal` file
/// exists (so no uncommitted pages can be missed) the file is opened as
/// immutable. Returns the connection and the mode used.
pub fn open_ro(path: &Path) -> Result<(Connection, &'static str)> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let uri = |q: &str| format!("file:{}?{q}", path.display());
    let ro = Connection::open_with_flags(uri("mode=ro"), flags).and_then(|c| {
        c.query_row("select count(*) from sqlite_master", [], |_| Ok(()))
            .map(|_| c)
    });
    match ro {
        Ok(c) => Ok((c, "mode=ro")),
        Err(e) => {
            let wal = PathBuf::from(format!("{}-wal", path.display()));
            if wal.exists() {
                return Err(e).with_context(|| {
                    format!(
                        "opening {} read-only (a -wal file exists, so immutable mode is unsafe)",
                        path.display()
                    )
                });
            }
            let c = Connection::open_with_flags(uri("immutable=1"), flags)
                .with_context(|| format!("opening {} immutable", path.display()))?;
            Ok((c, "immutable=1 (mode=ro failed; no -wal file)"))
        }
    }
}

#[derive(Debug, Clone)]
pub struct Thread {
    pub id: String,
    pub rollout_path: String,
    pub created_ms: Option<i64>,
    pub updated_ms: Option<i64>,
    pub source: String,
    pub cwd: String,
    pub title: Option<String>,
    pub tokens_used: Option<i64>,
    pub archived: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl Thread {
    /// Parent thread and spawn details from the `source` JSON.
    pub fn spawn(&self) -> Option<(String, Spawn)> {
        let v: Value = serde_json::from_str(&self.source).ok()?;
        let ts = v.get("subagent")?.get("thread_spawn")?;
        let parent = ts.get("parent_thread_id")?.as_str()?.to_string();
        Some((
            parent,
            Spawn {
                mechanism: "codex-spawn".into(),
                agent_type: ts
                    .get("agent_role")
                    .and_then(|r| r.as_str())
                    .map(str::to_string),
                depth: ts.get("depth").and_then(|d| d.as_u64()),
            },
        ))
    }
}

pub fn load_threads(conn: &Connection) -> Result<Vec<Thread>> {
    let mut st = conn.prepare(
        "select id, rollout_path,
                coalesce(created_at_ms, created_at * 1000), coalesce(updated_at_ms, updated_at * 1000),
                source, cwd, title, tokens_used, archived, model, reasoning_effort
         from threads order by id",
    )?;
    let rows = st.query_map([], |r| {
        Ok(Thread {
            id: r.get(0)?,
            rollout_path: r.get(1)?,
            created_ms: r.get(2)?,
            updated_ms: r.get(3)?,
            source: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            cwd: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            title: r.get(6)?,
            tokens_used: r.get(7)?,
            archived: r.get::<_, Option<i64>>(8)?.unwrap_or(0) != 0,
            model: r.get(9)?,
            effort: r.get(10)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("reading the Codex threads table")
}

/// Where a rollout's lines can be read from.
#[derive(Debug, Clone, PartialEq)]
pub enum Rollout {
    Plain(PathBuf),
    Zstd(PathBuf),
    Missing(PathBuf),
}

/// Resolves the recorded path, its `.zst` sibling, then the same file names
/// in `archived_sessions`.
pub fn resolve(recorded: &str, archived_dir: Option<&Path>) -> Rollout {
    let p = PathBuf::from(recorded);
    let zst = |p: &Path| PathBuf::from(format!("{}.zst", p.display()));
    if p.is_file() {
        return Rollout::Plain(p);
    }
    if zst(&p).is_file() {
        return Rollout::Zstd(zst(&p));
    }
    if let (Some(dir), Some(name)) = (archived_dir, p.file_name()) {
        let a = dir.join(name);
        if a.is_file() {
            return Rollout::Plain(a);
        }
        if zst(&a).is_file() {
            return Rollout::Zstd(zst(&a));
        }
    }
    Rollout::Missing(p)
}

#[derive(Deserialize)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    ordinal: Option<u64>,
    payload: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct Typed {
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize, Default)]
struct Meta {
    id: Option<String>,
    originator: Option<String>,
    forked_from_id: Option<String>,
    subagent_history_start_ordinal: Option<u64>,
}

#[derive(Deserialize, Default)]
struct TurnContext {
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
}

impl Usage {
    fn minus(&self, o: &Usage) -> Option<Usage> {
        Some(Usage {
            input_tokens: self.input_tokens.checked_sub(o.input_tokens)?,
            cached_input_tokens: self
                .cached_input_tokens
                .checked_sub(o.cached_input_tokens)?,
            output_tokens: self.output_tokens.checked_sub(o.output_tokens)?,
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .checked_sub(o.reasoning_output_tokens)?,
            total_tokens: self.total_tokens.checked_sub(o.total_tokens)?,
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .checked_sub(o.cache_write_input_tokens)?,
        })
    }

    /// Maps to disjoint classes. `input_tokens` includes cached input, and
    /// cache writes are taken to be part of it too (never observed non-zero).
    pub fn tokens(&self) -> Tokens {
        Tokens {
            input: self
                .input_tokens
                .saturating_sub(self.cached_input_tokens + self.cache_write_input_tokens),
            cache_write_unknown: self.cache_write_input_tokens,
            cache_read: self.cached_input_tokens,
            output: self.output_tokens,
            reasoning: self.reasoning_output_tokens,
            ..Tokens::default()
        }
    }
}

#[derive(Deserialize)]
struct TokenCount {
    info: Option<Info>,
    rate_limits: Option<RateLimitSnap>,
}

#[derive(Deserialize)]
struct Info {
    total_token_usage: Usage,
    last_token_usage: Usage,
    model_context_window: Option<u64>,
}

#[derive(Deserialize)]
struct RateLimitSnap {
    primary: Option<Window>,
    secondary: Option<Window>,
    plan_type: Option<String>,
    rate_limit_reached_type: Option<Value>,
}

#[derive(Deserialize)]
struct Window {
    used_percent: Option<f64>,
}

#[derive(Deserialize)]
struct TurnEnd {
    duration_ms: Option<i64>,
}

#[derive(Deserialize)]
struct Settings {
    thread_settings: Option<SettingsInner>,
}

#[derive(Deserialize)]
struct SettingsInner {
    service_tier: Option<String>,
}

#[derive(Deserialize)]
struct ItemCompleted {
    item: Option<Item>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(rename = "type")]
    kind: Option<String>,
    status: Option<String>,
    exit_code: Option<i64>,
    command: Option<Value>,
    parsed_cmd: Option<Vec<ParsedCmd>>,
    duration: Option<Duration>,
    server: Option<String>,
    result: Option<Value>,
    action: Option<Action>,
}

#[derive(Deserialize)]
struct ParsedCmd {
    #[serde(rename = "type")]
    kind: Option<String>,
    path: Option<String>,
}

#[derive(Deserialize)]
struct Duration {
    secs: Option<u64>,
    nanos: Option<u64>,
}

#[derive(Deserialize)]
struct Action {
    #[serde(rename = "type")]
    kind: Option<String>,
    url: Option<String>,
}

#[derive(Deserialize)]
struct RespItem {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    namespace: Option<String>,
    call_id: Option<String>,
    output: Option<Box<RawValue>>,
}

/// Characters of text in a tool output (string, or array of text items),
/// and whether it starts with one of Codex's failure prefixes.
fn output_text(raw: &RawValue) -> (u64, bool) {
    #[derive(Deserialize)]
    struct Part {
        text: Option<String>,
    }
    let s = raw.get();
    let text = if s.starts_with('"') {
        serde_json::from_str::<String>(s).unwrap_or_default()
    } else if s.starts_with('[') {
        serde_json::from_str::<Vec<Part>>(s)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| p.text)
            .collect::<String>()
    } else {
        String::new()
    };
    let failed = text.starts_with("Script failed") || text.starts_with("aborted");
    (text.chars().count() as u64, failed)
}

/// The shell command text of a `CommandExecution` (`[shell, -lc, cmd]`).
fn command_text(v: &Value) -> Option<String> {
    let parts: Vec<&str> = v.as_array()?.iter().filter_map(|x| x.as_str()).collect();
    match parts.as_slice() {
        [_, flag, cmd] if flag.starts_with('-') && flag.ends_with('c') => Some(cmd.to_string()),
        [] => None,
        p => Some(p.join(" ")),
    }
}

fn is_poll(name: &str) -> bool {
    name == "wait"
        || name.ends_with(".wait")
        || name.contains("wait_agent")
        || name.ends_with("sleep")
}

pub struct Ctx<'a> {
    pub pricing: &'a Pricing,
    pub tz: &'a TimeZone,
    pub include_titles: bool,
    pub large_output_chars: u64,
    pub context_ratio: f64,
}

/// Per-thread numbers for the cross-check against `threads.tokens_used`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Check {
    pub last_total: Option<u64>,
    pub inherited: u64,
    pub own_total: u64,
    pub token_events: u64,
    pub duplicate_events: u64,
}

pub struct Parsed {
    pub session: Session,
    pub trace: Trace,
    pub turn_ids: Vec<String>,
    pub check: Check,
}

fn open_reader(r: &Rollout) -> Result<Box<dyn BufRead>> {
    Ok(match r {
        Rollout::Plain(p) => Box::new(BufReader::with_capacity(
            1 << 20,
            File::open(p).with_context(|| format!("opening {}", p.display()))?,
        )),
        Rollout::Zstd(p) => {
            let f = File::open(p).with_context(|| format!("opening {}", p.display()))?;
            let d = zstd::Decoder::new(f)?;
            Box::new(BufReader::with_capacity(1 << 20, d))
        }
        Rollout::Missing(_) => Box::new(BufReader::new(std::io::empty())),
    })
}

/// Builds the session for one thread, streaming its rollout.
pub fn parse(th: &Thread, rollout: &Rollout, cx: &Ctx) -> Result<Parsed> {
    let reader = open_reader(rollout)?;
    parse_reader(th, rollout, reader, cx)
}

pub fn parse_reader(
    th: &Thread,
    rollout: &Rollout,
    mut reader: impl BufRead,
    cx: &Ctx,
) -> Result<Parsed> {
    let mut s = Session::new("codex", &th.id);
    let mut tr = Trace::default();
    s.cwd = Some(tilde(&th.cwd));
    if cx.include_titles {
        s.title = th.title.clone().filter(|t| !t.is_empty());
    }
    s.db_tokens_used = th.tokens_used.map(|v| v.max(0) as u64);
    if let Some((parent, spawn)) = th.spawn() {
        s.role = "subagent".into();
        s.parent_id = Some(parent);
        s.spawn = Some(spawn);
    }
    if th.archived {
        s.notes.push("thread is archived".into());
    }
    let (status, path) = match rollout {
        Rollout::Plain(p) => ("present", p),
        Rollout::Zstd(p) => ("zstd", p),
        Rollout::Missing(p) => ("missing", p),
    };
    tr.evidence = display_path(path);
    let mut transcript = Transcript {
        path: Some(display_path(path)),
        status: status.into(),
        ..Transcript::default()
    };
    let basis = cx.pricing.basis("openai");
    let mut check = Check::default();

    if let Rollout::Missing(_) = rollout {
        s.set_span(th.created_ms, th.updated_ms);
        s.token_basis = "codex: rollout missing; no per-request data. threads.tokens_used is shown as db_tokens_used (cumulative, may include inherited history) and is not added to totals".into();
        s.active_basis = "codex: unknown (rollout missing)".into();
        s.finish_cost(&basis, 0.0);
        s.transcript = Some(transcript);
        return Ok(Parsed {
            session: s,
            trace: tr,
            turn_ids: Vec::new(),
            check,
        });
    }

    let mut total = Acc::default();
    let mut prev: Option<Usage> = None;
    let mut model = th.model.clone();
    let mut effort = th.effort.clone();
    let mut tier: Option<String> = None;
    let mut history_start: Option<u64> = None;
    let mut start: Option<i64> = None;
    let mut end: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut turn_open: Option<i64> = None;
    let mut turns = 0u64;
    let mut active = 0i64;
    let mut open_turns = 0u64;
    let mut turn_ids = Vec::new();
    let mut pending: HashMap<String, (String, Option<i64>)> = HashMap::new();
    let mut unordered = 0u64;
    let mut buf = Vec::with_capacity(1 << 16);
    let mut ln = 0u64;

    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        ln += 1;
        if buf.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        transcript.lines += 1;
        let line: Line = match serde_json::from_slice(&buf) {
            Ok(l) => l,
            Err(_) => {
                transcript.bad_lines += 1;
                continue;
            }
        };
        let kind = line.kind.as_deref().unwrap_or("");
        let payload = line.payload.as_deref();
        let ts = line.timestamp.as_deref().and_then(parse_ts);

        if kind == "session_meta" {
            let m: Meta = payload
                .and_then(|p| serde_json::from_str(p.get()).ok())
                .unwrap_or_default();
            if m.id.as_deref() == Some(th.id.as_str()) {
                history_start = m.subagent_history_start_ordinal;
                if s.client.is_none() {
                    s.client = m.originator;
                }
                if let Some(f) = m.forked_from_id {
                    s.notes.push(format!("forked from codex:{f}"));
                }
                continue;
            }
        }
        // Lines copied from the parent thread's history.
        if let (Some(hs), Some(o)) = (history_start, line.ordinal)
            && o < hs
        {
            transcript.inherited_lines += 1;
            continue;
        }
        if history_start.is_some() && line.ordinal.is_none() {
            unordered += 1;
        }
        if kind == "session_meta" {
            continue;
        }
        if let Some(t) = ts {
            tr.times.push(t);
            start = Some(start.map_or(t, |s| s.min(t)));
            end = Some(end.map_or(t, |e| e.max(t)));
        }
        let Some(payload) = payload else { continue };

        match kind {
            "turn_context" => {
                let c: TurnContext = serde_json::from_str(payload.get()).unwrap_or_default();
                if c.model.is_some() {
                    model = c.model;
                }
                if c.effort.is_some() {
                    effort = c.effort;
                }
                #[derive(Deserialize)]
                struct TurnId {
                    turn_id: Option<String>,
                }
                if let Ok(TurnId { turn_id: Some(t) }) = serde_json::from_str(payload.get()) {
                    turn_ids.push(t);
                }
            }
            "compacted" => {
                s.compactions.count += 1;
                s.compactions.unknown += 1;
            }
            "event_msg" => {
                let sub = serde_json::from_str::<Typed>(payload.get())
                    .ok()
                    .and_then(|t| t.kind)
                    .unwrap_or_default();
                match sub.as_str() {
                    "token_count" => {
                        let Ok(tc) = serde_json::from_str::<TokenCount>(payload.get()) else {
                            continue;
                        };
                        if let Some(rl) = tc.rate_limits {
                            let r = &mut s.rate_limits;
                            r.observations += 1;
                            let pcts = [&rl.primary, &rl.secondary]
                                .into_iter()
                                .flatten()
                                .filter_map(|w| w.used_percent);
                            let mut saturated = false;
                            for p in pcts {
                                r.max_used_percent =
                                    Some(r.max_used_percent.map_or(p, |m| m.max(p)));
                                saturated |= p >= 100.0;
                            }
                            if saturated {
                                r.saturated_observations += 1;
                                tr.rate_limit_lines.push(ln);
                            }
                            if let Some(reached) =
                                rl.rate_limit_reached_type.filter(|v| !v.is_null())
                            {
                                r.limit_reached_observations += 1;
                                let name = reached
                                    .as_str()
                                    .map(str::to_string)
                                    .unwrap_or_else(|| reached.to_string());
                                *r.limit_types.entry(name).or_default() += 1;
                                if !saturated {
                                    tr.rate_limit_lines.push(ln);
                                }
                            }
                            if let Some(pt) = rl.plan_type {
                                s.billing.plan_type = Some(pt);
                            }
                        }
                        let Some(info) = tc.info else { continue };
                        check.token_events += 1;
                        if let Some(w) = info.model_context_window {
                            s.context_window = Some(w);
                        }
                        let cur = info.total_token_usage;
                        let last = info.last_token_usage;
                        check.last_total = Some(cur.total_tokens);
                        let delta = match prev {
                            None => {
                                // The counter may start at a base thread's value.
                                let base = cur.minus(&last).unwrap_or_default();
                                check.inherited = base.total_tokens;
                                s.inherited_tokens = Some(base.total_tokens);
                                Some(last)
                            }
                            Some(p) if p == cur => {
                                check.duplicate_events += 1;
                                None
                            }
                            Some(p) => match cur.minus(&p) {
                                Some(d) => {
                                    if d != last {
                                        tr.delta_mismatches += 1;
                                    }
                                    Some(d)
                                }
                                None => {
                                    tr.counter_decrease_lines.push(ln);
                                    Some(last)
                                }
                            },
                        };
                        prev = Some(cur);
                        let Some(d) = delta else { continue };
                        let tokens = d.tokens();
                        check.own_total += d.total_tokens;
                        let m = model.clone().unwrap_or_else(|| "unknown".into());
                        let standard = tier.as_deref().is_none_or(|t| t == "default");
                        let usd = if standard {
                            cx.pricing.request_cost(&m, &tokens, 1.0)
                        } else {
                            None
                        };
                        total.add_request(&tokens, usd);
                        s.by_model.entry(m).or_default().add_request(&tokens, usd);
                        s.by_effort
                            .entry(effort.clone().unwrap_or_else(|| "unknown".into()))
                            .or_default()
                            .add_request(&tokens, usd);
                        let dkey = ts
                            .map(|t| day(t, cx.tz))
                            .unwrap_or_else(|| "unknown".into());
                        s.by_day.entry(dkey).or_default().add_request(&tokens, usd);
                        let prompt = last.input_tokens;
                        s.max_prompt_tokens = Some(s.max_prompt_tokens.unwrap_or(0).max(prompt));
                        if let Some(w) = info.model_context_window
                            && w > 0
                            && prompt as f64 >= cx.context_ratio * w as f64
                        {
                            tr.near_context.push((prompt, w, ln));
                        }
                    }
                    "task_started" => {
                        if let (Some(st), Some(lt)) = (turn_open, last_ts) {
                            open_turns += 1;
                            active += (lt - st).max(0);
                            tr.intervals.push((st, lt));
                        }
                        turns += 1;
                        turn_open = ts;
                    }
                    "task_complete" | "turn_aborted" => {
                        if sub == "turn_aborted" {
                            s.aborts += 1;
                        }
                        let dur = serde_json::from_str::<TurnEnd>(payload.get())
                            .ok()
                            .and_then(|e| e.duration_ms);
                        if let Some(st) = turn_open.take() {
                            let d = dur.unwrap_or_else(|| ts.map_or(0, |t| (t - st).max(0)));
                            active += d;
                            tr.intervals.push((st, st + d));
                        } else if let (Some(d), Some(t)) = (dur, ts) {
                            // The start was not recorded (e.g. before a resume).
                            active += d;
                            tr.intervals.push((t - d, t));
                        }
                    }
                    "thread_settings_applied" => {
                        if let Ok(Settings {
                            thread_settings:
                                Some(SettingsInner {
                                    service_tier: Some(t),
                                }),
                        }) = serde_json::from_str(payload.get())
                        {
                            tier = Some(t);
                        }
                    }
                    "item_completed" => {
                        let Ok(ItemCompleted { item: Some(it) }) =
                            serde_json::from_str(payload.get())
                        else {
                            continue;
                        };
                        item(&mut s, &mut tr, it, ln);
                    }
                    _ => {}
                }
            }
            "response_item" => {
                let Ok(ri) = serde_json::from_str::<RespItem>(payload.get()) else {
                    continue;
                };
                match ri.kind.as_deref() {
                    Some("function_call" | "custom_tool_call" | "local_shell_call") => {
                        let name = match (&ri.namespace, &ri.name) {
                            (Some(ns), Some(n)) => format!("{ns}.{n}"),
                            (None, Some(n)) => n.clone(),
                            _ => ri.kind.clone().unwrap_or_default(),
                        };
                        s.tools.entry(name.clone()).or_default().calls += 1;
                        if is_poll(&name) {
                            *tr.polls.entry(name.clone()).or_default() += 1;
                        }
                        if let Some(id) = ri.call_id {
                            pending.insert(id, (name, ts));
                        }
                    }
                    Some("function_call_output" | "custom_tool_call_output") => {
                        let Some((name, call_ts)) =
                            ri.call_id.as_ref().and_then(|id| pending.remove(id))
                        else {
                            continue;
                        };
                        let (chars, failed) =
                            ri.output.as_deref().map(output_text).unwrap_or_default();
                        let stat = s.tools.entry(name.clone()).or_default();
                        stat.results += 1;
                        if failed {
                            stat.errors += 1;
                        }
                        if let (Some(a), Some(b)) = (call_ts, ts) {
                            let lat = (b - a).max(0) as u64;
                            stat.latency_ms_total += lat;
                            stat.latency_ms_max = stat.latency_ms_max.max(lat);
                        }
                        stat.result_chars_total += chars;
                        stat.result_chars_max = stat.result_chars_max.max(chars);
                        if chars >= cx.large_output_chars {
                            tr.large_outputs.push((name, chars, ln));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        if ts.is_some() {
            last_ts = ts;
        }
    }
    if let Some(st) = turn_open {
        open_turns += 1;
        if let Some(lt) = last_ts {
            active += (lt - st).max(0);
            tr.intervals.push((st, lt));
        }
    }
    if open_turns > 0 {
        s.notes.push(format!(
            "{open_turns} turn(s) have no completion event; counted until the next turn or the last record"
        ));
    }
    if !pending.is_empty() {
        s.notes
            .push(format!("{} tool call(s) without an output", pending.len()));
    }
    if unordered > 0 {
        s.notes.push(format!(
            "{unordered} line(s) lack an ordinal, so inherited history could not be excluded for them"
        ));
    }
    if tr.delta_mismatches > 0 {
        s.notes.push(format!(
            "{} counter increment(s) differ from the event's last_token_usage; increments were used",
            tr.delta_mismatches
        ));
    }
    if tier.is_none() && total.requests > 0 {
        s.notes
            .push("service tier not recorded; standard-tier prices applied".into());
    }
    if let Some(t) = tier.as_deref().filter(|t| *t != "default") {
        s.notes
            .push(format!("service tier {t}: requests left unpriced"));
    }
    if transcript.bad_lines > 0 {
        s.notes.push(format!(
            "{} unparseable line(s) skipped",
            transcript.bad_lines
        ));
    }
    if check.token_events == 0 {
        if s.db_tokens_used == Some(0) {
            s.total = Some(total);
            s.token_basis =
                "codex: rollout has no token counters and threads.tokens_used is 0".into();
        } else {
            s.token_basis = "codex: rollout has no token counters; threads.tokens_used is shown as db_tokens_used and not added to totals".into();
        }
    } else {
        s.total = Some(total);
        s.token_basis = "codex: sum of increments of the cumulative token_count counters (first event: its last_token_usage); inherited counter values excluded".into();
    }
    s.billing.mode = if s.billing.plan_type.is_some() {
        "subscription".into()
    } else {
        "unknown".into()
    };
    s.billing.basis = if s.billing.plan_type.is_some() {
        "codex: rate-limit snapshots carry a ChatGPT plan_type".into()
    } else {
        "codex: no plan_type in the rollout".into()
    };
    s.turns = Some(turns);
    s.open_turns = open_turns;
    s.active_ms = Some(active);
    s.active_basis = "codex: sum of task_complete/turn_aborted duration_ms; a turn without one counts from its task_started to the last record before the next turn".into();
    s.error_basis = "codex: tool output starting 'Script failed' or 'aborted'; operations: status failed or non-zero exit".into();
    s.set_span(start.or(th.created_ms), end.or(th.updated_ms));
    s.finish_cost(&basis, 0.0);
    s.transcript = Some(transcript);
    Ok(Parsed {
        session: s,
        trace: tr,
        turn_ids,
        check,
    })
}

fn item(s: &mut Session, tr: &mut Trace, it: Item, ln: u64) {
    let kind = it.kind.as_deref().unwrap_or("unknown");
    let key = match kind {
        "CommandExecution" => "command".to_string(),
        "McpToolCall" => format!("mcp:{}", it.server.as_deref().unwrap_or("unknown")),
        "Extension" => format!(
            "extension:{}",
            it.action
                .as_ref()
                .and_then(|a| a.kind.as_deref())
                .unwrap_or("unknown")
        ),
        "FileChange" => "file_change".into(),
        "Reasoning" | "AgentMessage" | "UserMessage" | "ContextCompaction" => return,
        other => other.to_string(),
    };
    let failed = match kind {
        "CommandExecution" => {
            it.status.as_deref() == Some("failed") || it.exit_code.is_some_and(|c| c != 0)
        }
        "McpToolCall" => {
            it.status.as_deref().is_some_and(|s| s != "completed")
                || it
                    .result
                    .as_ref()
                    .and_then(|r| r.get("isError"))
                    .and_then(|e| e.as_bool())
                    == Some(true)
        }
        _ => it.status.as_deref() == Some("failed"),
    };
    let dur = it
        .duration
        .as_ref()
        .map(|d| d.secs.unwrap_or(0) * 1000 + d.nanos.unwrap_or(0) / 1_000_000);
    let op: &mut OpStat = s.operations.entry(key).or_default();
    op.count += 1;
    if failed {
        op.failed += 1;
    }
    if let Some(d) = dur {
        op.duration_ms_total += d;
        op.duration_ms_max = op.duration_ms_max.max(d);
    }
    match kind {
        "CommandExecution" | "McpToolCall" => tr.outcomes.push((failed, ln)),
        _ => {}
    }
    if kind == "CommandExecution" {
        if let Some(cmd) = it.command.as_ref().and_then(command_text) {
            tr.command(&cmd, ln);
        }
        for pc in it.parsed_cmd.iter().flatten() {
            if pc.kind.as_deref() == Some("read")
                && let Some(p) = &pc.path
            {
                tr.read(p, ln);
            }
        }
    }
    if kind == "Extension"
        && let Some(a) = &it.action
    {
        if let Some(host) = a.url.as_deref().and_then(host_of) {
            // The rollout records no HTTP status for page opens.
            let status = if failed { "error-other" } else { "no-status" };
            *s.web
                .entry(host)
                .or_default()
                .entry(status.into())
                .or_default() += 1;
        } else if a.kind.as_deref() == Some("search") {
            let status = if failed { "error-other" } else { "no-status" };
            *s.web
                .entry("(web-search)".into())
                .or_default()
                .entry(status.into())
                .or_default() += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str, source: &str) -> Thread {
        Thread {
            id: id.into(),
            rollout_path: "/nonexistent/rollout.jsonl".into(),
            created_ms: Some(0),
            updated_ms: Some(1000),
            source: source.into(),
            cwd: "/work/proj".into(),
            title: None,
            tokens_used: None,
            archived: false,
            model: Some("gpt-6.1-sol".into()),
            effort: Some("high".into()),
        }
    }

    static UTC: TimeZone = TimeZone::UTC;

    fn cx(p: &Pricing) -> Ctx<'_> {
        Ctx {
            pricing: p,
            tz: &UTC,
            include_titles: false,
            large_output_chars: 100_000,
            context_ratio: 0.9,
        }
    }

    /// A token_count line: cumulative (input, cached, output, reasoning) and last.
    fn tc(ord: u64, ts: &str, total: [u64; 4], last: [u64; 4]) -> String {
        let u = |v: [u64; 4]| {
            format!(
                r#"{{"input_tokens":{},"cached_input_tokens":{},"output_tokens":{},"reasoning_output_tokens":{},"total_tokens":{},"cache_write_input_tokens":0}}"#,
                v[0],
                v[1],
                v[2],
                v[3],
                v[0] + v[2]
            )
        };
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","ordinal":{ord},"payload":{{"type":"token_count","info":{{"total_token_usage":{},"last_token_usage":{},"model_context_window":400000}},"rate_limits":{{"primary":{{"used_percent":40.0,"window_minutes":10080}},"secondary":null,"plan_type":"plus","rate_limit_reached_type":null}}}}}}"#,
            u(total),
            u(last)
        )
    }

    fn meta(ord: u64, id: &str, extra: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","ordinal":{ord},"payload":{{"id":"{id}","originator":"codex_vscode","cwd":"/work/proj"{extra}}}}}"#
        )
    }

    fn rollout(lines: &[String]) -> Vec<u8> {
        let mut s = lines.join("\n");
        s.push('\n');
        s.into_bytes()
    }

    fn run(th: &Thread, bytes: &[u8]) -> Parsed {
        let p = Pricing::load(None).unwrap();
        parse_reader(
            th,
            &Rollout::Plain("/x.jsonl".into()),
            BufReader::new(bytes),
            &cx(&p),
        )
        .unwrap()
    }

    #[test]
    fn cumulative_counters_are_differenced_not_summed() {
        let th = thread("t1", "vscode");
        let b = rollout(&[
            meta(0, "t1", ""),
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","ordinal":1,"payload":{"turn_id":"u1","model":"gpt-6.1-sol","effort":"high"}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","ordinal":2,"payload":{"type":"task_started","turn_id":"u1"}}"#.into(),
            tc(3, "2026-01-01T00:00:02Z", [1000, 0, 100, 40], [1000, 0, 100, 40]),
            // Repeated snapshot with an unchanged total: not a new request.
            tc(4, "2026-01-01T00:00:02Z", [1000, 0, 100, 40], [1000, 0, 100, 40]),
            tc(5, "2026-01-01T00:00:03Z", [2500, 900, 150, 50], [1500, 900, 50, 10]),
            r#"{"timestamp":"2026-01-01T00:00:11Z","type":"event_msg","ordinal":6,"payload":{"type":"task_complete","turn_id":"u1","duration_ms":10000}}"#.into(),
        ]);
        let p = run(&th, &b);
        let t = p.session.total.as_ref().unwrap();
        assert_eq!(t.requests, 2);
        assert_eq!(t.tokens.input, 2500 - 900);
        assert_eq!(t.tokens.cache_read, 900);
        assert_eq!(t.tokens.output, 150);
        assert_eq!(t.tokens.reasoning, 50);
        assert_eq!(p.check.duplicate_events, 1);
        assert_eq!(p.check.own_total, 2650);
        assert_eq!(p.session.inherited_tokens, Some(0));
        assert_eq!(p.session.active_ms, Some(10_000));
        assert_eq!(p.session.turns, Some(1));
        assert_eq!(p.session.billing.mode, "subscription");
        assert_eq!(p.session.billing.plan_type.as_deref(), Some("plus"));
        // 1600 uncached x $2 + 900 cached x $0.10 + 150 out x $10, per million.
        let usd = p.session.cost.usd.unwrap();
        assert!((usd - (1600.0 * 2.0 + 900.0 * 0.1 + 150.0 * 10.0) / 1e6).abs() < 1e-12);
        assert_eq!(p.turn_ids, vec!["u1".to_string()]);
    }

    #[test]
    fn fork_baseline_is_inherited_not_counted() {
        let mut th = thread("t2", "vscode");
        th.tokens_used = Some(1_000_000 + 300);
        let b = rollout(&[
            meta(0, "t2", r#","forked_from_id":"t0""#),
            tc(
                1,
                "2026-01-01T00:00:02Z",
                [1_000_100, 0, 100, 0],
                [100, 0, 100, 0],
            ),
            tc(
                2,
                "2026-01-01T00:00:03Z",
                [1_000_200, 0, 100, 0],
                [100, 0, 0, 0],
            ),
        ]);
        let p = run(&th, &b);
        assert_eq!(p.session.inherited_tokens, Some(1_000_000));
        assert_eq!(p.check.own_total, 300);
        assert_eq!(p.session.total.as_ref().unwrap().tokens.total(), 300);
        assert_eq!(p.check.last_total, Some(1_000_300));
        assert_eq!(p.session.db_tokens_used, Some(1_000_300));
    }

    #[test]
    fn copied_parent_history_is_skipped() {
        let th = thread(
            "child",
            r#"{"subagent":{"thread_spawn":{"parent_thread_id":"par","depth":1,"agent_role":"worker"}}}"#,
        );
        let b = rollout(&[
            meta(0, "child", r#","subagent_history_start_ordinal":3"#),
            meta(1, "par", ""),
            r#"{"timestamp":"2025-12-31T00:00:00Z","type":"compacted","ordinal":2,"payload":{"message":""}}"#.into(),
            tc(3, "2026-01-01T00:00:02Z", [10, 0, 5, 0], [10, 0, 5, 0]),
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"compacted","ordinal":4,"payload":{"message":""}}"#.into(),
        ]);
        let p = run(&th, &b);
        assert_eq!(p.session.role, "subagent");
        assert_eq!(p.session.parent_id.as_deref(), Some("par"));
        assert_eq!(
            p.session.spawn.as_ref().unwrap().agent_type.as_deref(),
            Some("worker")
        );
        assert_eq!(p.session.transcript.as_ref().unwrap().inherited_lines, 2);
        assert_eq!(p.session.compactions.count, 1);
        assert_eq!(p.session.start.as_deref(), Some("2026-01-01T00:00:02Z"));
    }

    #[test]
    fn counter_decrease_is_flagged() {
        let th = thread("t3", "vscode");
        let b = rollout(&[
            tc(1, "2026-01-01T00:00:02Z", [100, 0, 10, 0], [100, 0, 10, 0]),
            tc(2, "2026-01-01T00:00:03Z", [50, 0, 10, 0], [50, 0, 10, 0]),
        ]);
        let p = run(&th, &b);
        assert_eq!(p.trace.counter_decrease_lines, vec![2]);
    }

    #[test]
    fn tools_operations_and_saturation() {
        let th = thread("t4", "vscode");
        let b = rollout(&[
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","ordinal":1,"payload":{"type":"custom_tool_call","name":"exec","call_id":"c1","input":"x"}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","ordinal":2,"payload":{"type":"item_completed","item":{"type":"CommandExecution","status":"failed","exit_code":1,"command":["/bin/zsh","-lc","sleep 5"],"parsed_cmd":[{"type":"unknown","cmd":"sleep 5"}],"duration":{"secs":5,"nanos":0}}}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","ordinal":3,"payload":{"type":"item_completed","item":{"type":"CommandExecution","status":"completed","exit_code":0,"command":["/bin/zsh","-lc","cat a.md"],"parsed_cmd":[{"type":"read","path":"a.md","name":"a.md","cmd":"cat a.md"}],"duration":{"secs":0,"nanos":2000000}}}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"response_item","ordinal":4,"payload":{"type":"custom_tool_call_output","call_id":"c1","output":[{"type":"input_text","text":"Script failed: boom"}]}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:05Z","type":"event_msg","ordinal":5,"payload":{"type":"item_completed","item":{"type":"Extension","action":{"type":"openPage","url":"https://Example.org/x"}}}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:06Z","type":"event_msg","ordinal":6,"payload":{"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":100.0},"secondary":null,"plan_type":"plus","rate_limit_reached_type":"workspace_member_usage_limit_reached"}}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:07Z","type":"event_msg","ordinal":7,"payload":{"type":"turn_aborted","reason":"interrupted"}}"#.into(),
        ]);
        let p = run(&th, &b);
        let exec = &p.session.tools["exec"];
        assert_eq!(
            (exec.calls, exec.results, exec.errors, exec.latency_ms_total),
            (1, 1, 1, 3000)
        );
        let cmd = &p.session.operations["command"];
        assert_eq!((cmd.count, cmd.failed, cmd.duration_ms_total), (2, 1, 5002));
        assert_eq!(p.trace.sleeps, 1);
        assert_eq!(p.trace.reads["a.md"].0, 1);
        assert_eq!(p.session.web["example.org"]["no-status"], 1);
        let rl = &p.session.rate_limits;
        assert_eq!(
            (rl.saturated_observations, rl.limit_reached_observations),
            (1, 1)
        );
        assert_eq!(p.session.aborts, 1);
        // No counters and tokens_used unknown: tokens stay unknown.
        assert!(p.session.total.is_none());
    }

    #[test]
    fn zstd_rollouts_and_missing_rollouts_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = dir.path().join("rollout-a.jsonl");
        let b = rollout(&[tc(1, "2026-01-01T00:00:02Z", [10, 0, 5, 0], [10, 0, 5, 0])]);
        let z = zstd::encode_all(&b[..], 3).unwrap();
        std::fs::write(dir.path().join("rollout-a.jsonl.zst"), z).unwrap();
        let r = resolve(recorded.to_str().unwrap(), None);
        assert!(matches!(r, Rollout::Zstd(_)));
        let mut th = thread("t5", "vscode");
        let p = Pricing::load(None).unwrap();
        let parsed = parse(&th, &r, &cx(&p)).unwrap();
        assert_eq!(parsed.session.transcript.as_ref().unwrap().status, "zstd");
        assert_eq!(parsed.session.total.as_ref().unwrap().tokens.total(), 15);

        // Archived copy.
        let arch = dir.path().join("archived");
        std::fs::create_dir(&arch).unwrap();
        std::fs::write(arch.join("rollout-b.jsonl"), &b).unwrap();
        let r = resolve(
            dir.path().join("rollout-b.jsonl").to_str().unwrap(),
            Some(&arch),
        );
        assert!(matches!(r, Rollout::Plain(ref p) if p.starts_with(&arch)));

        let r = resolve(dir.path().join("gone.jsonl").to_str().unwrap(), Some(&arch));
        assert!(matches!(r, Rollout::Missing(_)));
        th.tokens_used = Some(77);
        let parsed = parse(&th, &r, &cx(&p)).unwrap();
        let s = &parsed.session;
        assert_eq!(s.transcript.as_ref().unwrap().status, "missing");
        assert!(s.total.is_none());
        assert_eq!(s.db_tokens_used, Some(77));
        assert_eq!(s.cost.usd, None);
        assert_eq!(s.wall_ms, Some(1000));
    }
}
