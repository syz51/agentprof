//! End-to-end: builds synthetic Claude, Codex, T3 and billing inputs, runs
//! `agentprof scan` and `agentprof report`, and checks the outputs.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::{Connection, params};
use serde_json::Value;

const SECRET_PROMPT: &str = "SECRET-PROMPT-TEXT";
const SECRET_OUTPUT: &str = "SECRET-TOOL-OUTPUT";
const SECRET_TITLE: &str = "SECRET-THREAD-TITLE";

fn claude_tree(root: &Path) -> PathBuf {
    let projects = root.join("claude/projects");
    let dir = projects.join("-work-proj");
    std::fs::create_dir_all(dir.join("s1/subagents")).unwrap();
    let main = format!(
        r#"{{"type":"user","uuid":"u1","sessionId":"s1","cwd":"/work/proj","entrypoint":"sdk-cli","timestamp":"2026-01-01T00:00:00Z","message":{{"role":"user","content":"{SECRET_PROMPT}"}}}}
{{"type":"assistant","uuid":"a1","requestId":"r1","sessionId":"s1","cwd":"/work/proj/sub","timestamp":"2026-01-01T00:00:02Z","message":{{"id":"m1","model":"claude-opus-5-5","content":[{{"type":"tool_use","id":"tu1","name":"Bash","input":{{"command":"sleep 30"}}}}],"usage":{{"input_tokens":10,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":50,"cache_creation":{{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":0}}}}}}}}
{{"type":"assistant","uuid":"a2","requestId":"r1","sessionId":"s1","cwd":"/work/proj/sub","timestamp":"2026-01-01T00:00:02Z","message":{{"id":"m1","model":"claude-opus-5-5","content":[{{"type":"text","text":"x"}}],"usage":{{"input_tokens":10,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":50,"cache_creation":{{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":0}}}}}}}}
{{"type":"user","uuid":"u2","sessionId":"s1","cwd":"/work/proj/sub","timestamp":"2026-01-01T00:00:40Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"tu1","is_error":true,"content":"{SECRET_OUTPUT}"}}]}}}}
{{"type":"ai-title","aiTitle":"{SECRET_TITLE}","sessionId":"s1"}}
"#
    );
    std::fs::write(dir.join("s1.jsonl"), main).unwrap();
    std::fs::write(
        dir.join("s1/subagents/agent-a7.jsonl"),
        r#"{"type":"assistant","uuid":"b1","requestId":"r9","sessionId":"s1","isSidechain":true,"cwd":"/work/proj","timestamp":"2026-01-01T00:00:05Z","message":{"id":"m9","model":"claude-haiku-5-5","content":[],"usage":{"input_tokens":100,"output_tokens":10}}}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("s1/subagents/agent-a7.meta.json"),
        r#"{"agentType":"Explore","toolUseId":"tu1","spawnDepth":1}"#,
    )
    .unwrap();
    // A session elsewhere that must not be selected.
    let other = projects.join("-elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        other.join("s9.jsonl"),
        r#"{"type":"assistant","uuid":"z","requestId":"rz","sessionId":"s9","cwd":"/work/project-two","timestamp":"2026-01-01T00:00:00Z","message":{"id":"mz","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}
"#,
    )
    .unwrap();
    projects
}

fn token_count(ts: &str, ord: u64, total: (u64, u64), last: (u64, u64)) -> String {
    format!(
        r#"{{"timestamp":"{ts}","type":"event_msg","ordinal":{ord},"payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{},"cached_input_tokens":0,"output_tokens":{},"reasoning_output_tokens":0,"total_tokens":{}}},"last_token_usage":{{"input_tokens":{},"cached_input_tokens":0,"output_tokens":{},"reasoning_output_tokens":0,"total_tokens":{}}},"model_context_window":400000}},"rate_limits":{{"primary":{{"used_percent":12.0}},"secondary":null,"plan_type":"plus","rate_limit_reached_type":null}}}}}}"#,
        total.0,
        total.1,
        total.0 + total.1,
        last.0,
        last.1,
        last.0 + last.1
    )
}

fn codex_tree(root: &Path) -> (PathBuf, PathBuf) {
    let sessions = root.join("codex/sessions");
    let archive = root.join("codex/archived_sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&archive).unwrap();
    let r1 = sessions.join("rollout-t1.jsonl");
    std::fs::write(
        &r1,
        [
            r#"{"timestamp":"2026-01-02T00:00:00Z","type":"session_meta","ordinal":0,"payload":{"id":"t1","originator":"codex_vscode"}}"#.to_string(),
            r#"{"timestamp":"2026-01-02T00:00:00Z","type":"turn_context","ordinal":1,"payload":{"turn_id":"turn-1","model":"gpt-6.1-terra","effort":"medium"}}"#.into(),
            r#"{"timestamp":"2026-01-02T00:00:00Z","type":"event_msg","ordinal":2,"payload":{"type":"task_started","turn_id":"turn-1"}}"#.into(),
            token_count("2026-01-02T00:00:01Z", 3, (1000, 100), (1000, 100)),
            token_count("2026-01-02T00:00:02Z", 4, (3000, 300), (2000, 200)),
            // A 300k-token prompt: over gpt-6.1-terra's 272k long-context threshold.
            token_count("2026-01-02T00:00:03Z", 5, (303_000, 1300), (300_000, 1000)),
            r#"{"timestamp":"2026-01-02T00:01:00Z","type":"event_msg","ordinal":6,"payload":{"type":"task_complete","turn_id":"turn-1","duration_ms":60000}}"#.into(),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    // Subagent rollout only as .zst; its first lines copy the parent's history.
    let r2 = sessions.join("rollout-t2.jsonl");
    let body = [
        r#"{"timestamp":"2026-01-02T00:00:10Z","type":"session_meta","ordinal":0,"payload":{"id":"t2","subagent_history_start_ordinal":2}}"#.to_string(),
        token_count("2026-01-02T00:00:01Z", 1, (1000, 100), (1000, 100)),
        token_count("2026-01-02T00:00:11Z", 2, (500, 50), (500, 50)),
    ]
    .join("\n")
        + "\n";
    std::fs::write(
        sessions.join("rollout-t2.jsonl.zst"),
        zstd::encode_all(body.as_bytes(), 3).unwrap(),
    )
    .unwrap();
    // t4 lives outside the project but a T3 thread links to it.
    let r4 = sessions.join("rollout-t4.jsonl");
    std::fs::write(
        &r4,
        token_count("2026-01-03T00:00:01Z", 1, (10, 1), (10, 1)) + "\n",
    )
    .unwrap();

    let db = root.join("codex/state_5.sqlite");
    let c = Connection::open(&db).unwrap();
    c.execute_batch(
        "create table threads (id text primary key, rollout_path text, created_at integer, updated_at integer,
            created_at_ms integer, updated_at_ms integer, source text, cwd text, title text, tokens_used integer,
            archived integer, model text, reasoning_effort text, first_user_message text);",
    )
    .unwrap();
    let ins = |id: &str, path: &Path, source: &str, cwd: &str, tokens: i64| {
        c.execute(
            "insert into threads values (?1, ?2, 1767225600, 1767225700, null, null, ?3, ?4, ?5, ?6, 0, 'gpt-6.1-terra', 'medium', ?7)",
            params![id, path.to_str().unwrap(), source, cwd, SECRET_TITLE, tokens, SECRET_PROMPT],
        )
        .unwrap();
    };
    ins("t1", &r1, "vscode", "/work/proj", 304_300);
    ins(
        "t2",
        &r2,
        r#"{"subagent":{"thread_spawn":{"parent_thread_id":"t1","depth":1,"agent_role":"worker"}}}"#,
        "/elsewhere/worktree",
        550,
    );
    ins(
        "t6",
        &r4,
        r#"{"subagent":{"thread_spawn":{"parent_thread_id":"t2","depth":2}}}"#,
        "/elsewhere/worktree2",
        11,
    );
    ins(
        "t3",
        &sessions.join("rollout-gone.jsonl"),
        "vscode",
        "/work/proj",
        999,
    );
    ins("t4", &r4, "vscode", "/somewhere/else", 11);
    ins("t5", &r4, "vscode", "/work/project-two", 11);
    (db, archive)
}

fn t3_db(root: &Path) -> PathBuf {
    let db = root.join("t3/statev2.sqlite");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let c = Connection::open(&db).unwrap();
    c.execute_batch(
        "create table projection_projects (project_id text, title text, workspace_root text, deleted_at text);
         create table orchestration_v2_projection_threads (thread_id text, project_id text, title text, default_provider text,
            created_at text, updated_at text, archived_at text, deleted_at text, payload_json text);
         create table orchestration_v2_projection_runs (run_id text, thread_id text, ordinal integer, provider text, status text, payload_json text);
         create table orchestration_v2_projection_provider_threads (provider_thread_id text, thread_id text, payload_json text);
         create table orchestration_v2_projection_provider_turns (provider_turn_id text, thread_id text, payload_json text);
         insert into projection_projects values ('p1', 'Proj', '/work/proj', null);
         insert into projection_projects values ('p2', 'Other', '/work/other', null);",
    )
    .unwrap();
    let thread = |id: &str, provider: &str, payload: &str| {
        c.execute(
            "insert into orchestration_v2_projection_threads values (?1, 'p1', ?2, ?3, '2026-01-01T00:00:00Z', '2026-01-01T01:00:00Z', null, null, ?4)",
            params![id, SECRET_TITLE, provider, payload],
        )
        .unwrap();
    };
    let run = |thread: &str, n: i64, provider: &str, status: &str, start: &str, end: &str| {
        let payload = format!(
            r#"{{"modelSelection":{{"model":"m-{provider}","options":[{{"id":"effort","value":"high"}}]}},"startedAt":"{start}","completedAt":"{end}"}}"#
        );
        c.execute(
            "insert into orchestration_v2_projection_runs values (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                format!("{thread}:{n}"),
                thread,
                n,
                provider,
                status,
                payload
            ],
        )
        .unwrap();
    };
    let pthread = |thread: &str, driver: &str, native: &str| {
        let payload = format!(
            r#"{{"nativeThreadRef":{{"driver":"{driver}","nativeId":"{native}","strength":"strong"}}}}"#
        );
        c.execute(
            "insert into orchestration_v2_projection_provider_threads values (?1, ?2, ?3)",
            params![
                format!("provider-thread:provider:{driver}:native-thread:pending%3A{thread}"),
                thread,
                payload
            ],
        )
        .unwrap();
    };
    thread("T-a", "claudeAgent", "{}");
    pthread("T-a", "claudeAgent", "s1");
    run(
        "T-a",
        1,
        "claudeAgent",
        "completed",
        "2026-01-01T00:00:00Z",
        "2026-01-01T00:00:40Z",
    );
    // The thread switched provider for its second run.
    run(
        "T-a",
        2,
        "codex",
        "completed",
        "2026-01-01T00:10:00Z",
        "2026-01-01T00:10:30Z",
    );
    thread("T-b", "codex", "{}");
    run(
        "T-b",
        1,
        "codex",
        "completed",
        "2026-01-02T00:00:00Z",
        "2026-01-02T00:01:00Z",
    );
    run(
        "T-b",
        2,
        "codex",
        "interrupted",
        "2026-01-02T00:02:00Z",
        "2026-01-02T00:03:00Z",
    );
    // Linked only through a turn reference.
    c.execute(
        "insert into orchestration_v2_projection_provider_turns values ('pt1', 'T-b', ?1)",
        [r#"{"nativeTurnRef":{"driver":"codex","nativeId":"turn-1","strength":"strong"}}"#],
    )
    .unwrap();
    thread(
        "thread:provider:codex:native-thread:t2",
        "codex",
        r#"{"lineage":{"parentThreadId":"T-b","relationshipToParent":"subagent"},"creationSource":"provider"}"#,
    );
    thread("T-d", "codex", "{}");
    pthread("T-d", "codex", "t4");
    run(
        "T-d",
        1,
        "codex",
        "completed",
        "2026-01-03T00:00:00Z",
        "2026-01-03T00:00:05Z",
    );
    thread("T-e", "claudeAgent", "{}");
    run(
        "T-e",
        1,
        "claudeAgent",
        "failed",
        "2026-01-04T00:00:00Z",
        "2026-01-04T00:00:01Z",
    );
    db
}

fn read_jsonl(p: &Path) -> Vec<Value> {
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn session<'a>(rows: &'a [Value], key: &str) -> &'a Value {
    rows.iter()
        .find(|s| {
            format!(
                "{}:{}",
                s["source"].as_str().unwrap(),
                s["id"].as_str().unwrap()
            ) == key
        })
        .unwrap_or_else(|| panic!("no session {key}"))
}

#[test]
fn scan_and_report_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let claude = claude_tree(root);
    let (codex_db, archive) = codex_tree(root);
    let t3 = t3_db(root);
    let billing = root.join("decisions.jsonl");
    std::fs::write(
        &billing,
        "{\"at\":\"2025-12-31T23:59:50Z\",\"mode\":\"api\",\"reason\":\"ok\",\"cwd\":\"/work/proj\"}\n",
    )
    .unwrap();
    let out = root.join("out");
    let bin = env!("CARGO_BIN_EXE_agentprof");
    let res = Command::new(bin)
        .current_dir(root)
        .args([
            "scan",
            "--project",
            "/work/proj/",
            "--format",
            "jsonl",
            "--timezone",
            "UTC",
        ])
        .arg("--out")
        .arg(&out)
        .arg("--claude-dir")
        .arg(&claude)
        .arg("--codex-db")
        .arg(&codex_db)
        .arg("--codex-archive")
        .arg(&archive)
        .arg("--t3-db")
        .arg(&t3)
        .arg("--billing-log")
        .arg(&billing)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert_eq!(res.status.code(), Some(0), "stderr: {stderr}");
    // stdout is rf.finding/1 JSON Lines.
    for line in String::from_utf8(res.stdout).unwrap().lines() {
        let f: Value = serde_json::from_str(line).unwrap();
        assert_eq!(f["schema"], "rf.finding/1");
    }

    let rows = read_jsonl(&out.join("sessions.jsonl"));
    let keys: Vec<String> = rows
        .iter()
        .map(|s| {
            format!(
                "{}:{}",
                s["source"].as_str().unwrap(),
                s["id"].as_str().unwrap()
            )
        })
        .collect();
    assert!(rows.iter().all(|s| s["schema"] == "rf.session/1"));
    assert!(!keys.contains(&"claude:s9".to_string()));
    assert!(!keys.contains(&"codex:t5".to_string()));

    let s1 = session(&rows, "claude:s1");
    assert_eq!(s1["match_basis"], "cwd");
    assert_eq!(s1["total"]["requests"], 1);
    assert_eq!(s1["total"]["tokens"]["output"], 50);
    assert_eq!(s1["billing"]["mode"], "api");
    assert_eq!(s1["t3_thread_id"], "T-a");
    assert_eq!(s1["title"], Value::Null);
    let sub = session(&rows, "claude:s1/agent-a7");
    assert_eq!(sub["role"], "subagent");
    assert_eq!(sub["parent_id"], "s1");

    let t1 = session(&rows, "codex:t1");
    assert_eq!(t1["total"]["tokens"]["input"], 303_000);
    assert_eq!(t1["total"]["tokens"]["output"], 1300);
    // Only the 300k request is at the tier: 300k x ($2 - $1) + 1k x ($7.50 - $5) per million.
    assert_eq!(t1["total"]["long_context_requests"], 1);
    let lc = t1["total"]["long_context_usd"].as_f64().unwrap();
    assert!((lc - 0.3025).abs() < 1e-9, "{lc}");
    assert_eq!(t1["active_ms"], 60000);
    assert_eq!(t1["t3_link"], "turn-ref");
    let t2 = session(&rows, "codex:t2");
    // Linked from T3 directly, so that basis wins over the parent link.
    assert_eq!(t2["match_basis"], "t3-link");
    assert_eq!(session(&rows, "codex:t6")["match_basis"], "parent");
    assert_eq!(t2["transcript"]["status"], "zstd");
    assert_eq!(t2["transcript"]["inherited_lines"], 1);
    assert_eq!(t2["total"]["tokens"]["input"], 500);
    let t3s = session(&rows, "codex:t3");
    assert_eq!(t3s["transcript"]["status"], "missing");
    assert_eq!(t3s["total"], Value::Null);
    assert_eq!(t3s["db_tokens_used"], 999);
    let t4 = session(&rows, "codex:t4");
    assert_eq!(t4["match_basis"], "t3-link");

    let tb = session(&rows, "t3:T-b");
    assert_eq!(tb["t3"]["linked"][0], "codex:t1");
    assert_eq!(tb["active_ms"], 120000);
    assert_eq!(tb["total"], Value::Null);
    let tsub = session(&rows, "t3:thread:provider:codex:native-thread:t2");
    assert_eq!(tsub["role"], "subagent");
    assert_eq!(tsub["t3"]["link_basis"], "thread-ref");
    assert_eq!(tsub["wall_ms"], Value::Null);
    assert_eq!(tb["wall_ms"], 180000);
    let te = session(&rows, "t3:T-e");
    assert_eq!(te["t3"]["link_basis"], "none");

    let findings = read_jsonl(&out.join("findings.jsonl"));
    let has = |rule: &str, subject: &str| {
        findings
            .iter()
            .any(|f| f["rule"] == rule && f["subject"] == subject)
    };
    assert!(has("run-failed", "t3:T-e"));
    assert!(has("t3-unlinked", "t3:T-e"));
    assert!(has("run-interrupted", "t3:T-b"));
    assert!(has("transcript-missing", "codex:t3"));
    assert!(has("long-context-tier", "codex:t1"));
    assert!(!has("long-context-tier", "codex:t2"));

    let scan: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("scan.json")).unwrap()).unwrap();
    let cc = &scan["cross_checks"];
    assert_eq!(cc["t3_runs"]["equal"], true);
    assert_eq!(cc["t3_runs"]["sql"]["codex"]["completed"], 3);
    assert_eq!(cc["claude_dedup"]["naive"]["output"], 110);
    assert_eq!(cc["claude_dedup"]["dedup"]["output"], 60);
    assert_eq!(
        cc["codex_tokens_used"]["tokens_used_equals_last_counter"],
        4
    );
    assert_eq!(scan["coverage"]["t3"]["threads"], 5);
    assert_eq!(scan["coverage"]["t3"]["threads_linked"], 4);
    assert_eq!(scan["coverage"]["codex"]["rollouts"]["missing"], 1);
    assert_eq!(
        scan["coverage"]["codex"]["unselected_threads_with_project_dir_name_in_cwd"],
        0
    );

    // Report.
    let rep = Command::new(bin).arg("report").arg(&out).output().unwrap();
    assert_eq!(
        rep.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&rep.stderr)
    );
    let md = String::from_utf8(rep.stdout).unwrap();
    assert!(md.contains("API-equivalent estimate"));
    assert!(md.contains("## Findings by rule"));
    assert!(
        md.contains("| codex / gpt-6.1-terra | 6 | 1 | 16.7% | $0.30 |"),
        "{md}"
    );
    assert!(md.contains("run-failed"));

    // Privacy: no message text, tool output or titles anywhere in the outputs.
    for name in ["sessions.jsonl", "findings.jsonl", "scan.json"] {
        let text = std::fs::read_to_string(out.join(name)).unwrap();
        for secret in [SECRET_PROMPT, SECRET_OUTPUT, SECRET_TITLE, "sleep 30"] {
            assert!(!text.contains(secret), "{name} leaks {secret}");
        }
    }
    assert!(!md.contains(SECRET_OUTPUT) && !md.contains(SECRET_TITLE));
}

#[test]
fn titles_only_with_flag_and_usage_errors_exit_2() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let claude = claude_tree(root);
    let out = root.join("out");
    let bin = env!("CARGO_BIN_EXE_agentprof");
    let res = Command::new(bin)
        .current_dir(root)
        .args([
            "scan",
            "--project",
            "/work/proj",
            "--include-titles",
            "--timezone",
            "UTC",
        ])
        .arg("--out")
        .arg(&out)
        .arg("--claude-dir")
        .arg(&claude)
        .args([
            "--codex-db",
            "/nonexistent/a.sqlite",
            "--t3-db",
            "/nonexistent/b.sqlite",
        ])
        .args(["--billing-log", "/nonexistent/c.jsonl"])
        .output()
        .unwrap();
    assert_eq!(
        res.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&res.stderr)
    );
    let rows = read_jsonl(&out.join("sessions.jsonl"));
    assert_eq!(session(&rows, "claude:s1")["title"], SECRET_TITLE);
    assert_eq!(session(&rows, "claude:s1")["billing"]["mode"], "unknown");

    let bad = Command::new(bin)
        .current_dir(root)
        .args(["scan", "--bogus"])
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
    let missing = Command::new(bin)
        .current_dir(root)
        .args(["report", "/nonexistent/dir"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2));
}

fn scan(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentprof"))
        .current_dir(root)
        .arg("scan")
        .args(args)
        .args(["--timezone", "UTC", "--billing-log", "/nonexistent/c.jsonl"])
        .output()
        .unwrap()
}

#[test]
fn report_rejects_other_schema_majors_and_survives_a_closed_pipe() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let claude = claude_tree(root);
    let out = root.join("out");
    let res = scan(
        root,
        &[
            "--project",
            "/work/proj",
            "--out",
            out.to_str().unwrap(),
            "--claude-dir",
            claude.to_str().unwrap(),
            "--codex-db",
            "/nonexistent/a.sqlite",
            "--t3-db",
            "/nonexistent/b.sqlite",
        ],
    );
    assert_eq!(res.status.code(), Some(0));
    let bin = env!("CARGO_BIN_EXE_agentprof");

    // stdout whose reader is already gone.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let st = Command::new(bin)
        .arg("report")
        .arg(&out)
        .stdout(writer)
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(st.status.code(), Some(0));
    assert!(
        st.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&st.stderr)
    );

    // A later minor version is read; another major is refused.
    let path = out.join("sessions.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replacen("rf.session/1", "rf.session/1.1", 1)).unwrap();
    let ok = Command::new(bin).arg("report").arg(&out).output().unwrap();
    assert_eq!(ok.status.code(), Some(0));
    std::fs::write(&path, text.replacen("rf.session/1", "rf.session/2", 1)).unwrap();
    let bad = Command::new(bin).arg("report").arg(&out).output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
    let err = String::from_utf8_lossy(&bad.stderr);
    assert!(
        err.contains("sessions.jsonl:1") && err.contains("rf.session/2"),
        "{err}"
    );
}
