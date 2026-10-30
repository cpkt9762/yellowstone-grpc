//! B-only ingress ledger. Callback hooks never wait for a registry lock.
use {
    crate::plugin::message::{Message as SourceMessage, MessageAccount, SlotStatus},
    anyhow::{ensure, Context, Result},
    std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, Mutex,
        },
    },
};
pub type Bank = (u64, u64);
#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
pub struct HistoryFault {
    pub reason: yellowstone_grpc_proto::prelude::OrderGapReason,
    pub bank: Option<Bank>,
    pub detail: &'static str,
}

pub const RETAINED_BYTES: usize = 64 * 1024 * 1024;
pub const BANK_BYTES: usize = 16 * 1024 * 1024;
pub const BANK_VERSIONS: usize = 65536;
const NODE_BYTES: usize = 512;
const WATCH_BYTES: usize = 4 * 1024 * 1024 + 16 * 1024 + 64 * 1024;
#[derive(Debug, Clone)]
pub struct Plan {
    pub generation: u64,
    pub switch: u64,
    pub keys: Vec<yellowstone_grpc_proto::prelude::OrderKeyCoverage>,
}
#[derive(Debug)]
struct Watch {
    plans: Vec<Plan>,
    end: Option<u64>,
    completed: BTreeSet<Bank>,
    failed: bool,
    sealed_through: u64,
}
impl Watch {
    fn plan(&self, slot: u64) -> Option<&Plan> {
        if self.end.is_some_and(|e| slot >= e) {
            return None;
        }
        self.plans.iter().rev().find(|p| p.switch <= slot)
    }
}
#[derive(Debug, Clone)]
pub struct Version {
    pub ordinal: u64,
    pub account: Arc<MessageAccount>,
}
#[derive(Debug, Default)]
struct BankState {
    inflight: usize,
    count: BTreeMap<[u8; 32], u64>,
    versions: Vec<Version>,
    awaiting: BTreeMap<usize, (u64, [u8; 32])>,
    sealed: bool,
    late: bool,
    bytes: usize,
}
#[derive(Debug, Default)]
struct State {
    next: u64,
    ordinal: u64,
    generation: u64,
    high: u64,
    bytes: usize,
    external_bytes: usize,
    watches: BTreeMap<u64, Watch>,
    banks: BTreeMap<Bank, BankState>,
    authoritative: BTreeMap<u64, u64>,
    floor: u64,
}
#[derive(Debug)]
pub struct HistoryRegistry {
    enabled: AtomicUsize,
    broken: AtomicBool,
    state: Mutex<State>,
    pub limits: crate::plugin::filter::limits::FilterLimitsHistory,
    max_watchers: usize,
}
impl Default for HistoryRegistry {
    fn default() -> Self {
        Self::new(Default::default(), 32)
    }
}
impl HistoryRegistry {
    pub fn new(
        limits: crate::plugin::filter::limits::FilterLimitsHistory,
        max_watchers: usize,
    ) -> Self {
        Self {
            enabled: AtomicUsize::new(0),
            broken: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            limits,
            max_watchers,
        }
    }
    pub fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Arc<RetentionLease>> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        ensure!(
            s.bytes
                .checked_add(bytes)
                .is_some_and(|n| n <= self.limits.retained_bytes),
            "history live allocation resource limit"
        );
        s.bytes += bytes;
        s.external_bytes += bytes;
        Ok(Arc::new(RetentionLease {
            registry: Arc::clone(self),
            bytes,
        }))
    }
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire) != 0
    }
    pub fn register(
        self: &Arc<Self>,
        keys: Vec<yellowstone_grpc_proto::prelude::OrderKeyCoverage>,
        watermark: u64,
    ) -> Result<(HistoryWatch, Plan)> {
        self.register_with(keys, || watermark)
    }
    pub fn register_with(
        self: &Arc<Self>,
        keys: Vec<yellowstone_grpc_proto::prelude::OrderKeyCoverage>,
        sample: impl FnOnce() -> u64,
    ) -> Result<(HistoryWatch, Plan)> {
        ensure!(
            !keys.is_empty()
                && keys
                    .iter()
                    .all(|k| k.owner_exempt_active_from_slot.is_some()),
            "invalid history owner coverage"
        );
        ensure!(
            !self.broken.load(Ordering::Acquire),
            "history ingress is broken"
        );
        ensure!(
            self.max_watchers > 0
                && self.max_watchers <= 32
                && self.limits.retained_bytes > 0
                && self.limits.retained_bytes <= RETAINED_BYTES
                && self.limits.bank_bytes > 0
                && self.limits.bank_bytes <= BANK_BYTES
                && self.limits.bank_versions > 0
                && self.limits.bank_versions <= BANK_VERSIONS,
            "history configured limits exceed hard limits"
        );
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        ensure!(
            s.watches.len() < self.max_watchers
                && s.bytes
                    .checked_add(WATCH_BYTES)
                    .is_some_and(|b| b <= self.limits.retained_bytes),
            "history watcher resource limit"
        );
        let previous_watchers = s.watches.len();
        self.enabled.store(previous_watchers + 1, Ordering::Release);
        s.high = s.high.max(sample());
        let Some(switch) = s.high.checked_add(1) else {
            self.enabled.store(previous_watchers, Ordering::Release);
            anyhow::bail!("history watermark exhausted");
        };
        if keys
            .iter()
            .any(|k| k.owner_exempt_active_from_slot.is_some_and(|a| a > switch))
        {
            self.enabled.store(previous_watchers, Ordering::Release);
            anyhow::bail!("owner coverage starts beyond callback cutover");
        }
        s.generation = s
            .generation
            .checked_add(1)
            .context("history generation exhausted")?;
        let mut keys = keys;
        for key in &mut keys {
            key.active_from_slot = Some(
                switch.max(
                    key.owner_exempt_active_from_slot
                        .context("owner active missing")?,
                ),
            );
        }
        let switch = keys
            .iter()
            .filter_map(|k| k.active_from_slot)
            .max()
            .context("empty history scope")?;
        let plan = Plan {
            generation: s.generation,
            switch,
            keys,
        };
        let id = s.next;
        s.next = s.next.checked_add(1).context("history watcher exhausted")?;
        s.bytes += WATCH_BYTES;
        s.watches.insert(
            id,
            Watch {
                plans: vec![plan.clone()],
                end: None,
                completed: BTreeSet::new(),
                failed: false,
                sealed_through: 0,
            },
        );
        self.enabled.store(s.watches.len(), Ordering::Release);
        Ok((
            HistoryWatch {
                registry: Arc::clone(self),
                id,
            },
            plan,
        ))
    }
    // A callback can race with registration. Both update high under this lock; a
    // contended hook invalidates B instead of waiting or changing the A result.
    pub fn enter(
        self: &Arc<Self>,
        slot: u64,
        bank_id: Option<u64>,
        key: Option<[u8; 32]>,
        dead: bool,
        authoritative: bool,
        data: bool,
    ) -> Ingress {
        if !self.enabled() {
            return Ingress::default();
        }
        let mut s = match self.state.try_lock() {
            Ok(s) => s,
            Err(_) => {
                self.broken.store(true, Ordering::Release);
                return Ingress::default();
            }
        };
        s.high = s.high.max(slot);
        let ordinal = s.ordinal;
        let Some(next) = ordinal.checked_add(1) else {
            self.broken.store(true, Ordering::Release);
            return Ingress::default();
        };
        s.ordinal = next;
        if dead {
            for (b, v) in &mut s.banks {
                if b.0 == slot {
                    v.late = true;
                }
            }
        }
        let Some(id) = bank_id else {
            return Ingress::default();
        };
        let bank = (slot, id);
        if authoritative {
            if s.authoritative.get(&slot).is_some_and(|old| *old != id) {
                for (b, v) in &mut s.banks {
                    if b.0 == slot {
                        v.late = true;
                    }
                }
            }
            if !s.authoritative.contains_key(&slot) {
                if s.bytes + NODE_BYTES > self.limits.retained_bytes {
                    self.broken.store(true, Ordering::Release);
                } else {
                    s.authoritative.insert(slot, id);
                    s.bytes += NODE_BYTES;
                }
            }
        }
        if data {
            if let Some(b) = s.banks.get_mut(&bank) {
                if b.sealed {
                    b.late = true;
                }
            }
        }
        let covered = s
            .watches
            .values()
            .any(|w| !w.failed && w.plan(slot).is_some());
        if !covered {
            return Ingress::default();
        }
        if slot < s.floor {
            self.broken.store(true, Ordering::Release);
            return Ingress::default();
        }
        let after_epoch_seal = (data || !s.banks.contains_key(&bank))
            && s.watches
                .values()
                .any(|w| w.plan(slot).is_some() && slot < w.sealed_through);
        if !s.banks.contains_key(&bank) {
            if s.banks.len() >= 16384 || s.bytes + NODE_BYTES > self.limits.retained_bytes {
                self.broken.store(true, Ordering::Release);
                return Ingress::default();
            }
            s.banks.insert(bank, BankState::default());
            s.bytes += NODE_BYTES;
        }
        let tracked = key.filter(|k| {
            s.watches.values().any(|w| {
                !w.failed
                    && w.plan(slot).is_some_and(|p| {
                        p.keys.iter().any(|c| {
                            c.pubkey.as_slice() == k
                                && c.active_from_slot.is_some_and(|a| a <= slot)
                        })
                    })
            })
        });
        // Reserve bookkeeping before allocation. Each node is conservatively charged.
        if tracked.is_some() && s.bytes + 2 * NODE_BYTES > self.limits.retained_bytes {
            self.broken.store(true, Ordering::Release);
            return Ingress::default();
        }
        if tracked.is_some() {
            s.bytes += 2 * NODE_BYTES;
        }
        let Some(b) = s.banks.get_mut(&bank) else {
            self.broken.store(true, Ordering::Release);
            return Ingress::default();
        };
        if after_epoch_seal {
            b.late = true;
        }
        if !data {
            return Ingress::default();
        }
        if b.sealed {
            b.late = true;
        }
        b.inflight += 1;
        if let Some(key) = tracked {
            let count = b.count.entry(key).or_default();
            if let Some(next) = count.checked_add(1) {
                *count = next;
            } else {
                self.broken.store(true, Ordering::Release);
            }
            b.bytes += 2 * NODE_BYTES;
            if b.count.values().copied().sum::<u64>() > self.limits.bank_versions as u64
                || b.bytes > self.limits.bank_bytes
            {
                self.broken.store(true, Ordering::Release);
            }
        }
        Ingress {
            registry: Some(Arc::clone(self)),
            bank,
            ordinal,
            key: tracked,
            attached: false,
        }
    }
    pub fn retain(&self, account: &Arc<MessageAccount>) {
        if !self.enabled() {
            return;
        }
        let result = (|| -> Result<()> {
            let mut s = self
                .state
                .try_lock()
                .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
            let bank = (
                account.slot,
                account.bank_id.context("history account Bank missing")?,
            );
            let ptr = Arc::as_ptr(account) as usize;
            let Some(b) = s.banks.get(&bank) else {
                return Ok(());
            };
            if b.sealed {
                if let Some(b) = s.banks.get_mut(&bank) {
                    b.late = true;
                }
                return Ok(());
            }
            let Some((ordinal, _)) = b.awaiting.get(&ptr).copied() else {
                return Ok(());
            };
            let charge = account
                .account
                .data
                .len()
                .checked_mul(2)
                .and_then(|n| n.checked_add(2048))
                .context("history charge overflow")?;
            ensure!(
                s.bytes
                    .checked_add(charge)
                    .is_some_and(|n| n <= self.limits.retained_bytes),
                "global history retained limit"
            );
            ensure!(
                b.bytes
                    .checked_add(charge)
                    .is_some_and(|n| n <= self.limits.bank_bytes),
                "history Bank byte limit"
            );
            // Pre-encoded ordinary account storage can be populated after this point;
            // the data*2 allowance includes that possible B-prolonged allocation.
            s.bytes += charge;
            let b = s.banks.get_mut(&bank).context("history Bank missing")?;
            b.bytes += charge;
            b.awaiting.remove(&ptr);
            b.versions.try_reserve_exact(1)?;
            if let Some(old) = b.versions.iter().find(|v| {
                v.account.account.pubkey == account.account.pubkey
                    && v.account.account.write_version == account.account.write_version
            }) {
                let a = &old.account.account;
                let b = &account.account;
                ensure!(
                    a.pubkey == b.pubkey
                        && a.owner == b.owner
                        && a.lamports == b.lamports
                        && a.executable == b.executable
                        && a.rent_epoch == b.rent_epoch
                        && a.data == b.data
                        && a.write_version == b.write_version
                        && a.txn_signature == b.txn_signature,
                    "history source conflict: duplicate version differs"
                );
            }
            b.versions.push(Version {
                ordinal,
                account: Arc::clone(account),
            });
            Ok(())
        })();
        if let Err(error) = result {
            log::warn!("tracked history retention failed: {error}");
            self.broken.store(true, Ordering::Release);
        }
    }
    pub fn fence(&self, bank: Bank) {
        if !self.enabled() {
            return;
        }
        match self.state.try_lock() {
            Ok(mut s) => {
                if let Some(b) = s.banks.get_mut(&bank) {
                    if b.inflight != 0
                        || !b.awaiting.is_empty()
                        || b.count.values().sum::<u64>() != b.versions.len() as u64
                    {
                        b.late = true;
                    }
                    b.sealed = true;
                }
            }
            Err(_) => self.broken.store(true, Ordering::Release),
        }
    }
    pub fn authoritative(&self, bank: Bank) {
        if !self.enabled() {
            return;
        }
        match self.state.try_lock() {
            Ok(mut s) => {
                if s.authoritative.get(&bank.0).is_some_and(|id| *id != bank.1) {
                    for (b, v) in &mut s.banks {
                        if b.0 == bank.0 {
                            v.late = true;
                        }
                    }
                } else if !s.authoritative.contains_key(&bank.0) {
                    if s.bytes + NODE_BYTES > self.limits.retained_bytes {
                        self.broken.store(true, Ordering::Release);
                    } else {
                        s.bytes += NODE_BYTES;
                        s.authoritative.insert(bank.0, bank.1);
                    }
                }
            }
            Err(_) => self.broken.store(true, Ordering::Release),
        }
    }
    pub fn observe_source(&self, message: &SourceMessage) {
        if !self.enabled() {
            return;
        }
        if let SourceMessage::EntryUpdateParent(update) = message {
            match self.state.try_lock() {
                Ok(mut s) => {
                    if let Some(b) = s.banks.get_mut(&(
                        update.update_parent.slot,
                        update.update_parent.cleared_bank_id,
                    )) {
                        b.late = true;
                    }
                }
                Err(_) => self.broken.store(true, Ordering::Release),
            }
        }
        if let SourceMessage::Slot(slot) = message {
            if slot.status == SlotStatus::Dead {
                match self.state.try_lock() {
                    Ok(mut s) => {
                        for (bank, b) in &mut s.banks {
                            if bank.0 == slot.slot {
                                b.late = true;
                            }
                        }
                    }
                    Err(_) => self.broken.store(true, Ordering::Release),
                }
            }
        }
    }
    pub fn gc(&self, floor: u64) {
        if !self.enabled() {
            return;
        }
        match self.state.try_lock() {
            Ok(mut s) => {
                s.floor = s.floor.max(floor);
                if s.banks.iter().any(|(bank, b)| bank.0 < floor && !b.sealed) {
                    self.broken.store(true, Ordering::Release);
                }
            }
            Err(_) => self.broken.store(true, Ordering::Release),
        }
    }
}
#[derive(Debug, Default)]
pub struct Ingress {
    registry: Option<Arc<HistoryRegistry>>,
    bank: Bank,
    ordinal: u64,
    key: Option<[u8; 32]>,
    attached: bool,
}
impl Ingress {
    pub fn attach(&mut self, account: &Arc<MessageAccount>) {
        let (Some(registry), Some(key)) = (&self.registry, self.key) else {
            return;
        };
        match registry.state.try_lock() {
            Ok(mut s) => {
                if let Some(b) = s.banks.get_mut(&self.bank) {
                    b.awaiting
                        .insert(Arc::as_ptr(account) as usize, (self.ordinal, key));
                    self.attached = true;
                }
            }
            Err(_) => registry.broken.store(true, Ordering::Release),
        }
    }
}
impl Drop for Ingress {
    fn drop(&mut self) {
        let Some(r) = &self.registry else {
            return;
        };
        match r.state.try_lock() {
            Ok(mut s) => {
                if let Some(b) = s.banks.get_mut(&self.bank) {
                    b.inflight = b.inflight.saturating_sub(1);
                    if self.key.is_some() && !self.attached {
                        b.late = true;
                    }
                }
            }
            Err(_) => r.broken.store(true, Ordering::Release),
        }
    }
}
#[derive(Debug)]
pub struct HistoryWatch {
    pub registry: Arc<HistoryRegistry>,
    id: u64,
}
impl HistoryWatch {
    pub fn check(&self) -> Result<()> {
        if self.registry.broken.load(Ordering::Acquire) {
            return Err(HistoryFault {
                reason: yellowstone_grpc_proto::prelude::OrderGapReason::ResourceLimit,
                bank: None,
                detail: "history ingress/resource failure",
            }
            .into());
        }
        let s = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        let w = s.watches.get(&self.id).context("history watcher missing")?;
        if let Some((bank, _)) = s
            .banks
            .iter()
            .find(|(bank, b)| b.late && w.plan(bank.0).is_some())
        {
            return Err(HistoryFault {
                reason: yellowstone_grpc_proto::prelude::OrderGapReason::SourceAfterSeal,
                bank: Some(*bank),
                detail: "history late/conflicting/incomplete Bank",
            }
            .into());
        }
        Ok(())
    }
    pub fn generation_for(&self, bank: Bank) -> Result<Option<u64>> {
        let s = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        Ok(s.watches
            .get(&self.id)
            .and_then(|w| w.plan(bank.0))
            .map(|p| p.generation))
    }
    pub fn replace(
        &self,
        keys: Vec<yellowstone_grpc_proto::prelude::OrderKeyCoverage>,
        watermark: u64,
    ) -> Result<Plan> {
        self.replace_with(keys, || watermark)
    }
    pub fn replace_with(
        &self,
        mut keys: Vec<yellowstone_grpc_proto::prelude::OrderKeyCoverage>,
        sample: impl FnOnce() -> u64,
    ) -> Result<Plan> {
        self.check()?;
        let mut s = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        s.high = s.high.max(sample());
        let switch = s.high.checked_add(1).context("cutover exhausted")?;
        let old = s
            .watches
            .get(&self.id)
            .and_then(|w| w.plans.last())
            .context("history scope missing")?;
        for key in &mut keys {
            key.active_from_slot = Some(
                old.keys
                    .iter()
                    .find(|k| k.pubkey == key.pubkey)
                    .and_then(|k| k.active_from_slot)
                    .unwrap_or(
                        switch.max(
                            key.owner_exempt_active_from_slot
                                .context("owner active missing")?,
                        ),
                    ),
            );
        }
        s.generation = s
            .generation
            .checked_add(1)
            .context("generation exhausted")?;
        let p = Plan {
            generation: s.generation,
            switch,
            keys,
        };
        ensure!(
            s.bytes + keys_charge(&p) <= self.registry.limits.retained_bytes,
            "history plan resource limit"
        );
        s.bytes += keys_charge(&p);
        s.watches
            .get_mut(&self.id)
            .context("watcher missing")?
            .plans
            .push(p.clone());
        Ok(p)
    }
    pub fn close(&self, watermark: u64) -> Result<u64> {
        self.close_with(|| watermark)
    }
    pub fn close_with(&self, sample: impl FnOnce() -> u64) -> Result<u64> {
        self.check()?;
        let mut s = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        s.high = s.high.max(sample());
        let end = s.high.checked_add(1).context("close cutoff exhausted")?;
        s.watches.get_mut(&self.id).context("watcher missing")?.end = Some(end);
        Ok(end)
    }
    pub fn pending(&self, before: u64) -> Result<bool> {
        self.check()?;
        let s = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        let w = s.watches.get(&self.id).context("watcher missing")?;
        Ok(s.banks
            .keys()
            .any(|bank| bank.0 < before && w.plan(bank.0).is_some() && !w.completed.contains(bank)))
    }
    pub fn versions(&self, bank: Bank, plan: &Plan) -> Result<(Vec<Version>, u64)> {
        self.check()?;
        let s = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        let b = s
            .banks
            .get(&bank)
            .context("Bank absent from callback ingress (partial Bank)")?;
        ensure!(
            b.sealed && b.inflight == 0 && b.awaiting.is_empty(),
            "Bank missing ingress fence"
        );
        let matches = |key: &[u8]| {
            plan.keys
                .iter()
                .any(|k| k.pubkey == key && k.active_from_slot.is_some_and(|a| a <= bank.0))
        };
        let count = b
            .count
            .iter()
            .filter(|(key, _)| matches(key.as_slice()))
            .map(|(_, n)| *n)
            .sum::<u64>();
        let mut versions = b
            .versions
            .iter()
            .filter(|v| matches(v.account.account.pubkey.as_ref()))
            .cloned()
            .collect::<Vec<_>>();
        ensure!(
            count == versions.len() as u64,
            "independent callback count mismatch"
        );
        versions.sort_by_key(|v| v.ordinal);
        Ok((versions, count))
    }
    pub fn seal_through(&self, cutoff: u64) -> Result<()> {
        self.check()?;
        let mut s = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        let w = s.watches.get(&self.id).context("watcher missing")?;
        ensure!(
            !s.banks.keys().any(|bank| bank.0 < cutoff
                && w.plan(bank.0).is_some()
                && !w.completed.contains(bank)),
            "callback entered before epoch seal fence"
        );
        s.watches
            .get_mut(&self.id)
            .context("watcher missing")?
            .sealed_through = cutoff;
        Ok(())
    }
    pub fn stop(&self) -> Result<()> {
        let mut s = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        // This is failure cleanup, never a successful coverage cutoff.
        let end = s.high.saturating_add(1);
        let w = s.watches.get_mut(&self.id).context("watcher missing")?;
        w.failed = true;
        w.end.get_or_insert(end);
        let done = s
            .banks
            .keys()
            .copied()
            .filter(|bank| {
                s.watches
                    .values()
                    .all(|w| w.failed || w.plan(bank.0).is_none() || w.completed.contains(bank))
            })
            .collect::<Vec<_>>();
        for bank in done {
            if let Some(b) = s.banks.get_mut(&bank) {
                let released = b.bytes;
                b.versions = Vec::new();
                b.count.clear();
                b.awaiting.clear();
                b.bytes = 0;
                b.sealed = true;
                s.bytes = s.bytes.saturating_sub(released);
            }
        }
        Ok(())
    }
    pub fn complete(&self, bank: Bank) -> Result<()> {
        let mut s = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        ensure!(
            s.bytes + NODE_BYTES <= self.registry.limits.retained_bytes,
            "history completed index limit"
        );
        s.bytes += NODE_BYTES;
        s.watches
            .get_mut(&self.id)
            .context("watcher missing")?
            .completed
            .insert(bank);
        // Drop B's raw ownership once every interested physical stream consumed it.
        let done = s
            .watches
            .values()
            .all(|w| w.failed || w.plan(bank.0).is_none() || w.completed.contains(&bank));
        if done {
            if let Some(b) = s.banks.get_mut(&bank) {
                let released = b.bytes;
                b.versions = Vec::new();
                b.count.clear();
                b.bytes = 0;
                s.bytes = s.bytes.saturating_sub(released);
            }
        }
        Ok(())
    }
}
const fn keys_charge(plan: &Plan) -> usize {
    NODE_BYTES + plan.keys.len() * 256
}
impl Drop for HistoryWatch {
    fn drop(&mut self) {
        match self.registry.state.lock() {
            Ok(mut s) => {
                if let Some(w) = s.watches.remove(&self.id) {
                    let charge = WATCH_BYTES
                        + w.plans.iter().skip(1).map(keys_charge).sum::<usize>()
                        + w.completed.len() * NODE_BYTES;
                    s.bytes = s.bytes.saturating_sub(charge);
                }
                self.registry
                    .enabled
                    .store(s.watches.len(), Ordering::Release);
                if s.watches.is_empty() {
                    let external_bytes = s.external_bytes;
                    *s = State {
                        bytes: external_bytes,
                        external_bytes,
                        ..Default::default()
                    };
                    self.registry.broken.store(false, Ordering::Release);
                }
            }
            Err(_) => self.registry.broken.store(true, Ordering::Release),
        }
    }
}

#[derive(Debug)]
pub struct RetentionLease {
    registry: Arc<HistoryRegistry>,
    bytes: usize,
}
impl Drop for RetentionLease {
    fn drop(&mut self) {
        match self.registry.state.lock() {
            Ok(mut s) => {
                s.bytes = s.bytes.saturating_sub(self.bytes);
                s.external_bytes = s.external_bytes.saturating_sub(self.bytes);
            }
            Err(_) => self.registry.broken.store(true, Ordering::Release),
        }
    }
}

#[cfg(test)]
mod tracked_account_history_v1 {
    use super::*;
    use crate::plugin::filter::fixtures;
    use solana_pubkey::Pubkey;
    use yellowstone_grpc_proto::prelude::OrderKeyCoverage;
    fn setup() -> (Arc<HistoryRegistry>, HistoryWatch, Plan) {
        let r = Arc::new(HistoryRegistry::default());
        let (w, p) = r
            .register(
                vec![OrderKeyCoverage {
                    pubkey: vec![1; 32],
                    active_from_slot: None,
                    owner_exempt_active_from_slot: Some(0),
                }],
                99,
            )
            .expect("history registration");
        (r, w, p)
    }
    fn account(wv: u64) -> Arc<MessageAccount> {
        let mut a =
            fixtures::simple_message_account(Pubkey::new_from_array([1; 32]), Pubkey::default());
        Arc::make_mut(&mut a).account.write_version = wv;
        a
    }
    fn enqueue(r: &Arc<HistoryRegistry>, a: &Arc<MessageAccount>, retain: bool) {
        let mut g = r.enter(
            a.slot,
            a.bank_id,
            Some(a.account.pubkey.to_bytes()),
            false,
            false,
            true,
        );
        g.attach(a);
        drop(g);
        if retain {
            r.retain(a);
        }
    }
    #[test]
    fn independent_count_detects_queue_or_decoder_drop() {
        let (r, w, p) = setup();
        enqueue(&r, &account(1), true);
        enqueue(&r, &account(2), false);
        r.fence((100, 100));
        assert!(w.versions((100, 100), &p).is_err());
        assert!(w.check().is_err());
    }
    #[test]
    fn inflight_before_created_bank_cannot_be_certified() {
        let (r, w, p) = setup();
        let a = account(1);
        let mut guard = r.enter(100, Some(100), Some([1; 32]), false, false, true);
        guard.attach(&a);
        r.fence((100, 100));
        assert!(w.versions((100, 100), &p).is_err());
        drop(guard);
        r.retain(&a);
        assert!(w.check().is_err());
    }
    #[test]
    fn registration_contention_is_sticky_and_does_not_wait() {
        let (r, w, _) = setup();
        let locked = r.state.lock().expect("hold registration lock");
        drop(r.enter(100, Some(100), Some([1; 32]), false, false, true));
        drop(locked);
        assert!(w.check().is_err());
    }
    #[test]
    fn repeated_identical_version_is_counted_and_conflicting_value_rejected() {
        let (r, w, p) = setup();
        let a = account(1);
        enqueue(&r, &a, true);
        let b = Arc::new(a.as_ref().clone());
        enqueue(&r, &b, true);
        r.fence((100, 100));
        let (v, count) = w
            .versions((100, 100), &p)
            .expect("full duplicate callbacks");
        assert_eq!((v.len(), count), (2, 2));
        let (r, w, _) = setup();
        enqueue(&r, &a, true);
        let mut b = a.as_ref().clone();
        b.account.lamports += 1;
        enqueue(&r, &Arc::new(b), true);
        assert!(w.check().is_err());
    }
    #[test]
    fn gc_floor_cannot_revive_old_bank_and_sealed_late_is_sticky() {
        let (r, w, _) = setup();
        enqueue(&r, &account(1), true);
        r.fence((100, 100));
        w.complete((100, 100)).expect("complete");
        r.gc(101);
        drop(r.enter(100, Some(100), Some([1; 32]), false, false, true));
        assert!(w.check().is_err());
    }
    #[test]
    fn competing_authoritative_banks_and_dead_revoke() {
        let (r, w, _) = setup();
        drop(r.enter(100, Some(100), None, false, true, false));
        drop(r.enter(100, Some(101), None, false, true, false));
        assert!(w.check().is_err());
        let (r, w, _) = setup();
        enqueue(&r, &account(1), true);
        drop(r.enter(100, None, None, true, false, false));
        assert!(w.check().is_err());
    }
    #[test]
    fn partial_bank_without_ingress_is_rejected_and_zero_bank_is_sealed() {
        let (r, w, p) = setup();
        assert!(w.versions((100, 100), &p).is_err());
        drop(r.enter(100, Some(100), None, false, false, false));
        r.fence((100, 100));
        let (v, count) = w.versions((100, 100), &p).expect("zero callback Bank");
        assert_eq!((v.len(), count), (0, 0));
    }
    #[test]
    fn replacement_pending_is_bounded_by_fixed_cutover() {
        let (r, w, p) = setup();
        drop(r.enter(100, Some(100), None, false, false, false));
        let q = w
            .replace(
                vec![
                    p.keys[0].clone(),
                    OrderKeyCoverage {
                        pubkey: vec![2; 32],
                        active_from_slot: None,
                        owner_exempt_active_from_slot: Some(0),
                    },
                ],
                100,
            )
            .expect("replacement");
        assert_eq!(q.switch, 101);
        drop(r.enter(101, Some(101), None, false, false, false));
        assert!(w.pending(101).expect("old pending"));
        r.fence((100, 100));
        w.complete((100, 100)).expect("old consumed");
        assert!(!w.pending(101).expect("new-side Bank cannot delay cutover"));
        assert!(w.pending(102).expect("new-side Bank still pending"));
    }
    #[test]
    fn all_watchers_share_live_budget_and_queued_ownership_outlives_watch() {
        let (r, w, _) = setup();
        let remaining = RETAINED_BYTES - r.state.lock().expect("ledger").bytes;
        let lease = r.reserve(remaining).expect("fill global limit");
        assert!(r.reserve(1).is_err());
        drop(w);
        assert_eq!(r.state.lock().expect("ledger").external_bytes, remaining);
        assert!(r.reserve(RETAINED_BYTES - remaining + 1).is_err());
        drop(lease);
        assert_eq!(r.state.lock().expect("ledger").bytes, 0);
    }
    #[test]
    fn fresh_watermark_is_sampled_after_ingress_gate_and_sealed_range_rejects_unseen_bank() {
        let registry = Arc::new(HistoryRegistry::default());
        let (watch, plan) = registry
            .register_with(
                vec![OrderKeyCoverage {
                    pubkey: vec![1; 32],
                    active_from_slot: None,
                    owner_exempt_active_from_slot: Some(0),
                }],
                || {
                    assert!(registry.enabled());
                    130
                },
            )
            .expect("fresh barrier");
        assert_eq!(plan.switch, 131);
        drop(registry.enter(131, Some(998), None, false, false, false));
        registry.fence((131, 998));
        watch.complete((131, 998)).expect("zero Bank delivered");
        watch.seal_through(132).expect("epoch fence");
        drop(registry.enter(131, Some(998), None, false, true, false));
        watch
            .check()
            .expect("ordinary later Rooted for the known Bank is not a data callback");
        drop(registry.enter(131, Some(999), None, false, false, false));
        assert!(
            watch.check().is_err(),
            "first sighting below an already sealed cutoff is late"
        );
    }
    #[test]
    fn failed_watch_releases_raw_and_stops_new_coverage_without_clearing_tombstones() {
        let (r, w, _) = setup();
        enqueue(&r, &account(1), true);
        r.fence((100, 100));
        let before = r.state.lock().expect("ledger").bytes;
        w.stop().expect("stop B");
        assert!(r.state.lock().expect("ledger").bytes < before);
        drop(r.enter(101, Some(101), None, false, false, false));
        assert!(!r
            .state
            .lock()
            .expect("ledger")
            .banks
            .contains_key(&(101, 101)));
        drop(r.enter(100, Some(100), Some([1; 32]), false, false, true));
        assert!(
            w.check().is_err(),
            "stopping admission preserves old late evidence"
        );
    }
    #[test]
    fn configured_bank_limits_are_enforced_and_hard_limits_cannot_be_increased() {
        let limits = crate::plugin::filter::limits::FilterLimitsHistory {
            bank_versions: 1,
            ..Default::default()
        };
        let r = Arc::new(HistoryRegistry::new(limits, 32));
        let (w, _) = r
            .register(
                vec![OrderKeyCoverage {
                    pubkey: vec![1; 32],
                    active_from_slot: None,
                    owner_exempt_active_from_slot: Some(0),
                }],
                99,
            )
            .expect("bounded registration");
        enqueue(&r, &account(1), true);
        enqueue(&r, &account(2), true);
        assert!(w.check().is_err());
        let r = Arc::new(HistoryRegistry::new(
            crate::plugin::filter::limits::FilterLimitsHistory {
                retained_bytes: RETAINED_BYTES + 1,
                ..Default::default()
            },
            32,
        ));
        assert!(r
            .register(
                vec![OrderKeyCoverage {
                    pubkey: vec![1; 32],
                    active_from_slot: None,
                    owner_exempt_active_from_slot: Some(0)
                }],
                99
            )
            .is_err());
    }
}
