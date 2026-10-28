//! B-only sealed-bank references survive the replay buffer's GC.
use {
    crate::plugin::filter::limits::FilterLimitsOrder,
    anyhow::{ensure, Result},
    std::{
        collections::{HashMap, HashSet},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
    },
};
pub type BankRef = (u64, u64);
#[derive(Debug, Default)]
struct BankWatch {
    epoch: Option<Vec<u8>>,
    late: bool,
    reported: bool,
}
#[derive(Debug, Default)]
struct WatchState {
    banks: HashMap<BankRef, BankWatch>,
    pending: HashMap<BankRef, std::time::Instant>,
    overflow: bool,
}
#[derive(Debug, Default)]
struct State {
    next: u64,
    watchers: HashMap<u64, WatchState>,
    notifications: HashSet<BankRef>,
}
#[derive(Debug)]
pub struct OrderIntegrity {
    state: Mutex<State>,
    pub limits: FilterLimitsOrder,
    broken: AtomicBool,
}
impl OrderIntegrity {
    pub fn new(limits: FilterLimitsOrder) -> Self {
        Self {
            state: Mutex::new(State::default()),
            limits,
            broken: AtomicBool::new(false),
        }
    }
    pub fn register(self: &Arc<Self>) -> Result<OrderWatch> {
        ensure!(
            !self.broken.load(Ordering::SeqCst),
            "B integrity source failed"
        );
        ensure!(
            self.limits.control_bytes >= 4096,
            "B control reserve must fit a complete gap"
        );
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
        ensure!(
            s.watchers.len() < self.limits.max_watchers,
            "B watcher resource limit"
        );
        let id = s.next;
        s.next = s
            .next
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("watcher id exhausted"))?;
        s.watchers.insert(id, WatchState::default());
        Ok(OrderWatch {
            owner: Arc::clone(self),
            id,
        })
    }
    pub fn register_bank(&self, bank: BankRef) {
        let result = (|| -> Result<()> {
            let mut s = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
            for watcher in s.watchers.values_mut() {
                watcher.pending.remove(&bank);
                if watcher.banks.contains_key(&bank) {
                    continue;
                }
                if watcher.banks.len() >= self.limits.max_banks_per_stream {
                    watcher.overflow = true;
                    continue;
                }
                watcher.banks.insert(bank, BankWatch::default());
            }
            Ok(())
        })();
        self.record_error(result);
    }
    pub fn note_pending(&self, bank: BankRef) {
        let result = (|| -> Result<()> {
            let mut s = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
            for watcher in s.watchers.values_mut() {
                if !watcher.banks.contains_key(&bank) {
                    if watcher.pending.len() >= self.limits.max_banks_per_stream {
                        watcher.overflow = true;
                        continue;
                    }
                    watcher
                        .pending
                        .entry(bank)
                        .or_insert_with(std::time::Instant::now);
                }
            }
            Ok(())
        })();
        self.record_error(result);
    }
    /// Called at the original drop sites and before GC could revive a late bank.
    pub fn note_late(&self, bank: BankRef) {
        let result = (|| -> Result<()> {
            let mut s = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
            let mut new = false;
            for watcher in s.watchers.values_mut() {
                if let Some(entry) = watcher.banks.get_mut(&bank) {
                    if !entry.late {
                        entry.late = true;
                        new = true;
                    }
                }
            }
            if new {
                ensure!(
                    s.notifications.len() < self.limits.max_notifications,
                    "B late notification budget exhausted"
                );
                s.notifications.insert(bank);
            }
            Ok(())
        })();
        self.record_error(result);
    }
    pub fn take_notifications(&self) -> Vec<BankRef> {
        match self.state.lock() {
            Ok(mut s) => s.notifications.drain().collect(),
            Err(error) => {
                log::error!("B integrity lock error: {error}");
                self.broken.store(true, Ordering::SeqCst);
                Vec::new()
            }
        }
    }
    fn record_error(&self, result: Result<()>) {
        if let Err(error) = result {
            log::error!("B integrity stopped: {error}");
            self.broken.store(true, Ordering::SeqCst);
        }
    }
}
#[derive(Debug)]
pub struct OrderWatch {
    owner: Arc<OrderIntegrity>,
    id: u64,
}
impl OrderWatch {
    pub fn pending(&self, floor: u64) -> Result<bool> {
        let s = self
            .owner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
        Ok(s.watchers
            .get(&self.id)
            .is_some_and(|w| w.pending.keys().any(|(slot, _)| *slot >= floor)))
    }
    pub fn incomplete(&self, floor: u64) -> Result<bool> {
        let s = self
            .owner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
        Ok(s.watchers.get(&self.id).is_some_and(|w| {
            w.pending.iter().any(|((slot, _), start)| {
                *slot >= floor && start.elapsed() >= std::time::Duration::from_secs(15)
            })
        }))
    }

    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.owner.broken.load(Ordering::SeqCst),
            "B integrity source failed"
        );
        let s = self
            .owner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
        let w = s
            .watchers
            .get(&self.id)
            .ok_or_else(|| anyhow::anyhow!("B watcher missing"))?;
        ensure!(!w.overflow, "B sealed-bank reference resource limit");
        Ok(())
    }
    pub fn bind(&self, bank: BankRef, epoch: &[u8]) -> Result<()> {
        self.check()?;
        let mut s = self
            .owner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
        let entry = s
            .watchers
            .get_mut(&self.id)
            .and_then(|w| w.banks.get_mut(&bank))
            .ok_or_else(|| anyhow::anyhow!("Bank was not registered before broadcast"))?;
        ensure!(
            !entry.late,
            "source data arrived after Bank seal before client consumed it"
        );
        ensure!(entry.epoch.is_none(), "duplicate confirmed Bank group");
        entry.epoch = Some(epoch.to_vec());
        Ok(())
    }
    pub fn pending_lates(&self) -> Result<Vec<BankRef>> {
        let s = self
            .owner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
        Ok(s.watchers
            .get(&self.id)
            .map(|w| {
                w.banks
                    .iter()
                    .filter(|(_, b)| b.late && !b.reported && b.epoch.is_some())
                    .map(|(bank, _)| *bank)
                    .collect()
            })
            .unwrap_or_default())
    }
    pub fn banks_at_slot(&self, slot: u64) -> Result<Vec<BankRef>> {
        let s = self
            .owner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
        Ok(s.watchers
            .get(&self.id)
            .map(|w| {
                w.banks
                    .keys()
                    .filter(|(s, _)| *s == slot)
                    .copied()
                    .collect()
            })
            .unwrap_or_default())
    }
    pub fn epoch(&self, bank: BankRef) -> Result<Option<Vec<u8>>> {
        let s = self
            .owner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
        Ok(s.watchers
            .get(&self.id)
            .and_then(|w| w.banks.get(&bank))
            .and_then(|b| b.epoch.clone()))
    }
    pub fn consume_epoch(&self, bank: BankRef) -> Result<Option<Vec<u8>>> {
        let mut s = self
            .owner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("B integrity mutex poisoned"))?;
        Ok(s.watchers
            .get_mut(&self.id)
            .and_then(|w| w.banks.get_mut(&bank))
            .and_then(|b| {
                if b.reported {
                    return None;
                }
                b.reported = true;
                b.epoch.clone()
            }))
    }
}
impl Drop for OrderWatch {
    fn drop(&mut self) {
        match self.owner.state.lock() {
            Ok(mut s) => {
                s.watchers.remove(&self.id);
            }
            Err(error) => {
                log::error!("B watcher cleanup failed: {error}");
                self.owner.broken.store(true, Ordering::SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod order_writes_v1 {
    use super::*;
    #[test]
    fn references_survive_replay_gc_and_registration_precedes_slow_client() {
        let registry = Arc::new(OrderIntegrity::new(FilterLimitsOrder::default()));
        let watch = registry.register().expect("watch");
        registry.register_bank((1, 7));
        watch.bind((1, 7), &[4; 16]).expect("bind before seal");
        registry.note_late((1, 7));
        assert_eq!(registry.take_notifications(), vec![(1, 7)]);
        assert_eq!(watch.epoch((1, 7)).expect("late epoch"), Some(vec![4; 16]));
        registry.register_bank((2, 8));
        registry.note_late((2, 8));
        assert!(watch.bind((2, 8), &[5; 16]).is_err());
    }
    #[test]
    fn bounds_fail_without_gc_of_promised_references() {
        let registry = Arc::new(OrderIntegrity::new(FilterLimitsOrder {
            max_banks_per_stream: 1,
            ..Default::default()
        }));
        let watch = registry.register().expect("watch");
        registry.register_bank((1, 1));
        watch.bind((1, 1), &[4; 16]).expect("bind");
        registry.register_bank((2, 2));
        assert!(watch.check().is_err());
        registry.note_late((1, 1));
        assert_eq!(
            watch.epoch((1, 1)).expect("retained history"),
            Some(vec![4; 16])
        );
    }
    #[test]
    fn warmup_pending_banks_do_not_become_false_coverage_gaps_or_delay_close() {
        let registry = Arc::new(OrderIntegrity::new(Default::default()));
        let watch = registry.register().expect("watch");
        registry.note_pending((2, 2));
        {
            let mut state = registry.state.lock().expect("state");
            *state
                .watchers
                .get_mut(&watch.id)
                .expect("watcher")
                .pending
                .get_mut(&(2, 2))
                .expect("pending bank") =
                std::time::Instant::now() - std::time::Duration::from_secs(16);
        }
        assert!(!watch.pending(4).expect("warmup is before coverage"));
        assert!(!watch
            .incomplete(4)
            .expect("warmup timeout is not B missing data"));
        assert!(watch.pending(1).expect("inside promised coverage"));
        assert!(watch
            .incomplete(1)
            .expect("promised incomplete Bank must fail"));
    }
}
