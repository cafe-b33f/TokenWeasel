//! In-memory browser session storage with periodic expiry cleanup.

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::Mutex;
use tower_sessions::{
    cookie::time::OffsetDateTime,
    session::{Id, Record},
    session_store, SessionStore,
};

/// How often expired browser sessions are removed from memory.
const SESSION_PURGE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// An in-memory session store that supports removing expired records.
#[derive(Clone, Debug, Default)]
pub(crate) struct PurgingMemoryStore(Arc<Mutex<HashMap<Id, Record>>>);

#[async_trait]
impl SessionStore for PurgingMemoryStore {
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        let mut records = self.0.lock().await;
        while records.contains_key(&record.id) {
            record.id = Id::default();
        }
        records.insert(record.id, record.clone());
        Ok(())
    }

    async fn save(&self, record: &Record) -> session_store::Result<()> {
        self.0.lock().await.insert(record.id, record.clone());
        Ok(())
    }

    async fn load(&self, session_id: &Id) -> session_store::Result<Option<Record>> {
        Ok(self
            .0
            .lock()
            .await
            .get(session_id)
            .filter(|record| record.expiry_date > OffsetDateTime::now_utc())
            .cloned())
    }

    async fn delete(&self, session_id: &Id) -> session_store::Result<()> {
        self.0.lock().await.remove(session_id);
        Ok(())
    }
}

impl PurgingMemoryStore {
    async fn purge_expired(&self) {
        let now = OffsetDateTime::now_utc();
        self.0
            .lock()
            .await
            .retain(|_, record| record.expiry_date > now);
    }

    #[cfg(test)]
    async fn record_count(&self) -> usize {
        self.0.lock().await.len()
    }
}

/// Start the lifetime-long cleanup task for the browser session store.
pub(crate) fn spawn_session_purger(store: PurgingMemoryStore) {
    spawn_session_purger_with_interval(store, SESSION_PURGE_INTERVAL);
}

fn spawn_session_purger_with_interval(store: PurgingMemoryStore, interval: Duration) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            store.purge_expired().await;
        }
    });
}

#[cfg(test)]
mod tests {
    use tower_sessions::cookie::time::Duration as TimeDuration;

    use super::*;

    #[tokio::test]
    async fn periodic_purge_removes_expired_records_from_memory() {
        let store = PurgingMemoryStore::default();
        let mut expired_soon = Record {
            id: Id::default(),
            data: Default::default(),
            expiry_date: OffsetDateTime::now_utc() + TimeDuration::milliseconds(10),
        };
        store.create(&mut expired_soon).await.unwrap();
        assert_eq!(store.record_count().await, 1);

        spawn_session_purger_with_interval(store.clone(), Duration::from_millis(20));

        tokio::time::timeout(Duration::from_secs(1), async {
            while store.record_count().await != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("expired session remained in the backing map");
    }
}
