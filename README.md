# agentprof

A profiler for coding-agent sessions. It reads the local records of Claude Code, Codex and the T3 Code orchestrator for one project. From them it measures time, tokens and API-equivalent cost, and it flags wasteful or risky patterns.

Design rules:

- **Traceable.** Every number comes from named source records, and every finding cites a file and line or a database row.
- **No double counting by construction.** Claude usage is counted once per API request across all transcripts. Codex usage is the sum of increments of its cumulative counter, with inherited history excluded. T3 threads carry no tokens; their tokens are counted in the native sessions they link to.
- **Unknown stays unknown.** A model without a verified price gets cost `null` and is listed as unpriced. The tool never guesses a price or substitutes a default.
- **Read-only.** SQLite databases are opened with `mode=ro`, or with `immutable=1` when read-only mode fails and no `-wal` file exists. The tool writes only to `--out`, and it never writes `research.toml`.

## Build

```sh
cargo build --release
./target/release/agentprof --help
```

The crate needs Rust 1.99 or later (edition 2024). SQLite is bundled.

## Versioning

Every pull request that changes the tool's behaviour raises the version in `Cargo.toml` and `Cargo.lock`.

## Commands

### `agentprof scan`

```sh
# Profile every session whose working directory is the project or below it.
agentprof scan --project ~/code/thesis --out ./profile

# Pin the T3 project, print findings as JSON Lines, and tighten two rules.
agentprof scan --project ~/code/thesis --out ./profile \
  --t3-project 0b4c... --format jsonl --reread-min 3 --idle-gap-minutes 20

# Use inputs copied elsewhere.
agentprof scan --project /work/thesis --out ./profile \
  --claude-dir ./copy/claude-projects --codex-db ./copy/state_5.sqlite \
  --t3-db ./copy/statev2.sqlite --billing-log ./copy/decisions.jsonl
```

`scan` writes three files to `--out`:

| File | Content |
|---|---|
| `sessions.jsonl` | One `rf.session/1` record per session, sorted by source and id. |
| `findings.jsonl` | One `rf.finding/1` record per finding. |
| `scan.json` | `agentprof.scan/1`: inputs, coverage, cross-checks, active-time unions, thresholds and the pricing used. |

Stdout gets a short text summary. With `--format jsonl`, it gets the findings instead.

Flags:

| Flag | Default |
|---|---|
| `--project` | The directory of `research.toml`. Relative paths resolve against the cwd; `.` and `..` are removed, symlinks are kept. |
| `--out` | `[agentprof] out`. One of the two is required. |
| `--config` | Searches upwards from the cwd for `research.toml`. |
| `--t3-project` | The T3 project whose workspace root is the project root. Without the flag, no such project means T3 is absent for this project (`coverage.t3.reason`). A given id that does not exist is an error. |
| `--claude-dir` | `~/.claude/projects` |
| `--codex-db` | `~/.codex/state_5.sqlite` |
| `--codex-archive` | `~/.codex/archived_sessions` |
| `--codex-sessions` | `~/.codex/sessions`. Searched, with the archive, for the earlier rollout segments of resumed threads. |
| `--t3-db` | `~/.t3/userdata/statev2.sqlite` |
| `--billing-log` | `~/.local/state/claude-api/decisions.jsonl` |
| `--pricing` | The built-in `pricing.toml`. |
| `--archive-root DIR` | `[agentprof] archive_roots`, else the directory of `[lab.archive] start`. Repeat the flag for several roots. See [Archive lookups](#archive-lookups). |
| `--timezone` | `[project] timezone`, else the system zone. Used for `by_day`. |
| `--include-titles` | Off. Adds thread and session titles to the records. |
| `--jobs` | Available cores, at most 8. The number of parallel Codex rollout readers. |
| `--format` | `text` or `jsonl`. |

The detector thresholds are listed under [Rules](#rules).

A missing input file is reported in `scan.json` under `inputs` and `coverage`. It is not an error. Neither is a damaged transcript:

- A Claude line that is not valid UTF-8 counts as a bad line (`coverage.claude.invalid_utf8_lines`). A transcript that cannot be read is skipped and counted in `coverage.claude.unreadable_files`. Only those in the project's own directory under `~/.claude/projects` are listed by path, in `coverage.claude.unreadable`; the rest are counted in `unreadable_in_other_project_dirs`, so other projects' names stay out of the output.
- A Codex rollout segment that fails to open or decompress part-way marks the transcript `damaged`. Its tokens, `by_*` breakdowns, active and wall time become unknown, and a `transcript-damaged` finding is written. The tokens counted before the break are only reported in aggregate, as `coverage.codex.tokens_counted_before_damage_excluded`.

### `agentprof report`

```sh
agentprof report ./profile --out ./profile/report.md --top 15
```

Renders a Markdown summary of a scan directory. It covers:

- Totals by source and time by source.
- Main sessions vs subagents.
- Breakdowns by model, effort, day and billing mode.
- Long-context pricing: per model, the priced requests, how many had prompts over the model's long-context threshold, the surcharge the tier added over standard rates, and the cost of all the model's requests.
- Top sessions by tokens and by wall time.
- Tools and Codex operations.
- Web statuses and failing hosts.
- Errors, rate limits, compactions and aborts.
- T3 runs and linkage.
- Findings by rule.
- Coverage and cross-checks.

Every cost is labelled an API-equivalent estimate. Time and turn sums skip sessions where the value is unknown and say how many, for example `6 (1 unknown)`.

`report` reads only schema major version 1 (`rf.session/1`, `rf.finding/1`, `agentprof.scan/1`, or a later minor such as `rf.session/1.1`); any other version is an error naming the file and line. When stdout is closed early (for example `| head`), it stops quietly with exit code 0.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | No `error` finding. |
| 1 | At least one `error` finding. |
| 2 | Usage error, a `--t3-project` id that does not exist, or an I/O or parse failure that stops the scan. |

## `research.toml`

```toml
[project]
timezone = "Asia/Tokyo"

[agentprof]
out = "profile"            # relative paths resolve against this file's directory
t3_project = "0b4c..."
archive_roots = ["docs/research"]   # see Archive lookups
# claude_dir, codex_db, codex_archive, codex_sessions, t3_db, billing_log, pricing
```

Flags override the file. Unknown keys in `[agentprof]` are an error. Of the other tools' tables, agentprof reads one key: `[lab.archive] start`, the default for the archive root.

## Selection

A session is in scope when one of these holds. `match_basis` records the first that applies, in this order:

| `match_basis` | Rule |
|---|---|
| `cwd` | The recorded working directory is the project root or below it. For Claude, a transcript must contain `"cwd":"<root>` followed by `"` or `/`. For Codex, `threads.cwd` must be within the root. |
| `t3-link` | A thread of the T3 project links to the session. |
| `parent` | Its parent session is in scope. This covers Claude subagent files and Codex spawned threads. |
| `t3-project` | T3 threads, which are selected by project id. |

## `rf.session/1`

One JSON object per line. The fields:

| Field | Type | Meaning |
|---|---|---|
| `schema` | string | `"rf.session/1"` |
| `source` | string | `claude`, `codex` or `t3` |
| `id` | string | Claude session id; a subagent is `<parent>/agent-<id>`. Codex thread id. T3 thread id. `source:id` is unique. |
| `role` | string | `main` or `subagent` |
| `spawn` | object or null | `mechanism`: `claude-task`, `codex-spawn`, `t3-delegate` or `provider-native` (a T3 mirror of a native subagent). Also `agent_type` and `depth`. |
| `parent_id` | string or null | Parent session in the same source. |
| `t3_thread_id`, `t3_link` | string or null | The T3 thread that links to this native session, and the link basis. |
| `match_basis` | string | See [Selection](#selection). |
| `cwd` | string or null | Working directory, with the home directory shown as `~`. |
| `title` | string or null | Only with `--include-titles`. |
| `client` | string or null | Claude `entrypoint` or Codex `originator`. |
| `start`, `end` | RFC 3339 or null | First and last record. |
| `wall_ms` | integer or null | `end - start`. Null when either is unknown. |
| `active_ms` | integer or null | See [Time](#time). |
| `active_basis` | string | How `active_ms` was computed. |
| `turns`, `open_turns` | integer or null, integer | User turns (null when the record has no turn events), and turns still open at the end of the record. |
| `total` | Acc or null | Tokens and cost. Null when the source has no per-request data. |
| `token_basis` | string | How `total` was computed. |
| `by_model`, `by_effort`, `by_day` | map of Acc | Disjoint partitions of `total`. `by_day` uses the configured time zone. |
| `db_tokens_used` | integer or null | Codex `threads.tokens_used`. Shown for comparison; never added to totals. |
| `inherited_tokens` | integer or null | Codex: the counter value a forked thread started from, its base thread's usage. Excluded from `total`. |
| `untracked_tokens` | integer or null | Codex: the counter value a thread that was not forked started from, its own usage in records that are missing or unreadable. Excluded from `total`, which is then a lower bound; see `history-gap`. |
| `max_prompt_tokens`, `context_window` | integer or null | Largest single-request prompt, and the model's window. |
| `tools` | map of ToolStat | Per tool name: `calls`, `results`, `errors`, `latency_ms_total`, `latency_ms_max`, `result_chars_total`, `result_chars_max`. |
| `reads` | object, absent when unknown | File reads that `file-reread` recognises: `calls`, and `result_chars` of their results. Claude: every `Read` call and each `Bash` call that is a read, with the characters of its tool result text. Codex: each `CommandExecution` with a parsed read of a file, with the characters of its recorded `aggregated_output`. That is the whole command's output, and Codex keeps at most 64 KiB of it. A Codex rollout in the legacy history mode keeps no `CommandExecution` items, so it shows no reads, as in `file-reread`. Lab can measure it as `rule: session.reads.result_chars`. Absent for T3 threads, Codex threads whose rollout is missing, and scans written before agentprof recorded it. |
| `archive` | object, absent when unknown | Archive lookups; see [Archive lookups](#archive-lookups). `searches`, `opens`, `related`, `no_hit_searches` (integers), and `greps`, `reads` (integers) and `search_share`, `grep_share` (numbers), which are null when the scan has no archive root. Absent as `reads` is. |
| `error_basis` | string | What counts as a tool error for this source. An archive query (`lab search` or `lab archive find`) that exits 1 found nothing: it is not a tool error and not a failed operation. |
| `operations` | map of OpStat | Codex `item_completed` kinds: `count`, `failed`, `duration_ms_total`, `duration_ms_max`. |
| `web` | map | Host, then status, then count. A status is an HTTP code or a failure class: `dns`, `connect-refused`, `connection-reset`, `timeout`, `tls`, `cloudflare-challenge`, `error-other`, `ok` (a successful Claude web search, under host `(web-search)`), `ok-no-status` (Claude: fetched, no code recorded) or `no-status` (Codex page open, no code recorded). |
| `compactions` | object | `count`, `auto`, `manual`, `unknown` |
| `aborts` | integer | Interrupted or aborted turns. |
| `api_errors` | map | API error kind, then count. |
| `rate_limits` | object | `observations`, `max_used_percent`, `saturated_observations` (≥ 100 %), `limit_reached_observations`, `limit_errors` (refused Claude requests), `limit_types` |
| `cost` | object | `usd` (null if any request is unpriced), `priced_usd`, `unpriced_requests`, `unpriced_models`, `server_tool_usd`, `basis` |
| `billing` | object | `mode`: `subscription`, `api`, `refused` or `unknown`. Also `basis` and `plan_type`. |
| `claude_code_cost_usd` | number or null | Claude Code's own cost counter. Reported, never used. |
| `transcript` | object or null | `path` (Codex: the newest segment), `status` (`present`, `zstd`, `missing`; Codex also `damaged` or `incomplete`, when the recorded newest segment is missing but earlier ones exist), `lines`, `bad_lines`, `inherited_lines`; `segments` (Codex: every segment file read, oldest first, when there are several or the recorded newest one is missing) and `error` (the read error of a damaged transcript). |
| `t3` | object or null | T3 threads only. `project_id`, `provider`, `runs_by_status`, `runs_by_model`, `runs_by_effort`, `runs_by_provider` (the run's own provider, then status), `archived`, `deleted`, `linked` (`<source>:<id>`), `link_basis`, `unresolved_links`. |
| `notes` | array of string | Caveats specific to this session, such as an assumed service tier. |

An **Acc** is `{requests, tokens, priced_usd, unpriced_requests, long_context_requests, long_context_usd}`. The last two count the priced requests whose prompt exceeded the model's long-context threshold, and what that tier added over the standard rates (already included in `priced_usd`). A scan written before they existed lacks them; `report` then says long-context use was not recorded instead of showing zero, and `long-context-tier` does not fire.

**Tokens** has seven fields:

- `input`: uncached input.
- `cache_write_5m`, `cache_write_1h` and `cache_write_unknown`: cache writes by TTL.
- `cache_read`.
- `output`.
- `reasoning`.

The classes `input`, the three cache writes, `cache_read` and `output` are disjoint, and their sum is the total. `reasoning` is a subset of `output`, never an extra class. Codex `input_tokens` includes cached input, so agentprof records `input = input_tokens − cached_input_tokens`.

### Archive lookups

`archive` measures whether agents use the archive's search tools or grep the archive. It is not a finding rule: it writes no finding.

**Archive root.** Nothing is built in. The roots come from the first of these that is set:

1. `--archive-root DIR`, which can repeat.
2. `[agentprof] archive_roots`, a list.
3. The directory of `[lab.archive] start` in `research.toml`. A project that uses lab's archive needs no agentprof setting.

A root is absolute, or relative to the project root. A session that started outside the project root (a git worktree of the project) also matches a relative root under the directory it started in. `scan.json` records the roots and their origin in `archive.roots` and `archive.roots_basis`, with the totals over all sessions. Without a root, `greps`, `reads`, `search_share` and `grep_share` are null.

| Field | Counts |
|---|---|
| `searches` | Shell commands `lab search`, `lab open`, `lab related` and `lab archive find`, and calls of tools named `archive_search`, `archive_open` or `archive_related` (the name after the last `__` or `.`, so any MCP server prefix matches). A command with `--help` or `-h` is not counted. |
| `opens` | The `lab open` and `archive_open` calls among `searches`. |
| `related` | The `lab related` and `archive_related` calls among `searches`. |
| `no_hit_searches` | `lab search` and `lab archive find` commands that exited 1. The query must be the last command of the shell call, so the exit status is its own; behind a pipe such as `\| head` the status is lost and nothing is counted. |
| `greps` | Shell commands `grep`, `egrep`, `fgrep`, `rg`, `ag` and `git grep` with a path operand under a root, and Claude `Grep` calls whose `path` (default: the working directory) is under a root. Without a path operand, `rg`, `ag`, `git grep` and `grep -r` count when the working directory is under a root and the command does not read a pipe. A path above the root, such as `docs` for the root `docs/research`, is not counted. |
| `reads` | The file reads of the `reads` field whose file is under a root. |
| `search_share` | `searches / (searches + greps)`. Null when both are 0. |
| `grep_share` | `greps / (searches + greps)`, which is 1 − `search_share`. Null when both are 0. |

Each shell command counts once, so one `Bash` call with three greps counts three. Shell commands are read without running them: agentprof splits a command line at `;`, `&&`, `||`, `|`, `&`, newlines and parentheses, follows `cd DIR` and `NAME=value` with literal values, and skips here-document bodies, quoted text, comments and redirect targets. The first operand of a grep is its pattern unless `-e` or `-f` gives one. It does not expand globs, `~`, command substitutions or loop variables, and it does not see commands inside `xargs`, `find -exec` or `sh -c`. Claude commands resolve against the working directory recorded with the call, Codex commands against the thread's working directory.

Lab reads these as lesson metrics: `agentprof:session.archive.searches`, `agentprof:session.archive.greps`, `agentprof:session.archive.reads`, `agentprof:session.archive.search_share`, `agentprof:session.archive.grep_share`, `agentprof:session.archive.no_hit_searches`, `agentprof:session.archive.opens` and `agentprof:session.archive.related`. A null value leaves the session out, so the two shares cover only sessions with at least one lookup. Lab treats a lower value of a session field as better, so a lesson that should raise the use of search measures `grep_share` or `greps`, not `search_share`.

### Time

- **Wall time** is first record to last record. For T3, it is first run start to last run end. A T3 thread without runs has unknown wall time.
- **Active time** is model-working time:
  - **Claude:** over turns, the time from the prompt to the turn's last assistant record, tool result or interruption. Meta lines (hook output) and system records (local commands, hooks, compaction) do not extend a turn.
  - **Codex:** the sum of `duration_ms` from `task_complete` and `turn_aborted`. A turn without an end event counts up to its last record.
  - **T3:** the sum over runs of `completedAt − startedAt`.
- Active time and turns are null, not 0, when the record has no turn events or no timed turn. The unions in `scan.json` leave those sessions out and count them under `sessions_with_unknown_active_time`.
- Summing active time over sessions counts parallel subagents twice. `scan.json` `active_union_ms` therefore also gives the length of the union of the intervals: per source, Claude plus Codex, and all sources.

### Cost

Cost is an **API-equivalent estimate**: what the counted tokens would cost at the list prices in `pricing.toml`. It is not what was billed. A subscription session costs its plan fee. Requests are priced individually, including long-context tiers. The tier follows the request's own prompt: for Claude, the request's prompt; for Codex, the event's `last_token_usage.input_tokens`. A Codex counter increment can span several requests (it then differs from `last_token_usage`); it is priced at the tier of the last of them, the only one whose size is known. `coverage.codex.multi_request_increments` counts such increments, and `multi_request_increments_tier_differs` those whose summed prompt would have crossed the threshold differently. A request is unpriced, so the session's `cost.usd` is null, in these cases:

- The model has no price row.
- Claude reports a non-standard `speed` or `service_tier`.
- Claude reports an `inference_geo` other than global or `us`. US-only inference costs 1.1×.
- A cache write has an unknown TTL.
- Codex reports a service tier other than `default`.

When Codex records no tier, standard is assumed and a note says so.

### Billing mode

- **Claude:** the latest decision in the billing log made for the same directory at most 120 s before the session's first record. Otherwise the mode is `unknown`. A subagent inherits its session's mode, with a basis that says so; only a subagent whose session is not in the scan is looked up by its own first record.
- **Codex:** `subscription` when the rollout's rate-limit snapshots carry a `plan_type`. Otherwise the mode is `unknown`.

### T3 linkage

Each T3 thread is linked to the native sessions it drove. The link bases, from strongest to weakest:

1. `thread-ref`: the provider thread reference.
2. `agent-ref`: the provider-native Claude subagent id.
3. `turn-ref`: provider turn ids matched against Codex turn ids or Claude message uuids.

T3 threads carry no tokens, so linking never adds tokens. `coverage.t3` reports linked and unlinked threads, and native sessions with and without a T3 thread.

## `rf.finding/1`

```json
{"schema":"rf.finding/1","tool":"agentprof","rule":"file-reread","severity":"warn",
 "subject":"claude:<session-id>","message":"3 file(s) were read at least 5 times; ...",
 "evidence":[{"path":"~/.claude/projects/<dir>/<session>.jsonl","locator":"line 812"}],
 "data":{"max_reads":9},"at":"2026-11-07T00:00:00Z"}
```

`subject` is `source:id`, or `project:<source>` for project-level findings. `evidence.path` shows the home directory as `~`; for a Codex thread read from several segments, it names the segment holding the line and the line number within it. Findings never contain file contents. Commands appear only as a hash plus the program name.

## Rules

| Rule | Severity | Fires when | Threshold flag (default) |
|---|---|---|---|
| `file-reread` | warn | One range of a file is read again: after a first read, at least N − 1 more reads repeat a range already read. A range is the whole file, or lines a–b, or a Claude `Read` offset/limit/pages that is not a line range. `sed -n 'a,bp'` reads lines a–b; a Claude `Read` with a numeric limit reads lines offset (default 1) to offset + limit − 1, so both key alike. `cat` and `nl` read the whole file, and any other Codex read command repeats only when identical. A Claude `Bash` call is a read when its command is one simple `cat FILE`, `nl [-ba] FILE` or `sed -n 'a,bp' FILE` with literal words, optionally after exactly one `cd DIR &&` with a literal DIR and no options. Any other pipe, `&&`, `\|\|`, `;`, `&`, redirect, subshell, `$`, glob or `~` makes it no read. DIR is resolved against the working directory recorded with the call, and a relative FILE against DIR (or that working directory without a `cd`), so the call keys like a `Read` of the same file. Reads of different ranges are not rereads. | `--reread-min` (5) |
| `repeated-command` | warn | One identical shell command runs at least N times. | `--repeat-cmd-min` (4) |
| `large-tool-output` | warn | A tool result returns at least N characters. | `--large-output-chars` (100000) |
| `tool-error-streak` | warn | At least N consecutive tool results are errors. | `--error-streak-min` (4) |
| `web-fetch-failure` | warn if blocked (403, 418, 429, `cloudflare-challenge`), else info | A host has failed web requests. One finding per host. | none |
| `sleep-poll-loop` | warn | Sleep commands plus polling calls (`wait`, `*.wait`, `wait_agent`, `*sleep`) number at least N. | `--sleep-min` (5) |
| `low-cache-read-ratio` | warn | Cache reads / prompt tokens is below R, in a session with at least M prompt tokens. | `--cache-read-min-ratio` (0.5), `--cache-min-prompt-tokens` (1000000) |
| `cache-churn` | warn | Cache writes / (writes + reads) is at least R, in a session with at least M prompt tokens. | `--cache-churn-ratio` (0.25) |
| `frequent-compaction` | warn | At least N compactions. | `--compactions-min` (3) |
| `near-context-window` | warn | A request's prompt is at least R × the context window. It cannot fire for models with a 1,000,000-token window, such as Claude Opus 5.5: Claude Code compacts the context long before the prompt reaches 0.9 × 1M. Claude transcripts record each compaction's `preTokens`, but not the threshold that triggered it. For context pressure in those sessions, use `frequent-compaction` or the session field `max_prompt_tokens` (`rule: session.max_prompt_tokens` in lab). | `--context-ratio` (0.9) |
| `long-context-tier` | warn | At least a share S of a session's priced requests had prompts over the model's long-context threshold (for example 272k tokens for gpt-6.1-sol), so they were priced at the higher tier. Reports the surcharge. | `--long-context-share` (0.1) |
| `long-idle-gap` | info | No record for at least N minutes inside a session. | `--idle-gap-minutes` (30) |
| `rate-limit-saturation` | warn | A usage window was at or above 100 %, a limit was reached, or a request was refused for a limit. | none |
| `api-error` | warn | API errors other than usage limits. | none |
| `turn-aborted` | info | Turns were interrupted or aborted. | none |
| `inherited-counter` | info | A forked Codex thread's counter started from its base thread's usage. The inherited part is excluded. | none |
| `history-gap` | warn | A Codex thread that was not forked has a counter that starts above zero: its own usage in records that are missing or unreadable. Those tokens are excluded, so the thread's totals are a lower bound. | none |
| `fork-baseline` | **error** if the fork's first counter repeats a counter of its base thread (the first request would be the base thread's, counted twice); warn if the starting counter matches no counter of the base thread | Forked Codex threads are checked against their base thread's counters when both are in the scan; `coverage.codex.fork_baselines` counts `verified`, `zero_base`, `parent_unavailable`, `replayed` and `mismatched`. | none |
| `counter-unparseable` | warn | A Codex `token_count` event did not parse. Later counters still cover its tokens; if it came before the first readable counter, its tokens end up in `untracked_tokens`. | none |
| `transcript-damaged` | warn | A transcript could not be read to the end (Codex `damaged`), or the recorded newest Codex segment is missing while earlier ones exist (`incomplete`). The thread's tokens are unknown. | none |
| `counter-decrease` | **error** | A Codex cumulative counter decreased, so the session's tokens are unreliable. | none |
| `usage-conflict` | warn | One Claude request appears with different usage on different lines. The line with the largest output is kept. | none |
| `transcript-missing` | warn | A Codex rollout file no longer exists. | none |
| `run-failed` | warn | T3 runs failed. | none |
| `run-interrupted` | info | T3 runs were interrupted, cancelled or rolled back. | none |
| `t3-unlinked` | info | A T3 thread links to no native session. | none |
| `subagent-fanout` | info | A session has at least N direct child sessions. A Claude subagent started by another subagent is that subagent's child (`coverage.claude.subagent_parent_nested`), not the session's. | `--fanout-min` (5) |
| `subagent-token-share` | info | The share of a source's tokens spent in subagents. Always reported, once per source. | none |

## `scan.json` cross-checks

| Check | Compares | Expected result |
|---|---|---|
| `claude_dedup` | Naive per-line usage sums vs the deduplicated totals. | The inflation factor per class that naive counting would cause. Usage conflicts and cross-file duplicates are counted. |
| `codex_tokens_used` | Each thread's last cumulative counter vs `threads.tokens_used`. Damaged and incomplete threads are not compared. | `tokens_used_equals_last_counter` for every compared thread, and `sum_own_total + sum_inherited_at_start + sum_untracked_at_start = sum_last_counter_total + sum_rewound` (`sum_rewound`: tokens of rolled-back requests, see [Limitations](#limitations)). `tokens_used_overcount_factor` is `tokens_used` vs own usage. |
| `t3_runs` | Parsed run counts by run provider and status vs a direct SQL `GROUP BY`. | `equal: true`. |

## Pricing

`pricing.toml` (schema `agentprof.pricing/1`) lists, per model:

- Standard-tier USD per million tokens for each token class.
- Optional long-context rates and their threshold.
- An optional context window.
- The source URL and the date it was checked.

`[[server_tool]]` rows price server-side tools per 1,000 uses. Edit a copy and pass `--pricing` to change prices. Add a row only with an official source.

## Privacy

By default, outputs contain no message text, tool input or tool output.

- Titles appear only with `--include-titles`.
- Commands are reduced to a hash and the program name.
- File paths appear in `file-reread` findings and as evidence. Every path the tool writes shows the home directory as `~`, and so does any other `/Users/<name>` or `/home/<name>` prefix.
- A program name is the first word of the command after `NAME=value` assignments and `env` with its options, with shell quoting removed. Anything that is not a plain program name, such as a word containing `=`, becomes `unknown`, so an inline secret never reaches a finding.

## Limitations

- Codex cache writes are mapped to `cache_write_unknown`. Codex logs have not been seen to report non-zero cache writes.
- Codex segments are matched by file name (`rollout-<time>-<thread>[_<segment>].jsonl[.zst]`) in `--codex-sessions` and `--codex-archive`. Segments elsewhere are not found; their usage then shows as `untracked_tokens`.
- A later Codex segment can resume from an earlier state: it restarts the last turn, and its first counter continues from the value before that turn, below the previous segment's last counter. The rolled-back requests were made, so their tokens stay in `total`; the segment's first request counts as its own `last_token_usage`, and no `counter-decrease` is reported. The thread's last counter, and `threads.tokens_used`, then omit the rolled-back tokens. `coverage.codex.segment_rewinds` gives the `count`, the `rewound_tokens`, and `starting_counter_unmatched`, rewinds whose starting value is no earlier counter of the thread; each such thread has a note.
- Codex `openPage` web actions carry no HTTP status and are counted as `no-status`.
- The billing mode is only as complete as the billing log. With no matching decision, it is `unknown`.
- T3 run times measure orchestration. A run's native session may continue after T3 marks the run complete.

## Licence

MIT OR Apache-2.0, at your option.
