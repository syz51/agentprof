//! agentprof: profile coding-agent sessions (Claude Code, Codex, T3 Code).

mod billing;
mod claude;
mod codex;
mod detect;
mod pricing;
mod report;
mod scan;
mod session;
mod t3;
mod util;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Profile every session of a project; writes sessions.jsonl, findings.jsonl and scan.json.
    Scan(Box<scan::ScanArgs>),
    /// Write a Markdown summary of a scan directory.
    Report {
        /// Directory written by `agentprof scan`.
        dir: PathBuf,
        /// Write the report here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Rows in the top-sessions tables.
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    let result = match cli.cmd {
        Cmd::Scan(a) => scan::run(&a).map(|o| if o.errors > 0 { 1 } else { 0 }),
        Cmd::Report { dir, out, top } => report::render(&dir, top).and_then(|md| {
            match out {
                Some(p) => std::fs::write(&p, md)
                    .map_err(|e| anyhow::anyhow!("writing {}: {e}", p.display()))?,
                None => {
                    use std::io::Write as _;
                    let mut out = std::io::stdout().lock();
                    out.write_all(md.as_bytes())?;
                    out.flush()?;
                }
            }
            Ok(0)
        }),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        // The reader closed stdout (e.g. `| head`): stop quietly. Scan
        // outputs are written to files before anything goes to stdout.
        Err(e) if is_broken_pipe(&e) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agentprof: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}
