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

/// Privacy rule for every path the tool writes: the current home directory,
/// and any `/Users/<name>` or `/home/<name>` prefix (another account's home,
/// or the same home seen from another machine), becomes `~`.
pub fn tilde(s: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        let home = home.trim_end_matches('/');
        if !home.is_empty() && within(s, home) {
            return format!("~{}", &s[home.len()..]);
        }
    }
    for base in ["/Users/", "/home/"] {
        if let Some(rest) = s.strip_prefix(base) {
            let (name, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            if !name.is_empty() {
                return format!("~{tail}");
            }
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

/// A shell word with quoting removed, and whether it is a `NAME=value`
/// assignment (an unquoted valid name before an unquoted `=`).
struct Word {
    text: String,
    assignment: bool,
}

/// Words of the first simple command in `cmd`, with shell quoting and
/// backslash escapes removed. Stops at an unquoted `;`, `|`, `&` or newline.
fn first_command_words(cmd: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let mut cur = String::new();
    // Still a candidate assignment name: no quoting or escape seen yet.
    let mut name_plain = true;
    let mut assignment = false;
    let mut in_word = false;
    let mut chars = cmd.chars();
    let (mut single, mut double) = (false, false);
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => {
                single = !single;
                in_word = true;
                name_plain &= assignment;
            }
            '"' if !single => {
                double = !double;
                in_word = true;
                name_plain &= assignment;
            }
            '\\' if !single => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                in_word = true;
                name_plain &= assignment;
            }
            c if single || double => cur.push(c),
            ';' | '|' | '&' | '\n' => break,
            c if c.is_whitespace() => {
                if in_word {
                    words.push(Word {
                        text: std::mem::take(&mut cur),
                        assignment,
                    });
                    in_word = false;
                    name_plain = true;
                    assignment = false;
                }
            }
            c => {
                if c == '=' && !assignment && name_plain && is_name(&cur) {
                    assignment = true;
                }
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(Word {
            text: cur,
            assignment,
        });
    }
    words
}

/// A valid shell variable name.
fn is_name(s: &str) -> bool {
    let mut ch = s.chars();
    ch.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && ch.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The program a shell command runs, as a basename; safe to print. Leading
/// `NAME=value` assignments and `env` with its options are skipped. Anything
/// that is not a plain program name (it contains `=`, a quote or other
/// unusual characters, which may be a secret) becomes `unknown`.
pub fn program_name(cmd: &str) -> String {
    let words = first_command_words(cmd);
    let mut it = words.iter().peekable();
    while it.peek().is_some_and(|w| w.assignment) {
        it.next();
    }
    if it.peek().is_some_and(|w| w.text == "env") {
        it.next();
        while let Some(w) = it.peek() {
            if w.text == "-u" || w.text == "--unset" {
                it.next();
                it.next();
            } else if w.text.starts_with('-') || w.assignment {
                it.next();
            } else {
                break;
            }
        }
    }
    let w = it
        .next()
        .map_or("", |w| w.text.as_str())
        .trim_start_matches('(');
    let name = w.rsplit('/').next().unwrap_or(w);
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
    if plain {
        name.to_string()
    } else {
        "unknown".to_string()
    }
}

/// The words of `cmd` with quotes removed, when it is one simple command
/// whose words are literal: `None` for a pipe, list (`&&`, `||`, `;`, `&`),
/// redirect, subshell, expansion (`$`, backquote, glob, `~`), comment,
/// backslash, newline or unclosed quote.
pub fn simple_command_words(cmd: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let (mut single, mut double) = (false, false);
    for c in cmd.chars() {
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            _ if single => cur.push(c),
            '$' | '`' | '\\' | '\n' => return None,
            _ if double => cur.push(c),
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '{' | '}' | '*' | '?' | '[' | '#' | '~' => {
                return None;
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
                continue;
            }
            c => cur.push(c),
        }
        in_word = true;
    }
    if single || double {
        return None;
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

/// A command that reads files, as its file-reread range key and its file
/// operands: `whole` for `cat FILE…` (no options) and `nl [-ba] FILE…`,
/// `lines a-b` for `sed -n 'a,bp' FILE…` (or `'ap'`). `None` for any other
/// command. Quotes around the `sed` script are ignored.
pub fn read_command<'a>(words: &[&'a str]) -> Option<(String, Vec<&'a str>)> {
    let (&prog, args) = words.split_first()?;
    match prog {
        "cat" if args.iter().all(|w| !w.starts_with('-')) => Some(("whole".into(), args.to_vec())),
        "nl" if args.iter().all(|w| !w.starts_with('-') || *w == "-ba") => Some((
            "whole".into(),
            args.iter().copied().filter(|w| *w != "-ba").collect(),
        )),
        "sed" if args.first() == Some(&"-n") => {
            let script = args.get(1)?.trim_matches(|c| c == '\'' || c == '"');
            let body = script.strip_suffix('p')?;
            let (a, b) = body.split_once(',').unwrap_or((body, body));
            let number = |s: &str| {
                s.bytes()
                    .all(|c| c.is_ascii_digit())
                    .then(|| s.parse::<u64>().ok())
                    .flatten()
            };
            let (a, b) = (number(a)?, number(b)?);
            Some((format!("lines {a}-{b}"), args[2..].to_vec()))
        }
        _ => None,
    }
}

/// `path` resolved against the directory `cwd` (both plain strings), with
/// `.` and `..` removed lexically. `None` for a relative path without an
/// absolute `cwd`.
pub fn resolve_path(cwd: Option<&str>, path: &str) -> Option<String> {
    use std::path::{Component, PathBuf};
    let joined = match Path::new(path).is_absolute() {
        true => PathBuf::from(path),
        false => Path::new(cwd.filter(|c| c.starts_with('/'))?).join(path),
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Some(out.to_string_lossy().into_owned())
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
    fn home_prefixes_of_any_account_become_tilde() {
        assert_eq!(tilde("/Users/someone/p/q.jsonl"), "~/p/q.jsonl");
        assert_eq!(tilde("/home/someone"), "~");
        assert_eq!(tilde("/home/someone/x"), "~/x");
        assert_eq!(tilde("/Users/"), "/Users/");
        assert_eq!(tilde("/opt/Users/x"), "/opt/Users/x");
        assert_eq!(tilde("/work/p"), "/work/p");
    }

    #[test]
    fn program_names_never_carry_assignments() {
        let fake = "sk-FAKESECRET2";
        let cases = [
            (format!("OPENAI_API_KEY={fake} codex exec x"), "codex"),
            (
                format!("OPENAI_API_KEY='{fake} b' A=1 /usr/bin/bun run"),
                "bun",
            ),
            (format!("env -u HOME OPENAI_API_KEY={fake} -- ls"), "ls"),
            (format!("env -i OPENAI_API_KEY={fake} ls -la"), "ls"),
            (format!("OPENAI_API_KEY={fake}"), "unknown"),
            (format!("\"OPENAI_API_KEY={fake}\" ls"), "unknown"),
            (format!("OPENAI_API_KEY=\"{fake} \\\" x\" git log"), "git"),
            (format!("(cd /x && OPENAI_API_KEY={fake} make)"), "cd"),
            (format!("X={fake}; curl x"), "unknown"),
        ];
        for (cmd, want) in &cases {
            let got = program_name(cmd);
            assert_eq!(got, *want, "{cmd}");
            assert!(!got.contains("FAKESECRET") && !got.contains('='));
        }
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
