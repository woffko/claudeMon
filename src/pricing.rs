//! API list prices used to turn token usage into an API-equivalent cost.
//!
//! Prices are USD per million tokens and live in `config.json` so they can
//! be updated without a release. Cache writes are billed by TTL (five-minute
//! and one-hour), cache reads at a model-specific rate.

use crate::usage::TokenBreakdown;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Date of the built-in price table.
pub const DEFAULT_PRICING_UPDATED: &str = "2026-06-24";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

impl ModelPrice {
    /// Standard multipliers: 5-minute writes 1.25x input, 1-hour writes 2x
    /// input, cache reads 0.1x input.
    fn standard(input: f64, output: f64) -> Self {
        Self {
            input,
            output,
            cache_write_5m: input * 1.25,
            cache_write_1h: input * 2.0,
            cache_read: input * 0.1,
        }
    }

    fn with_cache_read(self, cache_read: f64) -> Self {
        Self { cache_read, ..self }
    }
}

/// Cost in USD split by token category.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostBreakdown {
    pub input: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub output: f64,
}

impl CostBreakdown {
    pub fn total(&self) -> f64 {
        self.input + self.cache_write + self.cache_read + self.output
    }

    pub fn add(&mut self, other: CostBreakdown) {
        self.input += other.input;
        self.cache_write += other.cache_write;
        self.cache_read += other.cache_read;
        self.output += other.output;
    }
}

pub fn default_prices() -> BTreeMap<String, ModelPrice> {
    [
        (
            "claude-fable-5-1",
            ModelPrice::standard(10.0, 50.0).with_cache_read(0.25),
        ),
        ("claude-fable-5", ModelPrice::standard(10.0, 50.0)),
        (
            "claude-opus-5-5",
            ModelPrice::standard(4.0, 20.0).with_cache_read(0.20),
        ),
        ("claude-opus-5", ModelPrice::standard(5.0, 25.0)),
        ("claude-opus-4-8", ModelPrice::standard(5.0, 25.0)),
        ("claude-opus-4-7", ModelPrice::standard(5.0, 25.0)),
        ("claude-opus-4-6", ModelPrice::standard(5.0, 25.0)),
        ("claude-sonnet-5", ModelPrice::standard(2.0, 10.0)),
        ("claude-sonnet-4-6", ModelPrice::standard(3.0, 15.0)),
        ("claude-haiku-4-5", ModelPrice::standard(1.0, 5.0)),
    ]
    .into_iter()
    .map(|(model, price)| (model.to_string(), price))
    .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct PriceTable {
    pub prices: BTreeMap<String, ModelPrice>,
    pub updated: String,
}

impl Default for PriceTable {
    fn default() -> Self {
        Self {
            prices: default_prices(),
            updated: DEFAULT_PRICING_UPDATED.to_string(),
        }
    }
}

impl PriceTable {
    /// Price of a model id: exact match, then the id without a trailing
    /// `-YYYYMMDD` date, then the longest configured id it starts with.
    pub fn price_for(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(price) = self.prices.get(model) {
            return Some(price);
        }
        let undated = strip_date_suffix(model);
        if let Some(price) = self.prices.get(undated) {
            return Some(price);
        }
        self.prices
            .iter()
            .filter(|(id, _)| undated.starts_with(id.as_str()))
            .max_by_key(|(id, _)| id.len())
            .map(|(_, price)| price)
    }

    /// Cost of `tokens` for `model`, or `None` when the model has no price.
    pub fn cost(&self, model: &str, tokens: TokenBreakdown) -> Option<CostBreakdown> {
        let price = self.price_for(model)?;
        let per_token = |count: i64, usd_per_million: f64| count as f64 * usd_per_million / 1e6;
        let one_hour = tokens.cache_write_1h.clamp(0, tokens.cache_write);
        Some(CostBreakdown {
            input: per_token(tokens.input, price.input),
            cache_write: per_token(tokens.cache_write - one_hour, price.cache_write_5m)
                + per_token(one_hour, price.cache_write_1h),
            cache_read: per_token(tokens.cache_read, price.cache_read),
            output: per_token(tokens.output, price.output),
        })
    }
}

fn strip_date_suffix(model: &str) -> &str {
    match model.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.bytes().all(|byte| byte.is_ascii_digit()) => {
            head
        }
        _ => model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(
        input: i64,
        cache_write: i64,
        cache_write_1h: i64,
        cache_read: i64,
        output: i64,
    ) -> TokenBreakdown {
        TokenBreakdown {
            input,
            cache_write,
            cache_write_1h,
            cache_read,
            output,
        }
    }

    /// The formula matched Claude Code's own `cost-state` records to the cent
    /// when checked manually; this test pins it with synthetic counts.
    #[test]
    fn default_prices_apply_ttl_specific_cache_write_rates() {
        let table = PriceTable::default();
        // 1M input ($4) + 1M 5-minute writes ($5) + 1M 1-hour writes ($8)
        // + 10M cache reads ($2) + 100k output ($2).
        let opus = table
            .cost(
                "claude-opus-5-5",
                tokens(1_000_000, 2_000_000, 1_000_000, 10_000_000, 100_000),
            )
            .expect("opus price");
        assert!((opus.total() - 21.0).abs() < 1e-9, "{opus:?}");
        // 1M input ($1) + 1M 5-minute writes ($1.25) + 10M reads ($1) + 200k output ($1).
        let haiku = table
            .cost(
                "claude-haiku-4-5-20251001",
                tokens(1_000_000, 1_000_000, 0, 10_000_000, 200_000),
            )
            .expect("haiku price");
        assert!((haiku.total() - 4.25).abs() < 1e-9, "{haiku:?}");
    }

    #[test]
    fn price_lookup_handles_dates_and_prefixes() {
        let table = PriceTable::default();
        assert!(table.price_for("claude-haiku-4-5-20251001").is_some());
        assert_eq!(
            table
                .price_for("claude-opus-5-5")
                .map(|price| price.cache_read),
            Some(0.20)
        );
        assert_eq!(
            table.price_for("claude-opus-5-9").map(|price| price.input),
            Some(5.0),
            "falls back to the longest matching family prefix"
        );
        assert!(table.price_for("gpt-test").is_none());
        assert!(table.cost("unknown-model", tokens(1, 0, 0, 0, 1)).is_none());
    }
}
