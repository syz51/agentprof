//! `agentprof report <dir>`: a Markdown summary of a scan's outputs.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::detect::{Finding, is_blocked_status, is_failed_status};
use crate::session::{Acc, Session, ToolStat};

/// Checks a record's `schema` against `expected` (`name/major`): the name
/// must match and the major version must be the one this build reads; a
/// later minor version (`name/1.2`) only adds fields and is accepted.
fn check_schema(found: Option<&str>, expected: &str) -> Result<()> {
    let (name, major) = expected.split_once('/').expect("name/major");
    let ok = found
        .and_then(|f| f.split_once('/'))
        .is_some_and(|(n, v)| n == name && v.split('.').next() == Some(major));
    if !ok {
        bail!(
            "schema {} is not supported; this build reads {expected}",
            found.unwrap_or("(none)")
        );
    }
    Ok(())
}

fn read_jsonl<T: DeserializeOwned>(path: &Path, schema: &str) -> Result<Vec<T>> {
    #[derive(serde::Deserialize)]
    struct Head {
        schema: Option<String>,
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            let at = || format!("{}:{}", path.display(), i + 1);
            let head: Head =
                serde_json::from_str(l).with_context(|| format!("{}: invalid record", at()))?;
            check_schema(head.schema.as_deref(), schema).with_context(at)?;
            serde_json::from_str(l).with_context(|| format!("{}: invalid record", at()))
        })
        .collect()
}

/// `1234567` -> `1,234,567`.
pub fn num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn hours(ms: i64) -> String {
    format!("{:.1}", ms as f64 / 3_600_000.0)
}

fn usd(a: &Acc) -> String {
    match a.usd() {
        Some(u) => format!("${u:.2}"),
        None if a.priced_usd > 0.0 => format!(
            "unknown (≥ ${:.2}; {} unpriced)",
            a.priced_usd,
            num(a.unpriced_requests)
        ),
        None => format!("unknown ({} unpriced)", num(a.unpriced_requests)),
    }
}

fn pct(a: u64, b: u64) -> String {
    if b == 0 {
        "–".into()
    } else {
        format!("{:.1}%", a as f64 * 100.0 / b as f64)
    }
}

fn acc_header(out: &mut String, first: &str) {
    let _ = writeln!(
        out,
        "| {first} | Requests | Input (uncached) | Cache write | Cache read | Output | of which reasoning | Total tokens | Cache-read share of prompt | API-equivalent cost |"
    );
    let _ = writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
}

fn acc_row(out: &mut String, name: &str, a: &Acc) {
    let t = &a.tokens;
    let _ = writeln!(
        out,
        "| {name} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
        num(a.requests),
        num(t.input),
        num(t.cache_writes()),
        num(t.cache_read),
        num(t.output),
        num(t.reasoning),
        num(t.total()),
        pct(t.cache_read, t.prompt()),
        usd(a)
    );
}

#[derive(Default)]
struct Group {
    sessions: u64,
    acc: Acc,
    with_tokens: u64,
    wall_ms: i64,
    active_ms: i64,
    turns: u64,
    /// Sessions whose wall time, active time or turns are unknown (null).
    wall_unknown: u64,
    active_unknown: u64,
    turns_unknown: u64,
}

impl Group {
    fn add(&mut self, s: &Session) {
        self.sessions += 1;
        if let Some(t) = &s.total {
            self.acc.merge(t);
            self.with_tokens += 1;
        }
        match s.wall_ms {
            Some(v) => self.wall_ms += v,
            None => self.wall_unknown += 1,
        }
        match s.active_ms {
            Some(v) => self.active_ms += v,
            None => self.active_unknown += 1,
        }
        match s.turns {
            Some(v) => self.turns += v,
            None => self.turns_unknown += 1,
        }
    }
}

/// A sum over known values, noting how many sessions were unknown.
fn known(sum: String, unknown: u64) -> String {
    if unknown == 0 {
        sum
    } else {
        format!("{sum} ({} unknown)", num(unknown))
    }
}

fn session_tokens(s: &Session) -> u64 {
    s.total.as_ref().map_or(0, |a| a.tokens.total())
}

pub fn render(dir: &Path, top: usize) -> Result<String> {
    let sessions: Vec<Session> = read_jsonl(&dir.join("sessions.jsonl"), crate::session::SCHEMA)?;
    let findings: Vec<Finding> =
        read_jsonl(&dir.join("findings.jsonl"), crate::detect::FINDING_SCHEMA)?;
    let scan: Value = {
        let p = dir.join("scan.json");
        let text =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let v: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
        check_schema(v["schema"].as_str(), crate::scan::SCAN_SCHEMA)
            .with_context(|| p.display().to_string())?;
        v
    };
    let mut o = String::new();
    let g = |path: &[&str]| -> Value {
        let mut v = &scan;
        for k in path {
            v = &v[*k];
        }
        v.clone()
    };
    let s_ = |v: Value| {
        v.as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string())
    };

    let _ = writeln!(o, "# agentprof report: {}\n", s_(g(&["project"])));
    let _ = writeln!(
        o,
        "Scanned {} with agentprof {}. Days are in {}. {} sessions, {} findings.\n",
        s_(g(&["generated_at"])),
        s_(g(&["version"])),
        s_(g(&["timezone"])),
        sessions.len(),
        findings.len()
    );
    let _ = writeln!(
        o,
        "**Every cost here is an API-equivalent estimate**: the tokens each request used, priced at the provider's standard-tier list price from `pricing.toml`. It is not an invoice, and it ignores subscriptions. A cost reads \"unknown\" when any request in it has no verified price; the figure after \"≥\" covers the priced requests only.\n"
    );
    let _ = writeln!(o, "- Anthropic: {}", s_(g(&["pricing", "anthropic_basis"])));
    let _ = writeln!(o, "- OpenAI: {}\n", s_(g(&["pricing", "openai_basis"])));

    // ---- Totals by source.
    let mut by_src: BTreeMap<String, Group> = BTreeMap::new();
    let mut by_src_role: BTreeMap<(String, String), Group> = BTreeMap::new();
    for s in &sessions {
        by_src.entry(s.source.clone()).or_default().add(s);
        by_src_role
            .entry((s.source.clone(), s.role.clone()))
            .or_default()
            .add(s);
    }
    let _ = writeln!(o, "## Totals by source\n");
    acc_header(&mut o, "Source");
    let mut native = Acc::default();
    for (src, gr) in &by_src {
        if src == "t3" {
            continue;
        }
        native.merge(&gr.acc);
        acc_row(&mut o, src, &gr.acc);
    }
    acc_row(&mut o, "**claude + codex**", &native);
    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "| Source | Sessions | with token data | Turns or runs | Wall time, sum (h) | Active time, sum (h) | Active time, union (h) |"
    );
    let _ = writeln!(o, "|---|---:|---:|---:|---:|---:|---:|");
    for (src, gr) in &by_src {
        let union = g(&["active_union_ms", "by_source", src]).as_i64();
        let _ = writeln!(
            o,
            "| {src} | {} | {} | {} | {} | {} | {} |",
            num(gr.sessions),
            num(gr.with_tokens),
            known(num(gr.turns), gr.turns_unknown),
            known(hours(gr.wall_ms), gr.wall_unknown),
            known(hours(gr.active_ms), gr.active_unknown),
            union.map(hours).unwrap_or_else(|| "–".into())
        );
    }
    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "Active time, union across Claude and Codex: {} h; across all sources: {} h. Sums count concurrent sessions (subagents run in parallel with their parent) once each; unions count each moment once. T3 sessions carry no tokens: their tokens are those of the linked native sessions, so T3 never adds to token totals.\n",
        g(&["active_union_ms", "claude_and_codex"])
            .as_i64()
            .map(hours)
            .unwrap_or_else(|| "–".into()),
        g(&["active_union_ms", "all_sources"])
            .as_i64()
            .map(hours)
            .unwrap_or_else(|| "–".into()),
    );
    let _ = writeln!(o, "Definitions:");
    let mut bases: BTreeMap<String, (String, String)> = BTreeMap::new();
    for s in &sessions {
        bases
            .entry(s.source.clone())
            .or_insert((s.active_basis.clone(), s.token_basis.clone()));
    }
    for (src, (a, t)) in &bases {
        let _ = writeln!(o, "- {src} active time — {a}");
        let _ = writeln!(o, "- {src} tokens — {t}");
    }
    let _ = writeln!(o);

    // ---- Main vs subagent.
    let _ = writeln!(o, "## Main sessions and subagents\n");
    acc_header(&mut o, "Source / role");
    for ((src, role), gr) in &by_src_role {
        if src == "t3" {
            continue;
        }
        acc_row(
            &mut o,
            &format!("{src} / {role} ({} sessions)", num(gr.sessions)),
            &gr.acc,
        );
    }
    let _ = writeln!(o);

    // ---- By model, effort, day.
    for (title, key) in [("By model", 0), ("By reasoning effort", 1), ("By day", 2)] {
        let mut m: BTreeMap<(String, String), Acc> = BTreeMap::new();
        for s in sessions.iter().filter(|s| s.source != "t3") {
            let src = match key {
                0 => &s.by_model,
                1 => &s.by_effort,
                _ => &s.by_day,
            };
            for (k, a) in src {
                m.entry((
                    if key == 2 {
                        String::new()
                    } else {
                        s.source.clone()
                    },
                    k.clone(),
                ))
                .or_default()
                .merge(a);
            }
        }
        let _ = writeln!(o, "## {title}\n");
        acc_header(&mut o, if key == 2 { "Day" } else { "Source / value" });
        for ((src, k), a) in &m {
            let name = if src.is_empty() {
                k.clone()
            } else {
                format!("{src} / {k}")
            };
            acc_row(&mut o, &name, a);
        }
        let _ = writeln!(o);
    }

    // ---- Long-context pricing.
    let mut lc: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for s in sessions.iter().filter(|s| s.source != "t3") {
        for (k, a) in &s.by_model {
            lc.entry((s.source.clone(), k.clone()))
                .or_default()
                .merge(a);
        }
    }
    let mut lc_total = Acc::default();
    let _ = writeln!(o, "## Long-context pricing\n");
    let _ = writeln!(
        o,
        "Requests whose prompt exceeded the model's long-context threshold in `pricing.toml` are priced at that tier. The surcharge is the tier's cost minus the same tokens at standard rates; it is included in every cost above.\n"
    );
    let _ = writeln!(
        o,
        "| Source / model | Priced requests | Over the threshold | Share | Surcharge | Cost |\n|---|---:|---:|---:|---:|---:|"
    );
    for ((src, model), a) in lc.iter().filter(|(_, a)| a.long_context_requests > 0) {
        let priced = a.requests - a.unpriced_requests;
        let _ = writeln!(
            o,
            "| {src} / {model} | {} | {} | {} | ${:.2} | {} |",
            num(priced),
            num(a.long_context_requests),
            pct(a.long_context_requests, priced),
            a.long_context_usd,
            usd(a)
        );
        lc_total.merge(a);
    }
    if lc_total.long_context_requests == 0 {
        let _ = writeln!(o, "| none | – | 0 | – | $0.00 | – |");
    } else {
        let priced = lc_total.requests - lc_total.unpriced_requests;
        let _ = writeln!(
            o,
            "| **all models above** | {} | {} | {} | ${:.2} | {} |",
            num(priced),
            num(lc_total.long_context_requests),
            pct(lc_total.long_context_requests, priced),
            lc_total.long_context_usd,
            usd(&lc_total)
        );
    }
    let _ = writeln!(o);

    // ---- Billing mode.
    let _ = writeln!(o, "## Billing mode\n");
    let _ = writeln!(
        o,
        "Whether a session ran on a subscription or the API, where the data says so (Claude: the API-routing log; Codex: a ChatGPT `plan_type` in its rate-limit snapshots).\n"
    );
    let mut bill: BTreeMap<(String, String, String), Group> = BTreeMap::new();
    for s in sessions.iter().filter(|s| s.source != "t3") {
        bill.entry((
            s.source.clone(),
            s.billing.mode.clone(),
            s.billing.plan_type.clone().unwrap_or_default(),
        ))
        .or_default()
        .add(s);
    }
    acc_header(&mut o, "Source / mode (plan)");
    for ((src, mode, plan), gr) in &bill {
        let plan = if plan.is_empty() {
            String::new()
        } else {
            format!(" ({plan})")
        };
        acc_row(
            &mut o,
            &format!("{src} / {mode}{plan}, {} sessions", num(gr.sessions)),
            &gr.acc,
        );
    }
    let _ = writeln!(o);

    // ---- Top sessions.
    let mut by_tok: Vec<&Session> = sessions.iter().filter(|s| s.total.is_some()).collect();
    by_tok.sort_by(|a, b| {
        session_tokens(b)
            .cmp(&session_tokens(a))
            .then(a.id.cmp(&b.id))
    });
    let _ = writeln!(o, "## Top {top} sessions by tokens\n");
    top_table(&mut o, by_tok.iter().take(top).copied());
    let mut by_wall: Vec<&Session> = sessions.iter().filter(|s| s.wall_ms.is_some()).collect();
    by_wall.sort_by(|a, b| b.wall_ms.cmp(&a.wall_ms).then(a.id.cmp(&b.id)));
    let _ = writeln!(o, "## Top {top} sessions by wall time\n");
    top_table(&mut o, by_wall.iter().take(top).copied());

    // ---- Tools.
    let _ = writeln!(o, "## Tool calls\n");
    let _ = writeln!(
        o,
        "Latency is the time from the call record to its result record. Errors follow each source's `error_basis`: Claude's `is_error` flag; Codex tool output that starts with a failure prefix.\n"
    );
    let mut tools: BTreeMap<(String, String), ToolStat> = BTreeMap::new();
    for s in &sessions {
        for (name, t) in &s.tools {
            let e = tools.entry((s.source.clone(), name.clone())).or_default();
            e.calls += t.calls;
            e.results += t.results;
            e.errors += t.errors;
            e.latency_ms_total += t.latency_ms_total;
            e.latency_ms_max = e.latency_ms_max.max(t.latency_ms_max);
            e.result_chars_total += t.result_chars_total;
            e.result_chars_max = e.result_chars_max.max(t.result_chars_max);
        }
    }
    let mut tl: Vec<_> = tools.into_iter().collect();
    tl.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then(a.0.cmp(&b.0)));
    let _ = writeln!(
        o,
        "| Source / tool | Calls | Results | Errors | Error rate | Mean latency (s) | Max latency (s) | Mean result chars | Max result chars |"
    );
    let _ = writeln!(o, "|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for ((src, name), t) in &tl {
        let mean = |tot: u64| {
            if t.results == 0 {
                "–".to_string()
            } else {
                format!("{:.1}", tot as f64 / t.results as f64)
            }
        };
        let _ = writeln!(
            o,
            "| {src} / {name} | {} | {} | {} | {} | {} | {:.1} | {} | {} |",
            num(t.calls),
            num(t.results),
            num(t.errors),
            pct(t.errors, t.results),
            if t.results == 0 {
                "–".into()
            } else {
                format!(
                    "{:.1}",
                    t.latency_ms_total as f64 / 1000.0 / t.results as f64
                )
            },
            t.latency_ms_max as f64 / 1000.0,
            mean(t.result_chars_total),
            num(t.result_chars_max)
        );
    }
    let _ = writeln!(o);
    let mut ops: BTreeMap<String, (u64, u64, u64, u64)> = BTreeMap::new();
    for s in &sessions {
        for (k, v) in &s.operations {
            let e = ops.entry(format!("{} / {k}", s.source)).or_default();
            e.0 += v.count;
            e.1 += v.failed;
            e.2 += v.duration_ms_total;
            e.3 = e.3.max(v.duration_ms_max);
        }
    }
    if !ops.is_empty() {
        let _ = writeln!(
            o,
            "Codex operations (`item_completed` events inside tool calls):\n"
        );
        let _ = writeln!(
            o,
            "| Operation | Count | Failed | Failure rate | Mean duration (s) | Max duration (s) |"
        );
        let _ = writeln!(o, "|---|---:|---:|---:|---:|---:|");
        for (k, (n, f, d, m)) in &ops {
            let _ = writeln!(
                o,
                "| {k} | {} | {} | {} | {:.1} | {:.1} |",
                num(*n),
                num(*f),
                pct(*f, *n),
                if *n == 0 {
                    0.0
                } else {
                    *d as f64 / 1000.0 / *n as f64
                },
                *m as f64 / 1000.0
            );
        }
        let _ = writeln!(o);
    }

    // ---- Web requests.
    let _ = writeln!(o, "## Web requests and blocked access\n");
    let _ = writeln!(
        o,
        "Statuses are HTTP codes from the tool result, or a failure class (`dns`, `connect-refused`, `connection-reset`, `timeout`, `tls`, `cloudflare-challenge`, `error-other`). `ok-no-status`: the tool succeeded without reporting a code; `no-status`: the source records no outcome (Codex page opens). Blocked means 403, 418, 429 or a Cloudflare challenge.\n"
    );
    let mut web: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut statuses: BTreeMap<String, u64> = BTreeMap::new();
    for s in &sessions {
        for (host, st) in &s.web {
            for (code, n) in st {
                *web.entry(format!("{} / {host}", s.source))
                    .or_default()
                    .entry(code.clone())
                    .or_default() += n;
                *statuses.entry(code.clone()).or_default() += n;
            }
        }
    }
    let _ = writeln!(o, "| Status | Requests | Blocked |");
    let _ = writeln!(o, "|---|---:|---|");
    for (st, n) in &statuses {
        let _ = writeln!(
            o,
            "| {st} | {} | {} |",
            num(*n),
            if is_blocked_status(st) { "yes" } else { "" }
        );
    }
    let _ = writeln!(o);
    let mut hosts: Vec<(&String, u64, u64, u64, String)> = web
        .iter()
        .map(|(h, st)| {
            let total: u64 = st.values().sum();
            let failed: u64 = st
                .iter()
                .filter(|(k, _)| is_failed_status(k))
                .map(|(_, v)| v)
                .sum();
            let blocked: u64 = st
                .iter()
                .filter(|(k, _)| is_blocked_status(k))
                .map(|(_, v)| v)
                .sum();
            let detail = st
                .iter()
                .filter(|(k, _)| is_failed_status(k))
                .map(|(k, v)| format!("{k}×{v}"))
                .collect::<Vec<_>>()
                .join(", ");
            (h, total, failed, blocked, detail)
        })
        .filter(|h| h.2 > 0)
        .collect();
    hosts.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(b.0)));
    if hosts.is_empty() {
        let _ = writeln!(o, "No failed web requests.\n");
    } else {
        let _ = writeln!(o, "Hosts with failures:\n");
        let _ = writeln!(
            o,
            "| Source / host | Requests | Failed | Blocked | Failures by status |"
        );
        let _ = writeln!(o, "|---|---:|---:|---:|---|");
        for (h, t, f, b, d) in hosts.iter().take(40) {
            let _ = writeln!(o, "| {h} | {} | {} | {} | {d} |", num(*t), num(*f), num(*b));
        }
        if hosts.len() > 40 {
            let _ = writeln!(o, "\n{} more host(s) in sessions.jsonl.", hosts.len() - 40);
        }
        let _ = writeln!(o);
    }

    // ---- API errors and rate limits.
    let _ = writeln!(o, "## API errors, rate limits, compactions, aborts\n");
    let mut api: BTreeMap<String, u64> = BTreeMap::new();
    let mut lim: BTreeMap<String, u64> = BTreeMap::new();
    let (mut sat, mut reached, mut refused, mut maxp) = (0u64, 0u64, 0u64, None::<f64>);
    let (mut comp, mut comp_auto, mut comp_manual, mut aborts) =
        (0u64, 0u64, 0u64, BTreeMap::<String, u64>::new());
    for s in &sessions {
        for (k, v) in &s.api_errors {
            *api.entry(format!("{} / {k}", s.source)).or_default() += v;
        }
        for (k, v) in &s.rate_limits.limit_types {
            *lim.entry(format!("{} / {k}", s.source)).or_default() += v;
        }
        sat += s.rate_limits.saturated_observations;
        reached += s.rate_limits.limit_reached_observations;
        refused += s.rate_limits.limit_errors;
        if let Some(p) = s.rate_limits.max_used_percent {
            maxp = Some(maxp.map_or(p, |m: f64| m.max(p)));
        }
        if s.source != "t3" {
            comp += s.compactions.count;
            comp_auto += s.compactions.auto;
            comp_manual += s.compactions.manual;
        }
        *aborts.entry(s.source.clone()).or_default() += s.aborts;
    }
    let _ = writeln!(
        o,
        "- Requests refused at a usage limit (Claude): {}",
        num(refused)
    );
    let _ = writeln!(
        o,
        "- Codex rate-limit snapshots at or above 100%: {}; with a limit-reached type: {}; highest used percent seen: {}",
        num(sat),
        num(reached),
        maxp.map(|p| format!("{p:.0}%"))
            .unwrap_or_else(|| "–".into())
    );
    for (k, v) in &lim {
        let _ = writeln!(o, "  - limit kind {k}: {}", num(*v));
    }
    for (k, v) in &api {
        let _ = writeln!(o, "- API error {k}: {}", num(*v));
    }
    let _ = writeln!(
        o,
        "- Compactions: {} ({} auto, {} manual, the rest of unknown trigger)",
        num(comp),
        num(comp_auto),
        num(comp_manual)
    );
    for (k, v) in &aborts {
        let what = if k == "t3" {
            "runs interrupted or cancelled"
        } else {
            "turns aborted or interrupted"
        };
        let _ = writeln!(o, "- {k}: {} {what}", num(*v));
    }
    let _ = writeln!(o);

    // ---- T3.
    let t3: Vec<&Session> = sessions.iter().filter(|s| s.source == "t3").collect();
    if !t3.is_empty() {
        let _ = writeln!(o, "## T3 runs and linkage\n");
        let mut runs: BTreeMap<(String, String), u64> = BTreeMap::new();
        let mut models: BTreeMap<String, u64> = BTreeMap::new();
        for s in &t3 {
            let i = s.t3.as_ref().expect("t3 session has t3 info");
            for (p, st) in &i.runs_by_provider {
                for (k, v) in st {
                    *runs.entry((p.clone(), k.clone())).or_default() += v;
                }
            }
            for (k, v) in &i.runs_by_model {
                *models.entry(k.clone()).or_default() += v;
            }
        }
        let _ = writeln!(o, "| Provider | Status | Runs |");
        let _ = writeln!(o, "|---|---|---:|");
        for ((p, st), n) in &runs {
            let _ = writeln!(o, "| {p} | {st} | {} |", num(*n));
        }
        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "Runs by requested model: {}\n",
            models
                .iter()
                .map(|(k, v)| format!("{k} {}", num(*v)))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let cov = g(&["coverage", "t3"]);
        let _ = writeln!(
            o,
            "Linkage: {} of {} T3 threads link to a native session in this scan (rate {}). By basis: {}. Native sessions linked from T3: {}.\n",
            cov["threads_linked"],
            cov["threads"],
            cov["thread_link_rate"]
                .as_f64()
                .map(|r| format!("{:.1}%", r * 100.0))
                .unwrap_or_else(|| "–".into()),
            cov["threads_by_link_basis"],
            cov["native_sessions_by_source_linked"],
        );
    }

    // ---- Findings.
    let _ = writeln!(o, "## Findings by rule\n");
    #[derive(Default)]
    struct RuleSum<'a> {
        n: u64,
        severity: BTreeMap<&'a str, u64>,
        examples: Vec<&'a str>,
    }
    let mut rules: BTreeMap<&str, RuleSum> = BTreeMap::new();
    for f in &findings {
        let e = rules.entry(f.rule.as_str()).or_default();
        e.n += 1;
        *e.severity.entry(f.severity.as_str()).or_default() += 1;
        if e.examples.len() < 5 {
            e.examples.push(f.subject.as_str());
        }
    }
    let mut rl: Vec<_> = rules.into_iter().collect();
    rl.sort_by(|a, b| b.1.n.cmp(&a.1.n).then(a.0.cmp(b.0)));
    let _ = writeln!(o, "| Rule | Findings | Severity | Example subjects |");
    let _ = writeln!(o, "|---|---:|---|---|");
    for (
        rule,
        RuleSum {
            n,
            severity,
            examples,
        },
    ) in &rl
    {
        let sev = severity
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let ex = examples
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(o, "| {rule} | {} | {sev} | {ex} |", num(*n));
    }
    let _ = writeln!(
        o,
        "\nRule definitions and thresholds are in the agentprof README; the thresholds used are in `scan.json`.\n"
    );

    // ---- Coverage and cross-checks.
    let _ = writeln!(o, "## Coverage\n");
    let _ = writeln!(
        o,
        "```json\n{}\n```\n",
        serde_json::to_string_pretty(&g(&["coverage"]))?
    );
    let _ = writeln!(o, "## Cross-checks\n");
    let _ = writeln!(
        o,
        "```json\n{}\n```",
        serde_json::to_string_pretty(&g(&["cross_checks"]))?
    );
    Ok(o)
}

fn top_table<'a>(o: &mut String, rows: impl Iterator<Item = &'a Session>) {
    let _ = writeln!(
        o,
        "| Session | Role | Tokens | Cache-read share | Cost | Wall (h) | Active (h) | Turns | Linked T3 thread |"
    );
    let _ = writeln!(o, "|---|---|---:|---:|---:|---:|---:|---:|---|");
    for s in rows {
        let (tok, share, cost) = match &s.total {
            Some(a) => (
                num(a.tokens.total()),
                pct(a.tokens.cache_read, a.tokens.prompt()),
                usd(a),
            ),
            None => ("–".into(), "–".into(), "–".into()),
        };
        let _ = writeln!(
            o,
            "| `{}` | {} | {tok} | {share} | {cost} | {} | {} | {} | {} |",
            s.key(),
            s.role,
            s.wall_ms.map(hours).unwrap_or_else(|| "–".into()),
            s.active_ms.map(hours).unwrap_or_else(|| "–".into()),
            s.turns.map(num).unwrap_or_else(|| "–".into()),
            s.t3_thread_id.as_deref().unwrap_or("")
        );
    }
    let _ = writeln!(o);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands() {
        assert_eq!(num(0), "0");
        assert_eq!(num(1234), "1,234");
        assert_eq!(num(1234567), "1,234,567");
    }
}
