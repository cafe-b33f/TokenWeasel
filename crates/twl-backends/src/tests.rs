//! Tests for [`crate::Backend`] and [`crate::BackendRegistry`].
//!
//! Covers construction invariants, field population from config values,
//! model-based routing, and catalog updates via `set_backend_models`.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Barrier, Mutex};

use crate::energy::EnergySink;
use crate::{Backend, BackendParams, BackendRegistry, RouteError, RouteRequest, RoutingMode};
use rustc_hash::FxHasher;
use twl_provider::{ModelEntry, ModelKind, ProviderKind};

#[test]
fn backend_debug_views_redact_direct_and_recursive_authentication_secrets() {
    const API_KEY: &str = "M8_BACKEND_API_KEY_SENTINEL";
    const HEADER_VALUE: &str = "M8_EXTRA_HEADER_VALUE_SENTINEL";
    const BEARER_VALUE: &str = "Bearer M8_BACKEND_API_KEY_SENTINEL";

    let make_params = || BackendParams {
        upstream: "http://localhost:8080".to_string(),
        provider: ProviderKind::OpenRouter,
        api_key: Some(API_KEY.to_string()),
        extra_headers: vec![("x-private-token".to_string(), HEADER_VALUE.to_string())],
        gpu_watts: 100.0,
        models_poll_secs: Some(30),
        model_types: Default::default(),
        model_filter: None,
    };

    let params_debug = format!("{:?}", make_params());
    for sentinel in [API_KEY, HEADER_VALUE, BEARER_VALUE] {
        assert!(!params_debug.contains(sentinel), "{params_debug}");
    }
    assert!(params_debug.contains("[REDACTED]"), "{params_debug}");

    let sink = std::sync::Arc::new(TestSink::new());
    let backend = Backend::new(make_params(), sink.clone());
    let backend_debug = format!("{backend:?}");
    for sentinel in [API_KEY, HEADER_VALUE, BEARER_VALUE] {
        assert!(!backend_debug.contains(sentinel), "{backend_debug}");
    }
    assert!(backend_debug.contains("[REDACTED]"), "{backend_debug}");

    let registry = BackendRegistry::new(
        vec![Backend::new(make_params(), sink)],
        vec![vec!["model-a".to_string()]],
        RoutingMode::Catalog,
    );
    let registry_debug = format!("{registry:?}");
    for sentinel in [API_KEY, HEADER_VALUE, BEARER_VALUE] {
        assert!(!registry_debug.contains(sentinel), "{registry_debug}");
    }
    assert!(registry_debug.contains("[REDACTED]"), "{registry_debug}");
}

/// With a `Some` prefix hash and no load, the same backend is returned
/// across many calls (affinity stability).
#[test]
fn prefix_affinity_stable_across_repeated_calls() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![vec!["model".to_string()], vec!["model".to_string()]],
        RoutingMode::Catalog,
    );

    let prefix: u64 = 0xdeadbeef;
    let mut prev = None;
    for _ in 0..50 {
        let (idx, _) = registry
            .route(RouteRequest {
                model: Some("model"),
                prefix_hash: Some(prefix),
            })
            .unwrap();
        match prev {
            None => prev = Some(idx),
            Some(p) => assert_eq!(p, idx, "prefix affinity changed across calls"),
        }
    }
}

/// When the preferred backend is overloaded (many in-flight requests),
/// the prefix spills to a different candidate.
#[test]
fn prefix_spills_on_bounded_load() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![vec!["model".to_string()], vec!["model".to_string()]],
        RoutingMode::Catalog,
    );

    let prefix: u64 = 0xcafebabe;

    // Discover the preferred backend (no load).
    let (preferred_idx, _) = registry
        .route(RouteRequest {
            model: Some("model"),
            prefix_hash: Some(prefix),
        })
        .unwrap();

    // Load up the preferred backend with 20 in-flight requests.
    let guards: Vec<_> = (0..20)
        .map(|_| registry.begin_load(preferred_idx))
        .collect();

    // The route should now spill to a different backend.
    let (spill_idx, _) = registry
        .route(RouteRequest {
            model: Some("model"),
            prefix_hash: Some(prefix),
        })
        .unwrap();

    assert_ne!(
        spill_idx, preferred_idx,
        "expected spill to a different backend under high load, \
         preferred={preferred_idx}, spill={spill_idx}"
    );

    drop(guards);
}

/// Two different prefix values are each individually stable.
#[test]
fn two_prefixes_each_stable() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![vec!["model".to_string()], vec!["model".to_string()]],
        RoutingMode::Catalog,
    );

    let p1: u64 = 111;
    let p2: u64 = 222;

    // Determine which backend each prefix maps to.
    let idx1 = registry
        .route(RouteRequest {
            model: Some("model"),
            prefix_hash: Some(p1),
        })
        .unwrap()
        .0;
    let idx2 = registry
        .route(RouteRequest {
            model: Some("model"),
            prefix_hash: Some(p2),
        })
        .unwrap()
        .0;

    // They may or may not be the same (depends on hash values), but each
    // should be individually stable across repeated calls.
    for _ in 0..30 {
        let got1 = registry
            .route(RouteRequest {
                model: Some("model"),
                prefix_hash: Some(p1),
            })
            .unwrap()
            .0;
        assert_eq!(got1, idx1, "prefix 1 not stable");

        let got2 = registry
            .route(RouteRequest {
                model: Some("model"),
                prefix_hash: Some(p2),
            })
            .unwrap()
            .0;
        assert_eq!(got2, idx2, "prefix 2 not stable");
    }
}

/// Collects energy records in a thread-safe Vec.
struct TestSink {
    records: Mutex<Vec<(f64, f64)>>,
}

impl TestSink {
    fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
        }
    }
}

impl EnergySink for TestSink {
    fn record_energy(&self, elapsed_secs: f64, watts: f64) {
        self.records.lock().unwrap().push((elapsed_secs, watts));
    }
}

#[test]
fn backend_fields_populated_from_config() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params = BackendParams {
        upstream: "http://localhost:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: Some("sk-test".to_string()),
        extra_headers: vec![(String::from("X-Custom"), String::from("val"))],
        gpu_watts: 250.0,
        models_poll_secs: Some(30),
        model_types: Default::default(),
        model_filter: None,
    };
    let backend = Backend::new(params, sink.clone());
    assert_eq!(backend.upstream, "http://localhost:8080");
    assert_eq!(backend.gpu_watts, 250.0);
    assert_eq!(backend.provider, twl_provider::ProviderKind::LlamaCpp);
    assert_eq!(backend.models_poll_secs, Some(30));
    assert_eq!(backend.auth_headers.len(), 2);
    assert_eq!(backend.auth_headers[0].0, "authorization");
    assert_eq!(backend.auth_headers[0].1, "Bearer sk-test");
}

#[test]
fn backend_for_with_models_routes_correctly() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![vec!["foo".to_string()], vec!["bar".to_string()]],
        RoutingMode::Catalog,
    );

    // "foo" routes to backend 0.
    let backend = registry.backend_for(Some("foo")).unwrap();
    assert_eq!(backend.upstream, "http://host0:8080");

    // "bar" routes to backend 1.
    let backend = registry.backend_for(Some("bar")).unwrap();
    assert_eq!(backend.upstream, "http://host1:8080");

    // None returns MissingModel error.
    match registry.backend_for(None) {
        Err(RouteError::MissingModel) => {}
        Ok(_) => panic!("expected Err(RouteError::MissingModel)"),
        Err(e) => panic!("expected MissingModel, got {e}"),
    }
}

#[test]
fn backend_for_unknown_model_returns_error() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0],
        vec![vec!["foo".to_string()]],
        RoutingMode::Catalog,
    );

    // Unknown model returns RouteError::UnknownModel.
    match registry.backend_for(Some("unknown")) {
        Err(RouteError::UnknownModel(m)) => assert_eq!(m, "unknown"),
        Ok(_) => panic!("expected Err(RouteError::UnknownModel)"),
        Err(RouteError::MissingModel) => panic!("unexpected MissingModel"),
    }
}

#[test]
fn clearing_last_catalog_model_does_not_enable_flat_passthrough() {
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = Backend::new(
        BackendParams {
            upstream: "http://host0:8080".to_string(),
            provider: ProviderKind::LlamaCpp,
            api_key: None,
            extra_headers: Vec::new(),
            gpu_watts: 100.0,
            models_poll_secs: Some(30),
            model_types: Default::default(),
            model_filter: None,
        },
        sink,
    );
    let registry = BackendRegistry::new(
        vec![backend],
        vec![vec!["only-model".to_string()]],
        RoutingMode::Catalog,
    );

    assert!(registry.backend_for(Some("only-model")).is_ok());
    registry.set_backend_models(0, Vec::new());

    assert!(matches!(
        registry.backend_for(Some("only-model")),
        Err(RouteError::UnknownModel(model)) if model == "only-model"
    ));
    assert!(matches!(
        registry.backend_for(Some("unadvertised-model")),
        Err(RouteError::UnknownModel(model)) if model == "unadvertised-model"
    ));
}

#[test]
fn flat_passthrough_remains_catch_all_with_empty_catalog() {
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = Backend::new(
        BackendParams {
            upstream: "http://host0:8080".to_string(),
            provider: ProviderKind::LlamaCpp,
            api_key: None,
            extra_headers: Vec::new(),
            gpu_watts: 100.0,
            models_poll_secs: Some(30),
            model_types: Default::default(),
            model_filter: None,
        },
        sink,
    );
    let registry = BackendRegistry::new(
        vec![backend],
        vec![Vec::new()],
        RoutingMode::FlatPassthrough,
    );

    assert!(registry.backend_for(Some("any-model-name")).is_ok());
}

#[test]
fn empty_backends_panics_at_construction() {
    // No backends at all - should panic.
    assert!(std::panic::catch_unwind(|| {
        BackendRegistry::new(vec![], vec![], RoutingMode::Catalog)
    })
    .is_err());
}

#[test]
fn set_backend_models_updates_routing() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![Vec::new(), Vec::new()], // no models initially
        RoutingMode::Catalog,
    );

    // Initially, only None routes (missing model error).
    assert!(matches!(
        registry.backend_for(Some("foo")),
        Err(RouteError::UnknownModel(_))
    ));
    assert!(matches!(
        registry.backend_for(Some("bar")),
        Err(RouteError::UnknownModel(_))
    ));

    // Update backend 0 with "foo" and backend 1 with "bar".
    registry.set_backend_models(
        0,
        vec![twl_provider::ModelEntry {
            id: "foo".to_string(),
            raw: serde_json::json!({ "id": "foo", "object": "model" }),
        }],
    );
    registry.set_backend_models(
        1,
        vec![twl_provider::ModelEntry {
            id: "bar".to_string(),
            raw: serde_json::json!({ "id": "bar", "object": "model" }),
        }],
    );

    // Now both models route correctly.
    let b0 = registry.backend_for(Some("foo")).unwrap();
    assert_eq!(b0.upstream, "http://host0:8080");

    let b1 = registry.backend_for(Some("bar")).unwrap();
    assert_eq!(b1.upstream, "http://host1:8080");
}

#[test]
fn duplicate_static_models_are_single_candidates_per_backend() {
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = |upstream: &str| {
        Backend::new(
            BackendParams {
                upstream: upstream.to_string(),
                provider: ProviderKind::LlamaCpp,
                api_key: None,
                extra_headers: Vec::new(),
                gpu_watts: 100.0,
                models_poll_secs: None,
                model_types: Default::default(),
                model_filter: None,
            },
            sink.clone(),
        )
    };
    let registry = BackendRegistry::new(
        vec![backend("http://host0:8080"), backend("http://host1:8080")],
        vec![
            vec!["shared".to_string(), "shared".to_string()],
            vec!["shared".to_string()],
        ],
        RoutingMode::Catalog,
    );

    assert_eq!(registry.model_entries().len(), 1);
    assert_eq!(
        registry
            .route_candidates(RouteRequest {
                model: Some("shared"),
                prefix_hash: None,
            })
            .unwrap()
            .as_slice(),
        &[0, 1],
        "the duplicate must not add a second backend-0 failover attempt"
    );

    let _backend0_busy = registry.begin_load(0);
    assert_eq!(
        registry
            .route_candidates(RouteRequest {
                model: Some("shared"),
                prefix_hash: None,
            })
            .unwrap()
            .as_slice(),
        &[1, 0],
        "load balancing must fail over directly to the distinct backend"
    );
}

#[test]
fn duplicate_watchdog_models_are_single_candidates_per_backend() {
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = |upstream: &str| {
        Backend::new(
            BackendParams {
                upstream: upstream.to_string(),
                provider: ProviderKind::LlamaCpp,
                api_key: None,
                extra_headers: Vec::new(),
                gpu_watts: 100.0,
                models_poll_secs: Some(30),
                model_types: Default::default(),
                model_filter: None,
            },
            sink.clone(),
        )
    };
    let registry = BackendRegistry::new(
        vec![backend("http://host0:8080"), backend("http://host1:8080")],
        vec![Vec::new(), Vec::new()],
        RoutingMode::Catalog,
    );
    let model_entry = |source: &str| twl_provider::ModelEntry {
        id: "shared".to_string(),
        raw: serde_json::json!({ "id": "shared", "source": source }),
    };

    registry.set_backend_models(0, vec![model_entry("first"), model_entry("duplicate")]);
    registry.set_backend_models(1, vec![model_entry("other-backend")]);

    let entries = registry.model_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["source"], "first", "the first duplicate wins");
    assert_eq!(
        registry
            .route_candidates(RouteRequest {
                model: Some("shared"),
                prefix_hash: None,
            })
            .unwrap()
            .as_slice(),
        &[0, 1],
        "watchdog duplicates must not repeat a failover candidate"
    );
}

#[test]
fn replicated_model_is_listed_once_using_first_backend_metadata() {
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = |idx| {
        Backend::new(
            BackendParams {
                upstream: format!("http://host{idx}:8080"),
                provider: ProviderKind::LlamaCpp,
                api_key: None,
                extra_headers: Vec::new(),
                gpu_watts: 0.0,
                models_poll_secs: Some(30),
                model_types: Default::default(),
                model_filter: None,
            },
            sink.clone(),
        )
    };
    let registry = BackendRegistry::new(
        vec![backend(0), backend(1)],
        vec![Vec::new(), Vec::new()],
        RoutingMode::Catalog,
    );
    let entry = |backend: usize| twl_provider::ModelEntry {
        id: "replicated".to_string(),
        raw: serde_json::json!({ "id": "replicated", "backend": backend }),
    };
    registry.set_backend_models(0, vec![entry(0)]);
    registry.set_backend_models(1, vec![entry(1)]);

    let listed = registry.model_entries();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], "replicated");
    assert_eq!(listed[0]["backend"], 0, "first configured backend wins");
    assert_eq!(
        registry
            .route_candidates(RouteRequest {
                model: Some("replicated"),
                prefix_hash: None,
            })
            .unwrap()
            .as_slice(),
        &[0, 1],
        "deduplicating the listing must not remove routing replicas"
    );
}

#[test]
fn static_models_have_normalized_raw() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: ProviderKind::Ollama,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![vec!["model-a".to_string()], vec!["model-b".to_string()]],
        RoutingMode::Catalog,
    );

    // Verify raw entries from static models are normalized
    let entries = registry.model_entries();
    assert_eq!(entries.len(), 2);

    // LlamaCpp backend: owned_by should be "llamacpp"
    let raw0 = entries[0].as_object().unwrap();
    assert_eq!(raw0["id"].as_str().unwrap(), "model-a");
    assert_eq!(raw0["object"].as_str().unwrap(), "model");
    assert_eq!(raw0["owned_by"].as_str().unwrap(), "llamacpp");
    assert_eq!(
        raw0.keys().collect::<Vec<_>>(),
        vec!["id", "object", "owned_by"]
    );

    // Ollama backend: owned_by should be "ollama"
    let raw1 = entries[1].as_object().unwrap();
    assert_eq!(raw1["id"].as_str().unwrap(), "model-b");
    assert_eq!(raw1["object"].as_str().unwrap(), "model");
    assert_eq!(raw1["owned_by"].as_str().unwrap(), "ollama");
    assert_eq!(
        raw1.keys().collect::<Vec<_>>(),
        vec!["id", "object", "owned_by"]
    );
}

/// Regression test: two concurrent `set_backend_models` calls on different
/// backend indices must not lose one another.
///
/// A non-atomic load-then-store version would produce a lost update where
/// one thread's update overwrites the other's. The RCU-based `ArcSwap`
/// implementation applies each update atomically, so both models survive.
#[test]
fn concurrent_set_backend_models_no_lost_update() {
    let sink = std::sync::Arc::new(TestSink::new());
    let foo_entry = twl_provider::ModelEntry {
        id: "foo".to_string(),
        raw: serde_json::json!({ "id": "foo", "object": "model" }),
    };
    let bar_entry = twl_provider::ModelEntry {
        id: "bar".to_string(),
        raw: serde_json::json!({ "id": "bar", "object": "model" }),
    };

    for trial in 0..500 {
        // Build a fresh BackendRegistry with two backends, both starting empty.
        let params0 = BackendParams {
            upstream: "http://host0:8080".to_string(),
            provider: twl_provider::ProviderKind::LlamaCpp,
            api_key: None,
            extra_headers: Vec::new(),
            gpu_watts: 100.0,
            models_poll_secs: None,
            model_types: Default::default(),
            model_filter: None,
        };
        let b0 = Backend::new(params0, sink.clone());
        let params1 = BackendParams {
            upstream: "http://host1:8080".to_string(),
            provider: twl_provider::ProviderKind::LlamaCpp,
            api_key: None,
            extra_headers: Vec::new(),
            gpu_watts: 200.0,
            models_poll_secs: None,
            model_types: Default::default(),
            model_filter: None,
        };
        let b1 = Backend::new(params1, sink.clone());

        let registry = BackendRegistry::new(
            vec![b0, b1],
            vec![Vec::new(), Vec::new()],
            RoutingMode::Catalog,
        );

        let barrier = Barrier::new(2);

        std::thread::scope(|s| {
            s.spawn(|| {
                barrier.wait();
                registry.set_backend_models(0, vec![foo_entry.clone()]);
            });
            s.spawn(|| {
                barrier.wait();
                registry.set_backend_models(1, vec![bar_entry.clone()]);
            });
        });

        let b = registry.backend_for(Some("foo")).unwrap_or_else(|_| {
            panic!("trial {trial}: model 'foo' was lost (not found)");
        });
        assert_eq!(
            b.upstream, "http://host0:8080",
            "trial {trial}: model 'foo' routed to wrong backend"
        );

        let b = registry.backend_for(Some("bar")).unwrap_or_else(|_| {
            panic!("trial {trial}: model 'bar' was lost (not found)");
        });
        assert_eq!(
            b.upstream, "http://host1:8080",
            "trial {trial}: model 'bar' routed to wrong backend"
        );
    }
}

#[test]
fn collision_first_wins() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    // Both backends have "foo" - backend 0 should win.
    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![
            vec!["foo".to_string(), "baz".to_string()],
            vec!["foo".to_string(), "bar".to_string()],
        ],
        RoutingMode::Catalog,
    );

    let backend = registry.backend_for(Some("foo")).unwrap();
    assert_eq!(backend.upstream, "http://host0:8080");

    // "baz" only in backend 0, "bar" only in backend 1.
    let backend = registry.backend_for(Some("baz")).unwrap();
    assert_eq!(backend.upstream, "http://host0:8080");

    let backend = registry.backend_for(Some("bar")).unwrap();
    assert_eq!(backend.upstream, "http://host1:8080");
}

#[test]
fn removed_model_stops_routing_after_update() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0],
        vec![vec!["foo".to_string(), "bar".to_string()]],
        RoutingMode::Catalog,
    );

    // Initially both models are routable.
    assert!(registry.backend_for(Some("foo")).is_ok());
    assert!(registry.backend_for(Some("bar")).is_ok());

    // Update: remove "foo", keep "bar".
    registry.set_backend_models(
        0,
        vec![twl_provider::ModelEntry {
            id: "bar".to_string(),
            raw: serde_json::json!({ "id": "bar", "object": "model", "owned_by": "llamacpp" }),
        }],
    );

    // "foo" is no longer routable; "bar" still is.
    match registry.backend_for(Some("foo")) {
        Err(RouteError::UnknownModel(m)) => assert_eq!(m, "foo"),
        Ok(_) => panic!("expected Err(RouteError::UnknownModel)"),
        Err(RouteError::MissingModel) => panic!("unexpected MissingModel"),
    }

    assert!(registry.backend_for(Some("bar")).is_ok());
}

#[test]
fn least_loaded_balancing_with_tiebreak() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![
            vec!["shared-model".to_string()],
            vec!["shared-model".to_string()],
        ],
        RoutingMode::Catalog,
    );

    // No inflight load -> index 0 (host0) by tie-break.
    let (idx, backend) = registry
        .route(RouteRequest {
            model: Some("shared-model"),
            prefix_hash: None,
        })
        .unwrap();
    assert_eq!(idx, 0);
    assert_eq!(backend.upstream, "http://host0:8080");

    // Acquire load on backend 0 only -> routes to backend 1 (host1).
    let guard0 = registry.begin_load(0);
    let (idx, backend) = registry
        .route(RouteRequest {
            model: Some("shared-model"),
            prefix_hash: None,
        })
        .unwrap();
    assert_eq!(idx, 1);
    assert_eq!(backend.upstream, "http://host1:8080");

    // Acquire load on backend 1 too; both carry load 1, tie-break to 0.
    let guard1 = registry.begin_load(1);
    let (idx, backend) = registry
        .route(RouteRequest {
            model: Some("shared-model"),
            prefix_hash: None,
        })
        .unwrap();
    assert_eq!(idx, 0);
    assert_eq!(backend.upstream, "http://host0:8080");

    // Keep guards alive until here.
    drop(guard0);
    drop(guard1);
}

#[test]
fn exclusive_model_always_routes_to_its_only_backend() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![vec![], vec!["exclusive".to_string()]],
        RoutingMode::Catalog,
    );

    // Model only on backend 1, always returns it.
    let (idx, backend) = registry
        .route(RouteRequest {
            model: Some("exclusive"),
            prefix_hash: None,
        })
        .unwrap();
    assert_eq!(idx, 1);
    assert_eq!(backend.upstream, "http://host1:8080");

    // Even after adding load on backend 0, still returns backend 1.
    let g0 = registry.begin_load(0);
    let g1 = registry.begin_load(0);
    let g2 = registry.begin_load(0);
    let (idx, backend) = registry
        .route(RouteRequest {
            model: Some("exclusive"),
            prefix_hash: None,
        })
        .unwrap();
    assert_eq!(idx, 1);
    assert_eq!(backend.upstream, "http://host1:8080");

    drop(g0);
    drop(g1);
    drop(g2);
}

/// With RoundRobin strategy, two backends sharing a model rotate across
/// four calls so each backend is returned exactly twice and consecutive
/// calls alternate.
#[test]
fn round_robin_rotates_across_backends() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![vec!["shared".to_string()], vec!["shared".to_string()]],
        RoutingMode::Catalog,
    )
    .with_load_balancing(crate::LbStrategy::RoundRobin, crate::OVERLOAD_FACTOR);

    let mut results = Vec::new();
    for _ in 0..4 {
        let (idx, _) = registry
            .route(RouteRequest {
                model: Some("shared"),
                prefix_hash: None,
            })
            .unwrap();
        results.push(idx);
    }

    // Each backend returned exactly twice.
    assert_eq!(results.iter().filter(|&&i| i == 0).count(), 2);
    assert_eq!(results.iter().filter(|&&i| i == 1).count(), 2);

    // Consecutive calls alternate.
    assert_eq!(results[0], 0);
    assert_eq!(results[1], 1);
    assert_eq!(results[2], 0);
    assert_eq!(results[3], 1);
}

/// Models that landed on the same one of the old 64 counter stripes must
/// advance independently. Alternating calls pinned both models to backend 0
/// with the former shared counter because each model observed only even values.
#[test]
fn round_robin_is_independent_for_models_colliding_on_old_stripes() {
    fn old_stripe(model: &str) -> usize {
        let mut hasher = FxHasher::default();
        model.hash(&mut hasher);
        hasher.finish() as usize % 64
    }

    let mut seen = vec![None; 64];
    let (model_a, model_b) = (0..=64)
        .find_map(|n| {
            let model = format!("collision-model-{n}");
            let stripe = old_stripe(&model);
            seen[stripe]
                .clone()
                .map(|previous| (previous, model.clone()))
                .or_else(|| {
                    seen[stripe] = Some(model);
                    None
                })
        })
        .expect("65 model IDs must collide across 64 old rotation stripes");
    assert_eq!(old_stripe(&model_a), old_stripe(&model_b));

    let sink = std::sync::Arc::new(TestSink::new());
    let backend = |idx| {
        Backend::new(
            BackendParams {
                upstream: format!("http://host{idx}:8080"),
                provider: ProviderKind::LlamaCpp,
                api_key: None,
                extra_headers: Vec::new(),
                gpu_watts: 100.0,
                models_poll_secs: None,
                model_types: Default::default(),
                model_filter: None,
            },
            sink.clone(),
        )
    };
    let models = vec![model_a.clone(), model_b.clone()];
    let registry = BackendRegistry::new(
        vec![backend(0), backend(1)],
        vec![models.clone(), models],
        RoutingMode::Catalog,
    )
    .with_load_balancing(crate::LbStrategy::RoundRobin, crate::OVERLOAD_FACTOR);

    let mut routes_a = Vec::new();
    let mut routes_b = Vec::new();
    for _ in 0..2 {
        routes_a.push(
            registry
                .route(RouteRequest {
                    model: Some(&model_a),
                    prefix_hash: None,
                })
                .unwrap()
                .0,
        );
        routes_b.push(
            registry
                .route(RouteRequest {
                    model: Some(&model_b),
                    prefix_hash: None,
                })
                .unwrap()
                .0,
        );
    }

    assert_eq!(routes_a, vec![0, 1]);
    assert_eq!(routes_b, vec![0, 1]);
}

/// A watchdog catalog refresh must not reset a surviving model's cursor.
#[test]
fn round_robin_rotation_survives_catalog_updates() {
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = |idx| {
        Backend::new(
            BackendParams {
                upstream: format!("http://host{idx}:8080"),
                provider: ProviderKind::LlamaCpp,
                api_key: None,
                extra_headers: Vec::new(),
                gpu_watts: 100.0,
                models_poll_secs: None,
                model_types: Default::default(),
                model_filter: None,
            },
            sink.clone(),
        )
    };
    let registry = BackendRegistry::new(
        vec![backend(0), backend(1)],
        vec![vec!["shared".into()], vec!["shared".into()]],
        RoutingMode::Catalog,
    )
    .with_load_balancing(crate::LbStrategy::RoundRobin, crate::OVERLOAD_FACTOR);

    assert_eq!(
        registry
            .route(RouteRequest {
                model: Some("shared"),
                prefix_hash: None,
            })
            .unwrap()
            .0,
        0
    );

    registry.set_backend_models(
        0,
        vec![twl_provider::ModelEntry {
            id: "shared".into(),
            raw: serde_json::json!({"id": "shared", "refreshed": true}),
        }],
    );

    assert_eq!(
        registry
            .route(RouteRequest {
                model: Some("shared"),
                prefix_hash: None,
            })
            .unwrap()
            .0,
        1,
        "a persistent model must continue from its pre-refresh cursor"
    );
}

/// With LeastLoaded strategy and a prefix hash, the prefix affinity
/// applies and the rendezvous primary is preferred (when under the bound).
#[test]
fn least_loaded_uses_prefix_affinity() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![vec!["shared".to_string()], vec!["shared".to_string()]],
        RoutingMode::Catalog,
    )
    .with_load_balancing(crate::LbStrategy::LeastLoaded, crate::OVERLOAD_FACTOR);

    let _guard = registry.begin_load(0);

    // With prefix hash Some(42), rendezvous primary is backend 0, and its
    // inflight load (1) is below the bound (2), so backend 0 is returned.
    let (idx, _) = registry
        .route(RouteRequest {
            model: Some("shared"),
            prefix_hash: Some(42),
        })
        .unwrap();
    assert_eq!(idx, 0);
}

/// With LeastLoaded strategy and a prefix hash, spill follows rendezvous
/// order (not least in-flight load): the first non-primary backend in
/// rendezvous order that is under the bound is chosen, even if a later
/// backend carries zero load.
#[test]
fn least_loaded_spills_deterministically() {
    let sink = std::sync::Arc::new(TestSink::new());

    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());

    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let params2 = BackendParams {
        upstream: "http://host2:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 300.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b2 = Backend::new(params2, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1, b2],
        vec![
            vec!["model".to_string()],
            vec!["model".to_string()],
            vec!["model".to_string()],
        ],
        RoutingMode::Catalog,
    )
    .with_load_balancing(crate::LbStrategy::LeastLoaded, crate::OVERLOAD_FACTOR);

    let prefix: u64 = 987654321;

    // Step 1: discover the primary (rendezvous winner) with no load.
    let (primary, _) = registry
        .route(RouteRequest {
            model: Some("model"),
            prefix_hash: Some(prefix),
        })
        .unwrap();

    // Step 2: load primary with 20 in-flight guards.
    let guards_primary: Vec<_> = (0..20).map(|_| registry.begin_load(primary)).collect();

    let (secondary, _) = registry
        .route(RouteRequest {
            model: Some("model"),
            prefix_hash: Some(prefix),
        })
        .unwrap();
    assert_ne!(
        secondary, primary,
        "secondary must differ from primary under high load"
    );

    // Step 3: the remaining backend is the third.
    let third = 3 - primary - secondary;

    // Step 4: add 3 in-flight guards on secondary.
    let guards_secondary: Vec<_> = (0..3).map(|_| registry.begin_load(secondary)).collect();

    // Step 5: the spill must land on secondary (first non-primary in
    // rendezvous order, still under the bound), not on third (which has
    // zero in-flight load and would win under pure least-loaded).
    let (spill, _) = registry
        .route(RouteRequest {
            model: Some("model"),
            prefix_hash: Some(prefix),
        })
        .unwrap();

    assert_eq!(
        spill, secondary,
        "rendezvous spill must choose secondary, got {spill}"
    );

    assert_ne!(
        spill, third,
        "a least-loaded spill would have chosen third because it carries zero \
         in-flight load, whereas deterministic rendezvous spill must choose \
         secondary since it is first in rendezvous order among the non-primary \
         backends and is still under the bound"
    );

    drop(guards_primary);
    drop(guards_secondary);
}

/// RoundRobin alternates between two backends serving the same model.
#[test]
fn round_robin_alternates_per_model() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![vec!["shared".to_string()], vec!["shared".to_string()]],
        RoutingMode::Catalog,
    )
    .with_load_balancing(crate::LbStrategy::RoundRobin, crate::OVERLOAD_FACTOR);

    let (first, _) = registry
        .route(RouteRequest {
            model: Some("shared"),
            prefix_hash: None,
        })
        .unwrap();
    let (second, _) = registry
        .route(RouteRequest {
            model: Some("shared"),
            prefix_hash: None,
        })
        .unwrap();

    assert_ne!(
        first, second,
        "consecutive round-robin calls must alternate"
    );
}

/// Under RoundRobin, a prefix hash pins all requests to one backend
/// (prefix affinity), while requests without a hash still rotate.
#[test]
fn round_robin_pins_with_prefix() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![vec!["shared".to_string()], vec!["shared".to_string()]],
        RoutingMode::Catalog,
    )
    .with_load_balancing(crate::LbStrategy::RoundRobin, crate::OVERLOAD_FACTOR);

    // With prefix hash: all eight calls return the same backend.
    let pinned: Vec<_> = (0..8)
        .map(|_| {
            registry
                .route(RouteRequest {
                    model: Some("shared"),
                    prefix_hash: Some(12345),
                })
                .unwrap()
                .0
        })
        .collect();
    assert!(
        pinned.iter().all(|&i| i == pinned[0]),
        "all eight calls with a prefix hash must route to the same backend, got {pinned:?}"
    );

    // Without prefix hash: eight calls rotate - they are not all identical.
    let rotating: Vec<_> = (0..8)
        .map(|_| {
            registry
                .route(RouteRequest {
                    model: Some("shared"),
                    prefix_hash: None,
                })
                .unwrap()
                .0
        })
        .collect();
    assert!(
        !rotating.iter().all(|&i| i == rotating[0]),
        "eight calls without a prefix hash must not all land on the same backend, got {rotating:?}"
    );
}

/// LeastLoaded picks the backend with fewer in-flight requests.
#[test]
fn least_loaded_selects_less_loaded() {
    let sink = std::sync::Arc::new(TestSink::new());
    let params0 = BackendParams {
        upstream: "http://host0:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 100.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b0 = Backend::new(params0, sink.clone());
    let params1 = BackendParams {
        upstream: "http://host1:8080".to_string(),
        provider: twl_provider::ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 200.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
    };
    let b1 = Backend::new(params1, sink.clone());

    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![vec!["model".to_string()], vec!["model".to_string()]],
        RoutingMode::Catalog,
    )
    .with_load_balancing(crate::LbStrategy::LeastLoaded, crate::OVERLOAD_FACTOR);

    // Add load to backend 0 only.
    let _guard = registry.begin_load(0);

    let (idx, backend) = registry
        .route(RouteRequest {
            model: Some("model"),
            prefix_hash: None,
        })
        .unwrap();

    assert_eq!(idx, 1, "LeastLoaded should pick backend 1 (lower load)");
    assert_eq!(backend.upstream, "http://host1:8080");
}

/// Build a `BackendParams` with a specific `model_types` override map.
fn params_with_types(model_types: HashMap<String, ModelKind>) -> BackendParams {
    BackendParams {
        upstream: "http://localhost:8080".to_string(),
        provider: ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 0.0,
        models_poll_secs: None,
        model_types,
        model_filter: None,
    }
}

/// Build a `BackendParams` with a specific `model_filter` allowlist.
fn params_with_filter(model_filter: Option<Vec<String>>) -> BackendParams {
    BackendParams {
        upstream: "http://localhost:8080".to_string(),
        provider: ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        gpu_watts: 0.0,
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter,
    }
}

#[test]
fn static_seed_carries_configured_type() {
    let mut types = HashMap::new();
    types.insert("embed-model".to_string(), ModelKind::Embedding);
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = Backend::new(params_with_types(types), sink);
    let registry = BackendRegistry::new(
        vec![backend],
        vec![vec!["embed-model".to_string(), "chat-model".to_string()]],
        RoutingMode::Catalog,
    );

    let entries = registry.model_entries();
    let embed = entries.iter().find(|e| e["id"] == "embed-model").unwrap();
    assert_eq!(embed["type"].as_str().unwrap(), "embedding");
    let chat = entries.iter().find(|e| e["id"] == "chat-model").unwrap();
    assert!(
        chat.get("type").is_none(),
        "chat model must not carry a type field"
    );
}

#[test]
fn set_backend_models_applies_type_override() {
    let mut types = HashMap::new();
    types.insert("embed-model".to_string(), ModelKind::Embedding);
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = Backend::new(params_with_types(types), sink);
    let registry = BackendRegistry::new(vec![backend], vec![vec![]], RoutingMode::Catalog);

    // Polled entry carries no type; the config override must add one.
    let entry = ModelEntry {
        id: "embed-model".to_string(),
        raw: serde_json::json!({ "id": "embed-model", "object": "model" }),
    };
    registry.set_backend_models(0, vec![entry]);

    let entries = registry.model_entries();
    let embed = entries.iter().find(|e| e["id"] == "embed-model").unwrap();
    assert_eq!(embed["type"].as_str().unwrap(), "embedding");
}

#[test]
fn config_override_wins_over_upstream_detected_type() {
    let mut types = HashMap::new();
    types.insert("m".to_string(), ModelKind::Embedding);
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = Backend::new(params_with_types(types), sink);
    let registry = BackendRegistry::new(vec![backend], vec![vec![]], RoutingMode::Catalog);

    // Upstream raw already carries a (different) "type"; config must win.
    let entry = ModelEntry {
        id: "m".to_string(),
        raw: serde_json::json!({ "id": "m", "object": "model", "type": "chat" }),
    };
    registry.set_backend_models(0, vec![entry]);

    let entries = registry.model_entries();
    let m = entries.iter().find(|e| e["id"] == "m").unwrap();
    assert_eq!(
        m["type"].as_str().unwrap(),
        "embedding",
        "config override must replace the upstream-detected type"
    );
}

/// A static model list filtered by `model_filter` only exposes the allowed
/// IDs; filtered-out models are hidden from `/v1/models` and not routable.
#[test]
fn static_models_respect_model_filter() {
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = Backend::new(params_with_filter(Some(vec!["a".to_string()])), sink);
    let registry = BackendRegistry::new(
        vec![backend],
        vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]],
        RoutingMode::Catalog,
    );

    let entries = registry.model_entries();
    assert_eq!(entries.len(), 1, "only the allowed model must be listed");
    assert_eq!(entries[0]["id"], "a");

    assert!(registry.backend_for(Some("a")).is_ok());
    match registry.backend_for(Some("b")) {
        Err(RouteError::UnknownModel(m)) => assert_eq!(m, "b"),
        Ok(_) => panic!("expected Err(RouteError::UnknownModel) for filtered 'b'"),
        Err(e) => panic!("expected UnknownModel, got {e}"),
    }
}

/// A watchdog-refreshed catalog filtered by `model_filter` only keeps the
/// allowed IDs; filtered-out models are dropped before dedupe.
#[test]
fn set_backend_models_respects_model_filter() {
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = Backend::new(params_with_filter(Some(vec!["x".to_string()])), sink);
    let registry = BackendRegistry::new(vec![backend], vec![vec![]], RoutingMode::Catalog);

    registry.set_backend_models(
        0,
        vec![
            ModelEntry {
                id: "x".to_string(),
                raw: serde_json::json!({ "id": "x", "object": "model" }),
            },
            ModelEntry {
                id: "y".to_string(),
                raw: serde_json::json!({ "id": "y", "object": "model" }),
            },
        ],
    );

    let entries = registry.model_entries();
    assert_eq!(entries.len(), 1, "only the allowed model must be listed");
    assert_eq!(entries[0]["id"], "x");

    assert!(registry.backend_for(Some("x")).is_ok());
    match registry.backend_for(Some("y")) {
        Err(RouteError::UnknownModel(m)) => assert_eq!(m, "y"),
        Ok(_) => panic!("expected Err(RouteError::UnknownModel) for filtered 'y'"),
        Err(e) => panic!("expected UnknownModel, got {e}"),
    }
}

/// A backend without a `model_filter` is unaffected by another backend's
/// allowlist: its own models are still listed and routable.
#[test]
fn unfiltered_backend_unaffected_by_other_filter() {
    let sink = std::sync::Arc::new(TestSink::new());
    let b0 = Backend::new(
        params_with_filter(Some(vec!["a".to_string()])),
        sink.clone(),
    );
    let b1 = Backend::new(params_with_filter(None), sink.clone());
    let registry = BackendRegistry::new(
        vec![b0, b1],
        vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["c".to_string()],
        ],
        RoutingMode::Catalog,
    );

    let entries = registry.model_entries();
    let ids: Vec<&str> = entries.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"a"), "allowed model must be listed: {ids:?}");
    assert!(
        ids.contains(&"c"),
        "unfiltered backend's model must be listed: {ids:?}"
    );
    assert!(
        !ids.contains(&"b"),
        "filtered-out model must not be listed: {ids:?}"
    );

    assert!(registry.backend_for(Some("a")).is_ok());
    assert!(
        matches!(
            registry.backend_for(Some("b")),
            Err(RouteError::UnknownModel(_))
        ),
        "filtered-out model must not be routable"
    );
    assert!(registry.backend_for(Some("c")).is_ok());
}

/// With no `model_filter` (None), every model passes through unchanged.
#[test]
fn no_model_filter_passes_all_models() {
    let sink = std::sync::Arc::new(TestSink::new());
    let backend = Backend::new(params_with_filter(None), sink);
    let registry = BackendRegistry::new(
        vec![backend],
        vec![vec!["a".to_string(), "b".to_string()]],
        RoutingMode::Catalog,
    );

    assert_eq!(
        registry.model_entries().len(),
        2,
        "no filter must list every static model"
    );
    assert!(registry.backend_for(Some("a")).is_ok());
    assert!(registry.backend_for(Some("b")).is_ok());
}
