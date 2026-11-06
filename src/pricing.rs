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
            if rates.iter().any(|r| !r.is_finite() || *r < 0.0) {
                bail!("model {} has a negative or non-finite price", m.model);
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
    /// writes of unknown TTL. `multiplier` applies documented surcharges such
    /// as Anthropic's US-only inference (1.1).
    pub fn request_cost(&self, model: &str, t: &Tokens, multiplier: f64) -> Option<f64> {
        let m = self.model(model)?;
        if t.cache_write_unknown > 0 {
            return None;
        }
        let (input, cw5, cw1h, read, output) = match &m.long_context {
            Some(lc) if t.prompt() > lc.threshold_tokens => (
                lc.input,
                lc.cache_write_5m,
                lc.cache_write_1h,
                lc.cache_read,
                lc.output,
            ),
            _ => (
                m.input,
                m.cache_write_5m,
                m.cache_write_1h,
                m.cache_read,
                m.output,
            ),
        };
        let cw5 = if t.cache_write_5m > 0 { cw5? } else { 0.0 };
        let cw1h = if t.cache_write_1h > 0 { cw1h? } else { 0.0 };
        let usd = (t.input as f64 * input
            + t.cache_write_5m as f64 * cw5
            + t.cache_write_1h as f64 * cw1h
            + t.cache_read as f64 * read
            + t.output as f64 * output)
            / 1_000_000.0;
        Some(usd * multiplier)
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
            "API-equivalent estimate: per-request tokens x {provider} standard-tier list prices in pricing.toml (checked {}); not an invoice",
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
        let c = p.request_cost("claude-opus-5-5", &t, 1.0).unwrap();
        assert!((c - (4.0 + 5.0 + 8.0 + 0.2 + 20.0)).abs() < 1e-9);
        assert!((p.request_cost("claude-opus-5-5", &t, 1.1).unwrap() - c * 1.1).abs() < 1e-9);
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
        let c = p.request_cost("claude-haiku-5-5", &t, 1.0).unwrap();
        let expect = (60_000.0 * 0.5 + 50_000.0 * 0.05 + 1_000.0 * 2.5) / 1e6;
        assert!((c - expect).abs() < 1e-12);
    }

    #[test]
    fn unknown_models_and_unpriced_classes_are_unknown() {
        let p = Pricing::load(None).unwrap();
        let t = Tokens {
            input: 1,
            ..Tokens::default()
        };
        assert_eq!(p.request_cost("gpt-unknown", &t, 1.0), None);
        let w = Tokens {
            cache_write_5m: 1,
            ..Tokens::default()
        };
        // OpenAI rows carry no 5-minute cache-write price.
        assert_eq!(p.request_cost("gpt-6.1-sol", &w, 1.0), None);
        let u = Tokens {
            cache_write_unknown: 1,
            ..Tokens::default()
        };
        assert_eq!(p.request_cost("claude-opus-5-5", &u, 1.0), None);
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
}
