//! API list prices (`pricing.toml`) and per-request cost estimates.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::session::Tokens;

pub const DEFAULT_PRICING: &str = include_str!("../pricing.toml");

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct LongContext {
    pub threshold_tokens: u64,
    pub input: f64,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    pub cache_read: f64,
    pub output: f64,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub provider: String,
    pub model: String,
    pub input: f64,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    pub cache_read: f64,
    pub output: f64,
    pub context_window: Option<u64>,
    pub long_context: Option<LongContext>,
    pub source: String,
    pub checked: String,
    pub notes: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ServerTool {
    pub provider: String,
    pub name: String,
    pub usd_per_1000: f64,
    pub source: String,
    pub checked: String,
}

/// The API-equivalent cost of one request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RequestCost {
    pub usd: f64,
    /// The request was priced at the model's long-context tier.
    pub long_context: bool,
    /// What the tier added over the standard rates; 0 below the threshold.
    pub long_context_usd: f64,
}

/// Prices per million tokens for one tier; `None` marks an unpriced class.
struct Rates {
    input: f64,
    cw5: Option<f64>,
    cw1h: Option<f64>,
    read: f64,
    output: f64,
}

impl Rates {
    fn cost(&self, t: &Tokens) -> Option<f64> {
        let cw5 = if t.cache_write_5m > 0 { self.cw5? } else { 0.0 };
        let cw1h = if t.cache_write_1h > 0 {
            self.cw1h?
        } else {
            0.0
        };
        Some(
            (t.input as f64 * self.input
                + t.cache_write_5m as f64 * cw5
                + t.cache_write_1h as f64 * cw1h
                + t.cache_read as f64 * self.read
                + t.output as f64 * self.output)
                / 1_000_000.0,
        )
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    pub schema: String,
    #[serde(default)]
    pub model: Vec<ModelPrice>,
    #[serde(default)]
    pub server_tool: Vec<ServerTool>,
}

impl Pricing {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let (text, name) = match path {
            Some(p) => (
                std::fs::read_to_string(p)
                    .with_context(|| format!("reading pricing file {}", p.display()))?,
                p.display().to_string(),
            ),
            None => (
                DEFAULT_PRICING.to_string(),
                "built-in pricing.toml".to_string(),
            ),
        };
        Self::parse(&text).with_context(|| format!("in {name}"))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let p: Pricing = toml::from_str(text)?;
        if p.schema != "agentprof.pricing/1" {
            bail!("unsupported pricing schema {:?}", p.schema);
        }
        let mut seen = BTreeSet::new();
        for m in &p.model {
            if !seen.insert(m.model.clone()) {
                bail!("model {} is listed twice", m.model);
            }
            let rates = [m.input, m.cache_read, m.output];
            let optional = [m.cache_write_5m, m.cache_write_1h];
            if rates
                .iter()
                .chain(optional.iter().flatten())
                .any(|r| !r.is_finite() || *r < 0.0)
            {
                bail!("model {} has a negative or non-finite price", m.model);
            }
            // The surcharge is the tier's cost minus the standard cost of the
            // same tokens, so each tier price needs a standard price it is at
            // least as high as.
            if let Some(lc) = &m.long_context {
                let pairs = [
                    (Some(lc.input), Some(m.input)),
                    (lc.cache_write_5m, m.cache_write_5m),
                    (lc.cache_write_1h, m.cache_write_1h),
                    (Some(lc.cache_read), Some(m.cache_read)),
                    (Some(lc.output), Some(m.output)),
                ];
                for (tier, standard) in pairs {
                    let Some(tier) = tier else { continue };
                    if !tier.is_finite() || standard.is_none_or(|s| tier < s) {
                        bail!(
                            "model {}: each long_context price must be finite, have a standard price, and be at least that price",
                            m.model
                        );
                    }
                }
            }
            if m.source.is_empty() || m.checked.is_empty() {
                bail!("model {} lacks a source or checked date", m.model);
            }
        }
        Ok(p)
    }

    pub fn model(&self, name: &str) -> Option<&ModelPrice> {
        self.model.iter().find(|m| m.model == name)
    }

    pub fn server_tool(&self, provider: &str, name: &str) -> Option<&ServerTool> {
        self.server_tool
            .iter()
            .find(|s| s.provider == provider && s.name == name)
    }

    /// API-equivalent cost of one request, or `None` when it cannot be
    /// established: unknown model, a token class without a price, or cache
    /// writes of unknown TTL. [`Pricing::parse`] ensures a tier prices only
    /// classes the standard rates price, so the standard cost always exists
    /// when the tier's does. `multiplier` applies documented surcharges such
    /// as Anthropic's US-only inference (1.1). The result also says how much
    /// the long-context tier added: the cost at the tier minus the same tokens
    /// at the standard rates. A request at or below the threshold, or a model
    /// without a tier, adds nothing.
    ///
    /// `prompt_tokens` is the size of the request's own prompt, which picks
    /// the tier. It is passed separately because a Codex counter increment
    /// can cover several requests whose summed prompts say nothing about any
    /// one of them.
    pub fn request_cost(
        &self,
        model: &str,
        t: &Tokens,
        prompt_tokens: u64,
        multiplier: f64,
    ) -> Option<RequestCost> {
        let m = self.model(model)?;
        if t.cache_write_unknown > 0 {
            return None;
        }
        let standard = Rates {
            input: m.input,
            cw5: m.cache_write_5m,
            cw1h: m.cache_write_1h,
            read: m.cache_read,
            output: m.output,
        };
        let base = standard.cost(t)? * multiplier;
        match &m.long_context {
            Some(lc) if prompt_tokens > lc.threshold_tokens => {
                let tier = Rates {
                    input: lc.input,
                    cw5: lc.cache_write_5m,
                    cw1h: lc.cache_write_1h,
                    read: lc.cache_read,
                    output: lc.output,
                };
                let usd = tier.cost(t)? * multiplier;
                Some(RequestCost {
                    usd,
                    long_context: true,
                    long_context_usd: usd - base,
                })
            }
            _ => Some(RequestCost {
                usd: base,
                long_context: false,
                long_context_usd: 0.0,
            }),
        }
    }

    /// One-line description of the prices used, for cost `basis` fields.
    pub fn basis(&self, provider: &str) -> String {
        let checked: BTreeSet<&str> = self
            .model
            .iter()
            .filter(|m| m.provider == provider)
            .map(|m| m.checked.as_str())
            .collect();
        let checked: Vec<&str> = checked.into_iter().collect();
        format!(
            "API-equivalent estimate: per-request tokens x {provider} standard service tier list prices in pricing.toml, at the long-context rates for prompts over a model's threshold (checked {}); not an invoice",
            if checked.is_empty() {
                "never".to_string()
            } else {
                checked.join(", ")
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_pricing_parses() {
        let p = Pricing::load(None).unwrap();
        assert!(p.model("claude-opus-5-5").is_some());
        assert!(p.model("gpt-6.1-sol").is_some());
    }

    #[test]
    fn opus_request_cost() {
        let p = Pricing::load(None).unwrap();
        let t = Tokens {
            input: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
            cache_read: 1_000_000,
            output: 1_000_000,
            ..Tokens::default()
        };
        let c = p
            .request_cost("claude-opus-5-5", &t, t.prompt(), 1.0)
            .map(|c| c.usd)
            .unwrap();
        assert!((c - (4.0 + 5.0 + 8.0 + 0.2 + 20.0)).abs() < 1e-9);
        assert!(
            (p.request_cost("claude-opus-5-5", &t, t.prompt(), 1.1)
                .map(|c| c.usd)
                .unwrap()
                - c * 1.1)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn haiku_long_context_tier_uses_whole_prompt() {
        let p = Pricing::load(None).unwrap();
        // 60k uncached + 50k cache read = 110k prompt: over the 100k threshold.
        let t = Tokens {
            input: 60_000,
            cache_read: 50_000,
            output: 1_000,
            ..Tokens::default()
        };
        let c = p
            .request_cost("claude-haiku-5-5", &t, t.prompt(), 1.0)
            .map(|c| c.usd)
            .unwrap();
        let expect = (60_000.0 * 0.5 + 50_000.0 * 0.05 + 1_000.0 * 2.5) / 1e6;
        assert!((c - expect).abs() < 1e-12);
    }

    #[test]
    fn long_context_surcharge_is_tier_minus_standard() {
        let p = Pricing::load(None).unwrap();
        // 2k uncached + 300k cached = 302k prompt: over gpt-6.1-sol's 272k threshold.
        let t = Tokens {
            input: 2_000,
            cache_read: 300_000,
            output: 1_000,
            ..Tokens::default()
        };
        let c = p.request_cost("gpt-6.1-sol", &t, t.prompt(), 1.0).unwrap();
        let tier = (2_000.0 * 4.0 + 300_000.0 * 0.2 + 1_000.0 * 15.0) / 1e6;
        let standard = (2_000.0 * 2.0 + 300_000.0 * 0.1 + 1_000.0 * 10.0) / 1e6;
        assert!(c.long_context);
        assert!((c.usd - tier).abs() < 1e-12);
        assert!((c.long_context_usd - (tier - standard)).abs() < 1e-12);
        // At the threshold itself the standard rates apply.
        let at = Tokens {
            input: 0,
            cache_read: 272_000,
            ..Tokens::default()
        };
        let c = p
            .request_cost("gpt-6.1-sol", &at, at.prompt(), 1.0)
            .unwrap();
        assert!(!c.long_context);
        assert_eq!(c.long_context_usd, 0.0);
        // A model without a tier never adds a surcharge.
        let big = Tokens {
            cache_read: 900_000,
            ..Tokens::default()
        };
        let c = p
            .request_cost("claude-opus-5-5", &big, big.prompt(), 1.0)
            .unwrap();
        assert!(!c.long_context);
    }

    #[test]
    fn tier_follows_the_requests_own_prompt() {
        let p = Pricing::load(None).unwrap();
        // Two requests of 150k prompt tokens each, merged into one increment:
        // their sum (300k) is over the 272k threshold, but neither request is.
        let merged = Tokens {
            cache_read: 300_000,
            ..Tokens::default()
        };
        let c = p
            .request_cost("gpt-6.1-sol", &merged, 150_000, 1.0)
            .unwrap();
        assert!(!c.long_context);
        assert!((c.usd - 300_000.0 * 0.1 / 1e6).abs() < 1e-12);
        let c = p
            .request_cost("gpt-6.1-sol", &merged, 280_000, 1.0)
            .unwrap();
        assert!(c.long_context);
    }

    #[test]
    fn unknown_models_and_unpriced_classes_are_unknown() {
        let p = Pricing::load(None).unwrap();
        let t = Tokens {
            input: 1,
            ..Tokens::default()
        };
        assert_eq!(
            p.request_cost("gpt-unknown", &t, t.prompt(), 1.0)
                .map(|c| c.usd),
            None
        );
        let w = Tokens {
            cache_write_5m: 1,
            ..Tokens::default()
        };
        // OpenAI rows carry no 5-minute cache-write price.
        assert_eq!(
            p.request_cost("gpt-6.1-sol", &w, w.prompt(), 1.0)
                .map(|c| c.usd),
            None
        );
        let u = Tokens {
            cache_write_unknown: 1,
            ..Tokens::default()
        };
        assert_eq!(
            p.request_cost("claude-opus-5-5", &u, u.prompt(), 1.0)
                .map(|c| c.usd),
            None
        );
    }

    #[test]
    fn duplicate_rows_are_rejected() {
        let text = r#"
schema = "agentprof.pricing/1"
[[model]]
provider = "x"
model = "m"
input = 1.0
cache_read = 0.1
output = 2.0
source = "s"
checked = "d"
[[model]]
provider = "x"
model = "m"
input = 1.0
cache_read = 0.1
output = 2.0
source = "s"
checked = "d"
"#;
        assert!(Pricing::parse(text).is_err());
    }

    #[test]
    fn long_context_prices_need_a_standard_price_they_exceed() {
        let row = |standard: &str, tier: &str| {
            format!(
                "schema = \"agentprof.pricing/1\"\n[[model]]\nprovider = \"x\"\nmodel = \"m\"\ninput = 1.0\ncache_read = 0.1\noutput = 2.0\n{standard}source = \"s\"\nchecked = \"d\"\n[model.long_context]\nthreshold_tokens = 10\ninput = 2.0\ncache_read = 0.2\noutput = 3.0\n{tier}"
            )
        };
        assert!(Pricing::parse(&row("", "")).is_ok());
        assert!(Pricing::parse(&row("cache_write_5m = 1.25\n", "cache_write_5m = 2.5\n")).is_ok());
        // A tier price for a class the standard rates leave unpriced.
        assert!(Pricing::parse(&row("", "cache_write_5m = 2.5\n")).is_err());
        // A tier price below the standard one would make the surcharge negative.
        assert!(Pricing::parse(&row("cache_write_5m = 1.25\n", "cache_write_5m = 1.0\n")).is_err());
        assert!(Pricing::parse(&row("cache_write_5m = -1.0\n", "")).is_err());
    }
}
