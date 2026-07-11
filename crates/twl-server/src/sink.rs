//! Bounded, off-runtime persistence for per-request accounting records.
//!
//! Response streams finish (or are cancelled) on Tokio worker threads. Their
//! `Drop` paths must therefore never perform synchronous SQLite work. The
//! [`AccountingWriter`] hands usage and energy records to one dedicated
//! blocking thread through a bounded channel. When the queue is saturated or
//! disconnected, accounting remains best-effort: the record is dropped and a
//! diagnostic is logged instead of blocking the runtime.

use std::sync::{mpsc, Arc};

use twl_backends::energy::EnergySink;
use twl_store::{Usage, UsageStore};

/// Maximum number of completed accounting records waiting for SQLite.
const ACCOUNTING_QUEUE_CAPACITY: usize = 1024;

#[derive(Debug)]
enum Record {
    Usage(Usage),
    Energy { elapsed_secs: f64, watts: f64 },
}

/// Nonblocking handle to the dedicated accounting persistence thread.
#[derive(Clone, Debug)]
pub(crate) struct AccountingWriter {
    tx: mpsc::SyncSender<Record>,
}

impl AccountingWriter {
    /// Start the production writer backed by `UsageStore`.
    pub(crate) fn spawn(store: Arc<UsageStore>) -> std::io::Result<Self> {
        Self::spawn_with(ACCOUNTING_QUEUE_CAPACITY, move |record| match record {
            Record::Usage(usage) => store.record(&usage),
            Record::Energy {
                elapsed_secs,
                watts,
            } => store.record_energy(elapsed_secs, watts),
        })
    }

    fn spawn_with(
        capacity: usize,
        persist: impl Fn(Record) + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(capacity);
        std::thread::Builder::new()
            .name("twl-accounting-writer".to_owned())
            .spawn(move || {
                while let Ok(record) = rx.recv() {
                    persist(record);
                }
            })?;
        Ok(Self { tx })
    }

    /// Queue a usage record without waiting for SQLite or queue capacity.
    pub(crate) fn record_usage(&self, usage: Usage) -> bool {
        self.enqueue(Record::Usage(usage), "usage")
    }

    fn enqueue(&self, record: Record, kind: &'static str) -> bool {
        match self.tx.try_send(record) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::error!(record_kind = kind, "accounting queue full; dropping record");
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                tracing::error!(
                    record_kind = kind,
                    "accounting writer stopped; dropping record"
                );
                false
            }
        }
    }
}

impl EnergySink for AccountingWriter {
    fn record_energy(&self, elapsed_secs: f64, watts: f64) {
        self.enqueue(
            Record::Energy {
                elapsed_secs,
                watts,
            },
            "energy",
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    use twl_backends::energy::EnergyMeter;
    use twl_store::Usage;

    use super::AccountingWriter;

    #[test]
    fn drop_side_enqueue_does_not_run_blocking_persistence_inline() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = Arc::new(
            AccountingWriter::spawn_with(1, move |_| {
                entered_tx.send(std::thread::current().id()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap(),
        );

        let meter = EnergyMeter::new(100.0, writer);
        let guard = meter.begin();
        let (drop_thread_tx, drop_thread_rx) = mpsc::channel();
        let (drop_done_tx, drop_done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop_thread_tx.send(std::thread::current().id()).unwrap();
            drop(guard);
            drop_done_tx.send(()).unwrap();
        });

        drop_done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("MeterGuard::drop blocked on persistence");
        let drop_thread = drop_thread_rx.recv().unwrap();
        let persistence_thread = entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert_ne!(
            persistence_thread, drop_thread,
            "sink work ran inline in MeterGuard::drop"
        );
        release_tx.send(()).unwrap();
    }

    #[test]
    fn bounded_queue_drops_overflow_instead_of_blocking() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let persisted = Arc::new(AtomicUsize::new(0));
        let persisted_in_worker = Arc::clone(&persisted);
        let writer = AccountingWriter::spawn_with(1, move |_| {
            let invocation = persisted_in_worker.fetch_add(1, Ordering::SeqCst);
            if invocation == 0 {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
        })
        .unwrap();

        assert!(writer.record_usage(Usage::default()));
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            writer.record_usage(Usage::default()),
            "queue has one free slot"
        );
        assert!(
            !writer.record_usage(Usage::default()),
            "overflow record must be dropped"
        );

        release_tx.send(()).unwrap();
        drop(writer);
        for _ in 0..100 {
            if persisted.load(Ordering::SeqCst) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(persisted.load(Ordering::SeqCst), 2);
    }
}
