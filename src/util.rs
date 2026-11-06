//! Small helpers shared by the parsers: timestamps, paths, hosts and hashing.

use std::path::Path;

use jiff::Timestamp;
use jiff::tz::TimeZone;

/// Parses an RFC 3339 timestamp into Unix milliseconds.
pub fn parse_ts(s: &str) -> Option<i64> {
    s.parse::<Timestamp>().ok().map(|t| t.as_millisecond())
}

/// Formats Unix milliseconds as an RFC 3339 UTC timestamp.
pub fn fmt_ts(ms: i64) -> String {
    Timestamp::from_millisecond(ms)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| format!("invalid-ms:{ms}"))
}

/// Calendar day of `ms` in `tz`, as `YYYY-MM-DD`.
pub fn day(ms: i64, tz: &TimeZone) -> String {
    match Timestamp::from_millisecond(ms) {
        Ok(t) => t.to_zoned(tz.clone()).date().to_string(),
        Err(_) => "unknown".to_string(),
    }
}

/// True when `path` is `root` or lies below it. Both are plain absolute paths
/// without a trailing slash.
pub fn within(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Normalises a project path: strips trailing slashes.
pub fn normalise_root(p: &str) -> String {
    let t = p.trim_end_matches('/');
    if t.is_empty() {
        "/".to_string()
    } else {
        t.to_string()
    }
}

/// Replaces the home directory prefix with `~` so outputs do not carry it.
pub fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    tilde(&s)
}

pub fn tilde(s: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        let home = home.trim_end_matches('/');
        if !home.is_empty() && within(s, home) {
            return format!("~{}", &s[home.len()..]);
        }
    }
    s.to_string()
}

/// Host part of a URL, lowercased, without user info or port.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let host = if authority.starts_with('[') {
        authority.split(']').next().map(|h| format!("{h}]"))?
    } else {
        authority.split(':').next()?.to_string()
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// FNV-1a 64-bit hash as 16 hex digits; used to name commands without
/// printing them.
pub fn fnv64(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// First word of a shell command, as a basename; safe to print.
pub fn program_name(cmd: &str) -> String {
    let mut words = cmd.split_whitespace();
    let mut w = words.next().unwrap_or("");
    // Skip leading environment assignments such as `FOO=1 cmd`.
    while w.contains('=') && !w.starts_with('=') {
        match words.next() {
            Some(n) => w = n,
            None => break,
        }
    }
    let w = w.trim_matches(|c| c == '"' || c == '\'' || c == '(');
    w.rsplit('/').next().unwrap_or(w).to_string()
}

/// True when a shell command sleeps or waits for something to change.
pub fn is_sleep_command(cmd: &str) -> bool {
    cmd.split(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|' || c == '(')
        .any(|w| w == "sleep")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_matches_only_path_boundaries() {
        assert!(within("/a/b", "/a/b"));
        assert!(within("/a/b/c", "/a/b"));
        assert!(!within("/a/bc", "/a/b"));
        assert!(!within("/a", "/a/b"));
    }

    #[test]
    fn hosts() {
        assert_eq!(
            host_of("https://Arxiv.org/abs/1").as_deref(),
            Some("arxiv.org")
        );
        assert_eq!(
            host_of("http://u:p@h.example:8080/x").as_deref(),
            Some("h.example")
        );
        assert_eq!(host_of("not a url"), None);
    }

    #[test]
    fn programs_and_sleep() {
        assert_eq!(program_name("FOO=1 /usr/bin/git status"), "git");
        assert!(is_sleep_command("sleep 30 && ls"));
        assert!(is_sleep_command("while true; do sleep 5; done"));
        assert!(!is_sleep_command("echo sleepy"));
    }

    #[test]
    fn timestamps_round_trip() {
        let ms = parse_ts("2026-10-31T12:52:36.083Z").unwrap();
        assert_eq!(fmt_ts(ms), "2026-10-31T12:52:36.083Z");
        assert_eq!(day(ms, &TimeZone::UTC), "2026-10-31");
    }
}
