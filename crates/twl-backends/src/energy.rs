//! GPU energy metering with busy-interval-union semantics.
//!
//! Provides an energy meter that emits one `(elapsed_secs, watts)` record per
//! contiguous busy interval, handling concurrent requests without double-counting.
//! Integration happens through [`EnergySink`], implemented by the DB layer.

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Receives one record per finished busy interval. Called from request
/// threads, so implementations should not do blocking work inline; the
/// production sink queues to a dedicated writer thread in `twl-server`.
pub trait EnergySink: Send + Sync + 'static {
    /// Records a completed busy interval at the configured constant power draw.
    ///
    /// `elapsed_secs` covers the union of overlapping requests, rather than the
    /// sum of their individual durations, and `watts` is the meter's configured value.
    fn record_energy(&self, elapsed_secs: f64, watts: f64);
}

#[derive(Debug)]
struct State {
    active_count: usize,
    busy_since: Option<Instant>,
}

struct Inner {
    watts: f64,
    sink: Arc<dyn EnergySink>,
    state: Mutex<State>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("watts", &self.watts)
            .field("sink", &"_")
            .field("state", &self.state)
            .finish()
    }
}

/// Tracks how many concurrent requests are active and emits a single
/// `(elapsed_secs, watts)` record when the last one finishes, covering the
/// entire busy period. A meter constructed with `watts == 0.0` never emits.
#[derive(Clone, Debug)]
pub struct EnergyMeter {
    inner: Arc<Inner>,
}

impl EnergyMeter {
    /// Creates a meter backed by `sink`; a zero-watt meter intentionally emits nothing.
    pub fn new(watts: f64, sink: Arc<dyn EnergySink>) -> Self {
        Self {
            inner: Arc::new(Inner {
                watts,
                sink,
                state: Mutex::new(State {
                    active_count: 0,
                    busy_since: None,
                }),
            }),
        }
    }

    /// Returns the constant power draw associated with every emitted interval.
    pub fn watts(&self) -> f64 {
        self.inner.watts
    }

    /// Starts metering one request; hold the guard for the request's
    /// duration.
    pub fn begin(&self) -> MeterGuard {
        let mut state = self.inner.state.lock().unwrap();
        if state.active_count == 0 {
            state.busy_since = Some(Instant::now());
        }
        state.active_count += 1;
        MeterGuard {
            inner: self.inner.clone(),
        }
    }
}

/// The meter is busy while any guard exists; when the last guard drops, one
/// record is emitted. A guard dropped early by client disconnect still
/// emits, so cancelled requests are counted in the energy totals.
#[derive(Debug)]
pub struct MeterGuard {
    inner: Arc<Inner>,
}

impl Drop for MeterGuard {
    fn drop(&mut self) {
        let completed_interval = {
            let mut state = self.inner.state.lock().unwrap();
            state.active_count -= 1;
            if state.active_count == 0 {
                state.busy_since.take().and_then(|busy_since| {
                    (self.inner.watts > 0.0).then(|| busy_since.elapsed().as_secs_f64())
                })
            } else {
                None
            }
        };

        if let Some(elapsed) = completed_interval {
            self.inner.sink.record_energy(elapsed, self.inner.watts);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    use super::{EnergyMeter, EnergySink};

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

    struct ReentrantSink {
        meter: Mutex<Option<EnergyMeter>>,
        did_reenter: AtomicBool,
        records: AtomicUsize,
    }

    impl EnergySink for ReentrantSink {
        fn record_energy(&self, _elapsed_secs: f64, _watts: f64) {
            self.records.fetch_add(1, Ordering::Relaxed);
            if !self.did_reenter.swap(true, Ordering::Relaxed) {
                let guard = self.meter.lock().unwrap().as_ref().unwrap().begin();
                drop(guard);
            }
        }
    }

    #[test]
    fn sink_can_reenter_meter_without_deadlocking() {
        let sink = Arc::new(ReentrantSink {
            meter: Mutex::new(None),
            did_reenter: AtomicBool::new(false),
            records: AtomicUsize::new(0),
        });
        let meter = EnergyMeter::new(100.0, sink.clone());
        *sink.meter.lock().unwrap() = Some(meter.clone());

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop(meter.begin());
            done_tx.send(()).unwrap();
        });

        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("re-entering the meter from the sink must not deadlock");
        assert_eq!(sink.records.load(Ordering::Relaxed), 2);
        sink.meter.lock().unwrap().take();
    }

    struct PanicOnceSink(AtomicBool);

    impl EnergySink for PanicOnceSink {
        fn record_energy(&self, _elapsed_secs: f64, _watts: f64) {
            if !self.0.swap(true, Ordering::Relaxed) {
                panic!("injected sink panic");
            }
        }
    }

    #[test]
    fn panicking_sink_does_not_poison_meter_state() {
        let sink = Arc::new(PanicOnceSink(AtomicBool::new(false)));
        let meter = EnergyMeter::new(100.0, sink);

        let first = meter.begin();
        assert!(catch_unwind(AssertUnwindSafe(|| drop(first))).is_err());

        // The sink panicked after the interval state was finalized and its
        // mutex released, so a later interval can still be recorded.
        drop(meter.begin());
    }

    #[test]
    fn begin_drop_emits_one_record() {
        let sink = std::sync::Arc::new(TestSink::new());
        let meter = EnergyMeter::new(100.0, sink.clone());
        let watts = meter.watts();
        assert_eq!(watts, 100.0);

        {
            let _guard = meter.begin();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let records = sink.records.lock().unwrap();
        assert_eq!(records.len(), 1, "should emit exactly one record");
        let (elapsed, w) = records[0];
        assert!(elapsed >= 0.0, "elapsed should be non-negative");
        assert_eq!(w, 100.0, "watts should match the meter");
    }

    #[test]
    fn watts_zero_emits_nothing() {
        let sink = std::sync::Arc::new(TestSink::new());
        let meter = EnergyMeter::new(0.0, sink.clone());

        {
            let _guard = meter.begin();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let records = sink.records.lock().unwrap();
        assert!(records.is_empty(), "watts == 0.0 should emit nothing");
    }

    /// Two overlapping guards together emit exactly one record for the
    /// union busy interval, not two separate records.
    #[test]
    fn overlapping_guards_emit_single_union_record() {
        let sink = std::sync::Arc::new(TestSink::new());
        let meter = EnergyMeter::new(50.0, sink.clone());

        let guard_a = meter.begin();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let guard_b = meter.begin();
        std::thread::sleep(std::time::Duration::from_millis(10));
        drop(guard_a);
        std::thread::sleep(std::time::Duration::from_millis(10));
        drop(guard_b);

        let records = sink.records.lock().unwrap();
        assert_eq!(records.len(), 1, "should emit exactly one record");
        let (elapsed, w) = records[0];
        assert!(
            elapsed >= 0.030,
            "elapsed should be at least 30 ms, got {elapsed:.3} s"
        );
        assert_eq!(w, 50.0, "watts should match the meter");
    }

    /// Guards that do not overlap each emit a separate record.
    #[test]
    fn sequential_guards_emit_separate_records() {
        let sink = std::sync::Arc::new(TestSink::new());
        let meter = EnergyMeter::new(42.0, sink.clone());

        {
            let _guard = meter.begin();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        {
            let _guard = meter.begin();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let records = sink.records.lock().unwrap();
        assert_eq!(records.len(), 2, "should emit exactly two records");
        for (elapsed, w) in records.iter() {
            assert!(
                *elapsed >= 0.005,
                "elapsed should be at least 5 ms, got {elapsed:.3} s"
            );
            assert_eq!(*w, 42.0, "watts should match the meter");
        }
    }

    /// Guard dropped without explicit finish (simulating cancellation) still emits.
    #[test]
    fn guard_drop_on_cancellation_still_emits() {
        let sink = std::sync::Arc::new(TestSink::new());
        let meter = EnergyMeter::new(75.0, sink.clone());

        {
            let _guard = meter.begin();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let records = sink.records.lock().unwrap();
        assert_eq!(
            records.len(),
            1,
            "drop should emit even without explicit finish"
        );
        let (elapsed, w) = records[0];
        assert!(elapsed >= 0.0);
        assert_eq!(w, 75.0);
    }
}
