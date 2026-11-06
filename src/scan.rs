//! `agentprof scan`: selects the project's sessions in every source, links
//! them, runs the detectors and writes `sessions.jsonl`, `findings.jsonl` and
//! `scan.json`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::billing;
use crate::claude;
use crate::codex;
use crate::detect::{self, Finding, Thresholds, Trace};
use crate::pricing::Pricing;
use crate::session::{Session, Tokens};
use crate::t3::{self, Link};
use crate::util::{display_path, normalise_root, tilde, within};

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum Format {
    Text,
    Jsonl,
}

#[derive(Args, Debug)]
pub struct ScanArgs {
    /// Project root; sessions whose working directory is this path or below it are selected.
    /// Defaults to the directory of research.toml.
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Output directory for sessions.jsonl, findings.jsonl and scan.json.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// research.toml to use instead of searching upwards from the current directory.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// T3 project id; defaults to the T3 project whose workspace root is the project root.
    #[arg(long)]
    pub t3_project: Option<String>,
    /// Claude Code projects directory [default: ~/.claude/projects]
    #[arg(long)]
    pub claude_dir: Option<PathBuf>,
    /// Codex state database [default: ~/.codex/state_5.sqlite]
    #[arg(long)]
    pub codex_db: Option<PathBuf>,
    /// Codex archived rollouts [default: ~/.codex/archived_sessions]
    #[arg(long)]
    pub codex_archive: Option<PathBuf>,
    /// T3 Code state database [default: ~/.t3/userdata/statev2.sqlite]
    #[arg(long)]
    pub t3_db: Option<PathBuf>,
    /// API-routing log [default: ~/.local/state/claude-api/decisions.jsonl]
    #[arg(long)]
    pub billing_log: Option<PathBuf>,
    /// Pricing file [default: the built-in pricing.toml]
    #[arg(long)]
    pub pricing: Option<PathBuf>,
    /// IANA time zone for per-day breakdowns [default: research.toml, else the system zone]
    #[arg(long)]
    pub timezone: Option<String>,
    /// Add thread and session titles to the session records.
    #[arg(long)]
    pub include_titles: bool,
    /// Parallel Codex rollout readers [default: available cores, at most 8]
    #[arg(long)]
    pub jobs: Option<usize>,
    /// stdout: a text summary, or findings as rf.finding/1 JSON Lines.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
    #[command(flatten)]
    pub thresholds: Thresholds,
}

#[derive(Deserialize, Default)]
struct ResearchToml {
    #[serde(default)]
    project: ProjectTable,
    #[serde(default)]
    agentprof: AgentprofTable,
}

#[derive(Deserialize, Default)]
struct ProjectTable {
    timezone: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AgentprofTable {
    out: Option<PathBuf>,
    t3_project: Option<String>,
    claude_dir: Option<PathBuf>,
    codex_db: Option<PathBuf>,
    codex_archive: Option<PathBuf>,
    t3_db: Option<PathBuf>,
    billing_log: Option<PathBuf>,
    pricing: Option<PathBuf>,
}

fn find_config(explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(p) = explicit {
        if !p.is_file() {
            bail!("config file {} not found", p.display());
        }
        return Ok(Some(p.to_path_buf()));
    }
    let mut dir = std::env::current_dir()?;
    loop {
        let c = dir.join("research.toml");
        if c.is_file() {
            return Ok(Some(c));
        }
        if !dir.pop() {
            return Ok(None);
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Resolved inputs.
struct Inputs {
    root: String,
    out: PathBuf,
    t3_project: Option<String>,
    claude_dir: PathBuf,
    codex_db: PathBuf,
    codex_archive: PathBuf,
    t3_db: PathBuf,
    billing_log: PathBuf,
    pricing: Option<PathBuf>,
    tz: TimeZone,
    tz_name: String,
    config: Option<PathBuf>,
}

fn resolve(a: &ScanArgs) -> Result<Inputs> {
    let config = find_config(a.config.as_deref())?;
    let (cfg, base): (ResearchToml, Option<PathBuf>) = match &config {
        Some(p) => {
            let text =
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            let c: ResearchToml =
                toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
            (c, p.parent().map(Path::to_path_buf))
        }
        None => (ResearchToml::default(), None),
    };
    let rel = |p: Option<PathBuf>| -> Option<PathBuf> {
        p.map(|p| match (&base, p.is_absolute()) {
            (Some(b), false) => b.join(p),
            _ => p,
        })
    };
    let root = match (&a.project, &base) {
        (Some(p), _) => std::path::absolute(p)?,
        (None, Some(b)) => b.clone(),
        (None, None) => bail!("no --project given and no research.toml found"),
    };
    let root = normalise_root(&root.to_string_lossy());
    let ap = cfg.agentprof;
    let out = a
        .out
        .clone()
        .or(rel(ap.out))
        .context("no --out given and research.toml sets no [agentprof] out")?;
    let h = home();
    let tz_name = a.timezone.clone().or(cfg.project.timezone);
    let (tz, tz_name) = match tz_name {
        Some(n) => (
            TimeZone::get(&n).with_context(|| format!("unknown time zone {n}"))?,
            n,
        ),
        None => {
            let tz = TimeZone::system();
            let name = tz.iana_name().unwrap_or("system").to_string();
            (tz, name)
        }
    };
    Ok(Inputs {
        root,
        out,
        t3_project: a.t3_project.clone().or(ap.t3_project),
        claude_dir: a
            .claude_dir
            .clone()
            .or(rel(ap.claude_dir))
            .unwrap_or(h.join(".claude/projects")),
        codex_db: a
            .codex_db
            .clone()
            .or(rel(ap.codex_db))
            .unwrap_or(h.join(".codex/state_5.sqlite")),
        codex_archive: a
            .codex_archive
            .clone()
            .or(rel(ap.codex_archive))
            .unwrap_or(h.join(".codex/archived_sessions")),
        t3_db: a
            .t3_db
            .clone()
            .or(rel(ap.t3_db))
            .unwrap_or(h.join(".t3/userdata/statev2.sqlite")),
        billing_log: a
            .billing_log
            .clone()
            .or(rel(ap.billing_log))
            .unwrap_or(h.join(".local/state/claude-api/decisions.jsonl")),
        pricing: a.pricing.clone().or(rel(ap.pricing)),
        tz,
        tz_name,
        config,
    })
}

/// True when some `"cwd":"<root>` occurrence ends at a path boundary.
fn mentions_cwd(text: &str, needle: &str) -> bool {
    text.match_indices(needle)
        .any(|(i, _)| matches!(text.as_bytes().get(i + needle.len()), Some(b'"' | b'/')))
}

/// Length of the union of intervals, in milliseconds.
pub fn union_ms(mut v: Vec<(i64, i64)>) -> i64 {
    v.retain(|(a, b)| b > a);
    v.sort_unstable();
    let mut total = 0;
    let mut cur: Option<(i64, i64)> = None;
    for (a, b) in v {
        match cur {
            Some((ca, cb)) if a <= cb => cur = Some((ca, cb.max(b))),
            Some((ca, cb)) => {
                total += cb - ca;
                cur = Some((a, b));
            }
            None => cur = Some((a, b)),
        }
    }
    if let Some((a, b)) = cur {
        total += b - a;
    }
    total
}

fn ratio(a: u64, b: u64) -> Option<f64> {
    (b > 0).then(|| a as f64 / b as f64)
}

#[derive(Serialize)]
struct Inflation {
    input: Option<f64>,
    cache_write: Option<f64>,
    cache_read: Option<f64>,
    output: Option<f64>,
    total: Option<f64>,
}

fn inflation(naive: &Tokens, dedup: &Tokens) -> Inflation {
    Inflation {
        input: ratio(naive.input, dedup.input),
        cache_write: ratio(naive.cache_writes(), dedup.cache_writes()),
        cache_read: ratio(naive.cache_read, dedup.cache_read),
        output: ratio(naive.output, dedup.output),
        total: ratio(naive.total(), dedup.total()),
    }
}

pub const SCAN_SCHEMA: &str = "agentprof.scan/1";

pub struct Outcome {
    pub errors: usize,
}

pub fn run(a: &ScanArgs) -> Result<Outcome> {
    let inp = resolve(a)?;
    let pricing = Pricing::load(inp.pricing.as_deref())?;
    let now = Timestamp::now();
    let at = now.to_string();
    let th = &a.thresholds;
    let root = inp.root.as_str();
    let mut inputs = BTreeMap::new();
    let mut coverage = serde_json::Map::new();
    let mut checks = serde_json::Map::new();
    let mut sessions: Vec<Session> = Vec::new();
    let mut traces: HashMap<String, Trace> = HashMap::new();

    // ---- T3 first: its links widen the Claude and Codex selections.
    let t3data = if inp.t3_db.is_file() {
        let (conn, mode) = t3::open(&inp.t3_db)?;
        inputs.insert(
            "t3_db",
            json!({"path": display_path(&inp.t3_db), "open": mode}),
        );
        Some(t3::load(
            &conn,
            inp.t3_project.as_deref(),
            root,
            a.include_titles,
        )?)
    } else {
        inputs.insert(
            "t3_db",
            json!({"path": display_path(&inp.t3_db), "open": "absent"}),
        );
        None
    };
    let mut claude_linked: HashSet<String> = HashSet::new();
    let mut codex_linked: HashSet<String> = HashSet::new();
    for t in t3data.iter().flat_map(|d| &d.threads) {
        for l in &t.links {
            if let Link::Thread { source, id } = l {
                match source.as_str() {
                    "claude" => claude_linked.insert(id.clone()),
                    _ => codex_linked.insert(id.clone()),
                };
            }
        }
    }

    // ---- Claude.
    let mut claude_stats = claude::Stats::default();
    let mut uuid_owner: HashMap<String, String> = HashMap::new();
    let mut tool_use_owner: HashMap<String, String> = HashMap::new();
    // Raw first cwd and start time of each Claude session, for billing.
    let mut claude_raw: HashMap<String, (Option<String>, Option<i64>)> = HashMap::new();
    let mut claude_cov = json!({"present": false});
    if inp.claude_dir.is_dir() {
        let cands = claude::discover(&inp.claude_dir)?;
        let needle = claude::cwd_needle(root);
        let mut included_main: HashSet<String> = HashSet::new();
        let mut selected: Vec<(claude::Candidate, String, &'static str)> = Vec::new();
        let mut files_scanned = 0u64;
        for c in cands {
            files_scanned += 1;
            let text = std::fs::read_to_string(&c.path)
                .with_context(|| format!("reading {}", c.path.display()))?;
            let hit = mentions_cwd(&text, &needle);
            let basis = match &c.kind {
                claude::Kind::Main => {
                    if hit {
                        Some("cwd")
                    } else if claude_linked.contains(&c.stem) {
                        Some("t3-link")
                    } else {
                        None
                    }
                }
                claude::Kind::Subagent { parent, .. } => {
                    if hit {
                        Some("cwd")
                    } else if included_main.contains(parent) {
                        Some("parent")
                    } else {
                        None
                    }
                }
            };
            if let Some(b) = basis {
                if matches!(c.kind, claude::Kind::Main) {
                    included_main.insert(c.stem.clone());
                }
                selected.push((c, text, b));
            }
        }
        let mut seen = HashMap::new();
        let mut cx = claude::Ctx {
            pricing: &pricing,
            tz: &inp.tz,
            include_titles: a.include_titles,
            large_output_chars: th.large_output_chars,
            context_ratio: th.context_ratio,
            seen: &mut seen,
            stats: &mut claude_stats,
        };
        let mut parent_tool: Vec<(String, String)> = Vec::new();
        for (c, text, basis) in &selected {
            let mut p = claude::parse(c, text, &mut cx);
            p.session.match_basis = (*basis).to_string();
            let key = p.session.key();
            for u in p.uuids {
                uuid_owner.entry(u).or_insert_with(|| key.clone());
            }
            for t in p.tool_use_ids {
                tool_use_owner.insert(t, p.session.id.clone());
            }
            if let Some(t) = p.parent_tool_use {
                parent_tool.push((key.clone(), t));
            }
            claude_raw.insert(key.clone(), (p.first_cwd, p.start_ms));
            traces.insert(key, p.trace);
            sessions.push(p.session);
        }
        // Check subagent parents against the meta file's spawning tool call.
        let mut parent_mismatch = 0u64;
        let mut parent_unverified = 0u64;
        for (key, tool) in parent_tool {
            let s = sessions
                .iter_mut()
                .find(|s| s.key() == key)
                .expect("parsed");
            match tool_use_owner.get(&tool) {
                Some(owner) if Some(owner) == s.parent_id.as_ref() => {}
                Some(owner) => {
                    parent_mismatch += 1;
                    s.notes.push(format!(
                        "meta toolUseId belongs to claude:{owner}, not the directory's session"
                    ));
                }
                None => parent_unverified += 1,
            }
        }
        let (mut main, mut sub, mut by_basis) = (0u64, 0u64, BTreeMap::<String, u64>::new());
        for s in sessions.iter().filter(|s| s.source == "claude") {
            if s.role == "main" {
                main += 1;
            } else {
                sub += 1;
            }
            *by_basis.entry(s.match_basis.clone()).or_default() += 1;
        }
        claude_cov = json!({
            "present": true,
            "files_scanned": files_scanned,
            "main_sessions": main,
            "subagent_sessions": sub,
            "by_match_basis": by_basis,
            "lines": claude_stats.lines,
            "bad_lines": claude_stats.bad_lines,
            "subagent_parent_mismatch": parent_mismatch,
            "subagent_parent_unverified": parent_unverified,
        });
        let st = &claude_stats;
        checks.insert(
            "claude_dedup".into(),
            json!({
                "description": "per-line usage sum (naive) versus usage counted once per request id",
                "assistant_lines": st.assistant_lines,
                "synthetic_lines": st.synthetic_lines,
                "requests": st.requests,
                "naive": st.naive,
                "dedup": st.dedup,
                "inflation": inflation(&st.naive, &st.dedup),
                "usage_conflicts": st.usage_conflicts,
                "cross_file_duplicates": st.cross_file_duplicates,
            }),
        );
    }
    inputs.insert(
        "claude_dir",
        json!({"path": display_path(&inp.claude_dir), "present": inp.claude_dir.is_dir()}),
    );
    coverage.insert("claude".into(), claude_cov);

    // ---- Codex.
    let mut turn_owner: HashMap<String, String> = HashMap::new();
    let mut codex_cov = json!({"present": false});
    if inp.codex_db.is_file() {
        let (conn, mode) = codex::open_ro(&inp.codex_db)?;
        inputs.insert(
            "codex_db",
            json!({"path": display_path(&inp.codex_db), "open": mode}),
        );
        let threads = codex::load_threads(&conn)?;
        let mut basis: BTreeMap<String, &'static str> = BTreeMap::new();
        for t in &threads {
            if within(&t.cwd, root) {
                basis.insert(t.id.clone(), "cwd");
            } else if codex_linked.contains(&t.id) {
                basis.insert(t.id.clone(), "t3-link");
            }
        }
        // Children of selected threads, to a fixpoint.
        loop {
            let mut added = false;
            for t in &threads {
                if basis.contains_key(&t.id) {
                    continue;
                }
                if let Some((p, _)) = t.spawn()
                    && basis.contains_key(&p)
                {
                    basis.insert(t.id.clone(), "parent");
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        let name = Path::new(root)
            .file_name()
            .map(|n| n.to_string_lossy().to_string());
        let elsewhere = threads
            .iter()
            .filter(|t| !basis.contains_key(&t.id))
            .filter(|t| {
                name.as_ref().is_some_and(|n| {
                    Path::new(&t.cwd)
                        .components()
                        .any(|c| c.as_os_str().to_string_lossy() == *n)
                })
            })
            .count();
        let selected: Vec<&codex::Thread> = threads
            .iter()
            .filter(|t| basis.contains_key(&t.id))
            .collect();
        let jobs = a
            .jobs
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get().min(8)))
            .max(1);
        let cx = codex::Ctx {
            pricing: &pricing,
            tz: &inp.tz,
            include_titles: a.include_titles,
            large_output_chars: th.large_output_chars,
            context_ratio: th.context_ratio,
        };
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<(usize, Result<codex::Parsed>)>> = Mutex::new(Vec::new());
        std::thread::scope(|sc| {
            for _ in 0..jobs {
                sc.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(t) = selected.get(i) else { break };
                        let r = codex::resolve(&t.rollout_path, Some(&inp.codex_archive));
                        let p = codex::parse(t, &r, &cx);
                        results.lock().expect("no poisoned lock").push((i, p));
                    }
                });
            }
        });
        let mut results = results.into_inner().expect("no poisoned lock");
        results.sort_by_key(|(i, _)| *i);
        let mut status = BTreeMap::<String, u64>::new();
        let mut by_basis = BTreeMap::<String, u64>::new();
        let (mut main, mut sub) = (0u64, 0u64);
        let mut no_counters = 0u64;
        let mut fallback_tokens = 0u64;
        let mut inherited_lines = 0u64;
        let mut cmp = json!({});
        let (mut equal, mut differ, mut compared) = (0u64, 0u64, 0u64);
        let (mut sum_db, mut sum_last, mut sum_own, mut sum_inh) = (0u64, 0u64, 0u64, 0u64);
        let mut examples = Vec::new();
        for (i, r) in results {
            let t = selected[i];
            let mut p = r.with_context(|| format!("codex thread {}", t.id))?;
            p.session.match_basis = basis[&t.id].to_string();
            let key = p.session.key();
            for tid in p.turn_ids {
                turn_owner
                    .entry(tid)
                    .or_insert_with(|| p.session.id.clone());
            }
            let tr = p
                .session
                .transcript
                .as_ref()
                .expect("codex sets transcript");
            *status.entry(tr.status.clone()).or_default() += 1;
            inherited_lines += tr.inherited_lines;
            *by_basis.entry(p.session.match_basis.clone()).or_default() += 1;
            if p.session.role == "main" {
                main += 1;
            } else {
                sub += 1;
            }
            if p.session.total.is_none() {
                no_counters += 1;
                fallback_tokens += p.session.db_tokens_used.unwrap_or(0);
            }
            if let (Some(db), Some(last)) = (p.session.db_tokens_used, p.check.last_total) {
                compared += 1;
                sum_db += db;
                sum_last += last;
                sum_own += p.check.own_total;
                sum_inh += p.check.inherited;
                if db == last {
                    equal += 1;
                } else {
                    differ += 1;
                    if examples.len() < 20 {
                        examples.push(
                            json!({"thread": t.id, "tokens_used": db, "last_counter_total": last}),
                        );
                    }
                }
            }
            traces.insert(key, p.trace);
            sessions.push(p.session);
        }
        if compared > 0 {
            cmp = json!({
                "description": "threads.tokens_used versus the rollout's last cumulative counter, and the thread's own (incremental) total",
                "threads_compared": compared,
                "tokens_used_equals_last_counter": equal,
                "tokens_used_differs": differ,
                "differing_examples": examples,
                "sum_tokens_used": sum_db,
                "sum_last_counter_total": sum_last,
                "sum_inherited_at_start": sum_inh,
                "sum_own_total": sum_own,
                "own_plus_inherited_equals_last": sum_own + sum_inh == sum_last,
                "tokens_used_overcount_factor": ratio(sum_db, sum_own),
            });
        }
        checks.insert("codex_tokens_used".into(), cmp);
        codex_cov = json!({
            "present": true,
            "threads_in_db": threads.len(),
            "threads_selected": selected.len(),
            "main_threads": main,
            "subagent_threads": sub,
            "by_match_basis": by_basis,
            "rollouts": status,
            "threads_without_token_counters": no_counters,
            "db_tokens_used_of_those": fallback_tokens,
            "inherited_history_lines_skipped": inherited_lines,
            "unselected_threads_with_project_dir_name_in_cwd": elsewhere,
        });
    } else {
        inputs.insert(
            "codex_db",
            json!({"path": display_path(&inp.codex_db), "open": "absent"}),
        );
    }
    inputs.insert(
        "codex_archive",
        json!({"path": display_path(&inp.codex_archive), "present": inp.codex_archive.is_dir()}),
    );
    coverage.insert("codex".into(), codex_cov);

    // ---- Link T3 threads to native sessions.
    let mut t3_cov = json!({"present": false});
    if let Some(data) = t3data {
        let index: HashMap<String, usize> = sessions
            .iter()
            .enumerate()
            .map(|(i, s)| (s.key(), i))
            .collect();
        let agent_index: HashMap<String, usize> = sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.source == "claude" && s.role == "subagent")
            .filter_map(|(i, s)| {
                s.id.rsplit_once("/agent-")
                    .map(|(_, aid)| (aid.to_string(), i))
            })
            .collect();
        let mut claimed: HashMap<usize, String> = HashMap::new();
        let mut linked_threads = 0u64;
        let mut by_basis = BTreeMap::<String, u64>::new();
        let mut by_provider = BTreeMap::<String, (u64, u64)>::new();
        let mut multi_claim = 0u64;
        let mut runs_parsed: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
        let mut t3_sessions = Vec::new();
        for mut t in data.threads {
            let mut thread_hits = BTreeSet::new();
            let mut turn_hits = BTreeSet::new();
            let mut agent_hits = BTreeSet::new();
            let mut unresolved = BTreeSet::new();
            for l in &t.links {
                match l {
                    Link::Thread { source, id } => match index.get(&format!("{source}:{id}")) {
                        Some(i) => {
                            thread_hits.insert(*i);
                        }
                        None => {
                            unresolved.insert(format!("{source}:{id}"));
                        }
                    },
                    Link::Turn { source, id } => {
                        let owner = if source == "claude" {
                            uuid_owner.get(id).cloned()
                        } else {
                            turn_owner.get(id).map(|t| format!("codex:{t}"))
                        };
                        if let Some(i) = owner.and_then(|k| index.get(&k)) {
                            turn_hits.insert(*i);
                        }
                    }
                    Link::ClaudeAgent { id } => match agent_index.get(id) {
                        Some(i) => {
                            agent_hits.insert(*i);
                        }
                        None => {
                            unresolved.insert(format!("claude:agent-{id}"));
                        }
                    },
                }
            }
            let (hits, basis) = if !thread_hits.is_empty() {
                (thread_hits, "thread-ref")
            } else if !agent_hits.is_empty() {
                (agent_hits, "agent-ref")
            } else if !turn_hits.is_empty() {
                (turn_hits, "turn-ref")
            } else {
                (BTreeSet::new(), "none")
            };
            let info = t.session.t3.as_mut().expect("t3 sets info");
            for (provider, st) in &info.runs_by_provider {
                for (status, n) in st {
                    *runs_parsed
                        .entry(provider.clone())
                        .or_default()
                        .entry(status.clone())
                        .or_default() += n;
                }
            }
            info.link_basis = basis.into();
            info.unresolved_links = unresolved.into_iter().collect();
            let e = by_provider.entry(info.provider.clone()).or_default();
            e.0 += 1;
            if !hits.is_empty() {
                linked_threads += 1;
                e.1 += 1;
            }
            *by_basis.entry(basis.into()).or_default() += 1;
            for i in hits {
                info.linked.push(sessions[i].key());
                match claimed.get(&i) {
                    Some(other) => {
                        multi_claim += 1;
                        let note = format!("also linked from t3:{}", t.session.id);
                        if !sessions[i].notes.contains(&note) {
                            sessions[i].notes.push(note);
                        }
                        let _ = other;
                    }
                    None => {
                        claimed.insert(i, t.session.id.clone());
                        sessions[i].t3_thread_id = Some(t.session.id.clone());
                        sessions[i].t3_link = Some(basis.into());
                    }
                }
            }
            info.linked.sort();
            t3_sessions.push((t.session, t.intervals));
        }
        let total = t3_sessions.len() as u64;
        let natives_linked = claimed.len();
        let mut native_total = BTreeMap::<String, (u64, u64)>::new();
        for (i, s) in sessions.iter().enumerate() {
            let e = native_total.entry(s.source.clone()).or_default();
            e.0 += 1;
            if claimed.contains_key(&i) {
                e.1 += 1;
            }
        }
        t3_cov = json!({
            "present": true,
            "project_id": data.project_id,
            "workspace_root": data.workspace_root.as_deref().map(tilde),
            "threads": total,
            "threads_linked": linked_threads,
            "thread_link_rate": ratio(linked_threads, total),
            "threads_by_link_basis": by_basis,
            "threads_by_provider_linked": by_provider.iter().map(|(k, (n, l))| (k.clone(), json!({"threads": n, "linked": l, "rate": ratio(*l, *n)}))).collect::<BTreeMap<_, _>>(),
            "native_sessions_linked": natives_linked,
            "native_sessions_by_source_linked": native_total.iter().map(|(k, (n, l))| (k.clone(), json!({"sessions": n, "linked": l, "rate": ratio(*l, *n)}))).collect::<BTreeMap<_, _>>(),
            "extra_links_to_already_linked_sessions": multi_claim,
        });
        checks.insert(
            "t3_runs".into(),
            json!({
                "description": "runs by provider and status: direct SQL count versus the sum over parsed T3 threads",
                "sql": data.sql_runs,
                "parsed": runs_parsed,
                "equal": data.sql_runs == runs_parsed,
            }),
        );
        for (s, iv) in t3_sessions {
            traces.insert(
                s.key(),
                Trace {
                    intervals: iv,
                    ..Trace::default()
                },
            );
            sessions.push(s);
        }
    }
    coverage.insert("t3".into(), t3_cov);

    // ---- Billing (Claude only).
    let mut billing_cov = json!({"present": false});
    if inp.billing_log.is_file() {
        let log = billing::Log::load(&inp.billing_log)?;
        let mut matched = BTreeMap::<String, u64>::new();
        let range = match (log.first, log.last) {
            (Some(f), Some(l)) => {
                format!("{} to {}", crate::util::fmt_ts(f), crate::util::fmt_ts(l))
            }
            _ => "empty".into(),
        };
        for s in sessions.iter_mut().filter(|s| s.source == "claude") {
            let Some((cwd, start)) = claude_raw.get(&s.key()) else {
                continue;
            };
            let hit = match (cwd, start) {
                (Some(c), Some(t)) => log.lookup(c, *t),
                _ => None,
            };
            match hit {
                Some(d) => {
                    s.billing.mode = d.mode.clone();
                    s.billing.basis = format!(
                        "billing log decision at {} for the session's directory, {} s before its first record",
                        crate::util::fmt_ts(d.at),
                        (start.unwrap_or(d.at) - d.at) / 1000
                    );
                    *matched.entry(d.mode.clone()).or_default() += 1;
                }
                None => {
                    s.billing.basis = format!(
                        "no billing log decision for the session's directory within {} s before its first record (log covers {range})",
                        billing::WINDOW_MS / 1000
                    );
                }
            }
        }
        billing_cov = json!({
            "present": true,
            "decisions": log.decisions.len(),
            "bad_lines": log.bad_lines,
            "range": range,
            "decisions_in_project": log.in_project(root),
            "claude_sessions_matched_by_mode": matched,
        });
        inputs.insert(
            "billing_log",
            json!({"path": display_path(&inp.billing_log), "present": true}),
        );
    } else {
        inputs.insert(
            "billing_log",
            json!({"path": display_path(&inp.billing_log), "present": false}),
        );
        for s in sessions.iter_mut().filter(|s| s.source == "claude") {
            s.billing.basis = "billing log absent".into();
        }
    }
    coverage.insert("billing".into(), billing_cov);

    // ---- Detectors.
    sessions.sort_by(|a, b| {
        (a.source.as_str(), a.id.as_str()).cmp(&(b.source.as_str(), b.id.as_str()))
    });
    let dcx = detect::Ctx { th, at: &at };
    let mut findings: Vec<Finding> = Vec::new();
    let mut intervals: BTreeMap<&str, Vec<(i64, i64)>> = BTreeMap::new();
    for s in &sessions {
        let tr = traces.remove(&s.key()).unwrap_or_default();
        findings.extend(detect::per_session(s, &tr, &dcx));
        intervals
            .entry(match s.source.as_str() {
                "claude" => "claude",
                "codex" => "codex",
                _ => "t3",
            })
            .or_default()
            .extend(tr.intervals.iter().copied());
    }
    findings.extend(detect::cross_session(&sessions, &dcx));

    // ---- Active time without overlap.
    let mut union = BTreeMap::new();
    let mut all = Vec::new();
    let mut native = Vec::new();
    for (src, iv) in intervals {
        if src != "t3" {
            native.extend(iv.iter().copied());
        }
        all.extend(iv.iter().copied());
        union.insert(src, union_ms(iv));
    }
    let union_native = union_ms(native);
    let union_all = union_ms(all);

    // ---- Write.
    std::fs::create_dir_all(&inp.out).with_context(|| format!("creating {}", inp.out.display()))?;
    write_jsonl(&inp.out.join("sessions.jsonl"), &sessions)?;
    write_jsonl(&inp.out.join("findings.jsonl"), &findings)?;
    let errors = findings.iter().filter(|f| f.severity == "error").count();
    let scan = json!({
        "schema": SCAN_SCHEMA,
        "tool": "agentprof",
        "version": env!("CARGO_PKG_VERSION"),
        "generated_at": at,
        "project": tilde(root),
        "config": inp.config.as_deref().map(display_path),
        "timezone": inp.tz_name,
        "include_titles": a.include_titles,
        "pricing": {
            "file": inp.pricing.as_deref().map(display_path).unwrap_or_else(|| "built-in".into()),
            "anthropic_basis": pricing.basis("anthropic"),
            "openai_basis": pricing.basis("openai"),
            "models": pricing.model.iter().map(|m| json!({"model": m.model, "provider": m.provider, "source": m.source, "checked": m.checked, "notes": m.notes})).collect::<Vec<_>>(),
            "server_tools": pricing.server_tool.iter().map(|t| json!({"provider": t.provider, "name": t.name, "usd_per_1000": t.usd_per_1000, "source": t.source, "checked": t.checked})).collect::<Vec<_>>(),
        },
        "thresholds": th,
        "inputs": inputs,
        "coverage": coverage,
        "cross_checks": checks,
        "active_union_ms": {
            "by_source": union,
            "claude_and_codex": union_native,
            "all_sources": union_all,
            "basis": "union of the sessions' turn intervals (claude, codex) or run intervals (t3), so concurrent sessions are not double counted",
        },
        "sessions": sessions.len(),
        "findings": findings.len(),
        "error_findings": errors,
    });
    let mut f = std::fs::File::create(inp.out.join("scan.json"))?;
    serde_json::to_writer_pretty(&mut f, &scan)?;
    f.write_all(b"\n")?;

    // ---- stdout.
    let mut out = std::io::stdout().lock();
    match a.format {
        Format::Jsonl => {
            for fd in &findings {
                serde_json::to_writer(&mut out, fd).map_err(std::io::Error::from)?;
                out.write_all(b"\n")?;
            }
        }
        Format::Text => {
            let mut by_src = BTreeMap::<&str, u64>::new();
            for s in &sessions {
                *by_src.entry(s.source.as_str()).or_default() += 1;
            }
            writeln!(out, "agentprof scan of {}", tilde(root))?;
            for (k, v) in by_src {
                writeln!(out, "  {k}: {v} session(s)")?;
            }
            let mut by_rule = BTreeMap::<&str, u64>::new();
            for fd in &findings {
                *by_rule.entry(fd.rule.as_str()).or_default() += 1;
            }
            writeln!(out, "  findings: {} ({} error)", findings.len(), errors)?;
            for (k, v) in by_rule {
                writeln!(out, "    {k}: {v}")?;
            }
            writeln!(out, "  wrote {}", display_path(&inp.out))?;
        }
    }
    Ok(Outcome { errors })
}

fn write_jsonl<T: Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    let f = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut w = std::io::BufWriter::new(f);
    for r in rows {
        serde_json::to_writer(&mut w, r)?;
        w.write_all(b"\n")?;
    }
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_merges_overlaps() {
        assert_eq!(union_ms(vec![(0, 10), (5, 20), (30, 40), (40, 41)]), 31);
        assert_eq!(union_ms(vec![]), 0);
    }

    #[test]
    fn cwd_mentions_stop_at_path_boundaries() {
        let n = claude::cwd_needle("/p/q");
        assert!(mentions_cwd(r#"{"cwd":"/p/q"}"#, &n));
        assert!(mentions_cwd(r#"{"cwd":"/p/q/r"}"#, &n));
        assert!(!mentions_cwd(r#"{"cwd":"/p/qq"}"#, &n));
    }
}
