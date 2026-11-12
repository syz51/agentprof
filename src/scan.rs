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
    /// Codex rollout directory, searched for a thread's earlier segments [default: ~/.codex/sessions]
    #[arg(long)]
    pub codex_sessions: Option<PathBuf>,
    /// T3 Code state database [default: ~/.t3/userdata/statev2.sqlite]
    #[arg(long)]
    pub t3_db: Option<PathBuf>,
    /// API-routing log [default: ~/.local/state/claude-api/decisions.jsonl]
    #[arg(long)]
    pub billing_log: Option<PathBuf>,
    /// Pricing file [default: the built-in pricing.toml]
    #[arg(long)]
    pub pricing: Option<PathBuf>,
    /// Archive root, absolute or relative to the project root; repeat for several
    /// [default: research.toml `[agentprof] archive_roots`, else the directory of `[lab.archive] start`]
    #[arg(long = "archive-root", value_name = "DIR")]
    pub archive_roots: Vec<String>,
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
    #[serde(default)]
    lab: LabTable,
}

/// The one key of lab's table that agentprof reads: the archive's start page.
#[derive(Deserialize, Default)]
struct LabTable {
    #[serde(default)]
    archive: LabArchiveTable,
}

#[derive(Deserialize, Default)]
struct LabArchiveTable {
    start: Option<String>,
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
    codex_sessions: Option<PathBuf>,
    t3_db: Option<PathBuf>,
    billing_log: Option<PathBuf>,
    pricing: Option<PathBuf>,
    archive_roots: Option<Vec<String>>,
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
    codex_sessions: PathBuf,
    t3_db: PathBuf,
    billing_log: PathBuf,
    pricing: Option<PathBuf>,
    tz: TimeZone,
    tz_name: String,
    config: Option<PathBuf>,
    archive: crate::archive::Config,
    /// Where the archive roots come from.
    archive_basis: &'static str,
}

/// The absolute form of `p` with `.` and `..` removed lexically (symlinks
/// are kept, so a root matches the working directories agents recorded).
fn absolute_clean(p: &Path) -> Result<PathBuf> {
    use std::path::Component;
    let abs = std::path::absolute(p).with_context(|| format!("resolving {}", p.display()))?;
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn resolve(a: &ScanArgs) -> Result<Inputs> {
    let config = find_config(a.config.as_deref())?
        .map(|p| absolute_clean(&p))
        .transpose()?;
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
        (Some(p), _) => absolute_clean(p)?,
        (None, Some(b)) => b.clone(),
        (None, None) => bail!("no --project given and no research.toml found"),
    };
    let root = normalise_root(&root.to_string_lossy());
    let ap = cfg.agentprof;
    let (archive_roots, archive_basis) = if !a.archive_roots.is_empty() {
        (a.archive_roots.clone(), "--archive-root")
    } else if let Some(r) = ap.archive_roots {
        (r, "[agentprof] archive_roots")
    } else if let Some(start) = cfg.lab.archive.start {
        let dir = Path::new(&start).parent().unwrap_or(Path::new(""));
        (
            vec![dir.to_string_lossy().into_owned()],
            "directory of [lab.archive] start",
        )
    } else {
        (Vec::new(), "none")
    };
    let archive = crate::archive::Config {
        project: root.clone(),
        roots: archive_roots
            .iter()
            // An empty root (a start page in the project root) is the
            // project root itself.
            .map(|r| match r.as_str() {
                "" => ".".to_string(),
                r => normalise_root(r),
            })
            .collect(),
    };
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
        codex_sessions: a
            .codex_sessions
            .clone()
            .or(rel(ap.codex_sessions))
            .unwrap_or(h.join(".codex/sessions")),
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
        archive,
        archive_basis,
    })
}

/// True when some `"cwd":"<root>` occurrence ends at a path boundary.
/// How a fork's first counter (`total`, starting from `base`) relates to its
/// base thread's counters.
fn fork_outcome(parent: Option<&Vec<u64>>, total: u64, base: u64) -> &'static str {
    match parent {
        // The first snapshot repeats a counter of the base thread with a
        // request in it: its last_token_usage is the base thread's request.
        // Checked before a zero base, which a replay of the base thread's
        // first counter also has.
        Some(c) if total != base && c.contains(&total) => "replayed",
        _ if base == 0 => "zero_base",
        None => "parent_unavailable",
        Some(c) if c.contains(&base) => "verified",
        Some(_) => "mismatched",
    }
}

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
    let mut t3_absent: Option<String> = None;
    let t3data = if inp.t3_db.is_file() {
        let (conn, mode) = t3::open(&inp.t3_db)?;
        inputs.insert(
            "t3_db",
            json!({"path": display_path(&inp.t3_db), "open": mode}),
        );
        let d = t3::load(&conn, inp.t3_project.as_deref(), root, a.include_titles)?;
        if d.is_none() {
            t3_absent = Some(format!(
                "no T3 project has workspace root {}; pass --t3-project to choose one",
                tilde(root)
            ));
        }
        d
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
        let mut files_scanned = 0u64;
        // Unreadable transcripts cannot be matched to a project by content.
        // Those in the project's own directory are listed; the rest are only
        // counted, so other projects' names stay out of this project's output.
        let own_dir: String = root
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let mut unreadable: Vec<String> = Vec::new();
        let mut unreadable_elsewhere = 0u64;
        let mut invalid_utf8_lines = 0u64;
        let mut seen = HashMap::new();
        let mut cx = claude::Ctx {
            pricing: &pricing,
            tz: &inp.tz,
            include_titles: a.include_titles,
            large_output_chars: th.large_output_chars,
            context_ratio: th.context_ratio,
            archive: &inp.archive,
            seen: &mut seen,
            stats: &mut claude_stats,
        };
        let mut parent_tool: Vec<(usize, String)> = Vec::new();
        for c in cands {
            files_scanned += 1;
            let bytes = match std::fs::read(&c.path) {
                Ok(b) => b,
                Err(e) => {
                    let dir = c
                        .path
                        .strip_prefix(&inp.claude_dir)
                        .ok()
                        .and_then(|p| p.components().next());
                    if dir.is_some_and(|d| d.as_os_str() == own_dir.as_str()) {
                        unreadable.push(format!("{}: {e}", display_path(&c.path)));
                    } else {
                        unreadable_elsewhere += 1;
                    }
                    continue;
                }
            };
            let (text, bad) = claude::utf8_lines(bytes);
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
            let Some(basis) = basis else { continue };
            invalid_utf8_lines += bad;
            if matches!(c.kind, claude::Kind::Main) {
                included_main.insert(c.stem.clone());
            }
            // Parsed as soon as it is selected, so only one transcript's
            // text is held at a time.
            let mut p = claude::parse(&c, &text, &mut cx);
            p.session.match_basis = basis.to_string();
            let key = p.session.key();
            for u in p.uuids {
                uuid_owner.entry(u).or_insert_with(|| key.clone());
            }
            // The first transcript holding a tool use owns it: a later one
            // may hold a copy of its parent's history.
            for t in p.tool_use_ids {
                tool_use_owner
                    .entry(t)
                    .or_insert_with(|| p.session.id.clone());
            }
            if let Some(t) = p.parent_tool_use {
                parent_tool.push((sessions.len(), t));
            }
            claude_raw.insert(key.clone(), (p.first_cwd, p.start_ms));
            traces.insert(key, p.trace);
            sessions.push(p.session);
        }
        // Check subagent parents against the meta file's spawning tool call.
        // A subagent started by another subagent of the same session has the
        // directory's session as its directory parent but the spawning
        // subagent as its real parent.
        let mut parent_mismatch = 0u64;
        let mut parent_unverified = 0u64;
        let mut parent_nested = 0u64;
        for (i, tool) in parent_tool {
            let s = &mut sessions[i];
            match tool_use_owner.get(&tool) {
                Some(owner) if Some(owner) == s.parent_id.as_ref() => {}
                Some(owner)
                    if s.parent_id
                        .as_ref()
                        .is_some_and(|p| owner.starts_with(&format!("{p}/")))
                        && *owner != s.id =>
                {
                    parent_nested += 1;
                    s.notes.push(format!(
                        "started by subagent claude:{owner} of the same session"
                    ));
                    s.parent_id = Some(owner.clone());
                }
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
            "invalid_utf8_lines": invalid_utf8_lines,
            "unreadable_files": unreadable.len() as u64 + unreadable_elsewhere,
            "unreadable": unreadable,
            "unreadable_in_other_project_dirs": unreadable_elsewhere,
            "subagent_parent_mismatch": parent_mismatch,
            "subagent_parent_unverified": parent_unverified,
            "subagent_parent_nested": parent_nested,
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
    let mut fork_findings: Vec<Finding> = Vec::new();
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
            archive: &inp.archive,
        };
        // Earlier segments of resumed threads are not in the database.
        let seg_index = if selected.is_empty() {
            codex::Index::default()
        } else {
            codex::Index::build(&[&inp.codex_sessions, &inp.codex_archive])
        };
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<(usize, codex::Parsed)>> = Mutex::new(Vec::new());
        std::thread::scope(|sc| {
            for _ in 0..jobs {
                sc.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(t) = selected.get(i) else { break };
                        let r = codex::resolve(&t.rollout_path, Some(&inp.codex_archive));
                        let segs = seg_index.segments(&t.id, r);
                        let p = codex::parse(t, &segs, &cx);
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
        let (mut sum_untracked, mut sum_rewound) = (0u64, 0u64);
        let (mut rewinds, mut rewinds_unmatched, mut rewound_total) = (0u64, 0u64, 0u64);
        let mut examples = Vec::new();
        let (mut multi_inc, mut multi_tier, mut unparseable) = (0u64, 0u64, 0u64);
        let (mut untracked_threads, mut untracked_total) = (0u64, 0u64);
        let (mut segmented_threads, mut segment_files) = (0u64, 0u64);
        let mut damaged_counted = 0u64;
        // Fork baselines are checked against the base thread's counters.
        let mut counters: HashMap<String, Vec<u64>> = HashMap::new();
        let mut forks: Vec<(String, String, u64, u64)> = Vec::new();
        for (i, mut p) in results {
            let t = selected[i];
            multi_inc += p.check.multi_request_increments;
            multi_tier += p.check.multi_request_tier_differs;
            unparseable += p.check.unparseable_counters;
            rewinds += p.check.rewinds;
            rewinds_unmatched += p.check.rewinds_unmatched;
            rewound_total += p.check.rewound;
            if p.check.untracked > 0 {
                untracked_threads += 1;
                untracked_total += p.check.untracked;
            }
            if let (Some(f), Some((total, base))) = (&p.forked_from, p.first_counter) {
                forks.push((t.id.clone(), f.clone(), total, base));
            }
            counters.insert(t.id.clone(), std::mem::take(&mut p.counter_totals));
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
            if tr.segments.len() > 1 {
                segmented_threads += 1;
                segment_files += tr.segments.len() as u64;
            }
            if matches!(tr.status.as_str(), "damaged" | "incomplete") {
                damaged_counted += p.check.own_total;
            }
            *by_basis.entry(p.session.match_basis.clone()).or_default() += 1;
            if p.session.role == "main" {
                main += 1;
            } else {
                sub += 1;
            }
            if p.session.total.is_none() && p.check.token_events == 0 {
                no_counters += 1;
                fallback_tokens += p.session.db_tokens_used.unwrap_or(0);
            }
            if let (Some(db), Some(last)) = (p.session.db_tokens_used, p.check.last_total) {
                compared += 1;
                sum_db += db;
                sum_last += last;
                sum_own += p.check.own_total;
                sum_inh += p.check.inherited;
                sum_untracked += p.check.untracked;
                sum_rewound += p.check.rewound;
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
                "sum_untracked_at_start": sum_untracked,
                "sum_own_total": sum_own,
                "sum_rewound": sum_rewound,
                "own_plus_inherited_plus_untracked_minus_rewound_equals_last": sum_own + sum_inh + sum_untracked == sum_last + sum_rewound,
                "tokens_used_overcount_factor": ratio(sum_db, sum_own),
            });
        }
        checks.insert("codex_tokens_used".into(), cmp);
        let mut fb = BTreeMap::<&str, u64>::new();
        for (child, parent, total, base) in &forks {
            let outcome = fork_outcome(counters.get(parent), *total, *base);
            *fb.entry(outcome).or_default() += 1;
            if matches!(outcome, "replayed" | "mismatched") {
                let replayed = outcome == "replayed";
                fork_findings.push(detect::Ctx { th, at: &at }.finding(
                    "fork-baseline",
                    if replayed { "error" } else { "warn" },
                    &format!("codex:{child}"),
                    if replayed {
                        format!("the fork's first token counter repeats a counter of its base thread codex:{parent}, so its first request is the base thread's and was counted twice")
                    } else {
                        format!("the fork's starting counter ({base} tokens) matches no counter of its base thread codex:{parent}; the inherited/own split is unverified")
                    },
                    Vec::new(),
                    json!({"base_thread": format!("codex:{parent}"), "first_counter_total": total, "starting_counter": base}),
                ));
            }
        }
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
            "segment_index": {"files": seg_index.files, "unreadable_dirs": seg_index.unreadable_dirs, "unreadable_entries": seg_index.unreadable_entries},
            "threads_with_several_segments": segmented_threads,
            "segment_files_of_those": segment_files,
            "threads_with_untracked_tokens": untracked_threads,
            "untracked_tokens": untracked_total,
            "tokens_counted_before_damage_excluded": damaged_counted,
            "unparseable_token_counters": unparseable,
            "multi_request_increments": multi_inc,
            "multi_request_increments_tier_differs": multi_tier,
            "fork_baselines": fb,
            "segment_rewinds": {"count": rewinds, "starting_counter_unmatched": rewinds_unmatched, "rewound_tokens": rewound_total},
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
    inputs.insert(
        "codex_sessions",
        json!({"path": display_path(&inp.codex_sessions), "present": inp.codex_sessions.is_dir()}),
    );
    coverage.insert("codex".into(), codex_cov);

    // ---- Link T3 threads to native sessions.
    let mut t3_cov = match &t3_absent {
        Some(reason) => json!({"present": false, "reason": reason}),
        None => json!({"present": false}),
    };
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
        // Main sessions first: a subagent runs under its session's billing
        // route, decided when that session started.
        let mut main_billing: HashMap<String, crate::session::Billing> = HashMap::new();
        let mut inherited = 0u64;
        let mut order: Vec<usize> = (0..sessions.len())
            .filter(|i| sessions[*i].source == "claude")
            .collect();
        order.sort_by_key(|i| sessions[*i].role != "main");
        for i in order {
            let s = &mut sessions[i];
            if s.role == "subagent"
                && let Some(main) = s.id.split('/').next()
                && let Some(b) = main_billing.get(main)
            {
                s.billing.mode = b.mode.clone();
                s.billing.basis = format!("inherited from its session claude:{main}: {}", b.basis);
                inherited += 1;
                if b.mode != "unknown" {
                    *matched.entry(b.mode.clone()).or_default() += 1;
                }
                continue;
            }
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
            if s.role == "subagent" {
                s.billing
                    .basis
                    .push_str("; its session is not in the scan, so its own first record was used");
            } else {
                main_billing.insert(s.id.clone(), s.billing.clone());
            }
        }
        billing_cov = json!({
            "present": true,
            "decisions": log.decisions.len(),
            "bad_lines": log.bad_lines,
            "range": range,
            "decisions_in_project": log.in_project(root),
            "claude_sessions_matched_by_mode": matched,
            "claude_subagents_inheriting_session_billing": inherited,
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
    findings.extend(fork_findings);

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
    let mut active_unknown = BTreeMap::<&str, u64>::new();
    for s in sessions.iter().filter(|s| s.active_ms.is_none()) {
        *active_unknown.entry(s.source.as_str()).or_default() += 1;
    }
    let union_all = union_ms(all);

    // ---- Write.
    std::fs::create_dir_all(&inp.out).with_context(|| format!("creating {}", inp.out.display()))?;
    write_jsonl(&inp.out.join("sessions.jsonl"), &sessions)?;
    write_jsonl(&inp.out.join("findings.jsonl"), &findings)?;
    let errors = findings.iter().filter(|f| f.severity == "error").count();
    let archive_totals = {
        let known: Vec<_> = sessions.iter().filter_map(|s| s.archive.as_ref()).collect();
        let searches: u64 = known.iter().map(|a| a.searches).sum();
        let greps = (!inp.archive.roots.is_empty())
            .then(|| known.iter().filter_map(|a| a.greps).sum::<u64>());
        let reads = (!inp.archive.roots.is_empty())
            .then(|| known.iter().filter_map(|a| a.reads).sum::<u64>());
        json!({
            "roots": inp.archive.roots.iter().map(|r| tilde(r)).collect::<Vec<_>>(),
            "roots_basis": inp.archive_basis,
            "sessions": known.len(),
            "searches": searches,
            "opens": known.iter().map(|a| a.opens).sum::<u64>(),
            "related": known.iter().map(|a| a.related).sum::<u64>(),
            "no_hit_searches": known.iter().map(|a| a.no_hit_searches).sum::<u64>(),
            "greps": greps,
            "reads": reads,
            "search_share": greps.and_then(|g| ratio(searches, searches + g)),
            "grep_share": greps.and_then(|g| ratio(g, searches + g)),
        })
    };
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
        "archive": archive_totals,
        "coverage": coverage,
        "cross_checks": checks,
        "active_union_ms": {
            "by_source": union,
            "claude_and_codex": union_native,
            "all_sources": union_all,
            "basis": "union of the sessions' turn intervals (claude, codex) or run intervals (t3), so concurrent sessions are not double counted",
            "sessions_with_unknown_active_time": active_unknown,
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

    #[test]
    fn fork_outcomes() {
        let parent = vec![5000, 8000];
        assert_eq!(fork_outcome(Some(&parent), 9000, 8000), "verified");
        // A replay of the base thread's first counter has a zero base.
        assert_eq!(fork_outcome(Some(&parent), 5000, 0), "replayed");
        assert_eq!(fork_outcome(Some(&parent), 8000, 5000), "replayed");
        // A first snapshot without a request (last = 0) is not a replay.
        assert_eq!(fork_outcome(Some(&parent), 8000, 8000), "verified");
        assert_eq!(fork_outcome(Some(&parent), 300, 0), "zero_base");
        assert_eq!(fork_outcome(None, 300, 0), "zero_base");
        assert_eq!(fork_outcome(None, 9000, 8000), "parent_unavailable");
        assert_eq!(fork_outcome(Some(&parent), 9000, 7000), "mismatched");
    }
}
