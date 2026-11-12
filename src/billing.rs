//! The API-routing log (`decisions.jsonl`): one line per Claude Code launch
//! saying whether it ran on the subscription or the API.
//!
//! The log also holds lines that are not launch decisions: `refused` (an
//! earlier run was refused, written after that run) and `cost` (a run's
//! cost). They describe a run that already started, so they never set a
//! session's billing mode; `Log` counts them as `other_lines`.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::util::{parse_ts, within};

#[derive(Deserialize)]
struct Raw {
    at: Option<String>,
    mode: Option<String>,
    cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub at: i64,
    pub mode: String,
    pub cwd: String,
}

#[derive(Debug, Default)]
pub struct Log {
    pub decisions: Vec<Decision>,
    pub bad_lines: u64,
    /// Valid lines that are not launch decisions (`refused`, `cost`).
    pub other_lines: u64,
    pub first: Option<i64>,
    pub last: Option<i64>,
}

/// A decision applies to a session that starts in the same directory within
/// this many milliseconds after it.
pub const WINDOW_MS: i64 = 120_000;

/// Modes the router writes when it launches a run.
const LAUNCH_MODES: [&str; 3] = ["api", "subscription", "passthrough"];

/// Modes the router writes for something other than a launch.
const OTHER_MODES: [&str; 2] = ["refused", "cost"];

impl Log {
    pub fn load(path: &Path) -> Result<Log> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Self::parse(&text))
    }

    pub fn parse(text: &str) -> Log {
        let mut log = Log::default();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(r) = serde_json::from_str::<Raw>(line) else {
                log.bad_lines += 1;
                continue;
            };
            match r.mode.as_deref() {
                Some(m) if LAUNCH_MODES.contains(&m) => {}
                Some(m) if OTHER_MODES.contains(&m) => {
                    log.other_lines += 1;
                    continue;
                }
                _ => {
                    log.bad_lines += 1;
                    continue;
                }
            }
            let (Some(at), Some(mode), Some(cwd)) =
                (r.at.as_deref().and_then(parse_ts), r.mode, r.cwd)
            else {
                log.bad_lines += 1;
                continue;
            };
            log.first = Some(log.first.map_or(at, |f| f.min(at)));
            log.last = Some(log.last.map_or(at, |l| l.max(at)));
            log.decisions.push(Decision { at, mode, cwd });
        }
        log.decisions.sort_by_key(|d| d.at);
        log
    }

    /// Decisions logged under the project.
    pub fn in_project(&self, root: &str) -> usize {
        self.decisions
            .iter()
            .filter(|d| within(&d.cwd, root))
            .count()
    }

    /// The latest decision for `cwd` made at most `WINDOW_MS` before `start`.
    pub fn lookup(&self, cwd: &str, start: i64) -> Option<&Decision> {
        self.decisions
            .iter()
            .rev()
            .filter(|d| d.at <= start && start - d.at <= WINDOW_MS)
            .find(|d| d.cwd == cwd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_needs_same_cwd_and_window() {
        let log = Log::parse(
            r#"{"at":"2026-01-01T00:00:00Z","mode":"api","reason":"x","cwd":"/p"}
{"at":"2026-01-01T00:10:00Z","mode":"subscription","reason":"x","cwd":"/p"}
not json
"#,
        );
        assert_eq!(log.bad_lines, 1);
        let t0 = parse_ts("2026-01-01T00:00:30Z").unwrap();
        assert_eq!(log.lookup("/p", t0).unwrap().mode, "api");
        assert!(log.lookup("/q", t0).is_none());
        let late = parse_ts("2026-01-01T00:05:00Z").unwrap();
        assert!(log.lookup("/p", late).is_none());
    }

    #[test]
    fn refused_and_cost_lines_are_not_decisions() {
        // The router writes `refused` after a refused run and `cost` at a
        // run's end; neither may label a session that starts soon after.
        let log = Log::parse(
            r#"{"at":"2026-01-01T00:00:00Z","mode":"api","reason":"ok","cwd":"/p"}
{"at":"2026-01-01T00:00:20Z","mode":"refused","reason":"exhausted","cwd":"/p"}
{"at":"2026-01-01T00:00:21Z","mode":"cost","session":"s","costUSD":0.01,"source":"result"}
{"at":"2026-01-01T00:00:22Z","mode":"other","reason":"x","cwd":"/p"}
"#,
        );
        assert_eq!(log.decisions.len(), 1);
        assert_eq!(log.other_lines, 2);
        assert_eq!(log.bad_lines, 1);
        let t = parse_ts("2026-01-01T00:00:30Z").unwrap();
        assert_eq!(log.lookup("/p", t).unwrap().mode, "api");
    }
}
