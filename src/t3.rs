//! T3 Code orchestrator: the `orchestration_v2_projection_*` tables of
//! `statev2.sqlite`. T3 records runs and their status but no tokens; tokens
//! come from the native Claude or Codex session a thread links to.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use crate::session::{Session, Spawn, T3Info};
use crate::util::{parse_ts, tilde};

/// A reference from a T3 thread to a native session.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Link {
    /// Native thread or session id (strong reference).
    Thread { source: String, id: String },
    /// Native turn id (Codex turn id or Claude record uuid; weak).
    Turn { source: String, id: String },
    /// Claude subagent id, from a provider-native subagent thread id.
    ClaudeAgent { id: String },
}

pub struct T3Thread {
    pub session: Session,
    pub links: Vec<Link>,
    /// Run intervals (startedAt, completedAt) in Unix milliseconds.
    pub intervals: Vec<(i64, i64)>,
}

pub struct T3Data {
    pub project_id: String,
    pub workspace_root: Option<String>,
    pub threads: Vec<T3Thread>,
    /// Direct SQL count of runs by (provider, status), for the cross-check.
    pub sql_runs: BTreeMap<String, BTreeMap<String, u64>>,
}

fn source_of(driver: &str) -> Option<&'static str> {
    match driver {
        "claudeAgent" | "claude" => Some("claude"),
        "codex" => Some("codex"),
        _ => None,
    }
}

/// Decodes `%XX` escapes once.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Links encoded in a T3 provider-thread or child-thread id:
/// `...:provider:<driver>:[provider-instance:<x>:]native-thread:<native id>`.
pub fn links_in_id(id: &str) -> Vec<Link> {
    let Some(pos) = id.find("native-thread:") else {
        return Vec::new();
    };
    let head = &id[..pos];
    let driver = head
        .split(":provider:")
        .nth(1)
        .and_then(|r| r.split(':').next())
        .unwrap_or("");
    let Some(source) = source_of(driver) else {
        return Vec::new();
    };
    let native = percent_decode(&id[pos + "native-thread:".len()..]);
    if native.starts_with("pending:") {
        return Vec::new();
    }
    if native.starts_with("provider-thread:") {
        // A provider-native Claude subagent: `<parent provider thread>:<agent id>`.
        if source == "claude"
            && let Some(aid) = native.rsplit(':').next()
            && !aid.is_empty()
            && aid.bytes().all(|c| c.is_ascii_alphanumeric())
        {
            return vec![Link::ClaudeAgent {
                id: aid.to_string(),
            }];
        }
        return Vec::new();
    }
    vec![Link::Thread {
        source: source.to_string(),
        id: native,
    }]
}

#[derive(Deserialize, Default)]
struct ThreadPayload {
    lineage: Option<Lineage>,
    #[serde(rename = "creationSource")]
    creation_source: Option<String>,
    #[serde(rename = "activeProviderThreadId")]
    active_provider_thread_id: Option<String>,
    #[serde(rename = "worktreePath")]
    worktree_path: Option<String>,
}

#[derive(Deserialize, Default)]
struct Lineage {
    #[serde(rename = "parentThreadId")]
    parent: Option<String>,
    #[serde(rename = "relationshipToParent")]
    relationship: Option<String>,
}

#[derive(Deserialize, Default)]
struct RunPayload {
    #[serde(rename = "modelSelection")]
    model_selection: Option<ModelSelection>,
    #[serde(rename = "startedAt")]
    started_at: Option<String>,
    #[serde(rename = "completedAt")]
    completed_at: Option<String>,
}

#[derive(Deserialize, Default)]
struct ModelSelection {
    model: Option<String>,
    #[serde(default)]
    options: Vec<OptionKv>,
}

#[derive(Deserialize)]
struct OptionKv {
    id: Option<String>,
    value: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct NativeRef {
    driver: Option<String>,
    #[serde(rename = "nativeId")]
    native_id: Option<String>,
    strength: Option<String>,
}

#[derive(Deserialize)]
struct ProviderThreadPayload {
    #[serde(rename = "nativeThreadRef")]
    native_thread_ref: Option<NativeRef>,
}

#[derive(Deserialize)]
struct ProviderTurnPayload {
    #[serde(rename = "nativeTurnRef")]
    native_turn_ref: Option<NativeRef>,
}

/// Finds the project by id, or by a workspace root equal to `project_root`.
/// `None` when no id was given and no project has that root: T3 simply has
/// no record of this project. An explicit id that does not exist is an error.
fn find_project(
    conn: &Connection,
    id: Option<&str>,
    project_root: &str,
) -> Result<Option<(String, Option<String>)>> {
    if let Some(id) = id {
        let root: Option<Option<String>> = conn
            .query_row(
                "select workspace_root from projection_projects where project_id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .with_context(|| format!("looking up T3 project {id}"))?;
        let Some(root) = root else {
            bail!("T3 project {id} not found");
        };
        return Ok(Some((id.to_string(), root)));
    }
    let mut st = conn.prepare(
        "select project_id, workspace_root from projection_projects
         where rtrim(workspace_root, '/') = ?1 and deleted_at is null",
    )?;
    let rows: Vec<(String, Option<String>)> = st
        .query_map([project_root], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    match rows.len() {
        0 => Ok(None),
        1 => Ok(rows.into_iter().next()),
        n => bail!("{n} T3 projects have workspace root {project_root}; pass --t3-project"),
    }
}

pub fn load(
    conn: &Connection,
    project_id: Option<&str>,
    project_root: &str,
    include_titles: bool,
) -> Result<Option<T3Data>> {
    let Some((pid, root)) = find_project(conn, project_id, project_root)? else {
        return Ok(None);
    };

    let mut st = conn.prepare(
        "select thread_id, title, default_provider, created_at, updated_at, archived_at, deleted_at, payload_json
         from orchestration_v2_projection_threads where project_id = ?1 order by thread_id",
    )?;
    struct Row {
        id: String,
        title: Option<String>,
        provider: Option<String>,
        created: Option<String>,
        archived: bool,
        deleted: bool,
        payload: ThreadPayload,
    }
    let rows: Vec<Row> = st
        .query_map([&pid], |r| {
            let payload: Option<String> = r.get(7)?;
            Ok(Row {
                id: r.get(0)?,
                title: r.get(1)?,
                provider: r.get(2)?,
                created: r.get(3)?,
                archived: r.get::<_, Option<String>>(5)?.is_some(),
                deleted: r.get::<_, Option<String>>(6)?.is_some(),
                payload: payload
                    .and_then(|p| serde_json::from_str(&p).ok())
                    .unwrap_or_default(),
            })
        })?
        .collect::<rusqlite::Result<_>>()
        .context("reading T3 threads")?;

    let mut runs_st = conn.prepare(
        "select status, payload_json, provider from orchestration_v2_projection_runs where thread_id = ?1 order by ordinal",
    )?;
    let mut pt_st = conn.prepare(
        "select provider_thread_id, payload_json from orchestration_v2_projection_provider_threads where thread_id = ?1",
    )?;
    let mut turn_st = conn.prepare(
        "select payload_json from orchestration_v2_projection_provider_turns where thread_id = ?1",
    )?;

    let mut threads = Vec::new();
    for row in rows {
        let mut s = Session::new("t3", &row.id);
        let provider = row.provider.clone().unwrap_or_else(|| "unknown".into());
        if let Some(l) = &row.payload.lineage
            && l.relationship.as_deref() == Some("subagent")
        {
            s.role = "subagent".into();
            s.parent_id = l.parent.clone();
            s.spawn = Some(Spawn {
                mechanism: match row.payload.creation_source.as_deref() {
                    Some("provider") => "provider-native".into(),
                    _ => "t3-delegate".into(),
                },
                agent_type: row.payload.creation_source.clone(),
                depth: None,
            });
        }
        s.match_basis = "t3-project".into();
        s.client = Some("t3-code".into());
        s.cwd = row
            .payload
            .worktree_path
            .as_deref()
            .map(tilde)
            .or(root.as_deref().map(tilde));
        if include_titles {
            s.title = row.title.clone();
        }

        let mut info = T3Info {
            project_id: pid.clone(),
            provider: provider.clone(),
            archived: row.archived,
            deleted: row.deleted,
            ..T3Info::default()
        };
        let mut start: Option<i64> = None;
        let mut end: Option<i64> = None;
        let mut active = 0i64;
        let mut timed = 0u64;
        let mut intervals = Vec::new();
        let mut runs = 0u64;
        let run_rows: Vec<(String, Option<String>, Option<String>)> = runs_st
            .query_map([&row.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (status, payload, run_provider) in run_rows {
            runs += 1;
            *info.runs_by_status.entry(status.clone()).or_default() += 1;
            *info
                .runs_by_provider
                .entry(run_provider.unwrap_or_else(|| "unknown".into()))
                .or_default()
                .entry(status.clone())
                .or_default() += 1;
            let p: RunPayload = payload
                .and_then(|p| serde_json::from_str(&p).ok())
                .unwrap_or_default();
            let model = p
                .model_selection
                .as_ref()
                .and_then(|m| m.model.clone())
                .unwrap_or_else(|| "unknown".into());
            *info.runs_by_model.entry(model).or_default() += 1;
            let effort = p
                .model_selection
                .as_ref()
                .and_then(|m| m.options.iter().find(|o| o.id.as_deref() == Some("effort")))
                .and_then(|o| o.value.as_ref())
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            *info.runs_by_effort.entry(effort).or_default() += 1;
            let st = p.started_at.as_deref().and_then(parse_ts);
            let en = p.completed_at.as_deref().and_then(parse_ts);
            if let Some(t) = st {
                start = Some(start.map_or(t, |s| s.min(t)));
            }
            if let Some(t) = en {
                end = Some(end.map_or(t, |e| e.max(t)));
            }
            if let (Some(a), Some(b)) = (st, en) {
                active += (b - a).max(0);
                timed += 1;
                intervals.push((a, b));
            }
            if matches!(status.as_str(), "interrupted" | "cancelled") {
                s.aborts += 1;
            }
        }
        s.turns = (runs > 0).then_some(runs);
        s.open_turns = info.runs_by_status.get("running").copied().unwrap_or(0);
        if runs > 0 {
            s.active_ms = (timed > 0).then_some(active);
            if timed < runs {
                s.notes.push(format!(
                    "{} run(s) lack a start or completion time",
                    runs - timed
                ));
            }
        }
        s.active_basis = "t3: sum over runs of completedAt - startedAt".into();
        s.token_basis =
            "t3: no token data; tokens are counted in the linked native sessions".into();
        // The thread's updated_at moves on any metadata change, so only run
        // times establish a span. A thread without runs (a mirror of a
        // native subagent) has its creation time as start and no end.
        if runs > 0 {
            s.set_span(start, end);
        } else {
            s.set_span(row.created.as_deref().and_then(parse_ts), None);
            s.notes.push("no runs; wall time unknown".into());
        }

        // Links.
        let mut links = Vec::new();
        let mut strong = false;
        links.extend(links_in_id(&row.id));
        if let Some(a) = &row.payload.active_provider_thread_id {
            links.extend(links_in_id(a));
        }
        let pts: Vec<(String, Option<String>)> = pt_st
            .query_map([&row.id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (ptid, payload) in pts {
            links.extend(links_in_id(&ptid));
            let p: Option<ProviderThreadPayload> =
                payload.and_then(|p| serde_json::from_str(&p).ok());
            if let Some(NativeRef {
                driver: Some(d),
                native_id: Some(n),
                strength,
            }) = p.and_then(|p| p.native_thread_ref)
                && !n.starts_with("pending")
                && let Some(src) = source_of(&d)
            {
                strong |= strength.as_deref() == Some("strong");
                links.push(Link::Thread {
                    source: src.into(),
                    id: n,
                });
            }
        }
        let turns: Vec<Option<String>> = turn_st
            .query_map([&row.id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for payload in turns.into_iter().flatten() {
            if let Ok(ProviderTurnPayload {
                native_turn_ref:
                    Some(NativeRef {
                        driver: Some(d),
                        native_id: Some(n),
                        ..
                    }),
            }) = serde_json::from_str(&payload)
                && let Some(src) = source_of(&d)
                && !n.starts_with("pending")
                && !n.starts_with("turn:")
            {
                links.push(Link::Turn {
                    source: src.into(),
                    id: n,
                });
            }
        }
        links.sort();
        links.dedup();
        if !strong && links.iter().any(|l| matches!(l, Link::Thread { .. })) {
            s.notes
                .push("native thread id taken from the T3 thread or provider-thread id".into());
        }
        s.t3 = Some(info);
        threads.push(T3Thread {
            session: s,
            links,
            intervals,
        });
    }

    // Direct count for the cross-check.
    let mut sql_runs: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut st = conn.prepare(
        "select r.provider, r.status, count(*) from orchestration_v2_projection_runs r
         join orchestration_v2_projection_threads t on t.thread_id = r.thread_id
         where t.project_id = ?1 group by 1, 2",
    )?;
    for row in st.query_map(params![pid], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?
                .unwrap_or_else(|| "unknown".into()),
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (p, s, n) = row?;
        sql_runs.entry(p).or_default().insert(s, n as u64);
    }

    Ok(Some(T3Data {
        project_id: pid,
        workspace_root: root,
        threads,
        sql_runs,
    }))
}

pub fn open(path: &Path) -> Result<(Connection, &'static str)> {
    crate::codex::open_ro(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_decode_to_links() {
        assert_eq!(
            links_in_id("thread:provider:codex:native-thread:01a1-x"),
            vec![Link::Thread {
                source: "codex".into(),
                id: "01a1-x".into()
            }]
        );
        assert_eq!(
            links_in_id(
                "provider-thread:provider:claudeAgent:provider-instance:claudeAgent:native-thread:0ea8"
            ),
            vec![Link::Thread {
                source: "claude".into(),
                id: "0ea8".into()
            }]
        );
        assert!(
            links_in_id("provider-thread:provider:codex:native-thread:pending%3Arun%3Ax")
                .is_empty()
        );
        assert_eq!(
            links_in_id(
                "thread:provider:claudeAgent:native-thread:provider-thread%3Aprovider%3AclaudeAgent%3Anative-thread%3Apending%253Arun%3Aaf6bb09e"
            ),
            vec![Link::ClaudeAgent {
                id: "af6bb09e".into()
            }]
        );
    }
}
