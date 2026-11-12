//! Archive lookups per session: search-tool calls, greps over the archive
//! and reads of archive files. The README states what counts.

use std::collections::HashMap;

use crate::session::Archive;
use crate::util::{resolve_path, within};

/// The configured archive roots of a scan.
#[derive(Default, Debug, Clone)]
pub struct Config {
    /// The project root, absolute, without a trailing slash.
    pub project: String,
    /// Roots as configured: absolute, or relative to a checkout of the project.
    pub roots: Vec<String>,
}

impl Config {
    /// The absolute roots for a session that started in `base`: each
    /// relative root under the project root and, for a session that started
    /// outside it (a git worktree of the project), under `base` and each
    /// directory above it as well, since the transcript does not say which
    /// of them is the checkout's root.
    fn absolute(&self, base: Option<&str>) -> Vec<String> {
        let mut dirs = vec![self.project.as_str()];
        if let Some(b) = base.filter(|b| b.starts_with('/') && !within(b, &self.project)) {
            let mut dir = b.trim_end_matches('/');
            while !dir.is_empty() {
                dirs.push(dir);
                dir = dir.rsplit_once('/').map_or("", |(up, _)| up);
            }
        }
        let mut out = Vec::new();
        for r in &self.roots {
            for dir in &dirs {
                if let Some(p) = resolve_path(Some(dir), r)
                    && !out.contains(&p)
                {
                    out.push(p);
                }
            }
        }
        out
    }
}

/// What a `lab` call or an MCP tool does with the archive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lookup {
    /// `lab search`, `lab archive find`, `archive_search`: exits 1 on no hit.
    Query,
    Open,
    Related,
}

/// Counts one session's archive lookups.
pub struct Counter<'a> {
    cfg: &'a Config,
    /// Absolute roots, set from the first working directory seen.
    roots: Option<Vec<String>>,
    stat: Archive,
    greps: u64,
    reads: u64,
}

impl<'a> Counter<'a> {
    pub fn new(cfg: &'a Config) -> Self {
        Counter {
            cfg,
            roots: None,
            stat: Archive::default(),
            greps: 0,
            reads: 0,
        }
    }

    /// Records the session's working directory; the first one is where the
    /// session started.
    pub fn see_cwd(&mut self, cwd: &str) {
        if self.roots.is_none() {
            self.roots = Some(self.cfg.absolute(Some(cwd)));
        }
    }

    fn in_archive(&mut self, path: &str) -> bool {
        let cfg = self.cfg;
        self.roots
            .get_or_insert_with(|| cfg.absolute(None))
            .iter()
            .any(|r| within(path, r))
    }

    fn lookup(&mut self, l: Lookup) {
        self.stat.searches += 1;
        match l {
            Lookup::Query => {}
            Lookup::Open => self.stat.opens += 1,
            Lookup::Related => self.stat.related += 1,
        }
    }

    /// A tool call by name: counts the MCP tools `archive_search`,
    /// `archive_open` and `archive_related` under any server prefix.
    pub fn tool(&mut self, name: &str) {
        let tail = name.rsplit("__").next().unwrap_or(name);
        let tail = tail.rsplit('.').next().unwrap_or(tail);
        match tail {
            "archive_search" => self.lookup(Lookup::Query),
            "archive_open" => self.lookup(Lookup::Open),
            "archive_related" => self.lookup(Lookup::Related),
            _ => {}
        }
    }

    /// A file read; `path` is absolute.
    pub fn read(&mut self, path: &str) {
        if self.in_archive(path) {
            self.reads += 1;
        }
    }

    /// A Claude `Grep` call: `path` is its `path` input, which defaults to
    /// the working directory.
    pub fn grep_tool(&mut self, path: Option<&str>, cwd: Option<&str>) {
        if resolve_path(cwd, path.unwrap_or(".")).is_some_and(|p| self.in_archive(&p)) {
            self.greps += 1;
        }
    }

    /// Counts the lookups in a shell command run in `cwd`. True when the
    /// command is one query (`lab search`, `lab archive find`) after nothing
    /// but `cd` and assignments, so its exit status 1 means "no hit".
    pub fn shell(&mut self, cmd: &str, cwd: Option<&str>) -> bool {
        let mut sh = Shell {
            cwd: cwd.map(str::to_string),
            vars: HashMap::new(),
            last_is_query: false,
            others: false,
            subshells: Vec::new(),
            counter: self,
        };
        sh.run(cmd);
        sh.last_is_query && !sh.others
    }

    /// A query that exited 1: it found nothing.
    pub fn no_hit(&mut self) {
        self.stat.no_hit_searches += 1;
    }

    pub fn finish(mut self) -> Archive {
        if !self.cfg.roots.is_empty() {
            self.stat.greps = Some(self.greps);
            self.stat.reads = Some(self.reads);
            let lookups = self.stat.searches + self.greps;
            if lookups > 0 {
                self.stat.search_share = Some(self.stat.searches as f64 / lookups as f64);
                self.stat.grep_share = Some(self.greps as f64 / lookups as f64);
            }
        }
        self.stat
    }
}

/// A best-effort reader of shell commands: it splits a command line into
/// simple commands, follows `cd` and `NAME=value`, and skips here-documents.
/// It does not evaluate substitutions, loops or globs.
struct Shell<'c, 'a> {
    cwd: Option<String>,
    vars: HashMap<String, String>,
    last_is_query: bool,
    /// A command other than `cd` ran before the last one.
    others: bool,
    /// The working directories outside the open subshells.
    subshells: Vec<Option<String>>,
    counter: &'c mut Counter<'a>,
}

/// The words of the simple command being read.
#[derive(Default)]
struct Lexer {
    words: Vec<String>,
    cur: String,
    in_word: bool,
    /// The word being read is a redirect target and is dropped.
    redirect: bool,
}

impl Lexer {
    fn end_word(&mut self) {
        if self.in_word {
            let w = std::mem::take(&mut self.cur);
            if !self.redirect {
                self.words.push(w);
            }
            self.redirect = false;
            self.in_word = false;
        }
    }

    fn end_command(&mut self) -> Vec<String> {
        self.end_word();
        self.redirect = false;
        std::mem::take(&mut self.words)
    }
}

fn is_name(s: &str) -> bool {
    let mut ch = s.chars();
    ch.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && ch.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Shell<'_, '_> {
    fn run(&mut self, cmd: &str) {
        let c: Vec<char> = cmd.chars().collect();
        let mut lex = Lexer::default();
        let (mut single, mut double) = (false, false);
        let mut piped = false;
        // Here-document delimiters whose bodies start after the next newline.
        let mut heredocs: Vec<String> = Vec::new();
        let mut i = 0;
        while i < c.len() {
            let ch = c[i];
            i += 1;
            if single {
                if ch == '\'' {
                    single = false;
                } else {
                    lex.cur.push(ch);
                }
                continue;
            }
            match ch {
                '\'' if !double => {
                    single = true;
                    lex.in_word = true;
                }
                '"' => {
                    double = !double;
                    lex.in_word = true;
                }
                '\\' => {
                    if let Some(&n) = c.get(i) {
                        i += 1;
                        if n != '\n' {
                            lex.cur.push(n);
                            lex.in_word = true;
                        }
                    }
                }
                '$' => {
                    lex.in_word = true;
                    let braced = c.get(i) == Some(&'{');
                    let start = i + usize::from(braced);
                    let mut end = start;
                    while c
                        .get(end)
                        .is_some_and(|x| x.is_ascii_alphanumeric() || *x == '_')
                    {
                        end += 1;
                    }
                    let name: String = c[start..end].iter().collect();
                    let closed = !braced || c.get(end) == Some(&'}');
                    match self.vars.get(&name).filter(|_| closed && is_name(&name)) {
                        Some(v) => {
                            lex.cur.push_str(v);
                            i = end + usize::from(braced);
                        }
                        // Unknown: keep the `$`, so the word resolves nowhere.
                        None => lex.cur.push('$'),
                    }
                }
                _ if double => lex.cur.push(ch),
                '#' if !lex.in_word => {
                    while c.get(i).is_some_and(|x| *x != '\n') {
                        i += 1;
                    }
                }
                '\n' => {
                    self.simple(lex.end_command(), piped);
                    piped = false;
                    for delim in heredocs.drain(..) {
                        loop {
                            let start = i;
                            while c.get(i).is_some_and(|x| *x != '\n') {
                                i += 1;
                            }
                            let line: String = c[start..i].iter().collect();
                            let at_end = i >= c.len();
                            i += 1;
                            if at_end || line.trim_start_matches('\t') == delim {
                                break;
                            }
                        }
                    }
                }
                ';' | '(' | ')' => {
                    self.simple(lex.end_command(), piped);
                    piped = false;
                    // A subshell's `cd` ends with it.
                    if ch == '(' {
                        self.subshells.push(self.cwd.clone());
                    } else if ch == ')'
                        && let Some(cwd) = self.subshells.pop()
                    {
                        self.cwd = cwd;
                    }
                }
                '|' => {
                    let or = c.get(i) == Some(&'|');
                    i += usize::from(or);
                    // `|&` pipes stderr as well.
                    if !or && c.get(i) == Some(&'&') {
                        i += 1;
                    }
                    self.simple(lex.end_command(), piped);
                    piped = !or;
                }
                '&' => {
                    if c.get(i) == Some(&'>') {
                        // `&>file`
                        continue;
                    }
                    i += usize::from(c.get(i) == Some(&'&'));
                    self.simple(lex.end_command(), piped);
                    piped = false;
                }
                '<' if c.get(i) == Some(&'<') && c.get(i + 1) != Some(&'<') => {
                    // Here-document: read the delimiter word.
                    lex.cur.clear();
                    lex.in_word = false;
                    i += 1;
                    i += usize::from(c.get(i) == Some(&'-'));
                    while c.get(i).is_some_and(|x| *x == ' ' || *x == '\t') {
                        i += 1;
                    }
                    let mut delim = String::new();
                    while let Some(&x) = c.get(i) {
                        if x.is_whitespace() || matches!(x, ';' | '|' | '&' | '<' | '>' | ')') {
                            break;
                        }
                        if !matches!(x, '\'' | '"' | '\\') {
                            delim.push(x);
                        }
                        i += 1;
                    }
                    heredocs.push(delim);
                }
                '<' | '>' => {
                    // A redirect: drop a file-descriptor prefix and the target.
                    if lex.cur.bytes().all(|b| b.is_ascii_digit()) {
                        lex.cur.clear();
                        lex.in_word = false;
                    } else {
                        lex.end_word();
                    }
                    lex.redirect = true;
                    while c.get(i).is_some_and(|x| matches!(x, '<' | '>' | '&' | '|')) {
                        i += 1;
                    }
                }
                ch if ch.is_whitespace() => {
                    lex.end_word();
                }
                ch => {
                    lex.cur.push(ch);
                    lex.in_word = true;
                }
            }
        }
        lex.end_word();
        self.simple(lex.end_command(), piped);
    }

    /// One simple command with its words expanded.
    fn simple(&mut self, words: Vec<String>, piped: bool) {
        let assignment = |w: &String| w.split_once('=').is_some_and(|(n, _)| is_name(n));
        let mut rest = words.as_slice();
        while let Some((w, tail)) = rest.split_first() {
            let skip = assignment(w)
                || matches!(
                    w.as_str(),
                    "do" | "then"
                        | "else"
                        | "elif"
                        | "if"
                        | "while"
                        | "until"
                        | "!"
                        | "{"
                        | "time"
                        | "noglob"
                        | "command"
                        | "exec"
                        | "env"
                );
            if !skip {
                break;
            }
            rest = tail;
        }
        let Some((prog, args)) = rest.split_first() else {
            // Only assignments: they set shell variables.
            for w in &words {
                if let Some((n, v)) = w.split_once('=').filter(|(n, _)| is_name(n)) {
                    self.vars.insert(n.to_string(), v.to_string());
                }
            }
            return;
        };
        self.others |= self.last_is_query;
        self.last_is_query = false;
        let prog = prog.rsplit('/').next().unwrap_or(prog);
        let before = std::mem::replace(&mut self.others, true);
        match prog {
            "cd" => {
                self.others = before;
                self.cwd = match args {
                    [dir] if !dir.starts_with(['-', '~']) && !dir.contains('$') => {
                        resolve_path(self.cwd.as_deref(), dir)
                    }
                    _ => None,
                };
            }
            "lab" => {
                if args.iter().any(|a| a == "--help" || a == "-h") {
                    return;
                }
                // `--config FILE` is lab's one option with a separate value.
                let mut config = false;
                let mut sub = args.iter().filter(|a| {
                    let value = std::mem::replace(&mut config, a.as_str() == "--config");
                    !value && !a.starts_with('-')
                });
                let lookup = match sub.next().map(String::as_str) {
                    Some("search") => Some(Lookup::Query),
                    Some("open") => Some(Lookup::Open),
                    Some("related") => Some(Lookup::Related),
                    Some("archive") if sub.next().is_some_and(|a| a == "find") => {
                        Some(Lookup::Query)
                    }
                    _ => None,
                };
                if let Some(l) = lookup {
                    self.counter.lookup(l);
                    self.last_is_query = l == Lookup::Query;
                    self.others = before;
                }
            }
            "grep" | "egrep" | "fgrep" => self.grep(args, "dD", Some('r'), piped),
            "rg" => self.grep(args, "gtTrjME", None, piped),
            "ag" => self.grep(args, "G", None, piped),
            "git" => {
                // Skip git's own options up to the subcommand.
                let mut cwd = self.cwd.clone();
                let mut it = args.iter();
                let mut sub = None;
                while let Some(a) = it.next() {
                    match a.as_str() {
                        "-C" => {
                            cwd = it
                                .next()
                                .filter(|d| !d.contains('$'))
                                .and_then(|d| resolve_path(cwd.as_deref(), d));
                        }
                        "-c" | "--git-dir" | "--work-tree" => {
                            it.next();
                        }
                        a if a.starts_with('-') => {}
                        a => {
                            sub = Some(a);
                            break;
                        }
                    }
                }
                if sub == Some("grep") {
                    let saved = std::mem::replace(&mut self.cwd, cwd);
                    self.grep(it.as_slice(), "O", None, piped);
                    self.cwd = saved;
                }
            }
            _ => {}
        }
    }

    /// A grep-like command. `value_opts`: the program's short options that
    /// take a value, besides `-A -B -C -m` and the pattern options `-e -f`.
    /// `recursive_flag`: the short flag that makes a search without a path
    /// operand cover the working directory; `None` when that is the default.
    fn grep(
        &mut self,
        args: &[String],
        value_opts: &str,
        recursive_flag: Option<char>,
        piped: bool,
    ) {
        const LONG_VALUE: &[&str] = &[
            "--include",
            "--exclude",
            "--exclude-dir",
            "--max-count",
            "--after-context",
            "--before-context",
            "--context",
            "--glob",
            "--iglob",
            "--type",
            "--type-not",
            "--max-depth",
            "--depth",
            "--replace",
            "--threads",
            "--max-columns",
            "--sort",
            "--file-search-regex",
            "--ignore",
        ];
        // Invocations that search nothing.
        if args.iter().any(|a| {
            matches!(
                a.as_str(),
                "--help" | "--version" | "--files" | "--type-list"
            )
        }) {
            return;
        }
        let mut pattern_given = false;
        let mut recursive = recursive_flag.is_none();
        let mut operands: Vec<&str> = Vec::new();
        let mut options = true;
        let mut it = args.iter();
        while let Some(w) = it.next() {
            if !options || !w.starts_with('-') || w.len() == 1 {
                operands.push(w);
            } else if w == "--" {
                options = false;
            } else if w.starts_with("--") {
                let (name, attached) = match w.split_once('=') {
                    Some((n, _)) => (n, true),
                    None => (w.as_str(), false),
                };
                let pattern = matches!(name, "--regexp" | "--file");
                pattern_given |= pattern;
                recursive |= matches!(name, "--recursive" | "--dereference-recursive");
                if !attached && (pattern || LONG_VALUE.contains(&name)) {
                    it.next();
                }
            } else {
                // A cluster of short options; the first one that takes a
                // value ends it, with the value attached or in the next word.
                let cluster = &w[1..];
                for (at, o) in cluster.char_indices() {
                    if Some(o) == recursive_flag || (recursive_flag.is_some() && o == 'R') {
                        recursive = true;
                    }
                    let pattern = matches!(o, 'e' | 'f');
                    if pattern || "ABCm".contains(o) || value_opts.contains(o) {
                        pattern_given |= pattern;
                        // A digit after -A, -B, -C or -m is its value.
                        if at + o.len_utf8() == cluster.len() {
                            it.next();
                        }
                        break;
                    }
                }
            }
        }
        if !pattern_given && !operands.is_empty() {
            operands.remove(0);
        }
        let hit = if operands.is_empty() {
            // Without a path a recursive search covers the working
            // directory; behind a pipe the command reads its input.
            recursive
                && !piped
                && self
                    .cwd
                    .clone()
                    .is_some_and(|d| self.counter.in_archive(&d))
        } else {
            operands.iter().any(|p| {
                // An unknown `$NAME` stays in the word, so a path that
                // starts with one resolves outside every root. `~` is the
                // home directory, which is not expanded.
                !p.starts_with('~')
                    && resolve_path(self.cwd.as_deref(), p)
                        .is_some_and(|p| self.counter.in_archive(&p))
            })
        };
        if hit {
            self.counter.greps += 1;
        }
    }
}

/// The roots the unit tests of the parsers use.
#[cfg(test)]
pub static TEST: std::sync::LazyLock<Config> = std::sync::LazyLock::new(|| Config {
    project: "/work/proj".into(),
    roots: vec!["docs/research".into()],
});

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        TEST.clone()
    }

    /// (searches, opens, related, greps) after the commands run in /work/proj.
    fn count(cmds: &[&str]) -> (u64, u64, u64, u64) {
        let c = cfg();
        let mut k = Counter::new(&c);
        for cmd in cmds {
            k.shell(cmd, Some("/work/proj"));
        }
        let a = k.finish();
        (a.searches, a.opens, a.related, a.greps.unwrap())
    }

    fn greps(cmd: &str) -> u64 {
        count(&[cmd]).3
    }

    #[test]
    fn lab_calls_count_by_subcommand() {
        assert_eq!(
            count(&[
                "lab search 'reranker gain' \"second phrasing\" | head -20",
                "cd /work/proj && lab archive find H164 2>&1 | head",
                "lab open DR041#summary; lab related DR041",
                "/usr/local/bin/lab search x",
                "lab --config /x/research.toml search x",
                // Not lookups.
                "lab archive find --help",
                "lab archive update && lab check",
                "echo lab search x",
            ]),
            (6, 1, 1, 0)
        );
    }

    #[test]
    fn greps_count_only_under_a_root() {
        // Path operands.
        assert_eq!(greps("grep -rn 'foo bar' docs/research/followups"), 1);
        assert_eq!(greps("grep -n foo /work/proj/docs/research/a.md"), 1);
        assert_eq!(greps("rg -n foo docs/research/ | head -5"), 1);
        assert_eq!(greps("ag foo docs/research"), 1);
        assert_eq!(greps("git grep -n foo -- docs/research"), 1);
        assert_eq!(greps("git -C /work/proj/docs/research grep foo"), 1);
        assert_eq!(greps("grep -c foo docs/research/*.md"), 1);
        // Outside the root.
        assert_eq!(greps("grep -rn foo src"), 0);
        assert_eq!(greps("grep -rn foo docs"), 0);
        assert_eq!(greps("grep -rn foo /other/docs/research"), 0);
        assert_eq!(greps("grep -n foo docs/researching/a.md"), 0);
        assert_eq!(greps("rg foo"), 0);
        // The pattern is not a path, also when it names an archive file.
        assert_eq!(greps("grep -rn docs/research/a.md src"), 0);
        assert_eq!(greps("grep -rn -A 3 foo src"), 0);
        assert_eq!(greps("grep -A3 -e docs/research src"), 0);
        assert_eq!(greps("grep -rne foo docs/research"), 1);
        assert_eq!(greps("rg -g '*.md' foo docs/research"), 1);
        assert_eq!(greps("grep --include '*.md' -rn foo docs/research"), 1);
        // Input from a pipe or a redirect is not an archive path.
        assert_eq!(greps("cat docs/research/a.md | grep foo"), 0);
        assert_eq!(greps("grep foo < docs/research/a.md"), 0);
        assert_eq!(greps("grep foo src/a > docs/research/out.txt"), 0);
    }

    #[test]
    fn cd_variables_and_lists_are_followed() {
        assert_eq!(
            greps("cd docs/research && grep -n x references.md | cut -c1-9"),
            1
        );
        assert_eq!(greps("cd docs/research && rg x"), 1);
        assert_eq!(greps("cd docs/research && grep -r x"), 1);
        // Plain grep without a file reads its input.
        assert_eq!(greps("cd docs/research && grep x"), 0);
        assert_eq!(greps("cd docs/research && ls | rg x"), 0);
        assert_eq!(
            greps("R=/work/proj/docs/research; grep -n x $R/a.md; grep y \"${R}/b.md\""),
            2
        );
        assert_eq!(greps("grep -n x $UNSET/docs/research/a.md"), 0);
        assert_eq!(
            greps("for f in a b; do grep -n x docs/research/$f.md; done"),
            1
        );
        assert_eq!(
            greps("grep a docs/research/x || grep b docs/research/y\ngrep c docs/research/z &"),
            3
        );
        assert_eq!(greps("LC_ALL=C grep -n x docs/research/a.md"), 1);
        assert_eq!(greps("cd \"$X\" && grep -rn x docs/research"), 0);
        assert_eq!(greps("cd docs/research && grep -n x ~/notes.md"), 0);
        // A subshell's cd ends at its closing parenthesis.
        assert_eq!(greps("(cd docs/research && ls); rg x"), 0);
        assert_eq!(greps("(cd docs/research && rg x); rg y"), 1);
        assert_eq!(greps("(cd src && ls) && grep x docs/research/a.md"), 1);
        // Invocations that search nothing.
        assert_eq!(greps("cd docs/research && rg --files"), 0);
        assert_eq!(greps("rg --files docs/research"), 0);
        assert_eq!(greps("cd docs/research && git grep --help"), 0);
        // A redirect joined to a path operand.
        assert_eq!(greps("grep -n x docs/research/a.md>/dev/null 2>&1"), 1);
        assert_eq!(greps("grep -n x src/a.md 2>docs/research/err"), 0);
        assert_eq!(greps("cd ~/elsewhere && grep -rn x docs/research"), 0);
        // A relative path without a working directory resolves nowhere.
        let c = cfg();
        let mut k = Counter::new(&c);
        k.shell("grep x docs/research/a.md", None);
        assert_eq!(k.finish().greps, Some(0));
    }

    #[test]
    fn heredocs_quotes_and_comments_are_not_commands() {
        assert_eq!(
            greps(
                "python3 - <<'EOF'\ngrep -rn x docs/research\nlab search x\nEOF\ngrep y docs/research/a"
            ),
            1
        );
        assert_eq!(
            count(&["cat > f.py <<EOF\nlab search x\nEOF"]),
            (0, 0, 0, 0)
        );
        assert_eq!(greps("echo 'grep x docs/research/a; grep y'"), 0);
        assert_eq!(greps("ls # grep x docs/research/a"), 0);
        assert_eq!(
            greps("grep 'a|b; c' docs/research/a.md 2>&1 | grep -v 'x y'"),
            1
        );
    }

    #[test]
    fn worktree_sessions_use_their_own_checkout() {
        let c = cfg();
        let mut k = Counter::new(&c);
        k.see_cwd("/wt/proj-1");
        k.see_cwd("/elsewhere");
        k.shell("grep x docs/research/a.md", Some("/wt/proj-1"));
        k.shell("grep x /work/proj/docs/research/a.md", Some("/wt/proj-1"));
        k.shell("grep x docs/research/a.md", Some("/elsewhere"));
        k.grep_tool(Some("docs/research"), Some("/wt/proj-1"));
        k.grep_tool(None, Some("/wt/proj-1/docs/research/history"));
        k.grep_tool(None, Some("/wt/proj-1"));
        k.read("/wt/proj-1/docs/research/START.md");
        k.read("/wt/proj-1/README.md");
        let a = k.finish();
        assert_eq!((a.greps, a.reads), (Some(4), Some(1)));
        assert_eq!(a.search_share, Some(0.0));
        // A session that started below its checkout's root.
        let mut k = Counter::new(&c);
        k.see_cwd("/wt/proj-2/crates/x");
        k.read("/wt/proj-2/docs/research/START.md");
        k.read("/other/docs/research/START.md");
        assert_eq!(k.finish().reads, Some(1));
        // A root that is the project root itself.
        let c = Config {
            project: "/work/proj".into(),
            roots: vec![".".into()],
        };
        let mut k = Counter::new(&c);
        k.read("/work/proj/a.md");
        k.read("/work/other/a.md");
        assert_eq!(k.finish().reads, Some(1));
    }

    #[test]
    fn mcp_tools_queries_and_share() {
        let c = cfg();
        let mut k = Counter::new(&c);
        k.tool("mcp__lab__archive_search");
        k.tool("lab.archive_open");
        k.tool("archive_related");
        k.tool("mcp__lab__archive_searcher");
        k.tool("Grep");
        assert!(k.shell("cd /work/proj && lab search nothing", None));
        assert!(k.shell("lab archive find nothing", None));
        assert!(!k.shell("lab search x | head", None));
        assert!(!k.shell("lab open DR1", None));
        assert!(!k.shell("lab search x; false", None));
        // Another command before the query can set the exit status.
        assert!(!k.shell("false; lab search x", None));
        assert!(!k.shell("lab search x; lab search y", None));
        assert!(k.shell("M=/work/proj; cd $M && lab search x", None));
        k.no_hit();
        k.shell("grep x /work/proj/docs/research/a", None);
        let a = k.finish();
        assert_eq!(
            (a.searches, a.opens, a.related, a.no_hit_searches),
            (12, 2, 1, 1)
        );
        assert_eq!(a.greps, Some(1));
        assert_eq!(a.search_share, Some(12.0 / 13.0));
        assert_eq!(a.grep_share, Some(1.0 / 13.0));
    }

    #[test]
    fn without_a_root_only_searches_are_known() {
        let c = Config {
            project: "/work/proj".into(),
            roots: Vec::new(),
        };
        let mut k = Counter::new(&c);
        k.shell(
            "lab search x; grep y docs/research/a.md",
            Some("/work/proj"),
        );
        k.read("/work/proj/docs/research/a.md");
        let a = k.finish();
        assert_eq!(a.searches, 1);
        assert_eq!((a.greps, a.reads, a.search_share), (None, None, None));
        let c = cfg();
        let a = Counter::new(&c).finish();
        assert_eq!(
            (a.greps, a.search_share, a.grep_share),
            (Some(0), None, None)
        );
    }
}
