//! `rf.session/1` records. The schema is documented in the README.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::pricing::RequestCost;

pub const SCHEMA: &str = "rf.session/1";

/// Token counts in disjoint classes: `input`, the three cache-write classes,
/// `cache_read` and `output` add up to everything the model processed.
/// `reasoning` is a subset of `output`, not an extra class.
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tokens {
    /// Uncached input tokens.
    pub input: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    /// Cache writes whose TTL the record does not state.
    pub cache_write_unknown: u64,
    pub cache_read: u64,
    /// Output tokens, including reasoning/thinking tokens.
    pub output: u64,
    /// Reasoning or thinking tokens; already included in `output`.
    pub reasoning: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.prompt() + self.output
    }

    /// Everything sent to the model: uncached input, cache writes and reads.
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_writes() + self.cache_read
    }

    pub fn cache_writes(&self) -> u64 {
        self.cache_write_5m + self.cache_write_1h + self.cache_write_unknown
    }

    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.cache_write_5m += o.cache_write_5m;
        self.cache_write_1h += o.cache_write_1h;
        self.cache_write_unknown += o.cache_write_unknown;
        self.cache_read += o.cache_read;
        self.output += o.output;
        self.reasoning += o.reasoning;
    }
}

/// Tokens and API-equivalent cost accumulated over model requests.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Acc {
    /// Model responses counted (Claude: deduplicated API requests; Codex:
    /// token-counter increments).
    pub requests: u64,
    pub tokens: Tokens,
    /// Sum of the API-equivalent cost of the priced requests, in USD.
    pub priced_usd: f64,
    /// Requests whose cost could not be established; their tokens are in
    /// `tokens` but not in `priced_usd`.
    pub unpriced_requests: u64,
    /// Priced requests whose prompt was over the model's long-context
    /// threshold, and what that tier added over the standard rates (USD,
    /// included in `priced_usd`).
    #[serde(default)]
    pub long_context_requests: u64,
    #[serde(default)]
    pub long_context_usd: f64,
}

impl Acc {
    pub fn add_request(&mut self, t: &Tokens, cost: Option<RequestCost>) {
        self.requests += 1;
        self.tokens.add(t);
        match cost {
            Some(c) => {
                self.priced_usd += c.usd;
                if c.long_context {
                    self.long_context_requests += 1;
                    self.long_context_usd += c.long_context_usd;
                }
            }
            None => self.unpriced_requests += 1,
        }
    }

    pub fn priced_requests(&self) -> u64 {
        self.requests - self.unpriced_requests
    }

    pub fn merge(&mut self, o: &Acc) {
        self.requests += o.requests;
        self.tokens.add(&o.tokens);
        self.priced_usd += o.priced_usd;
        self.unpriced_requests += o.unpriced_requests;
        self.long_context_requests += o.long_context_requests;
        self.long_context_usd += o.long_context_usd;
    }

    /// Full cost when every request was priced, otherwise unknown.
    pub fn usd(&self) -> Option<f64> {
        (self.unpriced_requests == 0).then_some(self.priced_usd)
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct ToolStat {
    pub calls: u64,
    /// Calls whose result was found.
    pub results: u64,
    /// Results flagged as errors (see `error_basis`).
    pub errors: u64,
    /// Sum and maximum of the time from the call record to its result record.
    pub latency_ms_total: u64,
    pub latency_ms_max: u64,
    /// Characters of text returned to the model.
    pub result_chars_total: u64,
    pub result_chars_max: u64,
}

/// Codex `item_completed` operations (shell commands, MCP calls, web actions).
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct OpStat {
    pub count: u64,
    pub failed: u64,
    pub duration_ms_total: u64,
    pub duration_ms_max: u64,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct RateLimits {
    /// Codex rate-limit snapshots seen.
    pub observations: u64,
    pub max_used_percent: Option<f64>,
    /// Snapshots with a primary or secondary window at or above 100 percent.
    pub saturated_observations: u64,
    /// Snapshots whose `rate_limit_reached_type` was set.
    pub limit_reached_observations: u64,
    /// Requests refused because a usage limit was reached (Claude).
    pub limit_errors: u64,
    /// Limit kinds seen (Claude `rateLimitType`, Codex reached type).
    pub limit_types: BTreeMap<String, u64>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Compactions {
    pub count: u64,
    pub auto: u64,
    pub manual: u64,
    /// Compactions whose trigger the record does not state.
    pub unknown: u64,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Cost {
    /// API-equivalent estimate in USD; null when any request is unpriced.
    pub usd: Option<f64>,
    /// Estimate over the priced requests only.
    pub priced_usd: f64,
    pub unpriced_requests: u64,
    pub unpriced_models: Vec<String>,
    /// Server-side tool charges included in `priced_usd` (Claude web search).
    pub server_tool_usd: f64,
    pub basis: String,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Billing {
    /// `subscription`, `api`, `refused` or `unknown`.
    pub mode: String,
    pub basis: String,
    pub plan_type: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Transcript {
    pub path: Option<String>,
    /// `present`, `zstd`, `missing`, `damaged` (Codex: a segment failed to
    /// open or decompress part-way) or `incomplete` (Codex: earlier segments exist
    /// but the recorded newest one does not).
    pub status: String,
    pub lines: u64,
    pub bad_lines: u64,
    /// Codex lines copied from the parent thread's history and skipped.
    pub inherited_lines: u64,
    /// Codex rollout segment files read, oldest first, when there are several
    /// or the recorded newest one is missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<String>,
    /// The read error for a `damaged` transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Spawn {
    /// `claude-task`, `codex-spawn`, `t3-delegate` or `provider-native` (a T3 mirror of a native subagent).
    pub mechanism: String,
    pub agent_type: Option<String>,
    pub depth: Option<u64>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct T3Info {
    pub project_id: String,
    pub provider: String,
    pub runs_by_status: BTreeMap<String, u64>,
    pub runs_by_model: BTreeMap<String, u64>,
    pub runs_by_effort: BTreeMap<String, u64>,
    /// Runs by the provider that executed them, then by status. A thread can
    /// switch provider between runs.
    pub runs_by_provider: BTreeMap<String, BTreeMap<String, u64>>,
    pub archived: bool,
    pub deleted: bool,
    /// Native sessions this thread maps to, as `<source>:<id>`.
    pub linked: Vec<String>,
    /// `thread-ref` (provider-thread reference), `agent-ref` (provider-native
    /// Claude subagent id), `turn-ref` (weaker, through provider turn ids) or
    /// `none`.
    pub link_basis: String,
    /// Linked ids that the scan found no transcript or record for.
    pub unresolved_links: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Session {
    pub schema: String,
    /// `claude`, `codex` or `t3`.
    pub source: String,
    pub id: String,
    /// `main` or `subagent`.
    pub role: String,
    pub spawn: Option<Spawn>,
    /// Parent session id in the same source.
    pub parent_id: Option<String>,
    pub t3_thread_id: Option<String>,
    pub t3_link: Option<String>,
    /// Why the session is in scope: `cwd`, `t3-link`, `parent` or `t3-project`.
    pub match_basis: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// Claude `entrypoint` or Codex `originator`.
    pub client: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub wall_ms: Option<i64>,
    pub active_ms: Option<i64>,
    pub active_basis: String,
    pub turns: Option<u64>,
    /// Turns still open when the record ended.
    pub open_turns: u64,
    /// Token and cost totals; null when the source has no per-request data.
    pub total: Option<Acc>,
    pub token_basis: String,
    pub by_model: BTreeMap<String, Acc>,
    pub by_effort: BTreeMap<String, Acc>,
    pub by_day: BTreeMap<String, Acc>,
    /// Codex `threads.tokens_used` (cumulative counter; may include inherited
    /// history). Never added to totals.
    pub db_tokens_used: Option<u64>,
    /// Codex counter value inherited from the base thread at the start of
    /// a forked thread's rollout, excluded from `total`.
    pub inherited_tokens: Option<u64>,
    /// Codex counter value at the start of a thread that was not forked:
    /// the thread's own usage in records that are missing or unreadable.
    /// Excluded from `total`, so the thread's totals are a lower bound.
    #[serde(default)]
    pub untracked_tokens: Option<u64>,
    pub max_prompt_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub tools: BTreeMap<String, ToolStat>,
    pub error_basis: String,
    pub operations: BTreeMap<String, OpStat>,
    /// Web requests by host, then by status (HTTP code or failure class).
    pub web: BTreeMap<String, BTreeMap<String, u64>>,
    pub compactions: Compactions,
    pub aborts: u64,
    pub api_errors: BTreeMap<String, u64>,
    pub rate_limits: RateLimits,
    pub cost: Cost,
    pub billing: Billing,
    /// Claude Code's own `cost-state.totalCostUSD`; reported, never used.
    pub claude_code_cost_usd: Option<f64>,
    pub transcript: Option<Transcript>,
    pub t3: Option<T3Info>,
    pub notes: Vec<String>,
}

impl Session {
    pub fn new(source: &str, id: &str) -> Self {
        Session {
            schema: SCHEMA.to_string(),
            source: source.to_string(),
            id: id.to_string(),
            role: "main".to_string(),
            spawn: None,
            parent_id: None,
            t3_thread_id: None,
            t3_link: None,
            match_basis: String::new(),
            cwd: None,
            title: None,
            client: None,
            start: None,
            end: None,
            wall_ms: None,
            active_ms: None,
            active_basis: String::new(),
            turns: None,
            open_turns: 0,
            total: None,
            token_basis: String::new(),
            by_model: BTreeMap::new(),
            by_effort: BTreeMap::new(),
            by_day: BTreeMap::new(),
            db_tokens_used: None,
            inherited_tokens: None,
            untracked_tokens: None,
            max_prompt_tokens: None,
            context_window: None,
            tools: BTreeMap::new(),
            error_basis: String::new(),
            operations: BTreeMap::new(),
            web: BTreeMap::new(),
            compactions: Compactions::default(),
            aborts: 0,
            api_errors: BTreeMap::new(),
            rate_limits: RateLimits::default(),
            cost: Cost::default(),
            billing: Billing {
                mode: "unknown".to_string(),
                basis: String::new(),
                plan_type: None,
            },
            claude_code_cost_usd: None,
            transcript: None,
            t3: None,
            notes: Vec::new(),
        }
    }

    /// `<source>:<id>`, unique across sources.
    pub fn key(&self) -> String {
        format!("{}:{}", self.source, self.id)
    }

    pub fn set_span(&mut self, start: Option<i64>, end: Option<i64>) {
        self.start = start.map(crate::util::fmt_ts);
        self.end = end.map(crate::util::fmt_ts);
        self.wall_ms = match (start, end) {
            (Some(s), Some(e)) => Some(e - s),
            _ => None,
        };
    }

    /// Fills `cost` from `total` once all requests are accumulated.
    pub fn finish_cost(&mut self, basis: &str, server_tool_usd: f64) {
        if let Some(total) = &self.total {
            let mut unpriced: Vec<String> = self
                .by_model
                .iter()
                .filter(|(_, a)| a.unpriced_requests > 0)
                .map(|(m, _)| m.clone())
                .collect();
            unpriced.sort();
            self.cost = Cost {
                usd: total.usd(),
                priced_usd: total.priced_usd,
                unpriced_requests: total.unpriced_requests,
                unpriced_models: unpriced,
                server_tool_usd,
                basis: basis.to_string(),
            };
        } else {
            self.cost = Cost {
                usd: None,
                priced_usd: 0.0,
                unpriced_requests: 0,
                unpriced_models: Vec::new(),
                server_tool_usd: 0.0,
                basis: basis.to_string(),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acc_marks_cost_unknown_when_any_request_unpriced() {
        let t = Tokens {
            input: 10,
            output: 5,
            ..Tokens::default()
        };
        let one = RequestCost {
            usd: 1.0,
            long_context: false,
            long_context_usd: 0.0,
        };
        let mut a = Acc::default();
        a.add_request(&t, Some(one));
        assert_eq!(a.usd(), Some(1.0));
        a.add_request(&t, None);
        assert_eq!(a.usd(), None);
        assert_eq!(a.priced_usd, 1.0);
        assert_eq!(a.tokens.total(), 30);
        let long = RequestCost {
            usd: 2.0,
            long_context: true,
            long_context_usd: 0.5,
        };
        a.add_request(&t, Some(long));
        let mut b = Acc::default();
        b.merge(&a);
        assert_eq!((b.long_context_requests, b.long_context_usd), (1, 0.5));
        assert_eq!(b.priced_usd, 3.0);
    }
}
