//! Owns the backend abstraction and routing table that maps requests to a
//! backend by model name. The model catalog is seeded from config at startup,
//! updated by the model watchdog, and guarded by an `ArcSwap` for wait-free reads. Each backend
//! carries an [`EnergyMeter`] for GPU power accounting.

pub mod energy;

use crate::energy::{EnergyMeter, EnergySink};

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustc_hash::{FxBuildHasher, FxHasher};
use smallvec::{smallvec, SmallVec};

use twl_provider::ProviderKind;

use crossbeam_utils::CachePadded;
use std::sync::atomic::{AtomicU32, Ordering};

use thiserror::Error;

struct Redacted;

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

fn redacted_if_present<T>(value: &Option<T>) -> Option<Redacted> {
    value.as_ref().map(|_| Redacted)
}

/// Configuration inputs for constructing a [`Backend`].
pub struct BackendParams {
    /// Base URL to which provider requests are forwarded.
    pub upstream: String,
    /// Provider dialect used for authentication and response parsing.
    pub provider: ProviderKind,
    /// Optional provider credential; debug output always redacts its presence.
    pub api_key: Option<String>,
    /// Additional headers appended after provider-derived authentication headers.
    pub extra_headers: Vec<(String, String)>,
    /// Assumed GPU power draw used to convert busy time into energy usage.
    pub gpu_watts: f64,
    /// `None` disables polling (static model list only).
    pub models_poll_secs: Option<u64>,
    /// Per-model kind overrides that win over upstream-detected `type`.
    pub model_types: HashMap<String, twl_provider::ModelKind>,
    /// Optional allowlist restricting both advertised and routable models.
    pub model_filter: Option<Vec<String>>,
}

impl std::fmt::Debug for BackendParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendParams")
            .field("upstream", &self.upstream)
            .field("provider", &self.provider)
            .field("api_key", &redacted_if_present(&self.api_key))
            .field("extra_headers", &Redacted)
            .field("gpu_watts", &self.gpu_watts)
            .field("models_poll_secs", &self.models_poll_secs)
            .field("model_types", &self.model_types)
            .field("model_filter", &self.model_filter)
            .finish()
    }
}

/// A single upstream LLM backend with its provider-specific configuration.
pub struct Backend {
    /// Base URL to which provider requests are forwarded.
    pub upstream: String,
    /// Provider dialect used for authentication and usage parsing.
    pub provider: ProviderKind,
    /// Pre-computed: the provider's bearer header when an API key is
    /// present, followed by any extra headers from config.
    pub auth_headers: Vec<(String, String)>,
    /// Configured GPU power draw in watts; zero disables energy records.
    pub gpu_watts: f64,
    /// Meter shared by concurrent requests so overlapping work is counted once.
    pub meter: EnergyMeter,
    /// Model catalog refresh interval, or `None` for a static catalog.
    pub models_poll_secs: Option<u64>,
    /// Per-model kind overrides that win over upstream-detected `type`.
    pub model_types: HashMap<String, twl_provider::ModelKind>,
    /// Models not in this allowlist are hidden from `/v1/models` and not
    /// routable; applies to both static and polled models.
    pub model_filter: Option<HashSet<String>>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("upstream", &self.upstream)
            .field("provider", &self.provider)
            .field("auth_headers", &Redacted)
            .field("gpu_watts", &self.gpu_watts)
            .field("meter", &self.meter)
            .field("models_poll_secs", &self.models_poll_secs)
            .field("model_types", &self.model_types)
            .finish()
    }
}

impl Backend {
    /// Builds a backend and precomputes its authentication headers and energy meter.
    pub fn new(params: BackendParams, sink: Arc<dyn EnergySink>) -> Self {
        let BackendParams {
            upstream,
            provider,
            api_key,
            extra_headers,
            gpu_watts,
            models_poll_secs,
            model_types,
            model_filter,
        } = params;

        let mut auth_headers = provider.auth_headers(api_key.as_deref());
        auth_headers.extend(extra_headers);
        Self {
            upstream,
            provider,
            auth_headers,
            gpu_watts,
            models_poll_secs,
            model_types,
            model_filter: model_filter.map(|v| v.into_iter().collect()),
            meter: EnergyMeter::new(gpu_watts, sink),
        }
    }
}

/// Bounded-load multiplier: a backend may hold up to this multiple of the
/// average in-flight load before a prefix spills to another candidate.
pub const OVERLOAD_FACTOR: f64 = 1.25;

/// Requests carrying a prompt-prefix hash are always pinned to a backend by
/// that hash via rendezvous hashing (with deterministic spill on overload);
/// the strategy governs only prefix-less placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LbStrategy {
    /// Rotate prefix-less requests evenly through the model's candidates.
    RoundRobin,
    #[default]
    /// Prefer the candidate with the fewest requests currently in flight.
    LeastLoaded,
}

/// Routing inputs that influence model selection and cache affinity.
#[derive(Debug, Clone)]
pub struct RouteRequest<'a> {
    /// Requested model name; routing rejects a missing model explicitly.
    pub model: Option<&'a str>,
    /// Prompt-prefix hash for KV-cache-aware backend affinity.
    pub prefix_hash: Option<u64>,
}

/// Failure modes produced before a request can be assigned to a backend.
#[derive(Debug, Error)]
pub enum RouteError {
    #[error("request must specify a model")]
    /// The request omitted the model required by the routing contract.
    MissingModel,

    #[error("model {0} is not served by any backend")]
    /// No configured backend currently advertises the supplied model.
    UnknownModel(String),
}

#[derive(Clone, Debug)]
struct CatalogEntry {
    id: String,
    raw: serde_json::Value,
}

type Candidates = SmallVec<[u32; 4]>;

#[derive(Debug)]
struct RoutingEntry {
    candidates: Candidates,
    /// Shared so catalog rebuilds preserve rotation for models that remain.
    rotation: Arc<AtomicU32>,
}

type RoutingMap = HashMap<Box<str>, RoutingEntry, FxBuildHasher>;

#[derive(Debug)]
struct Catalog {
    /// Per-backend model entries (raw data for `/v1/models`).
    entries: Vec<Vec<CatalogEntry>>,
    routing: RoutingMap,
}

/// Config overrides win over any upstream-detected `type` already present
/// in `raw`, for both static and watchdog-discovered models.
fn apply_type_override(
    raw: &mut serde_json::Value,
    overrides: &HashMap<String, twl_provider::ModelKind>,
    id: &str,
) {
    if let Some(kind) = overrides.get(id) {
        if let Some(map) = raw.as_object_mut() {
            map.insert(
                "type".to_string(),
                serde_json::Value::String(kind.as_str().to_string()),
            );
        }
    }
}

/// Retains the first entry per model ID so configuration/poll response
/// order and provider metadata survive.
fn deduplicate_backend_entries(mut entries: Vec<CatalogEntry>) -> Vec<CatalogEntry> {
    let mut seen: HashSet<String, FxBuildHasher> = HashSet::with_hasher(FxBuildHasher);
    entries.retain(|entry| seen.insert(entry.id.clone()));
    entries
}

fn model_allowed(filter: Option<&HashSet<String>>, id: &str) -> bool {
    filter.is_none_or(|f| f.contains(id))
}

fn build_routing(entries: &[Vec<CatalogEntry>], previous: Option<&RoutingMap>) -> RoutingMap {
    use std::collections::hash_map::Entry;
    let mut routing = RoutingMap::with_hasher(FxBuildHasher);
    for (backend_idx, backend_entries) in entries.iter().enumerate() {
        for entry in backend_entries {
            match routing.entry(entry.id.clone().into()) {
                Entry::Vacant(v) => {
                    let rotation = previous
                        .and_then(|routing| routing.get(entry.id.as_str()))
                        .map_or_else(
                            || Arc::new(AtomicU32::new(0)),
                            |entry| Arc::clone(&entry.rotation),
                        );
                    v.insert(RoutingEntry {
                        candidates: smallvec![backend_idx as u32],
                        rotation,
                    });
                }
                Entry::Occupied(mut e) => {
                    let candidates = &mut e.get_mut().candidates;
                    // Entries are grouped by backend, so a duplicate model for
                    // this backend can only repeat the last candidate. Keep
                    // candidates from distinct backends for balancing/failover.
                    if candidates.last().copied() != Some(backend_idx as u32) {
                        candidates.push(backend_idx as u32);
                        tracing::debug!(
                            backend = backend_idx,
                            model = %entry.id,
                            "model served by multiple backends"
                        );
                    }
                }
            }
        }
    }
    routing
}

/// Holds all configured backends and selects one per request.
///
/// Read-only after construction except for [`Self::set_backend_models`]
/// (model watchdog updates); safe for concurrent use.
#[derive(Debug)]
pub struct BackendRegistry {
    catalog: ArcSwap<Catalog>,
    backends: Vec<Backend>,
    routing_mode: RoutingMode,
    inflight: Arc<[CachePadded<AtomicU32>]>,
    strategy: LbStrategy,
    overload_factor: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Controls whether routing requires membership in the discovered model catalog.
pub enum RoutingMode {
    /// Forward every supplied model name to the sole backend.
    FlatPassthrough,
    /// Only route model names currently present in the catalog.
    Catalog,
}

impl BackendRegistry {
    /// Seeds the model catalog from `static_models`; the model watchdog
    /// updates it later via [`Self::set_backend_models`].
    ///
    /// # Panics
    ///
    /// Panics if `backends` is empty.
    pub fn new(
        backends: Vec<Backend>,
        static_models: Vec<Vec<String>>,
        routing_mode: RoutingMode,
    ) -> Self {
        assert!(
            !backends.is_empty(),
            "BackendRegistry requires at least one backend"
        );
        assert_eq!(
            backends.len(),
            static_models.len(),
            "static_models must have one entry per backend"
        );
        assert!(
            routing_mode != RoutingMode::FlatPassthrough || backends.len() == 1,
            "flat passthrough routing requires exactly one backend"
        );

        let mut entries: Vec<Vec<CatalogEntry>> = Vec::with_capacity(backends.len());

        for (backend, models) in backends.iter().zip(static_models.iter()) {
            let filter = backend.model_filter.as_ref();
            let provider = backend.provider;
            let backend_entries: Vec<CatalogEntry> = models
                .iter()
                .filter(|id| model_allowed(filter, id))
                .map(|id| {
                    let mut map = serde_json::Map::new();
                    map.insert("id".to_string(), serde_json::Value::String(id.clone()));
                    map.insert(
                        "object".to_string(),
                        serde_json::Value::String("model".to_string()),
                    );
                    map.insert(
                        "owned_by".to_string(),
                        serde_json::Value::String(provider.name().to_string()),
                    );
                    let mut raw = serde_json::Value::Object(map);
                    apply_type_override(&mut raw, &backend.model_types, id);
                    CatalogEntry {
                        id: id.clone(),
                        raw,
                    }
                })
                .collect();

            entries.push(deduplicate_backend_entries(backend_entries));
        }

        let routing = build_routing(&entries, None);

        let inflight: Arc<[CachePadded<AtomicU32>]> = (0..backends.len())
            .map(|_| CachePadded::new(AtomicU32::new(0)))
            .collect::<Vec<_>>()
            .into();

        Self {
            catalog: ArcSwap::new(Arc::new(Catalog { entries, routing })),
            backends,
            routing_mode,
            inflight,
            strategy: LbStrategy::default(),
            overload_factor: OVERLOAD_FACTOR,
        }
    }

    /// Overrides the prefix-less placement strategy and bounded-load multiplier.
    ///
    /// Prefix-bearing requests retain rendezvous-hash affinity regardless of
    /// `strategy`; `overload_factor` only controls when those requests spill.
    pub fn with_load_balancing(mut self, strategy: LbStrategy, overload_factor: f64) -> Self {
        self.strategy = strategy;
        self.overload_factor = overload_factor;
        self
    }

    /// Return backend indices in selection order (best first).
    fn strategy_placement(&self, entry: &RoutingEntry) -> SmallVec<[usize; 4]> {
        let candidates = entry.candidates.as_slice();
        match self.strategy {
            LbStrategy::RoundRobin => {
                let n = candidates.len();
                let pos = entry.rotation.fetch_add(1, Ordering::Relaxed) as usize % n;
                let mut result: SmallVec<[usize; 4]> =
                    candidates.iter().map(|&i| i as usize).collect();
                result.rotate_left(pos);
                result
            }
            LbStrategy::LeastLoaded => {
                // Reads inflight counters before begin_load runs in the caller,
                // so several concurrent prefix-less requests can pick the same
                // least-loaded backend before any increment. This is a soft,
                // self-correcting placement bias (same caveat as the prefix path).
                let mut result: SmallVec<[usize; 4]> =
                    candidates.iter().map(|&i| i as usize).collect();
                result.sort_by_key(|&idx| self.inflight[idx].load(Ordering::Relaxed));
                result
            }
        }
    }

    /// Every strategy spills to the next backend in rendezvous order that is
    /// under the bound, so a hot prefix always overflows to the same stable
    /// secondary and keeps its KV cache warm there too, falling back to the
    /// primary if all candidates are over the bound.
    fn spill_target(&self, scored: &[(u64, usize)], bound: u32) -> usize {
        scored
            .iter()
            .find(|&&(_score, idx)| self.inflight[idx].load(Ordering::Relaxed) < bound)
            .map(|&(_, idx)| idx)
            .unwrap_or(scored[0].1)
    }

    fn ordered_candidates(
        &self,
        entry: &RoutingEntry,
        prefix_hash: Option<u64>,
    ) -> SmallVec<[usize; 4]> {
        let candidates = entry.candidates.as_slice();
        match prefix_hash {
            None => self.strategy_placement(entry),
            Some(h) => {
                // Score each candidate via rendezvous hashing.
                let mut scored: SmallVec<[(u64, usize); 4]> = candidates
                    .iter()
                    .map(|&idx| {
                        let mut hasher = FxHasher::default();
                        h.hash(&mut hasher);
                        idx.hash(&mut hasher);
                        let score = hasher.finish();
                        (score, idx as usize)
                    })
                    .collect();
                // Rendezvous order: highest score first.
                scored.sort_unstable_by(|a, b| b.0.cmp(&a.0));

                let n = candidates.len() as f64;
                let total: u32 = candidates
                    .iter()
                    .map(|&idx| self.inflight[idx as usize].load(Ordering::Relaxed))
                    .sum();
                // bound = max(1, ceil(overload_factor * (total + 1) / n))
                let bound = {
                    let raw = self.overload_factor * (total as f64 + 1.0) / n;
                    std::cmp::max(1u32, raw.ceil() as u32)
                };

                // Primary candidate (highest rendezvous score).
                let primary = scored[0].1;

                // Reads inflight counters before begin_load later increments
                // them, so under bursty concurrent load several requests can
                // select the same backend before any increments and briefly
                // exceed the bound. overload_factor is a soft target; this
                // transient overshoot is expected and self-correcting.
                let chosen = if self.inflight[primary].load(Ordering::Relaxed) < bound {
                    primary
                } else {
                    self.spill_target(&scored, bound)
                };

                let mut result = smallvec![chosen];
                for &(_score, idx) in &scored {
                    if idx != chosen {
                        result.push(idx);
                    }
                }
                result
            }
        }
    }

    /// Selects the best backend for a request and returns its stable registry index.
    pub fn route(&self, req: RouteRequest) -> Result<(usize, &Backend), RouteError> {
        let idx = self
            .route_candidates(req)?
            .into_iter()
            .next()
            .expect("route_candidates never returns empty");
        Ok((idx, &self.backends[idx]))
    }

    /// Resolves a model without prefix affinity, returning only the backend value.
    pub fn backend_for(&self, model: Option<&str>) -> Result<&Backend, RouteError> {
        self.route(RouteRequest {
            model,
            prefix_hash: None,
        })
        .map(|(_, backend)| backend)
    }

    /// Increments a backend's in-flight count until the returned guard is dropped.
    ///
    /// `idx` must be a valid index from this registry's routing APIs.
    pub fn begin_load(&self, idx: usize) -> LoadGuard {
        self.inflight[idx].fetch_add(1, Ordering::Relaxed);
        LoadGuard {
            counters: Arc::clone(&self.inflight),
            idx,
        }
    }

    /// Replaces a backend's model entries and rebuilds the routing table.
    /// Called by the model watchdog after a successful poll.
    pub fn set_backend_models(&self, idx: usize, entries: Vec<twl_provider::ModelEntry>) {
        let overrides = &self.backends[idx].model_types;
        let filter = self.backends[idx].model_filter.as_ref();
        let new_backend_entries: Vec<CatalogEntry> = entries
            .iter()
            .filter(|e| model_allowed(filter, &e.id))
            .map(|e| {
                let mut raw = e.raw.clone();
                apply_type_override(&mut raw, overrides, &e.id);
                CatalogEntry {
                    id: e.id.clone(),
                    raw,
                }
            })
            .collect();
        let new_backend_entries = deduplicate_backend_entries(new_backend_entries);

        // Atomically update the catalog via RCU so concurrent watchdog
        // updates cannot lose one another.
        let _ = self.catalog.rcu(|catalog| {
            let mut new_entries = catalog.entries.clone();
            new_entries[idx] = new_backend_entries.clone();
            let routing = build_routing(&new_entries, Some(&catalog.routing));
            Arc::new(Catalog {
                entries: new_entries,
                routing,
            })
        });
    }

    /// Returns deduplicated raw model objects in backend precedence order.
    pub fn model_entries(&self) -> Vec<serde_json::Value> {
        let catalog = self.catalog.load();
        let mut seen: HashSet<&str, FxBuildHasher> = HashSet::with_hasher(FxBuildHasher);
        catalog
            .entries
            .iter()
            .flat_map(|entries| entries.iter())
            .filter(|entry| seen.insert(entry.id.as_str()))
            .map(|entry| entry.raw.clone())
            .collect()
    }

    /// Returns the immutable backend list whose indices are used by routing APIs.
    pub fn backends(&self) -> &[Backend] {
        &self.backends
    }

    /// Returns the configured strategy for requests without prefix affinity.
    pub fn strategy(&self) -> LbStrategy {
        self.strategy
    }

    /// Like [`Self::route`] but returns all candidate indices in selection
    /// order (best first).
    pub fn route_candidates(&self, req: RouteRequest) -> Result<SmallVec<[usize; 4]>, RouteError> {
        let catalog = self.catalog.load();

        match req.model {
            None => Err(RouteError::MissingModel),
            Some(name) => match catalog.routing.get(name) {
                Some(entry) => {
                    if entry.candidates.len() == 1 {
                        Ok(smallvec![entry.candidates[0] as usize])
                    } else {
                        Ok(self.ordered_candidates(entry, req.prefix_hash))
                    }
                }
                None => {
                    if self.routing_mode == RoutingMode::FlatPassthrough {
                        Ok(smallvec![0])
                    } else {
                        Err(RouteError::UnknownModel(name.to_string()))
                    }
                }
            },
        }
    }

    /// Returns a backend by registry index.
    ///
    /// The index must originate from this registry; an out-of-range value panics.
    pub fn backend(&self, idx: usize) -> &Backend {
        &self.backends[idx]
    }
}

/// Decrements the in-flight counter for its backend when the request finishes.
#[derive(Debug)]
pub struct LoadGuard {
    counters: Arc<[CachePadded<AtomicU32>]>,
    idx: usize,
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        self.counters[self.idx].fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
