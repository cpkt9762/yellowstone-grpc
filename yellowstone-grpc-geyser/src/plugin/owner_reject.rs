//! Fork-local, explicitly subscribed exceptions to the owner drop list.
use {
    dashmap::DashMap,
    solana_pubkey::Pubkey,
    std::{
        collections::{HashMap, HashSet},
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    },
    yellowstone_grpc_proto::prelude::{
        OwnerRejectExemptAccount, SubscribeUpdateOwnerRejectExemption,
    },
};

#[derive(Debug, Default)]
pub struct OwnerRejectRegistry {
    references: DashMap<Pubkey, usize>,
    max_rejected_slot: AtomicU64,
}

impl OwnerRejectRegistry {
    pub fn allows(&self, pubkey: &Pubkey, slot: u64) -> bool {
        // The callback advances max BEFORE checking the table. Registration inserts
        // BEFORE reading max. With SeqCst, a callback that misses the insertion has
        // already published its slot before the registration's load: every dropped
        // write has slot <= r, so writes at slot >= r+1 are admitted. The table's
        // shard lock orders the insertion and lookup; no lock on ordinary owners.
        self.max_rejected_slot.fetch_max(slot, Ordering::SeqCst);
        self.references.contains_key(pubkey)
    }

    fn insert(&self, pubkey: Pubkey) {
        *self.references.entry(pubkey).or_default() += 1;
    }

    fn release(&self, pubkey: &Pubkey) {
        if let dashmap::mapref::entry::Entry::Occupied(mut entry) = self.references.entry(*pubkey) {
            if *entry.get() == 1 {
                entry.remove();
            } else {
                *entry.get_mut() -= 1;
            }
        }
    }
}

/// Owned by ClientSession, surviving request half-close and releasing on disconnect.
pub struct OwnerRejectSubscription {
    registry: Arc<OwnerRejectRegistry>,
    accounts: HashMap<Pubkey, u64>,
}

impl OwnerRejectSubscription {
    pub fn new(registry: Arc<OwnerRejectRegistry>) -> Self {
        Self {
            registry,
            accounts: HashMap::new(),
        }
    }

    pub fn prepare(&self, next: &HashSet<Pubkey>) -> anyhow::Result<OwnerRejectUpdate> {
        let mut update = OwnerRejectUpdate {
            registry: Arc::clone(&self.registry),
            accounts: HashMap::with_capacity(next.len()),
            added: Vec::new(),
        };
        for pubkey in next {
            let active_from = if let Some(active_from) = self.accounts.get(pubkey) {
                *active_from
            } else {
                self.registry.insert(*pubkey);
                update.added.push(*pubkey);
                self.registry
                    .max_rejected_slot
                    .load(Ordering::SeqCst)
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("owner_reject slot watermark exhausted"))?
            };
            update.accounts.insert(*pubkey, active_from);
        }
        Ok(update)
    }

    /// Call only after the consumer has installed the new filter, without an await.
    pub fn commit(&mut self, mut update: OwnerRejectUpdate) {
        for pubkey in self.accounts.keys() {
            if !update.accounts.contains_key(pubkey) {
                self.registry.release(pubkey);
            }
        }
        self.accounts = std::mem::take(&mut update.accounts);
        update.added.clear();
    }

    pub fn notification(
        &self,
        owners: impl Iterator<Item = Pubkey>,
    ) -> SubscribeUpdateOwnerRejectExemption {
        let mut accounts = self
            .accounts
            .iter()
            .map(|(pubkey, active_from_slot)| OwnerRejectExemptAccount {
                pubkey: pubkey.to_string(),
                active_from_slot: *active_from_slot,
            })
            .collect::<Vec<_>>();
        accounts.sort_unstable_by(|a, b| a.pubkey.cmp(&b.pubkey));
        let mut owner_reject = owners.map(|owner| owner.to_string()).collect::<Vec<_>>();
        owner_reject.sort_unstable();
        SubscribeUpdateOwnerRejectExemption {
            accounts,
            owner_reject,
        }
    }
}

impl Drop for OwnerRejectSubscription {
    fn drop(&mut self) {
        for pubkey in self.accounts.keys() {
            self.registry.release(pubkey);
        }
    }
}

/// Rolls back newly acquired references if installation fails or the task exits.
pub struct OwnerRejectUpdate {
    registry: Arc<OwnerRejectRegistry>,
    accounts: HashMap<Pubkey, u64>,
    added: Vec<Pubkey>,
}

impl Drop for OwnerRejectUpdate {
    fn drop(&mut self) {
        for pubkey in &self.added {
            self.registry.release(pubkey);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_reject_reference_counts_and_disconnect_cleanup() {
        let registry = Arc::new(OwnerRejectRegistry::default());
        let key = Pubkey::new_unique();
        let keys = HashSet::from([key]);
        let mut first = OwnerRejectSubscription::new(Arc::clone(&registry));
        let mut second = OwnerRejectSubscription::new(Arc::clone(&registry));
        first.commit(first.prepare(&keys).expect("register first client"));
        second.commit(second.prepare(&keys).expect("register second client"));
        assert_eq!(*registry.references.get(&key).expect("two references"), 2);
        drop(first);
        assert!(registry.allows(&key, 10));
        assert_eq!(
            *registry.references.get(&key).expect("remaining reference"),
            1
        );
        drop(second); // Same guard is owned by ClientSession on disconnect/cancellation.
        assert!(!registry.allows(&key, 11));
        assert!(registry.references.is_empty());
    }

    #[test]
    fn owner_reject_watermark_reuse_add_remove_and_rollback() {
        let registry = Arc::new(OwnerRejectRegistry::default());
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        assert!(!registry.allows(&a, 40));
        let mut client = OwnerRejectSubscription::new(Arc::clone(&registry));
        client.commit(client.prepare(&HashSet::from([a])).expect("register A"));
        assert_eq!(client.accounts[&a], 41);
        assert!(registry.allows(&a, 50));
        client.commit(client.prepare(&HashSet::from([a, b])).expect("add B"));
        assert_eq!(client.accounts[&a], 41);
        assert_eq!(client.accounts[&b], 51);
        client.commit(client.prepare(&HashSet::from([b])).expect("remove A"));
        assert!(!registry.references.contains_key(&a));
        let pending = client
            .prepare(&HashSet::from([a, b]))
            .expect("prepare replacement");
        assert!(registry.references.contains_key(&a));
        drop(pending); // Failed installation or cancelled task rolls back additions.
        assert!(!registry.references.contains_key(&a));
        assert_eq!(client.accounts[&b], 51);
        client.commit(client.prepare(&HashSet::from([a, b])).expect("re-add A"));
        assert_eq!(client.accounts[&a], 51);
        assert_eq!(client.accounts[&b], 51);
    }

    #[test]
    fn owner_reject_callback_between_insert_and_watermark_load_is_visible() {
        let registry = OwnerRejectRegistry::default();
        let key = Pubkey::new_unique();
        assert!(!registry.allows(&key, 70));
        registry.insert(key);
        assert!(registry.allows(&key, 71));
        let active = registry.max_rejected_slot.load(Ordering::SeqCst) + 1;
        assert_eq!(active, 72);
        assert!(registry.allows(&key, active));
        registry.release(&key);
    }

    #[test]
    fn owner_reject_watermark_overflow_rolls_back_registration() {
        let registry = Arc::new(OwnerRejectRegistry::default());
        let key = Pubkey::new_unique();
        assert!(!registry.allows(&key, u64::MAX));
        let client = OwnerRejectSubscription::new(Arc::clone(&registry));
        assert!(client.prepare(&HashSet::from([key])).is_err());
        assert!(registry.references.is_empty());
    }
}
