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
| `--t3-db` | `~/.t3/userdata/statev2.sqlite` |
| `--billing-log` | `~/.local/state/claude-api/decisions.jsonl` |
| `--pricing` | The built-in `pricing.toml`. |
| `--timezone` | `[project] timezone`, else the system zone. Used for `by_day`. |
| `--include-titles` | Off. Adds thread and session titles to the records. |
| `--jobs` | Available cores, at most 8. The number of parallel Codex rollout readers. |
| `--format` | `text` or `jsonl`. |

The detector thresholds are listed under [Rules](#rules).

A missing input file is reported in `scan.json` under `inputs` and `coverage`. It is not an error.

### `agentprof report`

```sh
agentprof report ./profile --out ./profile/report.md --top 15
```

Renders a Markdown summary of a scan directory. It covers:

- Totals by source and time by source.
- Main sessions vs subagents.
- Breakdowns by model, effort, day and billing mode.
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
# claude_dir, codex_db, codex_archive, t3_db, billing_log, pricing
```

Flags override the file. Unknown keys in `[agentprof]` are an error.

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
| `turns`, `open_turns` | integer | User turns, and turns still open at the end of the record. |
| `total` | Acc or null | Tokens and cost. Null when the source has no per-request data. |
| `token_basis` | string | How `total` was computed. |
| `by_model`, `by_effort`, `by_day` | map of Acc | Disjoint partitions of `total`. `by_day` uses the configured time zone. |
| `db_tokens_used` | integer or null | Codex `threads.tokens_used`. Shown for comparison; never added to totals. |
| `inherited_tokens` | integer or null | Codex counter value inherited from the base thread. Excluded from `total`. |
| `max_prompt_tokens`, `context_window` | integer or null | Largest single-request prompt, and the model's window. |
| `tools` | map of ToolStat | Per tool name: `calls`, `results`, `errors`, `latency_ms_total`, `latency_ms_max`, `result_chars_total`, `result_chars_max`. |
| `error_basis` | string | What counts as a tool error for this source. |
| `operations` | map of OpStat | Codex `item_completed` kinds: `count`, `failed`, `duration_ms_total`, `duration_ms_max`. |
| `web` | map | Host, then status, then count. A status is an HTTP code or a failure class: `dns`, `connect-refused`, `connection-reset`, `timeout`, `tls`, `cloudflare-challenge`, `error-other`, `ok` (a successful Claude web search, under host `(web-search)`), `ok-no-status` (Claude: fetched, no code recorded) or `no-status` (Codex page open, no code recorded). |
| `compactions` | object | `count`, `auto`, `manual`, `unknown` |
| `aborts` | integer | Interrupted or aborted turns. |
| `api_errors` | map | API error kind, then count. |
| `rate_limits` | object | `observations`, `max_used_percent`, `saturated_observations` (≥ 100 %), `limit_reached_observations`, `limit_errors` (refused Claude requests), `limit_types` |
| `cost` | object | `usd` (null if any request is unpriced), `priced_usd`, `unpriced_requests`, `unpriced_models`, `server_tool_usd`, `basis` |
| `billing` | object | `mode`: `subscription`, `api`, `refused` or `unknown`. Also `basis` and `plan_type`. |
| `claude_code_cost_usd` | number or null | Claude Code's own cost counter. Reported, never used. |
| `transcript` | object or null | `path`, `status` (`present`, `zstd`, `missing`), `lines`, `bad_lines`, `inherited_lines` |
| `t3` | object or null | T3 threads only. `project_id`, `provider`, `runs_by_status`, `runs_by_model`, `runs_by_effort`, `runs_by_provider` (the run's own provider, then status), `archived`, `deleted`, `linked` (`<source>:<id>`), `link_basis`, `unresolved_links`. |
| `notes` | array of string | Caveats specific to this session, such as an assumed service tier. |

An **Acc** is `{requests, tokens, priced_usd, unpriced_requests, long_context_requests, long_context_usd}`. The last two count the priced requests whose prompt exceeded the model's long-context threshold, and what that tier added over the standard rates (already included in `priced_usd`).

**Tokens** has seven fields:

- `input`: uncached input.
- `cache_write_5m`, `cache_write_1h` and `cache_write_unknown`: cache writes by TTL.
- `cache_read`.
- `output`.
- `reasoning`.

The classes `input`, the three cache writes, `cache_read` and `output` are disjoint, and their sum is the total. `reasoning` is a subset of `output`, never an extra class. Codex `input_tokens` includes cached input, so agentprof records `input = input_tokens − cached_input_tokens`.

### Time

- **Wall time** is first record to last record. For T3, it is first run start to last run end. A T3 thread without runs has unknown wall time.
- **Active time** is model-working time:
  - **Claude:** over turns, the time from the prompt to the turn's last record.
  - **Codex:** the sum of `duration_ms` from `task_complete` and `turn_aborted`. A turn without an end event counts up to its last record.
  - **T3:** the sum over runs of `completedAt − startedAt`.
- Summing active time over sessions counts parallel subagents twice. `scan.json` `active_union_ms` therefore also gives the length of the union of the intervals: per source, Claude plus Codex, and all sources.

### Cost

Cost is an **API-equivalent estimate**: what the counted tokens would cost at the list prices in `pricing.toml`. It is not what was billed. A subscription session costs its plan fee. Requests are priced individually, including long-context tiers. A request is unpriced, so the session's `cost.usd` is null, in these cases:

- The model has no price row.
- Claude reports a non-standard `speed` or `service_tier`.
- Claude reports an `inference_geo` other than global or `us`. US-only inference costs 1.1×.
- A cache write has an unknown TTL.
- Codex reports a service tier other than `default`.

When Codex records no tier, standard is assumed and a note says so.

### Billing mode

- **Claude:** the latest decision in the billing log made for the same directory at most 120 s before the session's first record. Otherwise the mode is `unknown`.
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

`subject` is `source:id`, or `project:<source>` for project-level findings. `evidence.path` shows the home directory as `~`. Findings never contain file contents. Commands appear only as a hash plus the program name.

## Rules

| Rule | Severity | Fires when | Threshold flag (default) |
|---|---|---|---|
| `file-reread` | warn | One file is read at least N times in a session. | `--reread-min` (5) |
| `repeated-command` | warn | One identical shell command runs at least N times. | `--repeat-cmd-min` (4) |
| `large-tool-output` | warn | A tool result returns at least N characters. | `--large-output-chars` (100000) |
| `tool-error-streak` | warn | At least N consecutive tool results are errors. | `--error-streak-min` (4) |
| `web-fetch-failure` | warn if blocked (403, 418, 429, `cloudflare-challenge`), else info | A host has failed web requests. One finding per host. | none |
| `sleep-poll-loop` | warn | Sleep commands plus polling calls (`wait`, `*.wait`, `wait_agent`, `*sleep`) number at least N. | `--sleep-min` (5) |
| `low-cache-read-ratio` | warn | Cache reads / prompt tokens is below R, in a session with at least M prompt tokens. | `--cache-read-min-ratio` (0.5), `--cache-min-prompt-tokens` (1000000) |
| `cache-churn` | warn | Cache writes / (writes + reads) is at least R, in a session with at least M prompt tokens. | `--cache-churn-ratio` (0.25) |
| `frequent-compaction` | warn | At least N compactions. | `--compactions-min` (3) |
| `near-context-window` | warn | A request's prompt is at least R × the context window. | `--context-ratio` (0.9) |
| `long-context-tier` | warn | At least a share S of a session's priced requests had prompts over the model's long-context threshold (for example 272k tokens for gpt-6.1-sol), so they were priced at the higher tier. Reports the surcharge. | `--long-context-share` (0.1) |
| `long-idle-gap` | info | No record for at least N minutes inside a session. | `--idle-gap-minutes` (30) |
| `rate-limit-saturation` | warn | A usage window was at or above 100 %, a limit was reached, or a request was refused for a limit. | none |
| `api-error` | warn | API errors other than usage limits. | none |
| `turn-aborted` | info | Turns were interrupted or aborted. | none |
| `inherited-counter` | info | A Codex thread's counter started from inherited history. The inherited part is excluded. | none |
| `counter-decrease` | **error** | A Codex cumulative counter decreased, so the session's tokens are unreliable. | none |
| `usage-conflict` | warn | One Claude request appears with different usage on different lines. The line with the largest output is kept. | none |
| `transcript-missing` | warn | A Codex rollout file no longer exists. | none |
| `run-failed` | warn | T3 runs failed. | none |
| `run-interrupted` | info | T3 runs were interrupted, cancelled or rolled back. | none |
| `t3-unlinked` | info | A T3 thread links to no native session. | none |
| `subagent-fanout` | info | A session has at least N direct child sessions. | `--fanout-min` (5) |
| `subagent-token-share` | info | The share of a source's tokens spent in subagents. Always reported, once per source. | none |

## `scan.json` cross-checks

| Check | Compares | Expected result |
|---|---|---|
| `claude_dedup` | Naive per-line usage sums vs the deduplicated totals. | The inflation factor per class that naive counting would cause. Usage conflicts and cross-file duplicates are counted. |
| `codex_tokens_used` | Each thread's last cumulative counter vs `threads.tokens_used`. | `tokens_used_equals_last_counter` for every compared thread, and `sum_own_total + sum_inherited_at_start = sum_last_counter_total`. `tokens_used_overcount_factor` is `tokens_used` vs own usage. |
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
- Codex `openPage` web actions carry no HTTP status and are counted as `no-status`.
- The billing mode is only as complete as the billing log. With no matching decision, it is `unknown`.
- T3 run times measure orchestration. A run's native session may continue after T3 marks the run complete.

## Licence

MIT OR Apache-2.0, at your option.
