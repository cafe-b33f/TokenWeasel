//! Per-model pricing configuration and cost computation for the LLM proxy.
//!
//! Loads a JSON pricing table and computes token costs from usage records.
//! Every numeric value is sanitized to guarantee finite, non-negative results
//! regardless of input quality.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Prices are per [`Pricing::unit`] tokens; missing JSON keys default to
/// zero (free model).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    /// Rate charged for non-cached input tokens.
    #[serde(default)]
    pub input: f64,
    /// Rate charged for generated output tokens.
    #[serde(default)]
    pub output: f64,
    /// Rate for cached input tokens (a discounted subset of `input`).
    #[serde(default)]
    pub input_cached: f64,
}

/// Cost split by token class; every field is finite and non-negative.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Cost {
    /// Cost attributable to non-cached input tokens.
    pub input: f64,
    /// Cost attributable to cached input tokens.
    pub input_cached: f64,
    /// Cost attributable to generated output tokens.
    pub output: f64,
    /// Sum of the input, cached-input, and output costs.
    pub total: f64,
}

/// A missing or malformed pricing file falls back to all-zero costs rather
/// than failing startup.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    /// Currency label shown in the dashboard (purely cosmetic).
    #[serde(default = "default_currency")]
    pub currency: String,
    /// Number of tokens each price entry is quoted for.
    #[serde(default = "default_unit")]
    pub unit: f64,
    #[serde(default)]
    /// Model identifiers mapped to their per-token-class rates.
    pub models: HashMap<String, ModelPrice>,
}

fn default_currency() -> String {
    "USD".to_string()
}

fn default_unit() -> f64 {
    1_000_000.0
}

fn clamp_finite_non_negative(v: f64) -> f64 {
    if v.is_finite() && v >= 0.0 {
        v
    } else {
        0.0
    }
}

fn normalize_rate(model: &str, field: &str, value: f64) -> f64 {
    if !value.is_finite() || value < 0.0 {
        tracing::warn!(
            model = model,
            field = field,
            original = value,
            "invalid model rate; normalizing to 0",
        );
        0.0
    } else {
        value
    }
}

impl Default for Pricing {
    fn default() -> Self {
        Pricing {
            currency: default_currency(),
            unit: default_unit(),
            models: HashMap::new(),
        }
    }
}

impl Pricing {
    /// Normalizes invalid units and rates into safe billing defaults.
    ///
    /// A non-finite or non-positive unit becomes one million tokens, while
    /// non-finite or negative rates become zero.
    pub fn sanitized(mut self) -> Self {
        if !self.unit.is_finite() || self.unit <= 0.0 {
            tracing::warn!(
                unit = self.unit,
                "pricing unit must be finite and > 0; falling back to 1_000_000",
            );
            self.unit = default_unit();
        }
        for (name, price) in self.models.iter_mut() {
            let norm_input = normalize_rate(name, "input", price.input);
            let norm_output = normalize_rate(name, "output", price.output);
            let norm_cached = normalize_rate(name, "input_cached", price.input_cached);
            if norm_input != price.input
                || norm_output != price.output
                || norm_cached != price.input_cached
            {
                price.input = norm_input;
                price.output = norm_output;
                price.input_cached = norm_cached;
            }
        }
        self
    }

    /// A missing file is not an error - costs are reported as zero until
    /// prices are supplied.
    pub fn load(path: &str) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Pricing>(&text) {
                Ok(p) => {
                    let p = p.sanitized();
                    tracing::info!(
                        "loaded pricing for {} model(s) from {path} (unit={}, {})",
                        p.models.len(),
                        p.unit,
                        p.currency
                    );
                    p
                }
                Err(e) => {
                    tracing::error!("failed to parse pricing file {path}: {e}; costs disabled");
                    Pricing::default()
                }
            },
            Err(_) => {
                tracing::info!("no pricing file at {path}; costs will be reported as 0");
                Pricing::default()
            }
        }
    }

    /// `cached` is a discounted subset of `input`; full-price input tokens
    /// are `input - cached`. Models without a price entry yield an all-zero
    /// cost.
    pub fn cost(&self, model: &str, input: i64, output: i64, cached: i64) -> Cost {
        let Some(p) = self.models.get(model).or_else(|| {
            // Strip quantization suffix (e.g. ":Q3_K_XL" -> "") so that
            // quantized variants fall back to the base-model price.
            let base = model.split(':').next()?;
            self.models.get(base)
        }) else {
            return Cost::default();
        };

        let unit = if self.unit.is_finite() && self.unit > 0.0 {
            self.unit
        } else {
            tracing::warn!(
                model = model,
                unit = self.unit,
                "pricing unit is non-finite or non-positive during cost(); using 1_000_000",
            );
            1_000_000.0
        };

        let rate_input = normalize_rate(model, "input", p.input);
        let rate_output = normalize_rate(model, "output", p.output);
        let rate_cached = normalize_rate(model, "input_cached", p.input_cached);

        let billable_input = input.saturating_sub(cached).max(0) as f64;
        let input_cost = clamp_finite_non_negative(billable_input * rate_input / unit);
        let input_cached_cost =
            clamp_finite_non_negative(cached.max(0) as f64 * rate_cached / unit);
        let output_cost = clamp_finite_non_negative(output.max(0) as f64 * rate_output / unit);

        let total = clamp_finite_non_negative(input_cost + input_cached_cost + output_cost);

        Cost {
            input: input_cost,
            input_cached: input_cached_cost,
            output: output_cost,
            total,
        }
    }
}

#[cfg(test)]
mod tests;
