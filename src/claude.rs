//! Claude Code transcripts: `<projects>/<encoded-cwd>/<session>.jsonl` and
//! subagent transcripts under `<session>/subagents/agent-<id>.jsonl`.
//!
//! One API response is written as several lines that share `requestId` and
//! `message.id` and repeat the same usage. Usage is therefore counted once
//! per request key (`requestId`, else `message.id`), across all files.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jiff::tz::TimeZone;
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;

use crate::detect::Trace;
use crate::pricing::{Pricing, RequestCost};
use crate::session::{Acc, Session, Spawn, Tokens, ToolStat, Transcript};
use crate::util::{day, display_path, host_of, parse_ts, tilde};

#[derive(Debug, Clone)]
pub enum Kind {
    Main,
    Subagent { parent: String, meta: PathBuf },
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    /// Session id (main) or agent file stem (subagent).
    pub stem: String,
    pub kind: Kind,
}

/// Lists every transcript under the projects directory: per project
/// directory, main transcripts sorted by path, then subagent transcripts
/// sorted by path. Requests seen in several files are credited to the first,
/// so a main transcript wins over its subagents.
pub fn discover(projects: &Path) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    let rd =
        std::fs::read_dir(projects).with_context(|| format!("listing {}", projects.display()))?;
    let mut dirs: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("listing {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        let mut subs = Vec::new();
        for p in entries {
            if p.extension().is_some_and(|e| e == "jsonl") && p.is_file() {
                let stem = p
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                out.push(Candidate {
                    path: p.clone(),
                    stem,
                    kind: Kind::Main,
                });
            } else if p.is_dir() {
                let parent = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let sub = p.join("subagents");
                if !sub.is_dir() {
                    continue;
                }
                let mut files: Vec<PathBuf> = std::fs::read_dir(&sub)
                    .with_context(|| format!("listing {}", sub.display()))?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|f| f.extension().is_some_and(|e| e == "jsonl"))
                    .collect();
                files.sort();
                for f in files {
                    let stem = f
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    let meta = f.with_extension("meta.json");
                    subs.push(Candidate {
                        path: f,
                        stem,
                        kind: Kind::Subagent {
                            parent: parent.clone(),
                            meta,
                        },
                    });
                }
            }
        }
        out.append(&mut subs);
    }
    Ok(out)
}

/// The byte pattern every in-project record contains; a cheap pre-filter.
pub fn cwd_needle(project: &str) -> String {
    let quoted = serde_json::to_string(project).expect("string serialises");
    format!("\"cwd\":{}", &quoted[..quoted.len() - 1])
}

#[derive(Deserialize, Default)]
struct Rec {
    #[serde(rename = "type")]
    kind: Option<String>,
    subtype: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    uuid: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    entrypoint: Option<String>,
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    #[serde(rename = "isCompactSummary")]
    is_compact_summary: Option<bool>,
    #[serde(rename = "isVisibleInTranscriptOnly")]
    visible_only: Option<bool>,
    #[serde(rename = "isApiErrorMessage")]
    is_api_error: Option<bool>,
    error: Option<Value>,
    #[serde(rename = "apiError")]
    api_error: Option<Value>,
    #[serde(rename = "apiErrorStatus")]
    api_error_status: Option<Value>,
    #[serde(rename = "quotaLimits")]
    quota: Option<Quota>,
    effort: Option<String>,
    message: Option<Msg>,
    #[serde(rename = "toolUseResult")]
    tool_use_result: Option<Box<RawValue>>,
    #[serde(rename = "compactMetadata")]
    compact: Option<CompactMeta>,
    #[serde(rename = "totalCostUSD")]
    total_cost_usd: Option<f64>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
}

#[derive(Deserialize, Default)]
struct Quota {
    #[serde(rename = "rateLimitType")]
    rate_limit_type: Option<String>,
}

#[derive(Deserialize, Default)]
struct CompactMeta {
    trigger: Option<String>,
}

#[derive(Deserialize, Default)]
struct Msg {
    id: Option<String>,
    model: Option<String>,
    content: Option<Box<RawValue>>,
    usage: Option<Usage>,
}

#[derive(Deserialize, Default, Clone)]
struct Usage {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation: Option<CacheCreation>,
    output_tokens_details: Option<OutputDetails>,
    server_tool_use: Option<ServerToolUse>,
    speed: Option<String>,
    service_tier: Option<String>,
    inference_geo: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
struct CacheCreation {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Deserialize, Default, Clone)]
struct OutputDetails {
    thinking_tokens: Option<u64>,
}

#[derive(Deserialize, Default, Clone)]
struct ServerToolUse {
    web_search_requests: Option<u64>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<Value>,
    tool_use_id: Option<String>,
    is_error: Option<bool>,
    content: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct InnerBlock {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

#[derive(Deserialize, Default)]
struct ToolUseResult {
    code: Option<Value>,
}

#[derive(Deserialize, Default)]
struct SubMeta {
    #[serde(rename = "agentType")]
    agent_type: Option<String>,
    #[serde(rename = "toolUseId")]
    tool_use_id: Option<String>,
    #[serde(rename = "spawnDepth")]
    spawn_depth: Option<u64>,
}

impl Usage {
    fn tokens(&self) -> Tokens {
        let total_write = self.cache_creation_input_tokens.unwrap_or(0);
        let (w5, w1h) = match &self.cache_creation {
            Some(c) => (
                c.ephemeral_5m_input_tokens.unwrap_or(0),
                c.ephemeral_1h_input_tokens.unwrap_or(0),
            ),
            None => (0, 0),
        };
        Tokens {
            input: self.input_tokens.unwrap_or(0),
            cache_write_5m: w5,
            cache_write_1h: w1h,
            cache_write_unknown: total_write.saturating_sub(w5 + w1h),
            cache_read: self.cache_read_input_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
            reasoning: self
                .output_tokens_details
                .as_ref()
                .and_then(|d| d.thinking_tokens)
                .unwrap_or(0),
        }
    }
}

struct Request {
    key: String,
    model: String,
    usage: Usage,
    tokens: Tokens,
    effort: Option<String>,
    ts: Option<i64>,
    line: u64,
}

struct Pending {
    name: String,
    ts: Option<i64>,
    url: Option<String>,
}

/// Totals for the cross-check: naive per-line sums versus deduplicated sums.
#[derive(Default, Debug, Clone, serde::Serialize)]
pub struct Stats {
    pub files: u64,
    pub lines: u64,
    pub bad_lines: u64,
    pub assistant_lines: u64,
    pub synthetic_lines: u64,
    pub naive: Tokens,
    pub dedup: Tokens,
    pub requests: u64,
    pub usage_conflicts: u64,
    pub cross_file_duplicates: u64,
}

pub struct Ctx<'a> {
    pub pricing: &'a Pricing,
    pub tz: &'a TimeZone,
    pub include_titles: bool,
    pub large_output_chars: u64,
    pub context_ratio: f64,
    /// Request key -> session key of its first owner.
    pub seen: &'a mut HashMap<String, String>,
    pub stats: &'a mut Stats,
}

pub struct Parsed {
    pub session: Session,
    pub trace: Trace,
    /// Tool-use ids issued by this session (for subagent parent links).
    pub tool_use_ids: Vec<String>,
    /// Record uuids (for T3 turn references).
    pub uuids: Vec<String>,
    pub first_cwd: Option<String>,
    pub start_ms: Option<i64>,
    /// Parent tool-use id from the subagent meta file.
    pub parent_tool_use: Option<String>,
}

fn result_text(raw: &RawValue) -> (u64, String) {
    let s = raw.get();
    if s.starts_with('"') {
        let t: String = serde_json::from_str(s).unwrap_or_default();
        (t.chars().count() as u64, t)
    } else {
        let blocks: Vec<InnerBlock> = serde_json::from_str(s).unwrap_or_default();
        let mut text = String::new();
        for b in blocks {
            if b.kind.as_deref() == Some("text")
                && let Some(t) = b.text
            {
                text.push_str(&t);
            }
        }
        (text.chars().count() as u64, text)
    }
}

/// Failure class of a web tool error message.
pub fn classify_web_error(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if let Some(code) = find_status(&lower) {
        return code;
    }
    if lower.contains("cloudflare") || lower.contains("just a moment") {
        "cloudflare-challenge".into()
    } else if lower.contains("enotfound") || lower.contains("getaddrinfo") {
        "dns".into()
    } else if lower.contains("econnrefused") {
        "connect-refused".into()
    } else if lower.contains("socket hang up") || lower.contains("econnreset") {
        "connection-reset".into()
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "timeout".into()
    } else if lower.contains("certificate") || lower.contains("ssl") || lower.contains("tls") {
        "tls".into()
    } else {
        "error-other".into()
    }
}

fn find_status(lower: &str) -> Option<String> {
    for marker in ["status code ", "http ", "status: "] {
        if let Some(i) = lower.find(marker) {
            let rest = &lower[i + marker.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.len() == 3 {
                return Some(digits);
            }
        }
    }
    None
}

fn is_challenge_page(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("cloudflare")
        && (lower.contains("just a moment") || lower.contains("attention required"))
}

/// The range a `Read` call covers, as a file-reread key: `whole` without
/// offset, limit or pages; `lines a-b` for a numeric limit and optional
/// numeric offset (the first line, 1-based), as a shell `sed -n 'a,bp'`
/// read keys; otherwise those values.
fn read_range(input: Option<&Value>) -> String {
    let get = |k: &str| {
        input
            .and_then(|v| v.get(k))
            .filter(|v| !v.is_null())
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            })
    };
    let (offset, limit, pages) = (get("offset"), get("limit"), get("pages"));
    let num = |s: &Option<String>| s.as_deref().and_then(|s| s.parse::<u64>().ok());
    if pages.is_none()
        && let Some(n) = num(&limit).filter(|&n| n > 0)
        && (offset.is_none() || num(&offset).is_some())
    {
        let a = num(&offset).unwrap_or(1).max(1);
        return format!("lines {a}-{}", a.saturating_add(n - 1));
    }
    match (offset, limit, pages) {
        (None, None, None) => "whole".into(),
        (o, l, p) => format!(
            "offset {} limit {} pages {}",
            o.unwrap_or_default(),
            l.unwrap_or_default(),
            p.unwrap_or_default()
        ),
    }
}

/// The file and range a Bash command reads, keyed like a `Read` of the same
/// lines: one simple `cat`, `nl` or `sed -n` command (see
/// `util::read_command`) with one file operand, resolved against `cwd`.
fn shell_read(cmd: &str, cwd: Option<&str>) -> Option<(String, String)> {
    let words = crate::util::simple_command_words(cmd)?;
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let (range, files) = crate::util::read_command(&words)?;
    let [file] = files.as_slice() else {
        return None;
    };
    Some((crate::util::resolve_path(cwd, file)?, range))
}

fn is_poll_tool(name: &str) -> bool {
    matches!(name, "TaskOutput" | "BashOutput") || name.ends_with("task_status")
}

fn text_of_user_content(raw: &RawValue) -> (bool, bool, String) {
    // Returns (has_text, has_tool_result, first text).
    let s = raw.get();
    if s.starts_with('"') {
        let t: String = serde_json::from_str(s).unwrap_or_default();
        return (!t.is_empty(), false, t);
    }
    let blocks: Vec<InnerBlock> = serde_json::from_str(s).unwrap_or_default();
    let mut has_text = false;
    let mut has_result = false;
    let mut first = String::new();
    for b in blocks {
        match b.kind.as_deref() {
            Some("text") => {
                if !has_text {
                    first = b.text.unwrap_or_default();
                }
                has_text = true;
            }
            Some("tool_result") => has_result = true,
            _ => {}
        }
    }
    (has_text, has_result, first)
}

/// A transcript's bytes as text. A line that is not valid UTF-8 (a damaged
/// or truncated file) is replaced by U+FFFD, which is not JSON, so the
/// parser counts it as a bad line; line numbers are kept. Returns the text
/// and the number of lines replaced.
pub fn utf8_lines(bytes: Vec<u8>) -> (String, u64) {
    let bytes = match String::from_utf8(bytes) {
        Ok(s) => return (s, 0),
        Err(e) => e.into_bytes(),
    };
    let mut out = String::with_capacity(bytes.len());
    let mut bad = 0;
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match std::str::from_utf8(line) {
            Ok(l) => out.push_str(l),
            Err(_) => {
                bad += 1;
                out.push('\u{fffd}');
            }
        }
    }
    (out, bad)
}

/// Parses one transcript already read into memory.
pub fn parse(c: &Candidate, text: &str, cx: &mut Ctx) -> Parsed {
    let (id, parent) = match &c.kind {
        Kind::Main => (c.stem.clone(), None),
        Kind::Subagent { parent, .. } => (format!("{parent}/{}", c.stem), Some(parent.clone())),
    };
    let mut s = Session::new("claude", &id);
    let mut tr = Trace {
        evidence: display_path(&c.path),
        ..Trace::default()
    };
    let mut parent_tool_use = None;
    if let Kind::Subagent { meta, .. } = &c.kind {
        s.role = "subagent".into();
        s.parent_id = parent.clone();
        let m: SubMeta = std::fs::read_to_string(meta)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if !meta.exists() {
            s.notes.push("subagent meta file missing".into());
        }
        parent_tool_use = m.tool_use_id;
        s.spawn = Some(Spawn {
            mechanism: "claude-task".into(),
            agent_type: m.agent_type,
            depth: m.spawn_depth,
        });
    }
    cx.stats.files += 1;

    let mut transcript = Transcript {
        path: Some(display_path(&c.path)),
        status: "present".into(),
        ..Transcript::default()
    };
    let mut requests: Vec<Request> = Vec::new();
    let mut req_index: HashMap<String, usize> = HashMap::new();
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut tool_ids: HashSet<String> = HashSet::new();
    let mut tool_use_ids = Vec::new();
    let mut uuids = Vec::new();
    let mut first_cwd: Option<String> = None;
    let mut start: Option<i64> = None;
    let mut end: Option<i64> = None;
    let mut turn_start: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut turns = 0u64;
    let mut active = 0i64;
    let mut timed = 0u64;
    let mut orphan_results = 0u64;
    let mut session_id_mismatch = 0u64;
    let mut title_custom: Option<String> = None;
    let mut title_ai: Option<String> = None;

    for (i, line) in text.lines().enumerate() {
        let ln = i as u64 + 1;
        if line.trim().is_empty() {
            continue;
        }
        transcript.lines += 1;
        cx.stats.lines += 1;
        let r: Rec = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => {
                transcript.bad_lines += 1;
                cx.stats.bad_lines += 1;
                continue;
            }
        };
        let kind = r.kind.as_deref().unwrap_or("");
        if let Some(u) = &r.uuid {
            uuids.push(u.clone());
        }
        if first_cwd.is_none()
            && let Some(cwd) = &r.cwd
        {
            first_cwd = Some(cwd.clone());
        }
        if matches!(c.kind, Kind::Main)
            && matches!(kind, "user" | "assistant")
            && r.session_id.as_deref().is_some_and(|sid| sid != c.stem)
        {
            session_id_mismatch += 1;
        }
        if s.client.is_none() {
            s.client = r.entrypoint.clone();
        }
        let ts = r.timestamp.as_deref().and_then(parse_ts);
        let timeline = matches!(kind, "user" | "assistant" | "system");
        if timeline && let Some(t) = ts {
            tr.times.push(t);
            start = Some(start.map_or(t, |s| s.min(t)));
            end = Some(end.map_or(t, |e| e.max(t)));
        }

        match kind {
            "assistant" => {
                let Some(msg) = &r.message else { continue };
                // A turn's work ends at its last assistant or tool-result
                // record; meta and system records (hooks, local commands)
                // do not extend it.
                if ts.is_some() {
                    last_ts = ts;
                }
                let model = msg.model.clone().unwrap_or_else(|| "unknown".into());
                cx.stats.assistant_lines += 1;
                if r.is_api_error == Some(true) {
                    let err = r.error.as_ref().and_then(|v| v.as_str()).unwrap_or("error");
                    let api = r.api_error.as_ref().and_then(|v| v.as_str());
                    let key = match api {
                        Some(a) => format!("{err}:{a}"),
                        None => match r.api_error_status.as_ref().and_then(|v| v.as_u64()) {
                            Some(st) => format!("{err}:{st}"),
                            None => err.to_string(),
                        },
                    };
                    *s.api_errors.entry(key).or_default() += 1;
                    tr.api_error_lines.push(ln);
                    if err == "rate_limit" || api == Some("usage_limit_reached") {
                        s.rate_limits.limit_errors += 1;
                        tr.rate_limit_lines.push(ln);
                        let lt = r
                            .quota
                            .as_ref()
                            .and_then(|q| q.rate_limit_type.clone())
                            .unwrap_or_else(|| "unknown".into());
                        *s.rate_limits.limit_types.entry(lt).or_default() += 1;
                    }
                }
                if model == "<synthetic>" {
                    cx.stats.synthetic_lines += 1;
                } else if let Some(u) = &msg.usage {
                    let tokens = u.tokens();
                    cx.stats.naive.add(&tokens);
                    let key = r
                        .request_id
                        .clone()
                        .or_else(|| msg.id.clone())
                        .or_else(|| r.uuid.clone())
                        .unwrap_or_else(|| format!("{id}#line{ln}"));
                    match req_index.get(&key) {
                        Some(&ix) => {
                            let prev = &mut requests[ix];
                            if prev.tokens != tokens {
                                tr.usage_conflicts += 1;
                                cx.stats.usage_conflicts += 1;
                                if tokens.output > prev.tokens.output {
                                    prev.tokens = tokens;
                                    prev.usage = u.clone();
                                }
                            }
                        }
                        None => {
                            req_index.insert(key.clone(), requests.len());
                            requests.push(Request {
                                key,
                                model: model.clone(),
                                usage: u.clone(),
                                tokens,
                                effort: r.effort.clone(),
                                ts,
                                line: ln,
                            });
                        }
                    }
                }
                if turn_start.is_none()
                    && let Some(t) = ts
                {
                    // Activity before any prompt (e.g. a resumed session).
                    turn_start = Some(t);
                    turns += 1;
                }
                if let Some(content) = &msg.content {
                    let blocks: Vec<Block> =
                        serde_json::from_str(content.get()).unwrap_or_default();
                    for b in blocks {
                        if b.kind.as_deref() != Some("tool_use") {
                            continue;
                        }
                        let Some(tid) = b.id.clone() else { continue };
                        if !tool_ids.insert(tid.clone()) {
                            continue;
                        }
                        tool_use_ids.push(tid.clone());
                        let name = b.name.clone().unwrap_or_else(|| "unknown".into());
                        s.tools.entry(name.clone()).or_default().calls += 1;
                        let input = b.input.as_ref();
                        let field = |k: &str| input.and_then(|v| v.get(k)).and_then(|v| v.as_str());
                        match name.as_str() {
                            "Read" => {
                                if let Some(p) = field("file_path") {
                                    tr.read(p, &read_range(input), ln);
                                }
                            }
                            "Bash" => {
                                if let Some(cmd) = field("command") {
                                    tr.command(cmd, ln);
                                    let cwd = r.cwd.as_deref().or(first_cwd.as_deref());
                                    if let Some((p, range)) = shell_read(cmd, cwd) {
                                        tr.read(&p, &range, ln);
                                    }
                                }
                            }
                            _ => {}
                        }
                        if is_poll_tool(&name) {
                            *tr.polls.entry(name.clone()).or_default() += 1;
                        }
                        pending.insert(
                            tid,
                            Pending {
                                name,
                                ts,
                                url: field("url").map(str::to_string),
                            },
                        );
                    }
                }
            }
            "user" => {
                let Some(msg) = &r.message else { continue };
                let Some(content) = &msg.content else {
                    continue;
                };
                let (has_text, has_result, first) = text_of_user_content(content);
                if has_text && !has_result {
                    if first.starts_with("[Request interrupted by user") {
                        s.aborts += 1;
                        // The agent worked until it was interrupted.
                        if ts.is_some() {
                            last_ts = ts;
                        }
                    } else if r.is_meta != Some(true)
                        && r.is_compact_summary != Some(true)
                        && r.visible_only != Some(true)
                        && let Some(t) = ts
                    {
                        if let (Some(st), Some(lt)) = (turn_start, last_ts) {
                            active += (lt - st).max(0);
                            timed += 1;
                            tr.intervals.push((st, lt.max(st)));
                        }
                        turn_start = Some(t);
                        last_ts = None;
                        turns += 1;
                    }
                }
                if has_result {
                    if ts.is_some() {
                        last_ts = ts;
                    }
                    let blocks: Vec<Block> =
                        serde_json::from_str(content.get()).unwrap_or_default();
                    let tur: Option<ToolUseResult> = r
                        .tool_use_result
                        .as_ref()
                        .filter(|raw| raw.get().starts_with('{'))
                        .and_then(|raw| serde_json::from_str(raw.get()).ok());
                    for b in blocks {
                        if b.kind.as_deref() != Some("tool_result") {
                            continue;
                        }
                        let Some(tid) = b.tool_use_id.as_deref() else {
                            continue;
                        };
                        let Some(p) = pending.remove(tid) else {
                            orphan_results += 1;
                            continue;
                        };
                        let is_err = b.is_error == Some(true);
                        let (chars, body) =
                            b.content.as_deref().map(result_text).unwrap_or_default();
                        let stat: &mut ToolStat = s.tools.entry(p.name.clone()).or_default();
                        stat.results += 1;
                        if is_err {
                            stat.errors += 1;
                        }
                        if let (Some(a), Some(b)) = (p.ts, ts) {
                            let lat = (b - a).max(0) as u64;
                            stat.latency_ms_total += lat;
                            stat.latency_ms_max = stat.latency_ms_max.max(lat);
                        }
                        stat.result_chars_total += chars;
                        stat.result_chars_max = stat.result_chars_max.max(chars);
                        tr.outcomes.push((is_err, ln));
                        if chars >= cx.large_output_chars {
                            tr.large_outputs.push((p.name.clone(), chars, ln));
                        }
                        match p.name.as_str() {
                            "WebFetch" => {
                                let host = p
                                    .url
                                    .as_deref()
                                    .and_then(host_of)
                                    .unwrap_or_else(|| "(unknown-host)".into());
                                let code = tur
                                    .as_ref()
                                    .and_then(|t| t.code.as_ref())
                                    .and_then(|c| c.as_u64());
                                let status = if is_challenge_page(&body) {
                                    "cloudflare-challenge".to_string()
                                } else if let Some(code) = code {
                                    code.to_string()
                                } else if is_err {
                                    classify_web_error(&body)
                                } else {
                                    "ok-no-status".to_string()
                                };
                                *s.web.entry(host).or_default().entry(status).or_default() += 1;
                            }
                            "WebSearch" => {
                                let status = if is_err { "error-other" } else { "ok" };
                                *s.web
                                    .entry("(web-search)".into())
                                    .or_default()
                                    .entry(status.into())
                                    .or_default() += 1;
                            }
                            _ => {}
                        }
                    }
                }
            }
            "system" => {
                if r.subtype.as_deref() == Some("compact_boundary") {
                    s.compactions.count += 1;
                    match r.compact.as_ref().and_then(|m| m.trigger.as_deref()) {
                        Some("auto") => s.compactions.auto += 1,
                        Some("manual") => s.compactions.manual += 1,
                        _ => s.compactions.unknown += 1,
                    }
                }
            }
            "cost-state" => s.claude_code_cost_usd = r.total_cost_usd.or(s.claude_code_cost_usd),
            "custom-title" => title_custom = r.custom_title.clone().or(title_custom),
            "ai-title" => title_ai = r.ai_title.clone().or(title_ai),
            _ => {}
        }
    }
    if let (Some(st), Some(lt)) = (turn_start, last_ts) {
        active += (lt - st).max(0);
        timed += 1;
        tr.intervals.push((st, lt.max(st)));
    }
    s.open_turns = pending.len() as u64;
    if !pending.is_empty() {
        s.notes
            .push(format!("{} tool call(s) without a result", pending.len()));
    }
    if orphan_results > 0 {
        s.notes.push(format!(
            "{orphan_results} tool result(s) without a matching call"
        ));
    }
    if session_id_mismatch > 0 {
        s.notes.push(format!(
            "{session_id_mismatch} record(s) name a different sessionId than the file"
        ));
    }
    if transcript.bad_lines > 0 {
        s.notes.push(format!(
            "{} unparseable line(s) skipped",
            transcript.bad_lines
        ));
    }

    // Account requests once, globally.
    let mut total = Acc::default();
    let mut server_usd = 0.0;
    let search_price = cx
        .pricing
        .server_tool("anthropic", "web_search")
        .map(|t| t.usd_per_1000 / 1000.0);
    let mut cross_dups = 0u64;
    for rq in &requests {
        if let Some(owner) = cx.seen.get(&rq.key) {
            if owner != &s.key() {
                cross_dups += 1;
            }
            continue;
        }
        cx.seen.insert(rq.key.clone(), s.key());
        cx.stats.requests += 1;
        cx.stats.dedup.add(&rq.tokens);
        let u = &rq.usage;
        let multiplier = match u.inference_geo.as_deref() {
            None | Some("global") | Some("") => Some(1.0),
            Some("us") => Some(1.1),
            Some(_) => None,
        };
        let standard = u.speed.as_deref().is_none_or(|v| v == "standard")
            && u.service_tier.as_deref().is_none_or(|v| v == "standard");
        let searches = u
            .server_tool_use
            .as_ref()
            .and_then(|t| t.web_search_requests)
            .unwrap_or(0);
        let search_usd = if searches == 0 {
            Some(0.0)
        } else {
            search_price.map(|p| p * searches as f64)
        };
        let usd = match (multiplier, standard, search_usd) {
            (Some(m), true, Some(su)) => cx
                .pricing
                .request_cost(&rq.model, &rq.tokens, rq.tokens.prompt(), m)
                .map(|c| RequestCost {
                    usd: c.usd + su,
                    ..c
                }),
            _ => None,
        };
        if usd.is_some() {
            server_usd += search_usd.unwrap_or(0.0);
        }
        total.add_request(&rq.tokens, usd);
        s.by_model
            .entry(rq.model.clone())
            .or_default()
            .add_request(&rq.tokens, usd);
        s.by_effort
            .entry(rq.effort.clone().unwrap_or_else(|| "unknown".into()))
            .or_default()
            .add_request(&rq.tokens, usd);
        let d = rq
            .ts
            .map(|t| day(t, cx.tz))
            .unwrap_or_else(|| "unknown".into());
        s.by_day.entry(d).or_default().add_request(&rq.tokens, usd);
        let prompt = rq.tokens.prompt();
        if usd.is_some_and(|c| c.long_context) {
            tr.long_context.push((prompt, rq.line));
        }
        s.max_prompt_tokens = Some(s.max_prompt_tokens.unwrap_or(0).max(prompt));
        if let Some(w) = cx.pricing.model(&rq.model).and_then(|m| m.context_window) {
            s.context_window = Some(w);
            if prompt as f64 >= cx.context_ratio * w as f64 {
                tr.near_context.push((prompt, w, rq.line));
            }
        }
    }
    if cross_dups > 0 {
        cx.stats.cross_file_duplicates += cross_dups;
        s.notes.push(format!(
            "{cross_dups} request(s) already counted in another transcript were skipped"
        ));
    }
    s.total = Some(total);
    s.token_basis =
        "claude: message.usage counted once per requestId (else message.id) across all transcripts"
            .into();
    s.turns = (turns > 0).then_some(turns);
    s.active_ms = (timed > 0).then_some(active);
    s.active_basis = "claude: sum over turns of (last assistant or tool-result record time - prompt time); a turn starts at a non-tool-result user prompt; meta and system records do not extend a turn".into();
    s.error_basis = "claude: tool_result is_error flag".into();
    s.set_span(start, end);
    s.cwd = first_cwd.as_deref().map(tilde);
    if cx.include_titles {
        s.title = title_custom.or(title_ai);
    }
    let basis = cx.pricing.basis("anthropic");
    s.finish_cost(&basis, server_usd);
    s.transcript = Some(transcript);

    Parsed {
        session: s,
        trace: tr,
        tool_use_ids,
        uuids,
        first_cwd,
        start_ms: start,
        parent_tool_use,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: &str = r#"{"type":"user","uuid":"u1","sessionId":"s1","cwd":"/work/proj","entrypoint":"cli","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"start"}}
{"type":"assistant","uuid":"a1","requestId":"r1","sessionId":"s1","cwd":"/work/proj","effort":"high","timestamp":"2026-01-01T00:00:02Z","message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"thinking","thinking":""}],"usage":{"input_tokens":10,"cache_creation_input_tokens":300,"cache_read_input_tokens":1000,"output_tokens":50,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":200},"output_tokens_details":{"thinking_tokens":20},"service_tier":"standard","speed":"standard","inference_geo":"global"}}}
{"type":"assistant","uuid":"a2","requestId":"r1","sessionId":"s1","cwd":"/work/proj","effort":"high","timestamp":"2026-01-01T00:00:03Z","message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"text","text":"x"}],"usage":{"input_tokens":10,"cache_creation_input_tokens":300,"cache_read_input_tokens":1000,"output_tokens":50,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":200},"output_tokens_details":{"thinking_tokens":20},"service_tier":"standard","speed":"standard","inference_geo":"global"}}}
{"type":"assistant","uuid":"a3","requestId":"r1","sessionId":"s1","cwd":"/work/proj","effort":"high","timestamp":"2026-01-01T00:00:04Z","message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"tu1","name":"Read","input":{"file_path":"/work/proj/a.md"}}],"usage":{"input_tokens":10,"cache_creation_input_tokens":300,"cache_read_input_tokens":1000,"output_tokens":50,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":200},"output_tokens_details":{"thinking_tokens":20},"service_tier":"standard","speed":"standard","inference_geo":"global"}}}
{"type":"user","uuid":"u2","sessionId":"s1","cwd":"/work/proj","timestamp":"2026-01-01T00:00:05Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"hello"}]},"toolUseResult":{"type":"text"}}
{"type":"assistant","uuid":"a4","requestId":"r2","sessionId":"s1","cwd":"/work/proj","effort":"high","timestamp":"2026-01-01T00:00:06Z","message":{"id":"m2","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"tu2","name":"WebFetch","input":{"url":"https://Blocked.example/page","prompt":"p"}}],"usage":{"input_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":2000,"output_tokens":10,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}}}
{"type":"user","uuid":"u3","sessionId":"s1","cwd":"/work/proj","timestamp":"2026-01-01T00:00:08Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu2","content":"Forbidden"}]},"toolUseResult":{"code":403,"codeText":"Forbidden","bytes":10,"durationMs":5}}
{"type":"system","subtype":"compact_boundary","uuid":"c1","sessionId":"s1","cwd":"/work/proj","timestamp":"2026-01-01T00:00:09Z","compactMetadata":{"trigger":"auto","preTokens":900000}}
{"type":"user","uuid":"u4","sessionId":"s1","cwd":"/work/proj","isCompactSummary":true,"timestamp":"2026-01-01T00:00:09Z","message":{"role":"user","content":"summary"}}
{"type":"user","uuid":"u5","sessionId":"s1","cwd":"/work/proj","timestamp":"2026-01-01T00:01:00Z","message":{"role":"user","content":[{"type":"text","text":"next"}]}}
{"type":"assistant","uuid":"a5","sessionId":"s1","cwd":"/work/proj","isApiErrorMessage":true,"error":"rate_limit","apiError":"usage_limit_reached","apiErrorStatus":429,"quotaLimits":{"status":"rejected","rateLimitType":"five_hour"},"timestamp":"2026-01-01T00:01:01Z","message":{"id":"x","model":"<synthetic>","content":[{"type":"text","text":"limit"}],"usage":{"input_tokens":0,"output_tokens":0}}}
{"type":"user","uuid":"u6","sessionId":"s1","cwd":"/work/proj","timestamp":"2026-01-01T00:01:02Z","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}
{"type":"cost-state","totalCostUSD":1.23}
this line is not json
"#;

    const SUB: &str = r#"{"type":"assistant","uuid":"b1","requestId":"r3","sessionId":"s1","isSidechain":true,"agentId":"a1","cwd":"/work/proj","timestamp":"2026-01-01T00:00:07Z","message":{"id":"m3","model":"claude-haiku-5-5","content":[{"type":"text","text":"y"}],"usage":{"input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":100}}}
{"type":"assistant","uuid":"a4","requestId":"r2","sessionId":"s1","isSidechain":true,"cwd":"/work/proj","timestamp":"2026-01-01T00:00:06Z","message":{"id":"m2","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":2000,"output_tokens":10}}}
"#;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("-work-proj");
        std::fs::create_dir_all(proj.join("s1/subagents")).unwrap();
        std::fs::write(proj.join("s1.jsonl"), MAIN).unwrap();
        std::fs::write(proj.join("s1/subagents/agent-a1.jsonl"), SUB).unwrap();
        std::fs::write(
            proj.join("s1/subagents/agent-a1.meta.json"),
            r#"{"agentType":"Explore","toolUseId":"tu9","spawnDepth":1,"model":"claude-haiku-5-5"}"#,
        )
        .unwrap();
        d
    }

    fn parse_all(d: &Path) -> (Vec<Parsed>, Stats) {
        let pricing = Pricing::load(None).unwrap();
        let tz = TimeZone::UTC;
        let mut seen = HashMap::new();
        let mut stats = Stats::default();
        let mut cx = Ctx {
            pricing: &pricing,
            tz: &tz,
            include_titles: false,
            large_output_chars: 100_000,
            context_ratio: 0.9,
            seen: &mut seen,
            stats: &mut stats,
        };
        let mut out = Vec::new();
        for c in discover(d).unwrap() {
            let text = std::fs::read_to_string(&c.path).unwrap();
            out.push(parse(&c, &text, &mut cx));
        }
        (out, stats)
    }

    #[test]
    fn duplicated_usage_lines_count_once() {
        let d = tree();
        let (parsed, stats) = parse_all(d.path());
        assert_eq!(parsed.len(), 2);
        let main = &parsed[0].session;
        assert_eq!(main.id, "s1");
        let t = main.total.as_ref().unwrap();
        assert_eq!(t.requests, 2);
        assert_eq!(t.tokens.input, 15);
        assert_eq!(t.tokens.cache_write_5m, 100);
        assert_eq!(t.tokens.cache_write_1h, 200);
        assert_eq!(t.tokens.cache_read, 3000);
        assert_eq!(t.tokens.output, 60);
        assert_eq!(t.tokens.reasoning, 20);
        // r1 appears on three lines, r2 in two files: naive sums inflate.
        assert_eq!(stats.naive.output, 50 * 3 + 10 * 2 + 100);
        assert_eq!(stats.dedup.output, 60 + 100);
        assert_eq!(stats.requests, 3);
        assert_eq!(stats.cross_file_duplicates, 1);
        assert_eq!(stats.usage_conflicts, 0);
        // r1: 10x4 + 100x5 + 200x8 + 1000x0.2 + 50x20; r2: 5x4 + 2000x0.2 + 10x20.
        let expect = (40.0 + 500.0 + 1600.0 + 200.0 + 1000.0 + 20.0 + 400.0 + 200.0) / 1e6;
        assert!((main.cost.usd.unwrap() - expect).abs() < 1e-12);
        assert_eq!(main.claude_code_cost_usd, Some(1.23));
        assert_eq!(main.transcript.as_ref().unwrap().bad_lines, 1);
    }

    #[test]
    fn turns_compaction_errors_and_tools() {
        let d = tree();
        let (parsed, _) = parse_all(d.path());
        let s = &parsed[0].session;
        assert_eq!(s.turns, Some(2));
        // 0:00 -> 0:08 (the last tool result; the compaction records at
        // 0:09 do not extend the turn) and 1:00 -> 1:02 (interrupted).
        assert_eq!(s.active_ms, Some(10_000));
        assert_eq!(s.wall_ms, Some(62_000));
        assert_eq!(s.compactions.count, 1);
        assert_eq!(s.compactions.auto, 1);
        assert_eq!(s.aborts, 1);
        assert_eq!(s.rate_limits.limit_errors, 1);
        assert_eq!(s.rate_limits.limit_types["five_hour"], 1);
        assert_eq!(s.api_errors["rate_limit:usage_limit_reached"], 1);
        assert_eq!(s.web["blocked.example"]["403"], 1);
        assert_eq!(s.tools["Read"].results, 1);
        assert_eq!(s.tools["Read"].latency_ms_total, 1000);
        assert_eq!(s.tools["WebFetch"].calls, 1);
        assert_eq!(parsed[0].trace.reads["/work/proj/a.md"].reads, 1);
        assert_eq!(s.client.as_deref(), Some("cli"));
        assert_eq!(s.by_effort["high"].requests, 2);
    }

    #[test]
    fn meta_and_system_records_do_not_extend_a_turn() {
        // A 10 s turn, then a hook's meta line and a local-command system
        // record five hours later.
        let text = r#"{"type":"user","uuid":"u1","sessionId":"s9","cwd":"/work/proj","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"go"}}
{"type":"assistant","uuid":"a1","requestId":"q1","sessionId":"s9","timestamp":"2026-01-01T00:00:10Z","message":{"id":"m1","model":"claude-haiku-5-5","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}
{"type":"user","uuid":"u2","sessionId":"s9","isMeta":true,"timestamp":"2026-01-01T05:00:00Z","message":{"role":"user","content":"hook output"}}
{"type":"system","uuid":"y1","sessionId":"s9","subtype":"local_command","timestamp":"2026-01-01T05:00:01Z","content":"x"}
"#;
        let pricing = Pricing::load(None).unwrap();
        let tz = TimeZone::UTC;
        let mut seen = HashMap::new();
        let mut stats = Stats::default();
        let mut cx = Ctx {
            pricing: &pricing,
            tz: &tz,
            include_titles: false,
            large_output_chars: 100_000,
            context_ratio: 0.9,
            seen: &mut seen,
            stats: &mut stats,
        };
        let c = Candidate {
            path: "/work/x/s9.jsonl".into(),
            stem: "s9".into(),
            kind: Kind::Main,
        };
        let p = parse(&c, text, &mut cx);
        assert_eq!(p.session.turns, Some(1));
        assert_eq!(p.session.active_ms, Some(10_000));
        // Wall time still spans every record.
        assert_eq!(p.session.wall_ms, Some(5 * 3_600_000 + 1_000));

        // No prompt and no reply: turns and active time are unknown.
        let only_meta = r#"{"type":"system","uuid":"y2","sessionId":"s8","subtype":"local_command","timestamp":"2026-01-01T05:00:01Z","content":"x"}
"#;
        let c = Candidate {
            path: "/work/x/s8.jsonl".into(),
            stem: "s8".into(),
            kind: Kind::Main,
        };
        let p = parse(&c, only_meta, &mut cx);
        assert_eq!(p.session.turns, None);
        assert_eq!(p.session.active_ms, None);
    }

    #[test]
    fn invalid_utf8_lines_become_bad_lines() {
        let mut b = br#"{"type":"user","uuid":"u1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"go"}}"#.to_vec();
        b.extend_from_slice(b"\n{\"type\":\"user\",\"x\":\"\xff\xfe\"}\n{\"trunc");
        b.extend_from_slice(&[0xe3, 0x81]);
        let (text, bad) = utf8_lines(b);
        assert_eq!(bad, 2);
        assert_eq!(text.lines().count(), 3);
        assert!(text.lines().nth(1).unwrap().starts_with('\u{fffd}'));
        let (ok, none) = utf8_lines(b"{}\n".to_vec());
        assert_eq!((ok.as_str(), none), ("{}\n", 0));
    }

    #[test]
    fn read_ranges_key_rereads() {
        let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!(read_range(Some(&v(r#"{"file_path":"/a"}"#))), "whole");
        assert_eq!(
            read_range(Some(&v(r#"{"file_path":"/a","offset":100,"limit":50}"#))),
            "lines 100-149"
        );
        assert_eq!(
            read_range(Some(&v(r#"{"file_path":"/a","limit":5}"#))),
            "lines 1-5"
        );
        assert_eq!(
            read_range(Some(&v(r#"{"file_path":"/a","offset":0,"limit":5}"#))),
            "lines 1-5"
        );
        assert_eq!(
            read_range(Some(&v(r#"{"file_path":"/a","offset":100}"#))),
            "offset 100 limit  pages "
        );
        assert_eq!(
            read_range(Some(&v(r#"{"file_path":"/a.pdf","pages":"1-3"}"#))),
            "offset  limit  pages 1-3"
        );
        assert_ne!(
            read_range(Some(&v(r#"{"file_path":"/a","offset":1}"#))),
            read_range(Some(&v(r#"{"file_path":"/a","offset":2}"#)))
        );
    }

    #[test]
    fn shell_reads_key_like_read_calls() {
        let cwd = Some("/work/proj");
        let read = |c: &str| shell_read(c, cwd);
        let lines = Some(("/work/proj/src/a.rs".to_string(), "lines 10-20".to_string()));
        assert_eq!(read("sed -n '10,20p' src/a.rs"), lines);
        assert_eq!(read("sed -n \"10,20p\" ./src/../src/a.rs"), lines);
        assert_eq!(read("sed -n 10,20p /work/proj/src/a.rs"), lines);
        let whole = Some(("/work/proj/a b.md".to_string(), "whole".to_string()));
        assert_eq!(read("cat 'a b.md'"), whole);
        assert_eq!(read("nl -ba \"a b.md\""), whole);
        assert_eq!(read("nl 'a b.md'"), whole);
        // Compound commands, redirects, expansions and other programs are
        // not reads.
        for c in [
            "cat a.md | head",
            "cd x && cat a.md",
            "cat a.md || true",
            "cat a.md; ls",
            "cat a.md > b.md",
            "cat < a.md",
            "cat a.md 2>&1",
            "sed -n '1,5p' a.md &",
            "cat $F",
            "cat ~/a.md",
            "cat *.md",
            "cat a.md b.md",
            "cat",
            "cat -n a.md",
            "nl -w2 a.md",
            "sed -n '1,$p' a.md",
            "sed -i '1,5p' a.md",
            "head -n 5 a.md",
            "cat 'a.md",
        ] {
            assert_eq!(read(c), None, "{c}");
        }
        assert_eq!(shell_read("cat a.md", None), None);
        assert_eq!(
            shell_read("cat /abs/a.md", None),
            Some(("/abs/a.md".to_string(), "whole".to_string()))
        );
    }

    #[test]
    fn bash_rereads_of_one_range_raise_file_reread() {
        let line = |n: usize, block: &str| {
            format!(
                r#"{{"type":"assistant","uuid":"a{n}","requestId":"r{n}","sessionId":"s5","cwd":"/work/proj","timestamp":"2026-01-01T00:00:{n:02}Z","message":{{"id":"m{n}","model":"claude-opus-5-5","content":[{block}],"usage":{{"input_tokens":1,"output_tokens":1}}}}}}"#
            )
        };
        let bash = |n: usize, cmd: &str| {
            line(
                n,
                &format!(
                    r#"{{"type":"tool_use","id":"tu{n}","name":"Bash","input":{{"command":{}}}}}"#,
                    serde_json::to_string(cmd).unwrap()
                ),
            )
        };
        let mut lines = vec![
            r#"{"type":"user","uuid":"u0","sessionId":"s5","cwd":"/work/proj","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"go"}}"#.to_string(),
        ];
        for n in 1..=5 {
            lines.push(bash(n, "sed -n '10,20p' src/a.rs"));
        }
        // Compound commands of the same file are not reads.
        lines.push(bash(6, "sed -n '10,20p' src/a.rs | head"));
        lines.push(bash(7, "cat src/b.rs && cat src/b.rs"));
        lines.push(line(
            8,
            r#"{"type":"tool_use","id":"tu8","name":"Read","input":{"file_path":"/work/proj/src/a.rs","offset":10,"limit":11}}"#,
        ));
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("-work-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("s5.jsonl"), lines.join("\n") + "\n").unwrap();
        let (parsed, _) = parse_all(d.path());
        let p = &parsed[0];
        assert_eq!(p.trace.reads.len(), 1);
        let a = &p.trace.reads["/work/proj/src/a.rs"];
        // Five shell reads and one Read of the same lines.
        assert_eq!((a.reads, a.rereads), (6, 5));
        assert_eq!(a.ranges["lines 10-20"], 6);
        let th = crate::detect::Thresholds::default();
        let dcx = crate::detect::Ctx {
            th: &th,
            at: "2026-01-01T00:00:00Z",
        };
        let f = crate::detect::per_session(&p.session, &p.trace, &dcx);
        let rr = f.iter().find(|f| f.rule == "file-reread").unwrap();
        assert_eq!(rr.data["max_reads"], 6);
        assert_eq!(rr.data["max_repeat_reads"], 5);

        // Five shell reads alone reach the default threshold.
        std::fs::write(proj.join("s5.jsonl"), lines[..6].join("\n") + "\n").unwrap();
        let (parsed, _) = parse_all(d.path());
        let f = crate::detect::per_session(&parsed[0].session, &parsed[0].trace, &dcx);
        let rr = f.iter().find(|f| f.rule == "file-reread").unwrap();
        assert_eq!(rr.data["max_reads"], 5);
    }

    #[test]
    fn subagents_link_to_their_parent() {
        let d = tree();
        let (parsed, _) = parse_all(d.path());
        let sub = &parsed[1];
        assert_eq!(sub.session.id, "s1/agent-a1");
        assert_eq!(sub.session.role, "subagent");
        assert_eq!(sub.session.parent_id.as_deref(), Some("s1"));
        let sp = sub.session.spawn.as_ref().unwrap();
        assert_eq!(sp.agent_type.as_deref(), Some("Explore"));
        assert_eq!(sp.depth, Some(1));
        assert_eq!(sub.parent_tool_use.as_deref(), Some("tu9"));
        // r2 was already counted in the main transcript.
        let t = sub.session.total.as_ref().unwrap();
        assert_eq!(t.requests, 1);
        assert_eq!(t.tokens.input, 1000);
        // Haiku 5.5, short prompt: 1000 x 0.10 + 100 x 0.50.
        assert!((sub.session.cost.usd.unwrap() - 150.0 / 1e6).abs() < 1e-12);
    }

    #[test]
    fn unknown_cache_ttl_and_fast_mode_stay_unpriced() {
        let text = r#"{"type":"assistant","uuid":"a","requestId":"q1","timestamp":"2026-01-01T00:00:00Z","message":{"id":"m","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":1,"cache_creation_input_tokens":5,"output_tokens":1}}}
{"type":"assistant","uuid":"b","requestId":"q2","timestamp":"2026-01-01T00:00:01Z","message":{"id":"n","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":1,"output_tokens":1,"speed":"fast"}}}
{"type":"assistant","uuid":"c","requestId":"q3","timestamp":"2026-01-01T00:00:02Z","message":{"id":"o","model":"claude-mystery-9","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}
"#;
        let pricing = Pricing::load(None).unwrap();
        let tz = TimeZone::UTC;
        let mut seen = HashMap::new();
        let mut stats = Stats::default();
        let mut cx = Ctx {
            pricing: &pricing,
            tz: &tz,
            include_titles: false,
            large_output_chars: 100_000,
            context_ratio: 0.9,
            seen: &mut seen,
            stats: &mut stats,
        };
        let c = Candidate {
            path: "/x/s.jsonl".into(),
            stem: "s".into(),
            kind: Kind::Main,
        };
        let s = parse(&c, text, &mut cx).session;
        assert_eq!(s.cost.usd, None);
        assert_eq!(s.cost.unpriced_requests, 3);
        assert_eq!(s.total.as_ref().unwrap().tokens.cache_write_unknown, 5);
        assert_eq!(
            s.cost.unpriced_models,
            vec!["claude-mystery-9", "claude-opus-5-5"]
        );
    }

    #[test]
    fn web_errors_are_classified() {
        assert_eq!(
            classify_web_error("Request failed with status code 429"),
            "429"
        );
        assert_eq!(classify_web_error("getaddrinfo ENOTFOUND x.example"), "dns");
        assert_eq!(
            classify_web_error("connect ECONNREFUSED 1.2.3.4:443"),
            "connect-refused"
        );
        assert_eq!(classify_web_error("socket hang up"), "connection-reset");
        assert_eq!(classify_web_error("weird"), "error-other");
    }

    #[test]
    fn needle_matches_serialised_cwd() {
        let n = cwd_needle("/p/q");
        assert_eq!(n, "\"cwd\":\"/p/q");
        assert!(r#"{"cwd":"/p/q/r"}"#.contains(&n));
    }
}
