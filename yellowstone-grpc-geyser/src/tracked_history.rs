//! B-only ingress ledger. Locks cover only bounded in-memory bookkeeping.
//! Bank payloads never share a registry lock with registration or other Banks.
use {
    crate::plugin::message::{Message as SourceMessage, MessageAccount, SlotStatus},
    anyhow::{ensure, Context, Result},
    std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::{Duration, Instant},
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
    _lease: Arc<RetentionLease>,
}
#[derive(Debug)]
struct Watch {
    plans: BTreeMap<u64, Arc<Plan>>,
    end: Option<u64>,
    completed: BTreeSet<Bank>,
    failed: bool,
    sealed_through: u64,
    lease: Arc<RetentionLease>,
}
impl Watch {
    fn plan(&self, slot: u64) -> Option<&Plan> {
        if self.end.is_some_and(|e| slot >= e) {
            return None;
        }
        self.plans
            .values()
            .rev()
            .find(|p| p.switch <= slot)
            .map(AsRef::as_ref)
    }
}
#[derive(Debug)]
pub struct Version {
    pub ordinal: u64,
    pub account: Arc<MessageAccount>,
    // Last field: release the raw owner before its charge. Registry indexes,
    // snapshots and canonical identities all share this same Arc<Version>.
    _lease: Arc<RetentionLease>,
}
// The high bit closes admission, the remaining bits count admitted callback
// guards. One CAS linearizes admission against the fence, without lock races.
const CLOSED: usize = 1usize << (usize::BITS - 1);
#[derive(Debug)]
struct BankIngress {
    gate: AtomicUsize,
    count: AtomicU64,
    late: AtomicBool,
    retired: AtomicBool,
    state: Mutex<BankState>,
    _lease: Arc<RetentionLease>,
}
#[derive(Debug, Default)]
struct BankState {
    count: BTreeMap<[u8; 32], AtomicU64>,
    // Small tree nodes avoid growing a large contiguous allocation under a lock.
    versions: BTreeMap<u64, Arc<Version>>,
    identities: BTreeMap<([u8; 32], u64), Arc<Version>>,
    awaiting: BTreeMap<usize, (u64, [u8; 32])>,
    obligations: BTreeMap<u64, Arc<RetentionLease>>,
    fenced_at: Option<Instant>,
    bytes: usize,
}
impl BankIngress {
    fn ready(&self) -> Result<bool> {
        let b = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history Bank mutex poisoned"))?;
        let gate = self.gate.load(Ordering::Acquire);
        if gate == CLOSED && b.awaiting.is_empty() {
            ensure!(
                self.count.load(Ordering::Acquire) == b.versions.len() as u64,
                "independent callback count mismatch"
            );
            return Ok(true);
        }
        ensure!(
            !b.fenced_at
                .is_some_and(|at| at.elapsed() >= Duration::from_secs(15)),
            "history ingress fence deadline"
        );
        Ok(false)
    }
}
#[derive(Debug, Default)]
struct State {
    next: u64,
    generation: u64,
    high: u64,
    watches: BTreeMap<u64, Watch>,
    banks: BTreeMap<Bank, Arc<BankIngress>>,
    authoritative: BTreeMap<u64, (u64, Arc<RetentionLease>)>,
    floor: u64,
}
#[derive(Debug)]
pub struct HistoryRegistry {
    enabled: AtomicUsize,
    broken: AtomicBool,
    ordinal: AtomicU64,
    pub watermark: AtomicU64,
    budget: Arc<LiveBudget>,
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
            ordinal: AtomicU64::new(0),
            watermark: AtomicU64::new(0),
            budget: Arc::new(LiveBudget {
                used: AtomicUsize::new(0),
                limit: limits.retained_bytes.min(RETAINED_BYTES),
            }),
            state: Mutex::new(State::default()),
            limits,
            max_watchers,
        }
    }
    pub fn reserve(&self, bytes: usize) -> Result<Arc<RetentionLease>> {
        let lease = Arc::new(RetentionLease {
            budget: Arc::clone(&self.budget),
            bytes: AtomicUsize::new(0),
        });
        lease.add(bytes)?;
        Ok(lease)
    }
    pub fn observe_callback(&self, slot: u64) {
        self.watermark.fetch_max(slot, Ordering::SeqCst);
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
                && self
                    .budget
                    .used
                    .load(Ordering::Acquire)
                    .checked_add(WATCH_BYTES)
                    .is_some_and(|b| b <= self.limits.retained_bytes),
            "history watcher resource limit"
        );
        let lease = self.reserve(WATCH_BYTES)?;
        let previous_watchers = s.watches.len();
        self.enabled.store(previous_watchers + 1, Ordering::Release);
        s.high = s
            .high
            .max(sample())
            .max(self.watermark.load(Ordering::SeqCst));
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
        let plan = Arc::new(Plan {
            generation: s.generation,
            switch,
            keys,
            _lease: Arc::clone(&lease),
        });
        let id = s.next;
        s.next = s.next.checked_add(1).context("history watcher exhausted")?;
        s.watches.insert(
            id,
            Watch {
                plans: BTreeMap::from([(plan.generation, Arc::clone(&plan))]),
                end: None,
                completed: BTreeSet::new(),
                failed: false,
                sealed_through: 0,
                lease,
            },
        );
        self.enabled.store(s.watches.len(), Ordering::Release);
        drop(s);
        Ok((
            HistoryWatch {
                registry: Arc::clone(self),
                id,
            },
            plan.as_ref().clone(),
        ))
    }
    // Registry metadata linearizes high-watermark / coverage with registration.
    // Payload retention, attachment and retirement use only the selected Bank.
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
        let result = (|| -> Result<Ingress> {
            let mut s = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
            s.high = s.high.max(slot);
            let ordinal = self
                .ordinal
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
                .map_err(|_| anyhow::anyhow!("history ordinal exhausted"))?;
            if dead {
                for (bank, b) in &s.banks {
                    if bank.0 == slot {
                        b.late.store(true, Ordering::Release);
                    }
                }
            }
            let Some(id) = bank_id else {
                return Ok(Ingress::default());
            };
            let bank = (slot, id);
            if authoritative {
                self.record_authoritative(&mut s, bank)?;
            }
            if data {
                if let Some(b) = s.banks.get(&bank) {
                    if b.gate.load(Ordering::Acquire) & CLOSED != 0 {
                        b.late.store(true, Ordering::Release);
                    }
                }
            }
            let covered = s
                .watches
                .values()
                .any(|w| !w.failed && w.plan(slot).is_some());
            if !covered {
                return Ok(Ingress::default());
            }
            ensure!(slot >= s.floor, "history callback below GC floor");
            let after_epoch_seal = (data || !s.banks.contains_key(&bank))
                && s.watches
                    .values()
                    .any(|w| w.plan(slot).is_some() && slot < w.sealed_through);
            if !s.banks.contains_key(&bank) {
                ensure!(s.banks.len() < 16384, "history Bank index limit");
                let lease = self.reserve(NODE_BYTES)?;
                s.banks.insert(
                    bank,
                    Arc::new(BankIngress {
                        gate: AtomicUsize::new(0),
                        count: AtomicU64::new(0),
                        late: AtomicBool::new(false),
                        retired: AtomicBool::new(false),
                        state: Mutex::new(BankState::default()),
                        _lease: lease,
                    }),
                );
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
            let callback_lease = if tracked.is_some() {
                Some(self.reserve(2 * NODE_BYTES)?)
            } else {
                None
            };
            let b = Arc::clone(s.banks.get(&bank).context("history Bank missing")?);
            if after_epoch_seal {
                b.late.store(true, Ordering::Release);
            }
            if !data && b.gate.load(Ordering::Acquire) & CLOSED != 0 {
                // Known post-seal Processed/Confirmed/Rooted statuses are legal;
                // data callbacks still revoke the old Bank below.
                return Ok(Ingress::default());
            }
            // Admission and closure have a single atomic order. A closed gate
            // never admits a fresh callback, but already admitted guards retire.
            if b.gate
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |gate| {
                    (gate < CLOSED - 1).then(|| gate + 1)
                })
                .is_err()
            {
                if data {
                    b.late.store(true, Ordering::Release);
                }
                return Ok(Ingress::default());
            }
            drop(s);
            let guard = Ingress {
                registry: Some(Arc::clone(self)),
                ingress: Some(Arc::clone(&b)),
                ordinal,
                key: tracked,
                attached: false,
                lease: callback_lease,
            };
            if let Some(key) = tracked {
                let mut payload = b
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("history Bank mutex poisoned"))?;
                ensure!(
                    !b.retired.load(Ordering::Acquire),
                    "history stopped during callback admission"
                );
                b.count
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
                    .map_err(|_| anyhow::anyhow!("history callback count exhausted"))?;
                payload
                    .count
                    .entry(key)
                    .or_default()
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
                    .map_err(|_| anyhow::anyhow!("history key count exhausted"))?;
                let lease = guard
                    .lease
                    .as_ref()
                    .context("history callback lease missing")?;
                payload.obligations.insert(ordinal, Arc::clone(lease));
                payload.bytes += 2 * NODE_BYTES;
                ensure!(
                    b.count.load(Ordering::Acquire) <= self.limits.bank_versions as u64
                        && payload.bytes <= self.limits.bank_bytes,
                    "history Bank callback/byte limit"
                );
            }
            Ok(guard)
        })();
        match result {
            Ok(guard) => guard,
            Err(error) => {
                self.broken.store(true, Ordering::Release);
                log::warn!("tracked history ingress failed: {error}");
                Ingress::default()
            }
        }
    }
    fn bank(&self, bank: Bank) -> Result<Option<Arc<BankIngress>>> {
        let s = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
        Ok(s.banks.get(&bank).cloned())
    }
    fn record_authoritative(&self, s: &mut State, bank: Bank) -> Result<()> {
        if s.authoritative
            .get(&bank.0)
            .is_some_and(|(id, _)| *id != bank.1)
        {
            for (key, b) in &s.banks {
                if key.0 == bank.0 {
                    b.late.store(true, Ordering::Release);
                }
            }
        } else if let std::collections::btree_map::Entry::Vacant(entry) =
            s.authoritative.entry(bank.0)
        {
            let lease = self.reserve(NODE_BYTES)?;
            entry.insert((bank.1, lease));
        }
        Ok(())
    }
    pub fn retain(&self, account: &Arc<MessageAccount>) {
        if !self.enabled() {
            return;
        }
        let result = (|| -> Result<()> {
            let bank = (
                account.slot,
                account.bank_id.context("history account Bank missing")?,
            );
            let Some(ingress) = self.bank(bank)? else {
                return Ok(());
            };
            if ingress.retired.load(Ordering::Acquire) {
                return Ok(());
            }
            let ptr = Arc::as_ptr(account) as usize;
            {
                let b = ingress
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("history Bank mutex poisoned"))?;
                if !b.awaiting.contains_key(&ptr) {
                    return Ok(());
                }
            }
            let charge = account
                .account
                .data
                .len()
                .checked_mul(2)
                .and_then(|n| n.checked_add(2048))
                .context("history charge overflow")?;
            let mut b = ingress
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("history Bank mutex poisoned"))?;
            if ingress.retired.load(Ordering::Acquire) {
                return Ok(());
            }
            let (ordinal, _) = *b.awaiting.get(&ptr).context("history repeated retention")?;
            let lease = Arc::clone(
                b.obligations
                    .get(&ordinal)
                    .context("history callback lease missing")?,
            );
            lease.add(charge)?;
            ensure!(
                b.bytes
                    .checked_add(charge)
                    .is_some_and(|n| n <= self.limits.bank_bytes),
                "history Bank byte limit"
            );
            let (ordinal, _) = b
                .awaiting
                .remove(&ptr)
                .context("history repeated retention")?;
            b.bytes += charge;
            let identity = (
                account.account.pubkey.to_bytes(),
                account.account.write_version,
            );
            if let Some(old) = b.identities.get(&identity) {
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
            let version = Arc::new(Version {
                ordinal,
                account: Arc::clone(account),
                _lease: lease,
            });
            b.identities
                .entry(identity)
                .or_insert_with(|| Arc::clone(&version));
            b.versions.insert(ordinal, version);
            Ok(())
        })();
        if let Err(error) = result {
            self.broken.store(true, Ordering::Release);
            log::warn!("tracked history retention failed: {error}");
        }
    }
    pub fn fence(&self, bank: Bank) {
        if !self.enabled() {
            return;
        }
        let result = (|| -> Result<()> {
            if let Some(ingress) = self.bank(bank)? {
                let mut b = ingress
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("history Bank mutex poisoned"))?;
                let prior = ingress.gate.fetch_or(CLOSED, Ordering::AcqRel);
                b.fenced_at.get_or_insert_with(Instant::now);
                // Missing retained callbacks with no active sender are actual
                // source loss. An active callback tail is polled by B instead.
                if prior == 0
                    && (!b.awaiting.is_empty()
                        || ingress.count.load(Ordering::Acquire) != b.versions.len() as u64)
                {
                    ingress.late.store(true, Ordering::Release);
                }
            }
            Ok(())
        })();
        if result.is_err() {
            self.broken.store(true, Ordering::Release);
        }
    }
    pub fn authoritative(&self, bank: Bank) {
        if !self.enabled() {
            return;
        }
        let result = (|| -> Result<()> {
            let mut s = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
            self.record_authoritative(&mut s, bank)
        })();
        if result.is_err() {
            self.broken.store(true, Ordering::Release);
        }
    }
    pub fn observe_source(&self, message: &SourceMessage) {
        if !self.enabled() {
            return;
        }
        let result = (|| -> Result<()> {
            let s = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
            match message {
                SourceMessage::EntryUpdateParent(update) => {
                    if let Some(b) = s.banks.get(&(
                        update.update_parent.slot,
                        update.update_parent.cleared_bank_id,
                    )) {
                        b.late.store(true, Ordering::Release);
                    }
                }
                SourceMessage::Slot(slot) if slot.status == SlotStatus::Dead => {
                    for (bank, b) in &s.banks {
                        if bank.0 == slot.slot {
                            b.late.store(true, Ordering::Release);
                        }
                    }
                }
                _ => (),
            }
            Ok(())
        })();
        if result.is_err() {
            self.broken.store(true, Ordering::Release);
        }
    }
    fn release_bank(&self, ingress: &BankIngress) -> Result<()> {
        let owners = {
            let mut b = ingress
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("history Bank mutex poisoned"))?;
            ingress.retired.store(true, Ordering::Release);
            ingress.gate.fetch_or(CLOSED, Ordering::AcqRel);
            std::mem::take(&mut *b)
        };
        drop(owners);
        Ok(())
    }
    pub fn gc(&self, floor: u64) {
        if !self.enabled() {
            return;
        }
        let result = (|| -> Result<()> {
            // Preallocate the bounded index snapshot outside the registry lock.
            let mut banks = Vec::new();
            banks.try_reserve_exact(16384)?;
            {
                let mut s = self
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
                s.floor = s.floor.max(floor);
                banks.extend(
                    s.banks
                        .iter()
                        .filter(|(bank, _)| bank.0 < floor)
                        .map(|(_, b)| Arc::clone(b)),
                );
            }
            for bank in banks {
                ensure!(
                    bank.retired.load(Ordering::Acquire) || bank.ready()?,
                    "history GC of incomplete Bank"
                );
            }
            Ok(())
        })();
        if result.is_err() {
            self.broken.store(true, Ordering::Release);
        }
    }
}
#[derive(Debug, Default)]
pub struct Ingress {
    registry: Option<Arc<HistoryRegistry>>,
    ingress: Option<Arc<BankIngress>>,
    ordinal: u64,
    key: Option<[u8; 32]>,
    attached: bool,
    lease: Option<Arc<RetentionLease>>,
}
impl Ingress {
    pub fn attach(&mut self, account: &Arc<MessageAccount>) {
        let (Some(registry), Some(ingress), Some(key)) = (&self.registry, &self.ingress, self.key)
        else {
            return;
        };
        match ingress.state.lock() {
            Ok(mut b) => {
                b.awaiting
                    .insert(Arc::as_ptr(account) as usize, (self.ordinal, key));
                self.attached = true;
            }
            Err(_) => registry.broken.store(true, Ordering::Release),
        }
    }
}
impl Drop for Ingress {
    fn drop(&mut self) {
        let (Some(r), Some(b)) = (&self.registry, &self.ingress) else {
            return;
        };
        if self.key.is_some() && !self.attached {
            b.late.store(true, Ordering::Release);
        }
        if b.gate
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |gate| {
                (gate & !CLOSED != 0).then(|| gate - 1)
            })
            .is_err()
        {
            r.broken.store(true, Ordering::Release);
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
            .find(|(bank, b)| b.late.load(Ordering::Acquire) && w.plan(bank.0).is_some())
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
        s.high = s
            .high
            .max(sample())
            .max(self.registry.watermark.load(Ordering::SeqCst));
        let switch = s.high.checked_add(1).context("cutover exhausted")?;
        let old = s
            .watches
            .get(&self.id)
            .and_then(|w| w.plans.values().next_back())
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
        let lease = Arc::clone(&s.watches.get(&self.id).context("watcher missing")?.lease);
        let p = Arc::new(Plan {
            generation: s.generation,
            switch,
            keys,
            _lease: lease,
        });
        p._lease.add(keys_charge(&p))?;
        s.watches
            .get_mut(&self.id)
            .context("watcher missing")?
            .plans
            .insert(p.generation, Arc::clone(&p));
        drop(s);
        Ok(p.as_ref().clone())
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
        s.high = s
            .high
            .max(sample())
            .max(self.registry.watermark.load(Ordering::SeqCst));
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
    pub fn ready(&self, bank: Bank) -> Result<bool> {
        self.check()?;
        self.registry
            .bank(bank)?
            .context("Bank absent from callback ingress (partial Bank)")?
            .ready()
            .map_err(|error| {
                anyhow::Error::new(HistoryFault {
                    reason: yellowstone_grpc_proto::prelude::OrderGapReason::IncompleteBank,
                    bank: Some(bank),
                    detail: "history ingress fence/count incomplete",
                })
                .context(error.to_string())
            })
    }
    pub fn versions(&self, bank: Bank, plan: &Plan) -> Result<(Vec<Arc<Version>>, u64)> {
        self.check()?;
        let ingress = self
            .registry
            .bank(bank)?
            .context("Bank absent from callback ingress (partial Bank)")?;
        ensure!(ingress.ready()?, "Bank missing ingress fence");
        let capacity = usize::try_from(ingress.count.load(Ordering::Acquire))?;
        // The caller has reserved encoding workspace. Allocate outside all locks.
        let mut versions = Vec::new();
        versions.try_reserve_exact(capacity)?;
        let b = ingress
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("history Bank mutex poisoned"))?;
        let matches = |key: &[u8]| {
            plan.keys
                .iter()
                .any(|k| k.pubkey == key && k.active_from_slot.is_some_and(|a| a <= bank.0))
        };
        let count = b
            .count
            .iter()
            .filter(|(key, _)| matches(key.as_slice()))
            .map(|(_, n)| n.load(Ordering::Acquire))
            .sum::<u64>();
        versions.extend(
            b.versions
                .values()
                .filter(|v| matches(v.account.account.pubkey.as_ref()))
                .cloned(),
        );
        ensure!(
            count == versions.len() as u64,
            "independent callback count mismatch"
        );
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
        let mut done = Vec::new();
        done.try_reserve_exact(16384)?;
        {
            let mut s = self
                .registry
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
            // Failure cleanup never establishes a successful coverage cutoff.
            let end = s.high.saturating_add(1);
            let w = s.watches.get_mut(&self.id).context("watcher missing")?;
            w.failed = true;
            w.end.get_or_insert(end);
            done.extend(
                s.banks
                    .iter()
                    .filter(|(bank, _)| {
                        s.watches.values().all(|w| {
                            w.failed || w.plan(bank.0).is_none() || w.completed.contains(bank)
                        })
                    })
                    .map(|(_, b)| Arc::clone(b)),
            );
        }
        for bank in done {
            self.registry.release_bank(&bank)?;
        }
        Ok(())
    }
    pub fn complete(&self, bank: Bank) -> Result<()> {
        let release = {
            let mut s = self
                .registry
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("history mutex poisoned"))?;
            s.watches
                .get(&self.id)
                .context("watcher missing")?
                .lease
                .add(NODE_BYTES)?;
            s.watches
                .get_mut(&self.id)
                .context("watcher missing")?
                .completed
                .insert(bank);
            let done = s
                .watches
                .values()
                .all(|w| w.failed || w.plan(bank.0).is_none() || w.completed.contains(&bank));
            if done {
                s.banks.get(&bank).cloned()
            } else {
                None
            }
        };
        if let Some(bank) = release {
            self.registry.release_bank(&bank)?;
        }
        Ok(())
    }
}
const fn keys_charge(plan: &Plan) -> usize {
    NODE_BYTES + plan.keys.len() * 256
}
impl Drop for HistoryWatch {
    fn drop(&mut self) {
        // Workspace for a bounded index snapshot is allocated outside the lock.
        let mut done = Vec::new();
        if done.try_reserve_exact(16384).is_err() {
            self.registry.broken.store(true, Ordering::Release);
            return;
        }
        let removed;
        let detached;
        match self.registry.state.lock() {
            Ok(mut s) => {
                removed = s.watches.remove(&self.id);
                self.registry
                    .enabled
                    .store(s.watches.len(), Ordering::Release);
                if s.watches.is_empty() {
                    // Detach the registry owners. Guards may still own an entire
                    // Bank; its leases remain charged until those Arcs retire.
                    detached = Some((
                        std::mem::take(&mut s.banks),
                        std::mem::take(&mut s.authoritative),
                    ));
                    self.registry.broken.store(false, Ordering::Release);
                } else {
                    done.extend(
                        s.banks
                            .iter()
                            .filter(|(bank, _)| {
                                s.watches.values().all(|w| {
                                    w.failed
                                        || w.plan(bank.0).is_none()
                                        || w.completed.contains(bank)
                                })
                            })
                            .map(|(_, b)| Arc::clone(b)),
                    );
                    detached = None;
                }
            }
            Err(_) => {
                self.registry.broken.store(true, Ordering::Release);
                return;
            }
        }
        for bank in done {
            if self.registry.release_bank(&bank).is_err() {
                self.registry.broken.store(true, Ordering::Release);
            }
        }
        drop((removed, detached));
    }
}
#[derive(Debug)]
// This ledger outlives registry generations. Only actual owner lease Drop
// decrements it; clearing metadata or completing a Bank never changes used.
struct LiveBudget {
    used: AtomicUsize,
    limit: usize,
}
#[derive(Debug)]
pub struct RetentionLease {
    budget: Arc<LiveBudget>,
    bytes: AtomicUsize,
}
impl RetentionLease {
    fn add(&self, bytes: usize) -> Result<()> {
        self.budget
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= self.budget.limit)
            })
            .map_err(|_| anyhow::anyhow!("history live allocation resource limit"))?;
        // The shared budget bound also bounds the sum stored in any one lease.
        self.bytes.fetch_add(bytes, Ordering::AcqRel);
        Ok(())
    }
}
impl Drop for RetentionLease {
    fn drop(&mut self) {
        self.budget
            .used
            .fetch_sub(self.bytes.load(Ordering::Acquire), Ordering::AcqRel);
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
    // Two rounds stress one shared Bank and four independent Banks. Account
    // admission is independent of slot / transaction / entry callback guards.
    #[test]
    fn concurrent_ingress_preserves_counts_zero_bank_a_bytes_and_real_late() {
        use crate::plugin::filter::{limits::FilterLimits, Filter};
        use prost::Message;
        use std::{sync::Barrier, thread};
        use yellowstone_grpc_proto::prelude::{SubscribeRequest, SubscribeRequestFilterAccounts};
        let r = Arc::new(HistoryRegistry::default());
        let keys = (1..=2)
            .map(|key| OrderKeyCoverage {
                pubkey: vec![key; 32],
                active_from_slot: None,
                owner_exempt_active_from_slot: Some(0),
            })
            .collect();
        let (w, p) = r.register(keys, 99).expect("two-key registration");
        let disabled = Arc::new(HistoryRegistry::default());
        let barrier = Arc::new(Barrier::new(16));
        thread::scope(|scope| {
            let mut handles = Vec::new();
            for worker in 0..16u64 {
                let (r, disabled, barrier) =
                    (Arc::clone(&r), Arc::clone(&disabled), Arc::clone(&barrier));
                handles.push(scope.spawn(move || {
                    let request = SubscribeRequest {
                        accounts: std::collections::HashMap::from([(
                            String::from("accounts"),
                            SubscribeRequestFilterAccounts::default(),
                        )]),
                        ..Default::default()
                    };
                    let filter = Filter::new(
                        &request,
                        &FilterLimits::default(),
                        &mut fixtures::filter_names(),
                    )
                    .expect("A filter");
                    for round in 0..2u64 {
                        let slot = if round == 0 { 100 } else { 101 + worker % 4 };
                        for index in 0..32u64 {
                            let mut a = account(worker * 64 + round * 32 + index);
                            let raw = Arc::make_mut(&mut a);
                            raw.slot = slot;
                            raw.bank_id = Some(slot + 1000);
                            raw.account.pubkey =
                                Pubkey::new_from_array([1 + (worker % 2) as u8; 32]);
                            barrier.wait();
                            let mut guard = r.enter(
                                slot,
                                a.bank_id,
                                Some(a.account.pubkey.to_bytes()),
                                false,
                                false,
                                true,
                            );
                            let mut control = disabled.enter(
                                slot,
                                a.bank_id,
                                Some(a.account.pubkey.to_bytes()),
                                false,
                                false,
                                true,
                            );
                            guard.attach(&a);
                            control.attach(&a);
                            // The same callback reaches ordinary A with either registry.
                            let message = SourceMessage::Account(Arc::clone(&a));
                            let wire = || {
                                filter
                                    .get_updates(&message, None)
                                    .iter()
                                    .map(Message::encode_to_vec)
                                    .collect::<Vec<_>>()
                            };
                            let expected = wire();
                            r.retain(&a);
                            disabled.retain(&a);
                            assert_eq!(wire(), expected, "history cannot change A bytes");
                            drop((guard, control));
                            for (data, authoritative) in
                                [(false, false), (true, false), (true, false)]
                            {
                                drop(r.enter(slot, a.bank_id, None, false, authoritative, data));
                            }
                        }
                    }
                }));
            }
            for handle in handles {
                handle.join().expect("parallel callbacks");
            }
        });
        w.check()
            .expect("normal callback contention is not a failure");
        let mut ordinals = BTreeSet::new();
        for slot in 100..=104 {
            r.fence((slot, slot + 1000));
            let (v, count) = w
                .versions((slot, slot + 1000), &p)
                .expect("all admitted versions");
            let expected = if slot == 100 { 512 } else { 128 };
            assert_eq!((v.len(), count), (expected, expected as u64));
            for key in 1..=2 {
                let expected_key = if slot == 100 {
                    256
                } else if key == 1 + ((slot - 101) % 2) as u8 {
                    128
                } else {
                    0
                };
                assert_eq!(
                    v.iter()
                        .filter(|v| v.account.account.pubkey.to_bytes() == [key; 32])
                        .count(),
                    expected_key
                );
                let mut key_plan = p.clone();
                key_plan
                    .keys
                    .retain(|coverage| coverage.pubkey == [key; 32]);
                let (key_versions, independent_count) = w
                    .versions((slot, slot + 1000), &key_plan)
                    .expect("per-key independent callback count");
                assert_eq!(
                    (key_versions.len(), independent_count),
                    (expected_key, expected_key as u64)
                );
            }
            for version in v {
                assert!(ordinals.insert(version.ordinal), "unique callback ordinal");
            }
        }
        drop(r.enter(105, Some(1105), None, false, false, false));
        r.fence((105, 1105));
        let (v, n) = w.versions((105, 1105), &p).expect("zero-write Bank seal");
        assert!(v.is_empty());
        assert_eq!(n, 0);
        w.check().expect("all normal seals");
        enqueue(
            &r,
            &{
                let mut a = account(9999);
                Arc::make_mut(&mut a).bank_id = Some(1100);
                a
            },
            true,
        );
        assert!(
            w.check().is_err(),
            "a new callback after the fence revokes the run"
        );
    }
    #[test]
    fn concurrent_ingress_fence_defers_callback_tail_without_false_late() {
        use std::{sync::Barrier, thread};
        let (r, w, p) = setup();
        let entered = Arc::new(Barrier::new(17));
        let release = Arc::new(Barrier::new(17));
        thread::scope(|scope| {
            for worker in 0..16u64 {
                let (r, entered, release) =
                    (Arc::clone(&r), Arc::clone(&entered), Arc::clone(&release));
                scope.spawn(move || {
                    let a = account(worker);
                    let mut guard = r.enter(100, Some(100), Some([1; 32]), false, false, true);
                    guard.attach(&a);
                    r.retain(&a);
                    entered.wait();
                    release.wait();
                    drop(guard);
                });
            }
            entered.wait();
            r.fence((100, 100));
            let checked = w.check();
            let pending = w.versions((100, 100), &p);
            release.wait();
            checked.expect("an admitted callback tail is not late");
            assert!(pending.is_err(), "no output while ingress is active");
        });
        let (v, n) = w
            .versions((100, 100), &p)
            .expect("seal after callback tails retire");
        assert_eq!((v.len(), n), (16, 16));
        w.check().expect("no false late evidence");
        let slot_guard = r.enter(106, Some(106), None, false, false, false);
        r.fence((106, 106));
        assert!(!w
            .ready((106, 106))
            .expect("slot callback tail also defers seal"));
        drop(slot_guard);
        assert_eq!(
            w.versions((106, 106), &p)
                .expect("zero-write slot tail retired")
                .1,
            0
        );
    }
    #[test]
    fn concurrent_ingress_deadline_and_gc_reject_unretired_admission() {
        let (r, w, _) = setup();
        let a = account(1);
        let mut guard = r.enter(100, Some(100), Some([1; 32]), false, false, true);
        guard.attach(&a);
        r.retain(&a);
        r.fence((100, 100));
        assert!(!w.ready((100, 100)).expect("pending ingress"));
        let bank = r.bank((100, 100)).expect("registry").expect("Bank");
        bank.state.lock().expect("Bank fence clock").fenced_at =
            Some(Instant::now() - Duration::from_secs(16));
        assert!(w.ready((100, 100)).is_err(), "bounded fence deadline");
        r.gc(101);
        assert!(
            w.check().is_err(),
            "GC cannot discard an active fenced Bank"
        );
        drop(guard);
    }
    fn live_bytes(r: &HistoryRegistry) -> usize {
        r.budget.used.load(Ordering::Acquire)
    }
    #[test]
    fn r2_guard_retains_bank_charge_across_last_watch_and_reregistration() {
        let (r, w, _) = setup();
        let mut a = account(1);
        Arc::make_mut(&mut a).account.data = bytes::Bytes::from(vec![7; 512 * 1024]);
        let raw_charge = 2 * NODE_BYTES + 2 * a.account.data.len() + 2048;
        let weak = Arc::downgrade(&a);
        enqueue(&r, &a, true);
        drop(a);
        let guard = r.enter(100, Some(100), None, false, false, true);
        drop(w);
        assert!(
            weak.upgrade().is_some(),
            "guard still owns raw Bank versions"
        );
        let required = NODE_BYTES + raw_charge;
        assert!(
            live_bytes(&r) == required,
            "live guard Bank cannot be uncharged"
        );
        let (next, _) = r
            .register(
                vec![OrderKeyCoverage {
                    pubkey: vec![1; 32],
                    active_from_slot: None,
                    owner_exempt_active_from_slot: Some(0),
                }],
                100,
            )
            .expect("new generation");
        assert!(
            r.reserve(RETAINED_BYTES - WATCH_BYTES - required + 1)
                .is_err(),
            "old guard bytes cannot be reused"
        );
        drop(next);
        drop(guard);
        assert!(weak.upgrade().is_none());
        assert_eq!(live_bytes(&r), 0);
    }
    #[test]
    fn r2_snapshot_retains_raw_charge_after_complete_and_last_watch() {
        let (r, w, p) = setup();
        let mut a = account(1);
        Arc::make_mut(&mut a).account.data = bytes::Bytes::from(vec![7; 512 * 1024]);
        let raw_charge = 2 * NODE_BYTES + 2 * a.account.data.len() + 2048;
        let weak = Arc::downgrade(&a);
        enqueue(&r, &a, true);
        drop(a);
        r.fence((100, 100));
        let (snapshot, n) = w.versions((100, 100), &p).expect("emit Bank snapshot");
        assert_eq!(n, 1);
        let before = live_bytes(&r);
        w.complete((100, 100))
            .expect("complete while snapshot lives");
        assert!(weak.upgrade().is_some());
        assert_eq!(
            live_bytes(&r),
            before + NODE_BYTES,
            "complete releases no live snapshot charge"
        );
        drop(w);
        drop(p);
        assert!(
            live_bytes(&r) == raw_charge,
            "last watcher cannot refund a live snapshot"
        );
        assert!(r.reserve(RETAINED_BYTES - raw_charge + 1).is_err());
        drop(snapshot);
        assert!(weak.upgrade().is_none());
        assert_eq!(live_bytes(&r), 0);
    }
    #[test]
    fn r2_disconnected_watcher_retires_payload_for_remaining_completed_watch() {
        let (r, w1, p) = setup();
        let (w2, _) = r.register(p.keys.clone(), 99).expect("second observer");
        let mut a = account(1);
        Arc::make_mut(&mut a).account.data = bytes::Bytes::from(vec![7; 512 * 1024]);
        let raw_charge = 2 * NODE_BYTES + 2 * a.account.data.len() + 2048;
        let weak = Arc::downgrade(&a);
        enqueue(&r, &a, true);
        drop(a);
        r.fence((100, 100));
        w1.complete((100, 100)).expect("W1 consumed");
        assert!(weak.upgrade().is_some(), "W2 still needs payload");
        let before = live_bytes(&r);
        drop(w2);
        assert!(
            weak.upgrade().is_none(),
            "remaining W1 completed: disconnect releases raw owners"
        );
        assert!(
            live_bytes(&r) + WATCH_BYTES + raw_charge == before,
            "payload and disconnected watcher both refunded"
        );
        w1.check().expect("tombstone survives");
        drop(w1);
        drop(p);
        assert_eq!(live_bytes(&r), 0);
    }
    #[test]
    fn r2_unwind_retires_guard_and_exposes_unattached_callback() {
        for attached in [true, false] {
            let (r, w, p) = setup();
            let a = account(1);
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut guard = r.enter(100, Some(100), Some([1; 32]), false, false, true);
                if attached {
                    guard.attach(&a);
                    r.retain(&a);
                }
                panic!("injected callback unwind outside ledger locks");
            }));
            assert!(panic.is_err(), "fault injection must unwind");
            assert_eq!(
                r.bank((100, 100))
                    .expect("registry")
                    .expect("Bank")
                    .gate
                    .load(Ordering::Acquire),
                0,
                "RAII fence retired even on unwind"
            );
            r.fence((100, 100));
            if attached {
                assert_eq!(
                    w.versions((100, 100), &p)
                        .expect("retained callback survived unwind")
                        .1,
                    1
                );
            } else {
                assert!(
                    w.check().is_err(),
                    "unattached callback cannot certify a Bank"
                );
            }
            drop((w, p));
            assert_eq!(live_bytes(&r), 0, "all B owners released after unwind");
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
        w.check().expect("fence defers active ingress");
        r.retain(&a);
        drop(guard);
        assert_eq!(w.versions((100, 100), &p).expect("retired ingress").1, 1);
    }
    #[test]
    fn registration_contention_waits_and_preserves_coverage() {
        let (r, w, p) = setup();
        std::thread::scope(|scope| {
            let locked = r.state.lock().expect("hold registration lock");
            let callback = scope.spawn(|| enqueue(&r, &account(1), true));
            drop(locked);
            callback
                .join()
                .expect("callback waits for registry metadata");
        });
        r.fence((100, 100));
        assert_eq!(w.versions((100, 100), &p).expect("complete Bank").1, 1);
        w.check().expect("ordinary contention is not a gap");
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
        let remaining = RETAINED_BYTES - r.budget.used.load(Ordering::Acquire);
        let lease = r.reserve(remaining).expect("fill global limit");
        assert!(r.reserve(1).is_err());
        drop(w);
        assert_eq!(r.budget.used.load(Ordering::Acquire), remaining);
        assert!(r.reserve(RETAINED_BYTES - remaining + 1).is_err());
        drop(lease);
        assert_eq!(r.budget.used.load(Ordering::Acquire), 0);
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
        let before = r.budget.used.load(Ordering::Acquire);
        w.stop().expect("stop B");
        assert!(r.budget.used.load(Ordering::Acquire) < before);
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
