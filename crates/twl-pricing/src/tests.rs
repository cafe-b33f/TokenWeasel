//! Unit tests for [`crate::Pricing`] cost computation and [`crate::load`].
//! Tests verify the core cost formula, quantization suffix stripping, and
//! the defensive design that guarantees all cost fields stay finite.

use std::collections::HashMap;

use crate::{ModelPrice, Pricing};

#[test]
fn pricing_rejects_unknown_top_level_field() {
    let err = serde_json::from_str::<Pricing>(
        r#"{"currency":"USD","unit":1000000,"models":{},"currncy":"EUR"}"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("currncy"));
}

#[test]
fn pricing_rejects_unknown_model_price_field() {
    let err =
        serde_json::from_str::<Pricing>(r#"{"models":{"model-a":{"input":1.0,"ouput":2.0}}}"#)
            .unwrap_err();
    assert!(err.to_string().contains("ouput"));
}

/// Build a `Pricing` struct for a single model at the standard unit.
fn pricing_with(model: &str, price: ModelPrice) -> Pricing {
    let mut models = HashMap::new();
    models.insert(model.to_string(), price);
    Pricing {
        currency: "USD".to_string(),
        unit: 1_000_000.0,
        models,
    }
}

#[test]
fn basic_input_and_output_cost() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: 2.0,
            input_cached: 0.5,
        },
    );
    // One unit (1e6) of each, no cache: 1.0 input + 2.0 output.
    let c = p.cost("m", 1_000_000, 1_000_000, 0);
    assert_eq!(c.input, 1.0);
    assert_eq!(c.output, 2.0);
    assert_eq!(c.input_cached, 0.0);
    assert_eq!(c.total, 3.0);
}

#[test]
fn cached_tokens_are_a_discounted_subset_of_prompt() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: 0.0,
            input_cached: 0.25,
        },
    );
    // 1e6 input tokens, 400k of them cached: 600k billed at input, 400k cached.
    let c = p.cost("m", 1_000_000, 0, 400_000);
    assert!((c.input - 0.6).abs() < 1e-9);
    assert!((c.input_cached - 0.1).abs() < 1e-9);
    assert!((c.total - 0.7).abs() < 1e-9);
}

#[test]
fn unknown_model_is_free() {
    let p = pricing_with("known", ModelPrice::default());
    let c = p.cost("other", 1_000_000, 1_000_000, 0);
    assert_eq!(c.total, 0.0);
}

#[test]
fn negative_or_over_cached_counts_never_go_negative() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: 1.0,
            input_cached: 1.0,
        },
    );
    // cached > prompt would make billable_input negative; it's clamped to 0.
    let c = p.cost("m", 100, -5, 1_000);
    assert_eq!(c.input, 0.0);
    assert_eq!(c.output, 0.0);
    assert!(c.input_cached >= 0.0);
}

#[test]
fn respects_custom_unit() {
    let mut p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: 0.0,
            input_cached: 0.0,
        },
    );
    p.unit = 1_000.0; // price quoted per 1k tokens
    let c = p.cost("m", 1_000, 0, 0);
    assert_eq!(c.input, 1.0);
}

#[test]
fn quantization_suffix_is_stripped_for_pricing() {
    // Base model has a price; quantized variants look it up automatically.
    let p = pricing_with(
        "unsloth/Qwen3.6-35B-A3B-MTP-GGUF",
        ModelPrice {
            input: 0.14,
            output: 0.9,
            input_cached: 0.14,
        },
    );

    // Each quantized variant should resolve to the base model price.
    let variants = [
        "unsloth/Qwen3.6-35B-A3B-MTP-GGUF:Q3_K_XL",
        "unsloth/Qwen3.6-35B-A3B-MTP-GGUF:Q4_K_XL",
        "unsloth/Qwen3.6-35B-A3B-MTP-GGUF:UD-Q4_K_XL",
    ];

    for variant in variants {
        let c = p.cost(variant, 1_000_000, 1_000_000, 0);
        assert_eq!(c.input, 0.14, "input price mismatch for {variant}");
        assert_eq!(c.output, 0.9, "output price mismatch for {variant}");
    }

    // Exact base model name still works.
    let c_base = p.cost("unsloth/Qwen3.6-35B-A3B-MTP-GGUF", 1_000_000, 1_000_000, 0);
    assert_eq!(c_base.input, 0.14);
    assert_eq!(c_base.output, 0.9);

    // Unknown model remains free.
    let c_unknown = p.cost("unknown-model", 1_000_000, 0, 0);
    assert_eq!(c_unknown.total, 0.0);
}

#[test]
fn nan_input_rate_normalizes_to_zero() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: f64::NAN,
            output: 2.0,
            input_cached: 0.5,
        },
    );
    let c = p.cost("m", 1_000_000, 1_000_000, 0);
    assert!(c.input.is_finite());
    assert!(c.input >= 0.0);
    assert!(c.output.is_finite() && c.output >= 0.0);
    assert!(c.input_cached.is_finite() && c.input_cached >= 0.0);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn nan_output_rate_normalizes_to_zero() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: f64::NAN,
            input_cached: 0.5,
        },
    );
    let c = p.cost("m", 1_000_000, 1_000_000, 0);
    assert!(c.output.is_finite() && c.output >= 0.0);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn nan_cached_rate_normalizes_to_zero() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: 2.0,
            input_cached: f64::NAN,
        },
    );
    let c = p.cost("m", 1_000_000, 1_000_000, 500_000);
    assert!(c.input_cached.is_finite() && c.input_cached >= 0.0);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn inf_input_rate_normalizes_to_zero() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: f64::INFINITY,
            output: 2.0,
            input_cached: 0.5,
        },
    );
    let c = p.cost("m", 1_000_000, 1_000_000, 0);
    assert!(c.input.is_finite() && c.input >= 0.0);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn inf_output_rate_normalizes_to_zero() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: f64::INFINITY,
            input_cached: 0.5,
        },
    );
    let c = p.cost("m", 1_000_000, 1_000_000, 0);
    assert!(c.output.is_finite() && c.output >= 0.0);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn negative_input_rate_normalizes_to_zero() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: -5.0,
            output: 2.0,
            input_cached: 0.5,
        },
    );
    let c = p.cost("m", 1_000_000, 1_000_000, 0);
    assert!(c.input.is_finite() && c.input >= 0.0);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn negative_output_rate_normalizes_to_zero() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: -5.0,
            input_cached: 0.5,
        },
    );
    let c = p.cost("m", 1_000_000, 1_000_000, 0);
    assert!(c.output.is_finite() && c.output >= 0.0);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn negative_cached_rate_normalizes_to_zero() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: 2.0,
            input_cached: -5.0,
        },
    );
    let c = p.cost("m", 1_000_000, 1_000_000, 0);
    assert!(c.input_cached.is_finite() && c.input_cached >= 0.0);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn zero_unit_does_not_divide_by_zero() {
    let mut p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: 2.0,
            input_cached: 0.5,
        },
    );
    p.unit = 0.0;
    let c = p.cost("m", 1_000_000, 1_000_000, 0);
    assert!(c.input.is_finite() && c.input >= 0.0);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn nan_unit_does_not_divide_by_zero() {
    let mut p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: 2.0,
            input_cached: 0.5,
        },
    );
    p.unit = f64::NAN;
    let c = p.cost("m", 1_000_000, 1_000_000, 0);
    assert!(c.input.is_finite() && c.input >= 0.0);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn negative_unit_does_not_divide_by_zero() {
    let mut p = pricing_with(
        "m",
        ModelPrice {
            input: 1.0,
            output: 2.0,
            input_cached: 0.5,
        },
    );
    p.unit = -1_000.0;
    let c = p.cost("m", 1_000_000, 1_000_000, 0);
    assert!(c.input.is_finite() && c.input >= 0.0);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn all_cost_fields_are_finite_and_non_negative() {
    // Stress test: construct a Pricing with every possible bad value.
    let mut p = pricing_with(
        "m",
        ModelPrice {
            input: f64::NAN,
            output: f64::NEG_INFINITY,
            input_cached: -999.0,
        },
    );
    p.unit = 0.0;
    let c = p.cost("m", 1_000_000, 1_000_000, 500_000);

    assert!(
        c.input.is_finite() && c.input >= 0.0,
        "input must be finite and non-negative, got {}",
        c.input
    );
    assert!(
        c.input_cached.is_finite() && c.input_cached >= 0.0,
        "input_cached must be finite and non-negative, got {}",
        c.input_cached
    );
    assert!(
        c.output.is_finite() && c.output >= 0.0,
        "output must be finite and non-negative, got {}",
        c.output
    );
    assert!(
        c.total.is_finite() && c.total >= 0.0,
        "total must be finite and non-negative, got {}",
        c.total
    );
}

#[test]
fn valid_prices_unchanged_after_hardening() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 0.5,
            output: 1.5,
            input_cached: 0.25,
        },
    );
    let c = p.cost("m", 2_000_000, 1_000_000, 500_000);
    // input: 1_500_000 tokens at 0.5/1M = 0.75
    // cached: 500_000 tokens at 0.25/1M = 0.125
    // output: 1_000_000 tokens at 1.5/1M = 1.5
    assert!((c.input - 0.75).abs() < 1e-9);
    assert!((c.input_cached - 0.125).abs() < 1e-9);
    assert!((c.output - 1.5).abs() < 1e-9);
    assert!((c.total - 2.375).abs() < 1e-9);
    assert!(c.total.is_finite() && c.total >= 0.0);
}

#[test]
fn f64_max_rates_overflow_to_infinity_and_return_zero() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: f64::MAX,
            output: f64::MAX,
            input_cached: f64::MAX,
        },
    );
    let c = p.cost("m", 100_000_000, 100_000_000, 50_000_000);
    assert!(
        c.input.is_finite() && c.input >= 0.0,
        "input must be finite and non-negative, got {}",
        c.input
    );
    assert!(
        c.input_cached.is_finite() && c.input_cached >= 0.0,
        "input_cached must be finite and non-negative, got {}",
        c.input_cached
    );
    assert!(
        c.output.is_finite() && c.output >= 0.0,
        "output must be finite and non-negative, got {}",
        c.output
    );
    assert!(
        c.total.is_finite() && c.total >= 0.0,
        "total must be finite and non-negative, got {}",
        c.total
    );
}

#[test]
fn i64_max_input_no_signed_subtract_overflow() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 0.1,
            output: 0.2,
            input_cached: 0.05,
        },
    );
    let c = p.cost("m", i64::MAX, 1_000_000, i64::MIN);
    assert!(
        c.input.is_finite() && c.input >= 0.0,
        "input must be finite and non-negative, got {}",
        c.input
    );
    assert!(
        c.total.is_finite() && c.total >= 0.0,
        "total must be finite and non-negative, got {}",
        c.total
    );
}

#[test]
fn i64_max_all_fields_no_overflow() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 0.1,
            output: 0.2,
            input_cached: 0.05,
        },
    );
    let c = p.cost("m", i64::MAX, i64::MAX, i64::MAX);
    assert!(
        c.input.is_finite() && c.input >= 0.0,
        "input must be finite and non-negative, got {}",
        c.input
    );
    assert!(
        c.input_cached.is_finite() && c.input_cached >= 0.0,
        "input_cached must be finite and non-negative, got {}",
        c.input_cached
    );
    assert!(
        c.output.is_finite() && c.output >= 0.0,
        "output must be finite and non-negative, got {}",
        c.output
    );
    assert!(
        c.total.is_finite() && c.total >= 0.0,
        "total must be finite and non-negative, got {}",
        c.total
    );
}

#[test]
fn i64_min_output_no_overflow() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: 0.1,
            output: 0.2,
            input_cached: 0.05,
        },
    );
    let c = p.cost("m", 1_000_000, i64::MIN, 0);
    assert_eq!(c.output, 0.0);
    assert!(
        c.total.is_finite() && c.total >= 0.0,
        "total must be finite and non-negative, got {}",
        c.total
    );
}

#[test]
fn max_rate_with_max_i64_tokens_never_panics() {
    let p = pricing_with(
        "m",
        ModelPrice {
            input: f64::MAX,
            output: f64::MAX,
            input_cached: f64::MAX,
        },
    );
    let _c = p.cost("m", i64::MAX, i64::MAX, i64::MAX);
}
