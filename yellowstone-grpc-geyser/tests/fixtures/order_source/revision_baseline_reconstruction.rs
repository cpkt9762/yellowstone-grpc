use {
    crate::{
        metrics,
        plugin::{
            filter::encoder::TransactionEncoder,
            message::{
                Message, MessageAccount, MessageBlock, MessageBlockFooter, MessageBlockMeta,
                MessageEntry, MessageSlot, MessageTransaction, SlotStatus,
            },
        },
    },
    foldhash::{HashMap as FoldHashMap, HashMapExt},
    solana_clock::{BankId, Slot},
    solana_commitment_config::CommitmentLevel,
    solana_pubkey::Pubkey,
    std::{
        collections::{btree_map::Range, BTreeMap, VecDeque},
        sync::{Arc, LazyLock},
    },
};

// Sysvars a bank must be observed to have written before it's allowed to seal. Clock and
// SlotHashes are rewritten inline as part of bank construction -- before `CreatedBank`
// itself fires -- for every single bank instance (confirmed against Agave's own source);
// SlotHistory and RecentBlockhashes are written later, as the block freezes. Requiring all
// four here is only safe because buffering is keyed by bank_id: an earlier, slot-keyed
// version of this pipeline required the same thing and stalled sealing forever, permanently,
// for every slot in production, because a spurious "bank reset" (a `FirstShredReceived`
// placeholder mistaken for a real prior bank -- a slot-keyed-only failure mode) discarded
// the sysvar writes that land right before `CreatedBank`, and a slot's bank only ever gets
// one shot at emitting them. Keyed by bank_id, nothing can clobber a bank's own buffer, so
// there's no equivalent failure mode -- and if a bank genuinely never sees one of these for
// some other reason, the blast radius is just that one bank never sealing (eventually swept
// by `sweep_stale_slots`), not every slot in the pipeline.
pub(crate) const MUST_HAVE_SYSVAR_ACCOUNTS: [Pubkey; 4] = [
    Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111"),
    Pubkey::from_str_const("SysvarS1otHashes111111111111111111111111111"),
    Pubkey::from_str_const("SysvarS1otHistory11111111111111111111111111"),
    Pubkey::from_str_const("SysvarRecentB1ockHashes11111111111111111111"),
];

// Fixed-address accounts written by the first bank of a new epoch on its fork (not
// necessarily the epoch's first slot), during bank construction and so before `CreatedBank`.
// Accounts whose address varies are left out: the feature accounts activated at that epoch,
// builtin/core-BPF migration targets, and the vote accounts charged the VAT and paid
// commission on Alpenglow. So is the Rent sysvar, which is only rewritten when a feature that
// changes rent activates at that epoch.
// NOTE: THIS CONSTANT IS NOT USED ANYWHERE, BESIDES TESTS, IT'S ONLY FOR DOCUMENTATION PURPOSES.
// PERHAPS SOMEDAY THIS CONSTANT WILL BE USED IN THE MAIN LOGIC, BUT FOR NOW IT'S PURELY DOCUMENTATION TO HELP AGENT.
#[allow(dead_code)]
pub(crate) const SYSVAR_ON_EPOCH_CROSSING: [Pubkey; 5] = [
    // Every epoch crossing: updated when the epoch is activated.
    Pubkey::from_str_const("SysvarStakeHistory1111111111111111111111111"),
    // Every epoch crossing: created active by `begin_partitioned_rewards`.
    Pubkey::from_str_const("SysvarEpochRewards1111111111111111111111111"),
    // Alpenglow or migration epoch: `REWARD_EPOCH_DELEGATED_STAKES_ACCOUNT`, the PDA of seed
    // `reward_epoch_delegated_stakes` under the alpenglow feature id.
    Pubkey::from_str_const("CHfmHwNcskfxg1ZnLatbHbmVvYbA6rLDz3Xok5a3Ku8i"),
    // Alpenglow: the incinerator receives the VAT burned from staked vote accounts. Banks also
    // write it at freeze, after `CreatedBank`, when they burn fees.
    Pubkey::from_str_const("1nc1nerator11111111111111111111111111111111"),
    // Alpenglow: `VOTE_REWARD_ACCOUNT_ADDR`, the PDA of seed `vote_reward_account` under the
    // alpenglow feature id.
    Pubkey::from_str_const("3vJzniWALu2qGFJ1ZY6JD3Y2ZmA5zWqmVdDX7yBXHgu7"),
];

// Agave writes this account only on Alpenglow, while it applies the footer and before
// BlockMeta. A bank that writes it waits for its footer before it seals.
static ALPENGLOW_CLOCK_ACCOUNT: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(&[b"alpenclock"], &agave_feature_set::alpenglow::id()).0
});

const fn commitment_rank(level: CommitmentLevel) -> u8 {
    match level {
        CommitmentLevel::Processed => 0,
        CommitmentLevel::Confirmed => 1,
        CommitmentLevel::Finalized => 2,
    }
}

const MINIMUM_FINALIZED_SLOT_TO_BUFFER: usize = 10;

// How many slots' worth of lag before genuinely in-flight (unresolved) state for a slot is
// considered abandoned and swept -- e.g. a bank that seals but whose slot never receives any
// commitment update naming a winner (validator restart, a fork that's silently abandoned
// without ever reaching Dead, etc.), or a slot that stays ambiguous between multiple
// candidate banks forever. Must comfortably exceed normal Confirmed/Finalized lag (typically
// well under 100 slots even under load) so in-flight processing is never swept prematurely --
// this is strictly a backstop against genuinely orphaned state, not a normal code path.
const STALE_SLOT_THRESHOLD: Slot = 300;
// How often (in slots of forward progress) to run the stale-slot sweep. A full scan on every
// `add()` call would be wasteful; running it only this often keeps the amortized cost
// negligible relative to normal message volume.
const STALE_GC_INTERVAL: Slot = 50;

enum TrySealError {
    NotSealable,
    AlreadySealed,
}

/// Accumulates block content (Account/Transaction/Entry/BlockMeta/BlockFooter) for a single bank
/// instance, keyed by `bank_id` rather than by slot. Unlike a slot number, a `bank_id` is
/// unique per bank instance, so there is no ambiguity to resolve when a slot ends up with
/// more than one bank (e.g. after a dump-and-repair replay) -- each gets its own buffer,
/// and neither can clobber the other's accumulated data.
struct BankBuffer {
    bank_id: BankId,
    slot: Slot,
    parent_slot: Option<Slot>,
    // The bank's `CreatedBank` status, kept to replay it. We do not assume the first event
    // seen for a bank_id is `CreatedBank` -- the bank's construction-time account writes
    // arrive before it -- so it is also pushed into `original_messages` at its arrival
    // position, and required at seal time.
    created_bank: Option<Arc<MessageSlot>>,
    // Bit `i` set means `MUST_HAVE_SYSVAR_ACCOUNTS[i]` has been observed for this bank.
    musthave_sysvar_accounts_bitmask: u8,
    // Set when the bank writes `ALPENGLOW_CLOCK_ACCOUNT`; the footer is then required to seal.
    footer_expected: bool,
    original_messages: Vec<Message>,
    account_write_version_map: FoldHashMap<Pubkey, u64>,
    blockmeta: Option<Arc<MessageBlockMeta>>,
    footer: Option<Arc<MessageBlockFooter>>,
    transactions: Vec<Arc<MessageTransaction>>,
    accounts: Vec<Arc<MessageAccount>>,
    entries: Vec<Arc<MessageEntry>>,
    is_sealed: bool,
}

impl BankBuffer {
    fn new(bank_id: BankId, slot: Slot) -> Self {
        Self {
            bank_id,
            slot,
            parent_slot: None,
            created_bank: None,
            musthave_sysvar_accounts_bitmask: 0,
            footer_expected: false,
            original_messages: Vec::with_capacity(4096),
            account_write_version_map: FoldHashMap::with_capacity(4096),
            blockmeta: None,
            footer: None,
            transactions: Vec::with_capacity(4096),
            accounts: Vec::with_capacity(4096),
            entries: Vec::with_capacity(64),
            is_sealed: false,
        }
    }

    fn add_event(&mut self, event: Message) {
        match &event {
            Message::Account(message_account) => {
                if message_account.owner_reject_exempt {
                    // Fork-local account stream only: retain every admitted write for
                    // Confirmed/Finalized, without altering ordinary dedup, sysvar
                    // readiness, block account count, or SubscribeUpdateBlock.accounts.
                    self.original_messages.push(event);
                    return;
                }
                let write_version = message_account.account.write_version;
                self.account_write_version_map
                    .entry(message_account.account.pubkey)
                    .and_modify(|entry| {
                        if *entry < write_version {
                            *entry = write_version;
                        }
                    })
                    .or_insert(write_version);
                if let Some(position) = MUST_HAVE_SYSVAR_ACCOUNTS
                    .iter()
                    .position(|pubkey| pubkey == &message_account.account.pubkey)
                {
                    self.musthave_sysvar_accounts_bitmask |= 1 << position;
                }
                if message_account.account.pubkey == *ALPENGLOW_CLOCK_ACCOUNT {
                    self.footer_expected = true;
                }
                self.accounts.push(Arc::clone(message_account));
            }
            Message::Transaction(message_transaction) => {
                if message_transaction.transaction.get_pre_encoded().is_none() {
                    TransactionEncoder::pre_encode(&message_transaction.transaction);
                }
                self.transactions.push(Arc::clone(message_transaction));
            }
            Message::Entry(message_entry) => {
                self.entries.push(Arc::clone(message_entry));
            }
            Message::Slot(message_slot)
                if message_slot.status == SlotStatus::CreatedBank
                    && self.created_bank.is_none() =>
            {
                self.created_bank = Some(Arc::clone(message_slot));
            }
            _ => return,
        }
        self.original_messages.push(event);
    }

    fn try_seal(&mut self) -> Result<(), TrySealError> {
        if self.is_sealed {
            return Err(TrySealError::AlreadySealed);
        }
        if self.created_bank.is_none() {
            return Err(TrySealError::NotSealable);
        }
        if self.musthave_sysvar_accounts_bitmask != (1 << MUST_HAVE_SYSVAR_ACCOUNTS.len()) - 1 {
            return Err(TrySealError::NotSealable);
        }
        let Some(blockmeta) = self.blockmeta.as_ref() else {
            return Err(TrySealError::NotSealable);
        };

        let expected_txn_count = blockmeta.executed_transaction_count as usize;
        if self.transactions.len() < expected_txn_count {
            return Err(TrySealError::NotSealable);
        }

        let expected_entry_count = blockmeta.entries_count as usize;
        if self.entries.len() < expected_entry_count {
            return Err(TrySealError::NotSealable);
        }
        if self.footer_expected && self.footer.is_none() {
            return Err(TrySealError::NotSealable);
        }
        self.is_sealed = true;
        Ok(())
    }

    fn seal(self) -> FrozenBank {
        let block_meta = self.blockmeta.expect("should be sealable");
        let account_info_vec = self
            .accounts
            .into_iter()
            .filter_map(|account| {
                let write_version = self
                    .account_write_version_map
                    .get(&account.account.pubkey)?;
                (*write_version == account.account.write_version).then_some(account)
            })
            .collect::<Vec<_>>();
        let created_bank = self.created_bank.expect("should be sealable");
        // The bank's content without its `CreatedBank`, which is kept apart: live
        // Confirmed/Finalized clients already got it when the bank was created.
        let mut dedup_messages = Vec::with_capacity(self.original_messages.len());
        let mut created_bank_index = 0;
        for message in self.original_messages {
            match &message {
                Message::Slot(_) => {
                    created_bank_index = dedup_messages.len();
                    continue;
                }
                Message::Account(account) if !account.owner_reject_exempt => {
                    let Some(write_version) =
                        self.account_write_version_map.get(&account.account.pubkey)
                    else {
                        continue;
                    };
                    if *write_version != account.account.write_version {
                        continue;
                    }
                }
                _ => {}
            }
            dedup_messages.push(message);
        }

        if self.transactions.len() != block_meta.executed_transaction_count as usize {
            metrics::incr_geyser_block_mismatch_transaction();
            log::warn!(
                "Block meta transaction count {} does not match actual transaction count {} for slot {}",
                block_meta.executed_transaction_count,
                self.transactions.len(),
                block_meta.slot
            );
        }

        let pre_computed_message_block = Arc::new(MessageBlock::new(
            Arc::clone(&block_meta),
            Arc::clone(&created_bank),
            self.transactions,
            account_info_vec,
            self.entries,
        ));

        FrozenBank {
            bank_id: self.bank_id,
            slot: self.slot,
            original_messages: Arc::new(dedup_messages),
            created_bank,
            created_bank_index,
            block_meta,
            block_footer: self.footer,
            pre_computed_message_block,
        }
    }
}

pub struct FrozenBank {
    bank_id: BankId,
    slot: Slot,
    // The bank's content in arrival order, without its `CreatedBank`.
    original_messages: Arc<Vec<Message>>,
    // The `CreatedBank` the plugin received, and where it arrived in `original_messages`:
    // after the bank's construction-time account writes.
    created_bank: Arc<MessageSlot>,
    created_bank_index: usize,
    block_meta: Arc<MessageBlockMeta>,
    block_footer: Option<Arc<MessageBlockFooter>>,
    pre_computed_message_block: Arc<MessageBlock>,
}

impl FrozenBank {
    pub fn get_message_block(&self) -> Arc<MessageBlock> {
        Arc::clone(&self.pre_computed_message_block)
    }

    /// The bank's content in arrival order, without its `CreatedBank`. See
    /// [`Self::created_bank`] and [`Self::created_bank_index`] to put it back.
    pub fn messages(&self) -> Arc<Vec<Message>> {
        Arc::clone(&self.original_messages)
    }

    /// The `CreatedBank` status the plugin received for this bank.
    pub fn created_bank(&self) -> Arc<MessageSlot> {
        Arc::clone(&self.created_bank)
    }

    /// Returns where the bank's [`SlotStatus::CreatedBank`] arrived among [`Self::messages`].
    ///
    /// Agave stores some accounts while it constructs a bank, before it sends
    /// [`SlotStatus::CreatedBank`]: SlotHashes and Clock for every bank, plus the
    /// epoch-crossing and partitioned-rewards accounts on some banks. Those writes are the
    /// messages before this index. A replay splits [`Self::messages`] here and sends
    /// [`Self::created_bank`] in between, so a replayed bank keeps the order of the live
    /// stream without copying its messages. An account written both before and after the
    /// [`SlotStatus::CreatedBank`], such as the Alpenglow Clock, only appears at its last
    /// write.
    ///
    /// # Returns
    ///
    /// The index in [`Self::messages`] of the first message that arrived after the
    /// [`SlotStatus::CreatedBank`], or the length of [`Self::messages`] if none did.
    pub const fn created_bank_index(&self) -> usize {
        self.created_bank_index
    }

    pub fn get_block_meta(&self) -> Arc<MessageBlockMeta> {
        Arc::clone(&self.block_meta)
    }

    pub fn get_block_footer(&self) -> Option<Arc<MessageBlockFooter>> {
        self.block_footer.clone()
    }
}

#[derive(Debug, Clone)]
pub struct SlotCommitmentStatusUpdate {
    pub slot: Slot,
    pub parent_slot: Option<Slot>,
    pub commitment: CommitmentLevel,
    pub bank_id: BankId,
}

struct SlotProgression {
    commitment: Vec<SlotCommitmentStatusUpdate>,
    max_commitment: CommitmentLevel,
}

pub struct ReplayedSlot<'frozen_bank> {
    pub frozen_block: &'frozen_bank FrozenBank,
    pub slot_status_messages: Vec<SlotCommitmentStatusUpdate>,
}

/// Yields one `ReplayedSlot` per replay-servable candidate bank, in ascending slot order.
/// A resolved slot (Confirmed/Finalized, or the common single-candidate case) yields
/// exactly one; a slot still genuinely ambiguous between two or more Processed banks with
/// no confirmed winner yet yields one per candidate, all at the same slot number -- there
/// is no other way to expose "no winner is clear yet" through this API.
pub struct ReplayIter<'storage> {
    storage: &'storage BlockMachineStorage,
    iter: Range<'storage, Slot, Vec<Arc<FrozenBank>>>,
    min_commitment: CommitmentLevel,
    current: std::slice::Iter<'storage, Arc<FrozenBank>>,
}

impl<'storage> Iterator for ReplayIter<'storage> {
    type Item = ReplayedSlot<'storage>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(frozen) = self.current.next() {
                return Some(ReplayedSlot {
                    frozen_block: frozen.as_ref(),
                    slot_status_messages: self.storage.replay_status_for(frozen),
                });
            }
            let (&slot, candidates) = self.iter.next()?;
            // A slot with no recorded progression at all is purely sealed-and-waiting --
            // treat it as Processed-level-available (it can't be anything higher without a
            // progression entry), matching `try_infer_sole_candidate_winner`'s inference.
            let max_commitment = self
                .storage
                .slot_commitment_progression_map
                .get(&slot)
                .map_or(CommitmentLevel::Processed, |p| p.max_commitment);
            if commitment_rank(max_commitment) < commitment_rank(self.min_commitment) {
                continue;
            }
            self.current = candidates.iter();
        }
    }
}

/// Bank-id-oriented block reconstruction. Replaces the earlier slot-keyed pipeline (backed
/// by the `yellowstone-block-machine` crate's `BlocksStateMachine`), which conflated
/// "a `FirstShredReceived` placeholder is sitting here" with "a genuine prior bank already
/// existed here" -- both looked identical from a slot-keyed perspective, which made every
/// slot's very first (and typically only) `CreatedBank` look like a reset, discarding the
/// sysvar writes that land right before it, on every slot, permanently.
///
/// Buffering by `bank_id` sidesteps that class of bug entirely: each bank instance -- real
/// or a repair replay -- gets its own independent buffer, so there is nothing for one to
/// clobber in another. A slot only ever has more than one live buffer while forks/repairs
/// are still being resolved; the moment any commitment status update names a `bank_id` as
/// the canonical bank for its slot, every sibling buffer for that slot is discarded.
pub struct BlockMachineStorage {
    // Bank instances still accumulating content, keyed by bank_id.
    banks: FoldHashMap<BankId, BankBuffer>,
    // Every bank_id ever seen for a slot, so the losers can be found once a winner is known.
    slot_to_banks: FoldHashMap<Slot, Vec<BankId>>,
    // bank_ids explicitly discarded as losers (via `discard_losing_banks`) or abandoned
    // (via `handle_dead_slot`), paired with the slot they belonged to so `sweep_stale_slots`
    // can age them out. A straggler event arriving afterward for one of these is ignored
    // outright rather than reviving a fresh buffer for it -- distinct from a bank_id that
    // simply hasn't been resolved as a winner *yet*, which must still be allowed to
    // accumulate normally (see `processed_banks_are_peers_until_one_is_confirmed`).
    discarded_bank_ids: FoldHashMap<BankId, Slot>,
    // Banks that received their own Processed status, paired with their slot so
    // `sweep_stale_slots` can age them out. Processed is delivered per bank, not per slot:
    // agave reports Processed for every bank it freezes, including a replacement for a bank
    // of the same slot that was already delivered, so a bank in here is delivered at
    // Processed as soon as it seals, whether or not it is its slot's resolved bank.
    processed_bank_ids: FoldHashMap<BankId, Slot>,
    // The bank_id currently believed to be a slot's canonical bank. Multiple banks for the
    // same slot can be simultaneously Processed with no precedence between them -- a
    // Processed sighting never sets or changes this (see `handle_commitment_update`) --
    // only Confirmed/Finalized status updates do, since Solana guarantees at most one bank
    // per slot ever reaches those. It can still change once, from a bank that was only ever
    // Processed to a different one that reaches Confirmed first (see
    // `supersede_resolved_bank`) -- e.g. a dump-and-repair replay correcting an earlier
    // bank whose hash was wrong -- but once a slot reaches Confirmed or Finalized this is
    // treated as final and can no longer change. The one exception is
    // `try_infer_sole_candidate_winner`'s single-candidate inference, which may set this
    // ahead of any direct status update when there's only ever been one bank_id for the
    // slot at all.
    resolved_bank_per_slot: FoldHashMap<Slot, BankId>,
    // The highest commitment level a slot must be treated as having reached, whether from a
    // direct status update or inherited from a confirmed/finalized descendant.
    slot_min_commitment: FoldHashMap<Slot, CommitmentLevel>,
    slot_commitment_progression_map: FoldHashMap<Slot, SlotProgression>,
    // Content available for replay, per slot. Holds every sealed candidate bank for a slot
    // that hasn't resolved a winner yet -- so a replay request can be served even while a
    // slot is genuinely ambiguous between two or more banks, all at Processed, with none
    // having precedence -- and collapses to exactly the one resolved bank the moment the
    // slot reaches Confirmed or Finalized (see `discard_losing_banks`). Never empty for a
    // tracked slot: an entry only exists once at least one candidate has sealed.
    replayed_slot: BTreeMap<Slot, Vec<Arc<FrozenBank>>>,
    ready_queue: VecDeque<(SlotCommitmentStatusUpdate, Arc<FrozenBank>)>,
    replayed_capacity: usize,
    num_buffered_finalized_slot: usize,
    min_slot: Option<Slot>,
    // Highest slot number observed across any message so far, and the value it had at the
    // last stale-slot sweep -- together these throttle `sweep_stale_slots` to run only every
    // `STALE_GC_INTERVAL` slots of forward progress instead of on every single message.
    max_slot_seen: Slot,
    last_stale_gc_at: Slot,
}

impl BlockMachineStorage {
    pub fn new(replayed_capacity: usize) -> Self {
        Self {
            banks: FoldHashMap::with_capacity(replayed_capacity),
            slot_to_banks: FoldHashMap::with_capacity(replayed_capacity),
            discarded_bank_ids: FoldHashMap::new(),
            processed_bank_ids: FoldHashMap::new(),
            resolved_bank_per_slot: FoldHashMap::with_capacity(replayed_capacity),
            slot_min_commitment: FoldHashMap::with_capacity(replayed_capacity),
            slot_commitment_progression_map: FoldHashMap::with_capacity(replayed_capacity),
            replayed_slot: BTreeMap::new(),
            ready_queue: VecDeque::with_capacity(replayed_capacity),
            replayed_capacity,
            num_buffered_finalized_slot: 0,
            min_slot: None,
            max_slot_seen: 0,
            last_stale_gc_at: 0,
        }
    }

    pub fn add(&mut self, message: Message) {
        self.observe_slot(message.get_slot());
        match message {
            Message::Slot(message_slot) => self.handle_slot_status(&message_slot),
            Message::Account(_) | Message::Transaction(_) | Message::Entry(_) => {
                self.handle_block_data(message);
            }
            Message::BlockMeta(block_meta) => self.handle_block_meta(block_meta),
            Message::BlockFooter(block_footer) => self.handle_block_footer(block_footer),
            Message::EntryUpdateParent(message) => self.handle_cleared_bank(
                message.update_parent.slot,
                message.update_parent.cleared_bank_id,
            ),
            _ => {
                // Message::Block is synthesized internally and never fed back in;
                // Message::DeshredTransaction goes through a separate pipeline entirely.
            }
        }
    }

    /// Tracks forward progress and periodically sweeps genuinely orphaned per-slot state
    /// that fell too far behind to ever plausibly resolve -- see `STALE_SLOT_THRESHOLD`.
    fn observe_slot(&mut self, slot: Slot) {
        if slot <= self.max_slot_seen {
            return;
        }
        self.max_slot_seen = slot;
        if self.max_slot_seen - self.last_stale_gc_at >= STALE_GC_INTERVAL {
            self.sweep_stale_slots();
            self.last_stale_gc_at = self.max_slot_seen;
        }
    }

    /// Backstop garbage collection for per-slot/per-bank state that `gc()` (capacity-based,
    /// triggered only for slots that successfully reached `replayed_slot`) never reaches:
    /// a bank that seals but whose slot never gets a commitment update naming any winner, a
    /// slot left ambiguous between multiple never-resolved candidate banks, or a discarded
    /// bank_id sitting in `discarded_bank_ids` forever. Slots already in `replayed_slot` are
    /// deliberately left alone here -- they're bounded by `gc()` instead, and remain valid
    /// replay content regardless of age.
    fn sweep_stale_slots(&mut self) {
        let cutoff = self.max_slot_seen.saturating_sub(STALE_SLOT_THRESHOLD);

        let stale_slots: Vec<Slot> = self
            .slot_to_banks
            .keys()
            .copied()
            .filter(|&slot| slot < cutoff && !self.replayed_slot.contains_key(&slot))
            .collect();
        for slot in stale_slots {
            if let Some(bank_ids) = self.slot_to_banks.remove(&slot) {
                for bank_id in bank_ids {
                    self.banks.remove(&bank_id);
                }
            }
            self.resolved_bank_per_slot.remove(&slot);
            self.slot_min_commitment.remove(&slot);
            self.slot_commitment_progression_map.remove(&slot);
            log::warn!(
                "garbage-collecting slot {slot}: never resolved within {STALE_SLOT_THRESHOLD} slots of the latest seen ({}) -- likely an abandoned bank that never received a commitment update",
                self.max_slot_seen
            );
        }

        self.discarded_bank_ids
            .retain(|_, &mut slot| slot >= cutoff);
        self.processed_bank_ids
            .retain(|_, &mut slot| slot >= cutoff);
    }

    fn handle_slot_status(&mut self, message_slot: &Arc<MessageSlot>) {
        match message_slot.status {
            // Neither carries a bank_id, and neither is needed: buffering is keyed by
            // bank_id and starts on first sight of any bank-scoped event, not on
            // FirstShredReceived: see the module doc comment for why that distinction
            // matters.
            SlotStatus::FirstShredReceived | SlotStatus::Completed => {}
            SlotStatus::Dead => self.handle_dead_slot(message_slot.slot),
            SlotStatus::CreatedBank => self.handle_created_bank(message_slot),
            SlotStatus::Processed | SlotStatus::Confirmed | SlotStatus::Finalized => {
                self.handle_commitment_update(message_slot);
            }
        }
    }

    fn register_bank_for_slot(&mut self, slot: Slot, bank_id: BankId) {
        let ids = self.slot_to_banks.entry(slot).or_default();
        if !ids.contains(&bank_id) {
            ids.push(bank_id);
        }
    }

    fn handle_created_bank(&mut self, message_slot: &Arc<MessageSlot>) {
        let Some(bank_id) = message_slot.bank_id else {
            log::warn!(
                "CreatedBank status for slot {} carries no bank_id; ignoring",
                message_slot.slot
            );
            return;
        };
        if self.discarded_bank_ids.contains_key(&bank_id) {
            return;
        }
        let slot = message_slot.slot;
        self.register_bank_for_slot(slot, bank_id);
        let bank = self
            .banks
            .entry(bank_id)
            .or_insert_with(|| BankBuffer::new(bank_id, slot));
        bank.add_event(Message::Slot(Arc::clone(message_slot)));
        if bank.parent_slot.is_none() {
            bank.parent_slot = message_slot.parent;
        }
        self.try_seal_bank(bank_id);
    }

    /// An update parent clears a bank and replays its slot on a new parent under a new
    /// bank_id; the cleared bank never freezes. Left registered, it stays a second candidate
    /// for the slot and a commitment inherited from a descendant cannot resolve a winner.
    fn handle_cleared_bank(&mut self, slot: Slot, bank_id: BankId) {
        if let Some(ids) = self.slot_to_banks.get_mut(&slot) {
            ids.retain(|id| *id != bank_id);
            if ids.is_empty() {
                self.slot_to_banks.remove(&slot);
            }
        }
        self.banks.remove(&bank_id);
        self.discarded_bank_ids.insert(bank_id, slot);
        if self.resolved_bank_per_slot.get(&slot) == Some(&bank_id) {
            self.resolved_bank_per_slot.remove(&slot);
        }
    }

    fn handle_dead_slot(&mut self, slot: Slot) {
        if let Some(bank_ids) = self.slot_to_banks.remove(&slot) {
            for bank_id in bank_ids {
                self.banks.remove(&bank_id);
                self.discarded_bank_ids.insert(bank_id, slot);
            }
        }
        self.resolved_bank_per_slot.remove(&slot);
        if self.replayed_slot.remove(&slot).is_some() {
            let max_commitment = self
                .slot_commitment_progression_map
                .get(&slot)
                .map(|p| p.max_commitment);
            if max_commitment != Some(CommitmentLevel::Finalized) {
                log::warn!(
                    "Slot {slot} marked Dead after already being partially delivered (highest commitment so far: {max_commitment:?}) -- any commitment level above that will never be delivered for this slot.",
                );
            }
        }
    }

    fn handle_commitment_update(&mut self, message_slot: &MessageSlot) {
        let Some(bank_id) = message_slot.bank_id else {
            log::warn!(
                "{:?} status for slot {} carries no bank_id; ignoring",
                message_slot.status,
                message_slot.slot
            );
            return;
        };
        if self.discarded_bank_ids.contains_key(&bank_id) {
            log::warn!(
                "commitment update for slot {} targets bank_id {bank_id}, which was already discarded as a loser -- dropping",
                message_slot.slot
            );
            return;
        }
        let slot = message_slot.slot;
        let commitment = match message_slot.status {
            SlotStatus::Processed => CommitmentLevel::Processed,
            SlotStatus::Confirmed => CommitmentLevel::Confirmed,
            SlotStatus::Finalized => CommitmentLevel::Finalized,
            _ => unreachable!("handle_commitment_update only called for commitment statuses"),
        };

        self.register_bank_for_slot(slot, bank_id);
        if let Some(parent) = message_slot.parent {
            // Bank-scoped, not slot-scoped: two banks for the same slot can legitimately
            // disagree on their parent (skipped slots are normal, and an equivocating
            // leader isn't required to pick the same parent for both of its conflicting
            // versions), so this must only ever update *this* bank_id's own record, never
            // a slot-keyed cache shared across bank_ids.
            if let Some(bank) = self.banks.get_mut(&bank_id) {
                if bank.parent_slot.is_none() {
                    bank.parent_slot = Some(parent);
                }
            }
        }

        // Processed is optimistic, and multiple banks for the same slot can legitimately
        // be simultaneously Processed with no precedence between any of them -- so a
        // Processed sighting must never resolve, supersede, or discard anything. Only
        // Confirmed/Finalized is authoritative: Solana guarantees at most one bank per
        // slot ever reaches it, and once it does, that resolution is final.
        if commitment != CommitmentLevel::Processed {
            if let Some(&existing) = self.resolved_bank_per_slot.get(&slot) {
                if existing != bank_id {
                    let existing_level = self
                        .slot_commitment_progression_map
                        .get(&slot)
                        .map(|p| p.max_commitment);
                    if matches!(
                        existing_level,
                        Some(CommitmentLevel::Confirmed) | Some(CommitmentLevel::Finalized)
                    ) {
                        log::warn!(
                            "commitment update for slot {slot} targets bank_id {bank_id}, but bank_id {existing} was already {existing_level:?} for this slot -- dropping",
                        );
                        return;
                    }
                    log::warn!(
                        "slot {slot}'s resolved bank changed from {existing} to {bank_id} (previous commitment: {existing_level:?}) -- superseding, likely a dump-and-repair replay correcting an optimistic Processed bank",
                    );
                    self.supersede_resolved_bank(slot, bank_id);
                }
            } else {
                self.resolved_bank_per_slot.insert(slot, bank_id);
            }
            self.discard_losing_banks(slot, bank_id);
        }

        self.bump_slot_commitment(slot, commitment);
        if commitment == CommitmentLevel::Processed {
            self.processed_bank_ids.insert(bank_id, slot);
            self.deliver_own_processed(slot, bank_id);
        }
    }

    /// Replaces `slot`'s resolved bank_id, dropping the live-delivery history that was
    /// built around the old (now-superseded) one so any subsequent commitment delivery is
    /// synthesized fresh for the new winner instead of being deduped against stale entries.
    /// `slot_min_commitment` is deliberately left untouched: it's the slot's target
    /// commitment level, independent of which bank_id ultimately satisfies it, so it stays
    /// valid across the swap. `replayed_slot` isn't touched here either -- the caller
    /// always follows this with `discard_losing_banks`, which collapses it down to just the
    /// new winner while preserving that winner's own already-sealed content if present.
    fn supersede_resolved_bank(&mut self, slot: Slot, new_winner: BankId) {
        self.resolved_bank_per_slot.insert(slot, new_winner);
        // The old winner only ever reached Processed, so only Processed deliveries are
        // recorded here. The new winner's own one was really delivered, so it is kept and
        // not sent again.
        if let Some(progression) = self.slot_commitment_progression_map.get_mut(&slot) {
            progression.commitment.retain(|u| u.bank_id == new_winner);
            if progression.commitment.is_empty() {
                self.slot_commitment_progression_map.remove(&slot);
            }
        }
    }

    fn discard_losing_banks(&mut self, slot: Slot, winner: BankId) {
        let Some(ids) = self.slot_to_banks.get_mut(&slot) else {
            return;
        };
        let losers: Vec<BankId> = ids.iter().copied().filter(|&id| id != winner).collect();
        ids.retain(|&id| id == winner);
        for loser in losers {
            self.banks.remove(&loser);
            self.discarded_bank_ids.insert(loser, slot);
        }
        // Collapse the replay-servable set down to just the winner -- once a slot resolves,
        // any other candidate that had been sitting there while it was still ambiguous is
        // no longer valid replay content (see the module doc comment).
        if let Some(candidates) = self.replayed_slot.get_mut(&slot) {
            candidates.retain(|c| c.bank_id == winner);
        }
    }

    /// Raises the effective commitment floor for `slot` to `level` (a no-op for the floor
    /// itself if it's already there or higher -- but the resolved winner's delivery is
    /// still (re-)attempted regardless, since a `supersede_resolved_bank` can clear
    /// delivered history without changing the floor), emits the corresponding update(s)
    /// immediately if the resolved winner's content is available, and retroactively
    /// propagates the same floor up the parent chain the first time it's raised, so an
    /// ancestor that never gets its own direct commitment status update from geyser (this
    /// does happen -- geyser does not guarantee one arrives for every slot) still ends up
    /// at least at its descendants' commitment level.
    fn bump_slot_commitment(&mut self, slot: Slot, level: CommitmentLevel) {
        let already_at_level = self
            .slot_min_commitment
            .get(&slot)
            .is_some_and(|&current| commitment_rank(current) >= commitment_rank(level));
        if !already_at_level {
            self.slot_min_commitment.insert(slot, level);
        }

        self.try_infer_sole_candidate_winner(slot);
        if let Some(frozen) = self.resolved_frozen_bank(slot) {
            self.emit_commitment_levels_up_to(slot, level, &frozen);
        }

        if already_at_level {
            // The floor was already raised (and thus already propagated to ancestors) on
            // an earlier call; nothing further to walk up for.
            return;
        }
        if let Some(parent) = self.resolved_parent_of(slot) {
            self.bump_slot_commitment(parent, level);
        }
    }

    /// The parent slot of whichever bank_id is *currently* resolved as `slot`'s canonical
    /// bank -- looked up fresh from that bank's own record every time, never cached
    /// per-slot. Two banks for the same slot can legitimately disagree on their parent
    /// (skipped slots are normal, and an equivocating leader isn't required to pick the
    /// same parent for both of its conflicting versions), so the only parent that's ever
    /// safe to use is the one belonging to the specific bank instance actually being
    /// treated as canonical right now -- which can itself change via `supersede_resolved_bank`.
    fn resolved_parent_of(&self, slot: Slot) -> Option<Slot> {
        let bank_id = *self.resolved_bank_per_slot.get(&slot)?;
        if let Some(bank) = self.banks.get(&bank_id) {
            return bank.parent_slot;
        }
        self.replay_candidate(slot, bank_id)
            .map(|frozen| frozen.block_meta.parent_slot)
    }

    /// The resolved winner's frozen content for `slot`, if it has both a known resolution
    /// and sealed (replay-servable) content for that specific bank_id.
    fn resolved_frozen_bank(&self, slot: Slot) -> Option<Arc<FrozenBank>> {
        let bank_id = *self.resolved_bank_per_slot.get(&slot)?;
        self.replay_candidate(slot, bank_id).cloned()
    }

    /// The replay candidate for `slot` belonging to `bank_id`, if it has sealed.
    fn replay_candidate(&self, slot: Slot, bank_id: BankId) -> Option<&Arc<FrozenBank>> {
        self.replayed_slot
            .get(&slot)?
            .iter()
            .find(|frozen| frozen.bank_id == bank_id)
    }

    /// Adds `frozen` as a candidate available for replay at its slot. Idempotent: a second
    /// call for a bank_id already present is a no-op, so callers never need to check first.
    fn add_replay_candidate(&mut self, slot: Slot, frozen: Arc<FrozenBank>) {
        let candidates = self.replayed_slot.entry(slot).or_default();
        if !candidates.iter().any(|c| c.bank_id == frozen.bank_id) {
            candidates.push(frozen);
        }
    }

    /// Infers `slot`'s resolved winning bank_id when it hasn't been named by a direct
    /// Confirmed/Finalized status update yet but there's exactly one known candidate for
    /// it -- geyser doesn't guarantee a direct commitment status update arrives for every
    /// slot (see `resolved_bank_per_slot`). A sole sealed bank wins even next to banks that
    /// never sealed: agave never announces a bank dumped before it froze, so it stays
    /// registered forever, and agave holds only one bank per slot at a time. With more than
    /// one still-unresolved candidate, which one is canonical genuinely can't be determined
    /// here, so it's left unresolved and logged (both remain available for Processed-level
    /// replay regardless).
    fn try_infer_sole_candidate_winner(&mut self, slot: Slot) {
        if self.resolved_bank_per_slot.contains_key(&slot) {
            return;
        }
        if let Some([sealed]) = self.replayed_slot.get(&slot).map(Vec::as_slice) {
            self.resolved_bank_per_slot.insert(slot, sealed.bank_id);
            return;
        }
        match self.slot_to_banks.get(&slot).map(Vec::as_slice) {
            Some([single]) => {
                self.resolved_bank_per_slot.insert(slot, *single);
            }
            Some(ids) if ids.len() > 1 => {
                log::warn!(
                    "slot {slot} needs to resolve (e.g. a descendant's commitment level is propagating up to it) but has {} competing bank_ids and none was ever directly confirmed -- cannot determine which is canonical",
                    ids.len()
                );
            }
            _ => {}
        }
    }

    fn try_seal_bank(&mut self, bank_id: BankId) {
        let Some(bank) = self.banks.get_mut(&bank_id) else {
            return;
        };
        if bank.try_seal().is_ok() {
            let bank = self.banks.remove(&bank_id).expect("just verified present");
            let slot = bank.slot;
            let frozen = Arc::new(bank.seal());
            self.on_bank_sealed(slot, bank_id, frozen);
        }
    }

    /// A sealed bank becomes replay-servable immediately, regardless of whether its slot
    /// has resolved a winner yet -- multiple banks can be simultaneously replay-servable
    /// while a slot is still ambiguous between them (see the module doc comment on
    /// `replayed_slot`). `discard_losing_banks` collapses this down to just the winner once
    /// the slot resolves. If this bank is (or becomes, via single-candidate inference) the
    /// resolved winner and the slot already has a known commitment floor -- e.g. inherited
    /// from a descendant before this bank even sealed -- its delivery is emitted now too.
    fn on_bank_sealed(&mut self, slot: Slot, bank_id: BankId, frozen: Arc<FrozenBank>) {
        self.min_slot = Some(self.min_slot.map_or(slot, |m| m.min(slot)));
        self.add_replay_candidate(slot, frozen);
        self.try_infer_sole_candidate_winner(slot);
        if self.resolved_bank_per_slot.get(&slot) == Some(&bank_id) {
            if let Some(&level) = self.slot_min_commitment.get(&slot) {
                if let Some(frozen) = self.resolved_frozen_bank(slot) {
                    self.emit_commitment_levels_up_to(slot, level, &frozen);
                }
            }
        }
        if self.processed_bank_ids.contains_key(&bank_id) {
            self.deliver_own_processed(slot, bank_id);
        }
    }

    /// Delivers `bank_id` at Processed if it has sealed, whether or not it is `slot`'s
    /// resolved bank -- see `processed_bank_ids`. A no-op if it was already delivered.
    fn deliver_own_processed(&mut self, slot: Slot, bank_id: BankId) {
        if let Some(frozen) = self.replay_candidate(slot, bank_id).cloned() {
            self.emit_commitment_levels_up_to(slot, CommitmentLevel::Processed, &frozen);
        }
    }

    fn emit_commitment_levels_up_to(
        &mut self,
        slot: Slot,
        level: CommitmentLevel,
        frozen: &Arc<FrozenBank>,
    ) {
        let bank_id = frozen.bank_id;
        // Read directly off this specific bank's own blockmeta -- authoritative and
        // always in sync with whichever bank is actually being delivered, unlike a
        // slot-keyed cache (two banks for the same slot can disagree on their parent).
        let parent_slot = Some(frozen.block_meta.parent_slot);
        let mut to_emit: Vec<SlotCommitmentStatusUpdate> = Vec::new();
        let mut newly_finalized = false;
        {
            let progression = self
                .slot_commitment_progression_map
                .entry(slot)
                .or_insert_with(|| SlotProgression {
                    commitment: Vec::new(),
                    max_commitment: CommitmentLevel::Processed,
                });
            for candidate in [
                CommitmentLevel::Processed,
                CommitmentLevel::Confirmed,
                CommitmentLevel::Finalized,
            ] {
                if commitment_rank(candidate) > commitment_rank(level) {
                    break;
                }
                // Processed is delivered once per bank, Confirmed/Finalized once per slot.
                if progression.commitment.iter().any(|u| {
                    u.commitment == candidate
                        && (candidate != CommitmentLevel::Processed || u.bank_id == bank_id)
                }) {
                    continue;
                }
                let update = SlotCommitmentStatusUpdate {
                    slot,
                    parent_slot,
                    commitment: candidate,
                    bank_id,
                };
                progression.commitment.push(update.clone());
                if commitment_rank(candidate) > commitment_rank(progression.max_commitment) {
                    progression.max_commitment = candidate;
                }
                if candidate == CommitmentLevel::Finalized {
                    newly_finalized = true;
                }
                to_emit.push(update);
            }
        }
        if newly_finalized {
            self.num_buffered_finalized_slot += 1;
        }
        for update in to_emit {
            self.ready_queue.push_back((update, Arc::clone(frozen)));
        }
        if newly_finalized {
            self.gc();
        }
    }

    fn handle_block_data(&mut self, message: Message) {
        let slot = message.get_slot();
        let bank_id = match &message {
            Message::Account(a) => a.bank_id,
            Message::Transaction(t) => Some(t.bank_id),
            Message::Entry(e) => Some(e.bank_id),
            _ => None,
        };
        let Some(bank_id) = bank_id else {
            // No bank_id -- e.g. a startup snapshot account -- is not part of live bank
            // reconstruction, so it's ignored here rather than buffered.
            return;
        };
        if self.discarded_bank_ids.contains_key(&bank_id) {
            return;
        }
        // A bank that has already sealed is immutable -- more data for it afterward is
        // always anomalous, whether or not its slot has resolved a winner yet.
        if self.replay_candidate(slot, bank_id).is_some() {
            log::error!(
                "UNEXPECTED: received block data for bank {bank_id} (slot {slot}) that is already sealed. Dropping.",
            );
            return;
        }
        self.register_bank_for_slot(slot, bank_id);
        let bank = self
            .banks
            .entry(bank_id)
            .or_insert_with(|| BankBuffer::new(bank_id, slot));
        bank.add_event(message);
        self.try_seal_bank(bank_id);
    }

    fn handle_block_meta(&mut self, block_meta: Arc<MessageBlockMeta>) {
        let bank_id = block_meta.bank_id;
        let slot = block_meta.slot;
        if self.discarded_bank_ids.contains_key(&bank_id) {
            return;
        }
        if self.replay_candidate(slot, bank_id).is_some() {
            return;
        }
        self.register_bank_for_slot(slot, bank_id);
        let bank = self
            .banks
            .entry(bank_id)
            .or_insert_with(|| BankBuffer::new(bank_id, slot));
        if bank.parent_slot.is_none() {
            bank.parent_slot = Some(block_meta.parent_slot);
        }
        bank.blockmeta = Some(Arc::clone(&block_meta));
        self.try_seal_bank(bank_id);
    }

    fn handle_block_footer(&mut self, block_footer: Arc<MessageBlockFooter>) {
        let bank_id = block_footer.bank_id;
        let slot = block_footer.slot;
        if self.discarded_bank_ids.contains_key(&bank_id) {
            return;
        }
        // Only a bank that never wrote `ALPENGLOW_CLOCK_ACCOUNT` can seal before its footer.
        if self.replay_candidate(slot, bank_id).is_some() {
            log::warn!("block footer for bank {bank_id} (slot {slot}) arrived after the bank sealed; dropping");
            return;
        }
        self.register_bank_for_slot(slot, bank_id);
        let bank = self
            .banks
            .entry(bank_id)
            .or_insert_with(|| BankBuffer::new(bank_id, slot));
        bank.footer = Some(block_footer);
        self.try_seal_bank(bank_id);
    }

    fn gc(&mut self) {
        while self.replayed_slot.len() > self.replayed_capacity
            && self.num_buffered_finalized_slot > MINIMUM_FINALIZED_SLOT_TO_BUFFER
        {
            let Some((&oldest_slot, _)) = self.replayed_slot.iter().next() else {
                break;
            };
            self.evict_slot(oldest_slot);
        }
        self.refresh_min_slot();
    }

    fn evict_slot(&mut self, slot: Slot) {
        self.replayed_slot.remove(&slot);
        if let Some(progression) = self.slot_commitment_progression_map.remove(&slot) {
            if progression.max_commitment == CommitmentLevel::Finalized {
                self.num_buffered_finalized_slot =
                    self.num_buffered_finalized_slot.saturating_sub(1);
            }
        }
        self.slot_min_commitment.remove(&slot);
        self.resolved_bank_per_slot.remove(&slot);
        self.slot_to_banks.remove(&slot);
    }

    fn refresh_min_slot(&mut self) {
        self.min_slot = self.replayed_slot.keys().next().copied();
    }

    pub fn pop_ready_block(&mut self) -> Option<(SlotCommitmentStatusUpdate, Arc<FrozenBank>)> {
        self.ready_queue.pop_front()
    }

    pub fn replay_from_slot(&self, slot: Slot, min_commitment: CommitmentLevel) -> ReplayIter<'_> {
        let iter = self.replayed_slot.range(slot..);
        ReplayIter {
            storage: self,
            iter,
            min_commitment,
            current: [].iter(),
        }
    }

    /// The banks that have not sealed yet, so are not in [`Self::replay_from_slot`].
    pub fn unsealed_bank_ids(&self) -> Vec<BankId> {
        self.banks.keys().copied().collect()
    }

    pub const fn min_replayable_slot(&self) -> Option<Slot> {
        self.min_slot
    }

    /// The status update history to attach to a replayed candidate bank. For the single
    /// resolved winner of a slot with a recorded progression, this is the real
    /// Processed/Confirmed/Finalized history. For one of several still-ambiguous
    /// candidates (or a resolved winner whose progression was never recorded, e.g. sealed
    /// purely via single-candidate inference with no direct status update at all), only a
    /// synthesized Processed entry for that specific bank is returned, since that's the
    /// only thing ever actually confirmed about it.
    fn replay_status_for(&self, frozen: &FrozenBank) -> Vec<SlotCommitmentStatusUpdate> {
        if self.resolved_bank_per_slot.get(&frozen.slot) == Some(&frozen.bank_id) {
            if let Some(progression) = self.slot_commitment_progression_map.get(&frozen.slot) {
                let history: Vec<_> = progression
                    .commitment
                    .iter()
                    .filter(|u| u.bank_id == frozen.bank_id)
                    .cloned()
                    .collect();
                if !history.is_empty() {
                    return history;
                }
            }
        }
        vec![SlotCommitmentStatusUpdate {
            slot: frozen.slot,
            parent_slot: Some(frozen.block_meta.parent_slot),
            commitment: CommitmentLevel::Processed,
            bank_id: frozen.bank_id,
        }]
    }
}
