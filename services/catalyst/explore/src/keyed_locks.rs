//! Per-key exclusion with registry entries retained only by active claims.
use parking_lot::Mutex;
use std::{collections::HashMap, hash::Hash, sync::Arc};

pub(crate) struct KeyedLocks<K> {
    entries: Mutex<HashMap<K, Arc<tokio::sync::Mutex<()>>>>,
}

impl<K> Default for KeyedLocks<K> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Clone + Eq + Hash> KeyedLocks<K> {
    pub(crate) fn claim(&self, key: &K) -> Claim<'_, K> {
        let lock = self.entries.lock().entry(key.clone()).or_default().clone();
        Claim {
            registry: self,
            key: key.clone(),
            lock: Some(lock),
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().len()
    }
}

pub(crate) struct Claim<'a, K: Eq + Hash> {
    registry: &'a KeyedLocks<K>,
    key: K,
    lock: Option<Arc<tokio::sync::Mutex<()>>>,
}

impl<K: Eq + Hash> Claim<'_, K> {
    // Borrowing the claim keeps registry cleanup outside the lock guard's lifetime.
    pub(crate) async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock.as_ref().expect("live claim").lock().await
    }
}

impl<K: Eq + Hash> Drop for Claim<'_, K> {
    fn drop(&mut self) {
        let mut entries = self.registry.entries.lock();
        let lock = self.lock.take().expect("live claim");
        let same_entry = entries
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &lock));
        // Drop our reference while holding the registry lock: concurrent claim
        // drops must observe one another before deciding who removes the entry.
        drop(lock);
        if same_entry
            && entries
                .get(&self.key)
                .is_some_and(|entry| Arc::strong_count(entry) == 1)
        {
            entries.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_waiter_and_owner_preserve_remaining_waiters() {
        let locks = KeyedLocks::default();
        let mut owner = Box::pin(async {
            let claim = locks.claim(&"same");
            let _guard = claim.lock().await;
            std::future::pending::<()>().await;
        });
        assert!(futures::poll!(&mut owner).is_pending());
        let mut cancelled = Box::pin(async {
            let claim = locks.claim(&"same");
            let _guard = claim.lock().await;
        });
        assert!(futures::poll!(&mut cancelled).is_pending());
        let waiter = locks.claim(&"same");
        let mut waiting = Box::pin(waiter.lock());
        assert!(futures::poll!(&mut waiting).is_pending());
        drop(cancelled);
        assert_eq!(locks.len(), 1);
        drop(owner);
        assert_eq!(locks.len(), 1);
        let held = waiting.await;
        let newcomer = locks.claim(&"same");
        assert!(Arc::ptr_eq(
            waiter.lock.as_ref().unwrap(),
            newcomer.lock.as_ref().unwrap()
        ));
        let mut next = Box::pin(newcomer.lock());
        assert!(futures::poll!(&mut next).is_pending());
        let other = locks.claim(&"other");
        let independent = other.lock().await;
        drop(independent);
        drop(other);
        assert_eq!(locks.len(), 1);
        drop(held);
        drop(waiter);
        let held = next.await;
        drop(held);
        drop(newcomer);
        assert_eq!(locks.len(), 0);
    }

    #[tokio::test]
    async fn cancelling_active_owner_removes_last_entry() {
        let locks = KeyedLocks::default();
        let mut owner = Box::pin(async {
            let claim = locks.claim(&"same");
            let _guard = claim.lock().await;
            std::future::pending::<()>().await;
        });
        assert!(futures::poll!(&mut owner).is_pending());
        assert_eq!(locks.len(), 1);
        drop(owner);
        assert_eq!(locks.len(), 0);
    }
}
