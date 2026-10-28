//! Per-stream B state, driven exclusively by the original client_loop sender.
use {
    crate::{
        order_protocol::*,
        plugin::{
            filter::message::{FilteredUpdate, FilteredUpdateOneof},
            message::{Message as SourceMessage, MessageBlock, SlotStatus},
            order_permissions::Permissions,
        },
    },
    anyhow::{ensure, Context, Result},
    prost::Message,
    std::{
        collections::{HashMap, HashSet},
        ops::Deref,
        sync::Arc,
        time::{Duration, Instant},
    },
    yellowstone_grpc_proto::prelude::*,
};

#[derive(Debug, Clone)]
pub struct BankGroup {
    pub slot: u64,
    pub bank_id: u64,
    pub observed: u64,
    pub executed: u64,
    pub valid: bool,
    pub resource_limit: bool,
    pub source_after_seal: bool,
}
impl BankGroup {
    pub(crate) fn from_frozen(
        frozen: &crate::block_reconstruction_v2::FrozenBank,
        limits: &crate::plugin::filter::limits::FilterLimitsOrder,
    ) -> Self {
        let mut group = Self::with_limit(
            &frozen.get_message_block(),
            limits.max_transactions_per_bank,
        );
        group.source_after_seal = !frozen.order_source_valid();
        group.valid &= !group.source_after_seal;
        group
    }
    pub fn from_block(block: &MessageBlock) -> Self {
        Self::with_limit(block, 65536)
    }
    fn with_limit(block: &MessageBlock, max_transactions: usize) -> Self {
        let mut indices = HashSet::new();
        let mut signatures = HashSet::new();
        let executed = block.meta.executed_transaction_count;
        let observed = block.transactions.len() as u64;
        let resource_limit = block.transactions.len() > max_transactions;
        let valid = !resource_limit
            && observed == executed
            && block.transactions.iter().all(|tx| {
                let i = tx.transaction.index as u64;
                i < executed && indices.insert(i) && signatures.insert(tx.transaction.signature)
            });
        Self {
            slot: block.meta.slot,
            bank_id: block.meta.bank_id,
            observed,
            executed,
            valid,
            resource_limit,
            source_after_seal: false,
        }
    }
}
#[derive(Debug, Clone)]
pub enum GroupBoundary {
    Begin(BankGroup),
    End { slot: u64, bank_id: u64 },
}
#[derive(Debug, Clone)]
pub struct BroadcastEnvelope {
    pub messages: Arc<Vec<SourceMessage>>,
    pub group: Option<GroupBoundary>,
    pub late: Option<Arc<Vec<crate::order_integrity::BankRef>>>,
}
impl Deref for BroadcastEnvelope {
    type Target = Vec<SourceMessage>;
    fn deref(&self) -> &Self::Target {
        &self.messages
    }
}

struct Epoch {
    context: OrderContext,
    keys: Vec<OrderKeyCoverage>,
    tracked: HashSet<[u8; 32]>,
    key_hash: Vec<u8>,
    seq: u64,
    events: OrderedDigest,
    seals: OrderedDigest,
    seal_count: u64,
}
struct Pending {
    request: SubscribeRequestOrderWrites,
    owner: SubscribeUpdateOwnerRejectExemption,
    owner_hash: Vec<u8>,
}
struct Group {
    bank: BankGroup,
    started: Instant,
    covered: bool,
    count: u64,
    first: Option<u64>,
    last: Option<u64>,
    digest: OrderedDigest,
    meta_hash: Option<Vec<u8>>,
    confirmed_hash: Option<Vec<u8>>,
}

pub struct OrderSession {
    watch: Option<crate::order_integrity::OrderWatch>,
    historical: Vec<(OrderContext, u64)>,
    stream: std::sync::OnceLock<Vec<u8>>,
    active: Option<Epoch>,
    pending: Option<Pending>,
    group: Option<Group>,
    closing: Option<Instant>,
    ever: bool,
    stopped: bool,
}
impl Default for OrderSession {
    fn default() -> Self {
        Self {
            watch: None,
            historical: Vec::new(),
            stream: std::sync::OnceLock::new(),
            active: None,
            pending: None,
            group: None,
            closing: None,
            ever: false,
            stopped: false,
        }
    }
}

/// Pure request coverage checks use explicit account subscriptions, never transaction include.
pub fn validate_coverage(request: &SubscribeRequest, keys: &[Vec<u8>]) -> Result<()> {
    ensure!(
        request.commitment == Some(1)
            && request.from_slot.is_none()
            && request.accounts_data_slice.is_empty()
            && request.ping.is_none(),
        "B requires live confirmed, no slice/replay/ping"
    );
    ensure!(
        request
            .slots
            .values()
            .any(|f| f.interslot_updates == Some(true))
            && !request.blocks_meta.is_empty(),
        "B needs complete slot and BlockMeta filters"
    );
    let mut explicit = HashSet::new();
    for filter in request.accounts.values() {
        if filter.include_owner_rejected == Some(true)
            && filter.owner.is_empty()
            && filter.filters.is_empty()
            && filter.nonempty_txn_signature.is_none()
            && filter.cuckoo_accounts_filter.is_none()
        {
            for key in &filter.account {
                let decoded = bs58::decode(key).into_vec()?;
                if decoded.len() == 32 {
                    explicit.insert(decoded);
                }
            }
        }
    }
    ensure!(
        keys.iter().all(|key| explicit.contains(key)),
        "tracked keys lack unrestricted explicit owner exemption"
    );
    Ok(())
}

impl OrderSession {
    fn stream_id(&self) -> Vec<u8> {
        self.stream
            .get_or_init(|| uuid::Uuid::new_v4().as_bytes().to_vec())
            .clone()
    }
    pub fn mark_requested(&mut self) {
        self.ever = true;
    }

    pub fn register_watch(
        &mut self,
        registry: &Arc<crate::order_integrity::OrderIntegrity>,
    ) -> Result<()> {
        if self.watch.is_none() {
            self.watch = Some(registry.register()?);
        }
        Ok(())
    }
    pub fn late(
        &mut self,
        bank: crate::order_integrity::BankRef,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        let epoch = match self.watch.as_ref() {
            Some(w) => w.unreported_epoch(bank)?,
            None => None,
        };
        let Some(epoch) = epoch else {
            return Ok(());
        };
        if self
            .active
            .as_ref()
            .is_some_and(|e| e.context.source_epoch == epoch)
        {
            self.fail(
                OrderGapReason::SourceAfterSeal,
                Some(bank),
                "source data after immutable Bank seal",
                send,
            )?;
            if let Some(watch) = self.watch.as_ref() {
                watch.mark_reported(bank, &epoch)?;
            }
            return Ok(());
        }
        if let Some((context, seq)) = self
            .historical
            .iter()
            .find(|(c, _)| c.source_epoch == epoch)
        {
            let update = control(subscribe_update::UpdateOneof::OrderGap(
                SubscribeUpdateOrderGap {
                    context: Some(context.clone()),
                    next_source_seq: Some(*seq),
                    reason: Some(OrderGapReason::SourceAfterSeal as i32),
                    slot: Some(bank.0),
                    bank_id: Some(bank.1),
                    detail: "source data after sealed historical epoch".into(),
                },
            ));
            send(update)?;
            if let Some(watch) = self.watch.as_ref() {
                watch.mark_reported(bank, &epoch)?;
            }
            self.fail(
                OrderGapReason::SourceAfterSeal,
                Some(bank),
                "historical B epoch revoked",
                send,
            )?;
        }
        Ok(())
    }

    pub const fn ever_requested(&self) -> bool {
        self.ever
    }
    pub const fn has_active(&self) -> bool {
        self.active.is_some()
    }
    pub fn reject(
        &self,
        request: Option<&SubscribeRequestOrderWrites>,
        reason: OrderRejectReason,
        detail: &str,
    ) -> FilteredUpdate {
        control(subscribe_update::UpdateOneof::OrderRejected(
            SubscribeUpdateOrderRejected {
                source_stream_id: self.stream_id(),
                request_version: request.and_then(|r| r.request_version),
                request_sha256: request
                    .filter(|r| r.request_sha256.len() == 32)
                    .map(|r| r.request_sha256.clone()),
                reason: Some(reason as i32),
                detail: bounded_detail(detail),
            },
        ))
    }
    pub fn request(
        &mut self,
        raw: &SubscribeRequest,
        owner: &FilteredUpdate,
        watermark: u64,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        self.ever = true;
        let Some(request) = raw.order_writes.as_ref() else {
            return Ok(());
        };
        let precheck = if self.pending.is_some() || self.closing.is_some() {
            Some((
                OrderRejectReason::PendingExists,
                "another B control is pending".to_owned(),
            ))
        } else if request.protocol_version != Some(1) {
            Some((
                OrderRejectReason::UnsupportedCapability,
                "unsupported source protocol version".to_owned(),
            ))
        } else {
            validate_coverage(raw, &request.tracked_keys)
                .err()
                .map(|e| (OrderRejectReason::ScopeCoverageInvalid, e.to_string()))
        };
        if let Some((reason, detail)) = precheck {
            send(self.reject(Some(request), reason, &detail))?;
            return Ok(());
        }
        let result = (|| {
            ensure!(!self.stopped, "B already stopped; start a new stream/run");
            ensure!(raw.order_close.is_none(), "mutually exclusive B controls");
            ensure!(
                self.pending.is_none() && self.closing.is_none(),
                "another B control is pending"
            );
            validate_request(request)?;
            validate_coverage(raw, &request.tracked_keys)?;
            match self.active.as_ref() {
                Some(epoch) => {
                    ensure!(
                        request.previous_epoch.as_ref() == Some(&epoch.context.source_epoch),
                        "previous epoch mismatch"
                    );
                    ensure!(
                        request.request_version
                            == epoch.context.request_version.and_then(|v| v.checked_add(1)),
                        "request version must advance exactly once"
                    );
                    ensure!(
                        epoch.keys.len() < request.tracked_keys.len()
                            && epoch
                                .keys
                                .iter()
                                .all(|key| request.tracked_keys.binary_search(&key.pubkey).is_ok()),
                        "scope must strictly expand"
                    );
                }
                None => ensure!(
                    request.request_version == Some(1) && request.previous_epoch.is_none(),
                    "first request must be version one"
                ),
            }
            let update = owner.as_subscribe_update();
            let Some(subscribe_update::UpdateOneof::OwnerRejectExemption(owner)) =
                update.update_oneof
            else {
                anyhow::bail!("missing actual owner ack");
            };
            self.pending = Some(Pending {
                request: request.clone(),
                owner,
                owner_hash: Vec::new(),
            });
            Ok(())
        })();
        // Filled below outside the closure to keep ownership of the exact owner update.
        if result.is_ok() {
            if let Some(p) = self.pending.as_mut() {
                p.owner_hash = sha256(&owner.encode_to_vec());
            }
        }
        if let Err(error) = result {
            let reason = if self.pending.is_some() || self.closing.is_some() {
                OrderRejectReason::PendingExists
            } else {
                OrderRejectReason::InvalidRequest
            };
            send(self.reject(Some(request), reason, &error.to_string()))?;
            return Ok(());
        }
        if self.group.is_none() {
            self.switch(watermark, send)?;
        }
        Ok(())
    }
    fn switch(
        &mut self,
        watermark: u64,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        if self.pending.is_none() {
            return Ok(());
        }
        if self.active.is_some() && !self.normal_boundary_ready(send)? {
            return Ok(());
        }
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        let previous = self.active.take();
        if let Some(epoch) = &previous {
            self.historical.push((epoch.context.clone(), epoch.seq));
        }
        let previous_epoch = previous.as_ref().map(|e| e.context.source_epoch.clone());
        let previous_hash = if let Some(epoch) = &previous {
            let seal = Self::epoch_seal(epoch, OrderEpochSealReason::Replaced);
            let hash = sha256(&seal.encode_to_vec());
            send(seal)?;
            Some(hash)
        } else {
            None
        };
        let floor = watermark
            .checked_add(1)
            .context("callback watermark exhausted")?;
        let mut owner_slots = HashMap::new();
        for key in &pending.owner.accounts {
            owner_slots.insert(bs58::decode(&key.pubkey).into_vec()?, key.active_from_slot);
        }
        let mut keys = Vec::new();
        let mut tracked = HashSet::new();
        for key in &pending.request.tracked_keys {
            let exempt = *owner_slots
                .get(key)
                .context("owner ack lacks tracked key")?;
            let old = previous
                .as_ref()
                .and_then(|e| e.keys.iter().find(|k| k.pubkey == *key));
            let active = old
                .and_then(|k| k.active_from_slot)
                .unwrap_or(floor.max(exempt));
            keys.push(OrderKeyCoverage {
                pubkey: key.clone(),
                active_from_slot: Some(active),
                owner_exempt_active_from_slot: Some(exempt),
            });
            tracked.insert(<[u8; 32]>::try_from(key.as_slice())?);
        }
        let active_from = keys
            .iter()
            .filter_map(|k| k.active_from_slot)
            .max()
            .context("empty scope")?;
        let key_hash = keys_digest(&pending.request.tracked_keys)?;
        let context = OrderContext {
            protocol_version: Some(1),
            source_stream_id: self.stream_id(),
            source_epoch: uuid::Uuid::new_v4().as_bytes().to_vec(),
            request_version: pending.request.request_version,
        };
        let ack = control(subscribe_update::UpdateOneof::OrderCoverageAck(
            SubscribeUpdateOrderCoverageAck {
                context: Some(context.clone()),
                request_sha256: pending.request.request_sha256,
                tracked_keys_sha256: key_hash.clone(),
                keys: keys.clone(),
                active_from_slot: Some(active_from),
                previous_epoch,
                previous_epoch_seal_sha256: previous_hash,
                observed_callback_slot_high_watermark: Some(watermark),
                owner_exemption_update_sha256: pending.owner_hash,
                capabilities: Some(capabilities()),
            },
        ));
        send(ack)?;
        self.active = Some(Epoch {
            context,
            keys,
            tracked,
            key_hash,
            seq: 0,
            events: OrderedDigest::default(),
            seals: OrderedDigest::default(),
            seal_count: 0,
        });
        Ok(())
    }
    pub fn coverage_changed(
        &mut self,
        raw: &SubscribeRequest,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| validate_coverage(raw, &p.request.tracked_keys).is_err())
        {
            if let Some(pending) = self.pending.take() {
                send(self.reject(
                    Some(&pending.request),
                    OrderRejectReason::ScopeCoverageInvalid,
                    "installed A update removed pending scope coverage",
                ))?;
            }
        }
        if let Some(epoch) = &self.active {
            let keys = epoch
                .keys
                .iter()
                .map(|k| k.pubkey.clone())
                .collect::<Vec<_>>();
            if validate_coverage(raw, &keys).is_err() {
                self.fail(
                    OrderGapReason::SubscriptionProtocol,
                    None,
                    "A replacement removed active B coverage",
                    send,
                )?;
            }
        }
        Ok(())
    }
    pub const fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
    /// The caller has just installed this filter and enqueued this actual full owner ack.
    pub fn refresh_pending_owner(&mut self, owner: &FilteredUpdate) -> Result<()> {
        if let Some(pending) = self.pending.as_mut() {
            let Some(subscribe_update::UpdateOneof::OwnerRejectExemption(current)) =
                owner.as_subscribe_update().update_oneof
            else {
                anyhow::bail!("pending scope requires actual owner registry acknowledgement");
            };
            pending.owner = current;
            pending.owner_hash = sha256(&owner.encode_to_vec());
        }
        Ok(())
    }
    fn coverage_floor(&self) -> Option<u64> {
        self.active
            .as_ref()
            .and_then(|e| e.keys.iter().filter_map(|k| k.active_from_slot).min())
    }
    pub fn close(
        &mut self,
        raw: &SubscribeRequest,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        let Some(close) = &raw.order_close else {
            return Ok(());
        };
        let only = SubscribeRequest {
            order_close: Some(close.clone()),
            ..Default::default()
        };
        let valid = !self.stopped
            && *raw == only
            && self.pending.is_none()
            && self.closing.is_none()
            && self.active.as_ref().is_some_and(|e| {
                close.context.as_ref() == Some(&e.context)
                    && close.tracked_keys_sha256 == e.key_hash
            });
        if !valid {
            send(self.reject(
                None,
                OrderRejectReason::InvalidRequest,
                "invalid close control",
            ))?;
            return Ok(());
        }
        let floor = self
            .coverage_floor()
            .context("active close coverage missing")?;
        self.closing = Some(Instant::now());
        if self.group.is_none()
            && !self
                .watch
                .as_ref()
                .map(|w| w.pending(floor))
                .transpose()?
                .unwrap_or(false)
        {
            self.finish_close(send)?;
        }
        Ok(())
    }
    /// Every normal epoch sealing exit uses the same current source health and
    /// pending-bank barrier; completing an unrelated group cannot clear it.
    fn normal_boundary_ready(
        &mut self,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<bool> {
        self.check_deadline(send)?;
        if self.stopped || self.active.is_none() {
            return Ok(false);
        }
        if let (Some(watch), Some(floor)) = (&self.watch, self.coverage_floor()) {
            watch.check()?;
            return Ok(!watch.pending(floor)?);
        }
        Ok(true)
    }
    fn finish_close(&mut self, send: &mut impl FnMut(FilteredUpdate) -> Result<()>) -> Result<()> {
        if !self.normal_boundary_ready(send)? {
            return Ok(());
        }
        if let Some(epoch) = self.active.as_ref() {
            send(Self::epoch_seal(epoch, OrderEpochSealReason::NormalEnd))?;
        }
        if let Some(epoch) = self.active.take() {
            self.historical.push((epoch.context.clone(), epoch.seq));
        }
        self.closing = None;
        self.stopped = true;
        Ok(())
    }
    fn epoch_seal(epoch: &Epoch, reason: OrderEpochSealReason) -> FilteredUpdate {
        control(subscribe_update::UpdateOneof::OrderEpochSeal(
            SubscribeUpdateOrderEpochSeal {
                context: Some(epoch.context.clone()),
                event_count: Some(epoch.seq),
                last_source_seq: epoch.seq.checked_sub(1),
                ordered_events_sha256: epoch.events.finish(),
                slot_seal_count: Some(epoch.seal_count),
                ordered_slot_seals_sha256: epoch.seals.finish(),
                reason: Some(reason as i32),
                tracked_keys_sha256: epoch.key_hash.clone(),
            },
        ))
    }
    pub fn begin(
        &mut self,
        bank: BankGroup,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        if self.group.is_some() {
            self.fail(
                OrderGapReason::IncompleteBank,
                Some((bank.slot, bank.bank_id)),
                "nested or missing group end",
                send,
            )?;
        }
        let covered = self.active.as_ref().is_some_and(|e| {
            e.keys
                .iter()
                .any(|k| k.active_from_slot.is_some_and(|s| s <= bank.slot))
        });
        if covered && !bank.valid {
            self.fail(
                if bank.resource_limit {
                    OrderGapReason::ResourceLimit
                } else if bank.source_after_seal {
                    OrderGapReason::SourceAfterSeal
                } else {
                    OrderGapReason::SourceCountMismatch
                },
                Some((bank.slot, bank.bank_id)),
                "bank count/index/signature mismatch",
                send,
            )?;
        }
        if covered {
            if let Some(watch) = self.watch.as_ref() {
                if let Err(error) = watch.check() {
                    self.fail(
                        OrderGapReason::ResourceLimit,
                        Some((bank.slot, bank.bank_id)),
                        &error.to_string(),
                        send,
                    )?;
                }
            }
            if let (Some(watch), Some(epoch)) = (&self.watch, &self.active) {
                if let Err(error) =
                    watch.bind((bank.slot, bank.bank_id), &epoch.context.source_epoch)
                {
                    self.fail(
                        OrderGapReason::SourceAfterSeal,
                        Some((bank.slot, bank.bank_id)),
                        &error.to_string(),
                        send,
                    )?;
                }
            }
        }
        self.group = Some(Group {
            bank,
            started: Instant::now(),
            covered,
            count: 0,
            first: None,
            last: None,
            digest: OrderedDigest::default(),
            meta_hash: None,
            confirmed_hash: None,
        });
        Ok(())
    }
    pub fn observe_sent(
        &mut self,
        message: &SourceMessage,
        updates: &[FilteredUpdate],
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        if let SourceMessage::Slot(slot) = message {
            if slot.status == SlotStatus::Dead {
                let banks = match &self.watch {
                    Some(w) => w.banks_at_slot(slot.slot)?,
                    None => Vec::new(),
                };
                for bank in banks {
                    self.late(bank, send)?;
                }
            }
            if slot.status == SlotStatus::Dead
                && self.active.as_ref().is_some_and(|e| {
                    e.keys
                        .iter()
                        .any(|k| k.active_from_slot.is_some_and(|s| s <= slot.slot))
                })
            {
                self.fail(
                    OrderGapReason::IncompleteBank,
                    None,
                    "Dead in active coverage",
                    send,
                )?;
            }
        }
        if self.stopped {
            return Ok(());
        }
        let Some(group) = self.group.as_mut() else {
            if let SourceMessage::Transaction(tx) = message {
                if self.active.as_ref().is_some_and(|e| {
                    e.keys
                        .iter()
                        .any(|k| k.active_from_slot.is_some_and(|s| s <= tx.slot))
                }) {
                    self.fail(
                        OrderGapReason::IncompleteBank,
                        Some((tx.slot, tx.bank_id)),
                        "joined partial group within committed coverage",
                        send,
                    )?;
                }
            }
            return Ok(());
        };
        if !group.covered {
            return Ok(());
        }
        for update in updates {
            match &update.message {
                FilteredUpdateOneof::BlockMeta(meta)
                    if meta.slot == group.bank.slot && meta.bank_id == group.bank.bank_id =>
                {
                    group.meta_hash = Some(sha256(&update.encode_to_vec()))
                }
                FilteredUpdateOneof::Slot(slot)
                    if slot.slot == group.bank.slot
                        && slot.bank_id == Some(group.bank.bank_id)
                        && slot.status == SlotStatus::Confirmed =>
                {
                    group.confirmed_hash = Some(sha256(&update.encode_to_vec()))
                }
                _ => {}
            }
        }
        if let SourceMessage::Transaction(tx) = message {
            ensure!(
                tx.slot == group.bank.slot && tx.bank_id == group.bank.bank_id,
                "transaction outside current bank group"
            );
            let Some(epoch) = self.active.as_mut() else {
                return Ok(());
            };
            let permissions = tx.transaction.order_permissions();
            if !permissions.affects(&epoch.tracked) {
                return Ok(());
            }
            let next = epoch.seq.checked_add(1).context("source seq exhausted")?;
            let update = wrap(
                subscribe_update::UpdateOneof::TransactionOrder(SubscribeUpdateTransactionOrder {
                    context: Some(epoch.context.clone()),
                    source_seq: Some(epoch.seq),
                    slot: Some(tx.slot),
                    bank_id: Some(tx.bank_id),
                    index: Some(tx.transaction.index as u64),
                    signature: tx.transaction.signature.as_ref().to_vec(),
                    is_vote: Some(tx.transaction.is_vote),
                    transaction: Some(tx.transaction.transaction.clone()),
                    err: tx.transaction.meta.err.clone(),
                    loaded_writable: tx.transaction.meta.loaded_writable_addresses.clone(),
                    loaded_readonly: tx.transaction.meta.loaded_readonly_addresses.clone(),
                    permissions_state: Some(match permissions {
                        Permissions::Known(_) => 1,
                        Permissions::Unknown(_) => 2,
                    }),
                }),
                tx.created_at,
            );
            let bytes = update.encode_to_vec();
            send(update)?;
            epoch.events.append(&bytes)?;
            group.digest.append(&bytes)?;
            group.first = group.first.or(Some(epoch.seq));
            group.last = Some(epoch.seq);
            group.count = group.count.checked_add(1).context("group count overflow")?;
            epoch.seq = next;
        }
        Ok(())
    }
    pub fn end(
        &mut self,
        slot: u64,
        bank_id: u64,
        watermark: u64,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        self.check_deadline(send)?;
        if self.stopped {
            self.group = None;
            return self.check_deadline(send);
        }
        if let Some(group) = self.group.take() {
            if group.covered && self.active.is_some() {
                if group.bank.slot != slot
                    || group.bank.bank_id != bank_id
                    || group.meta_hash.is_none()
                    || group.confirmed_hash.is_none()
                {
                    self.fail(
                        OrderGapReason::IncompleteBank,
                        Some((slot, bank_id)),
                        "group end missing original metadata/Confirmed",
                        send,
                    )?;
                } else if let Some(epoch) = self.active.as_mut() {
                    let update = control(subscribe_update::UpdateOneof::OrderSlotSeal(
                        SubscribeUpdateOrderSlotSeal {
                            context: Some(epoch.context.clone()),
                            slot: Some(slot),
                            bank_id: Some(bank_id),
                            event_count: Some(group.count),
                            first_source_seq: group.first,
                            last_source_seq: group.last,
                            ordered_events_sha256: group.digest.finish(),
                            confirmed_update_sha256: group
                                .confirmed_hash
                                .context("Confirmed hash")?,
                            block_meta_update_sha256: group.meta_hash.context("BlockMeta hash")?,
                            observed_transaction_count: Some(group.bank.observed),
                            executed_transaction_count: Some(group.bank.executed),
                            complete_bank: Some(true),
                        },
                    ));
                    let bytes = update.encode_to_vec();
                    send(update)?;
                    epoch.seals.append(&bytes)?;
                    epoch.seal_count = epoch
                        .seal_count
                        .checked_add(1)
                        .context("seal count overflow")?;
                }
            }
        } else if self.active.as_ref().is_some_and(|e| {
            e.keys
                .iter()
                .any(|k| k.active_from_slot.is_some_and(|s| s <= slot))
        }) {
            self.fail(
                OrderGapReason::IncompleteBank,
                Some((slot, bank_id)),
                "partial group in covered range",
                send,
            )?;
        }
        if self.closing.is_some() {
            self.finish_close(send)?;
        } else {
            self.switch(watermark, send)?;
        }
        Ok(())
    }
    pub fn check_deadline(
        &mut self,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        // Sticky source notifications also cover an idle stream after NORMAL_END.
        let late = match &self.watch {
            Some(w) => w.pending_lates()?,
            None => Vec::new(),
        };
        for bank in late {
            self.late(bank, send)?;
        }
        if let (Some(watch), Some(floor)) = (self.watch.as_ref(), self.coverage_floor()) {
            if watch.incomplete(floor)? {
                self.fail(
                    OrderGapReason::IncompleteBank,
                    None,
                    "confirmed source Bank never became complete",
                    send,
                )?;
            }
        }
        if let Some(watch) = self.watch.as_ref() {
            if let Err(error) = watch.check() {
                self.fail(
                    OrderGapReason::ResourceLimit,
                    None,
                    &error.to_string(),
                    send,
                )?;
            }
        }
        if self
            .closing
            .is_some_and(|t| t.elapsed() >= Duration::from_secs(15))
            || self
                .group
                .as_ref()
                .is_some_and(|g| g.covered && g.started.elapsed() >= Duration::from_secs(15))
        {
            self.fail(
                OrderGapReason::IncompleteBank,
                None,
                "B group/close deadline exceeded",
                send,
            )?;
        }
        Ok(())
    }
    pub fn fail(
        &mut self,
        reason: OrderGapReason,
        bank: Option<(u64, u64)>,
        detail: &str,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        self.stopped = true;
        self.pending = None;
        self.closing = None;
        self.group = None;
        if let Some(epoch) = self.active.as_ref() {
            send(control(subscribe_update::UpdateOneof::OrderGap(
                SubscribeUpdateOrderGap {
                    context: Some(epoch.context.clone()),
                    next_source_seq: Some(epoch.seq),
                    reason: Some(reason as i32),
                    slot: bank.map(|b| b.0),
                    bank_id: bank.map(|b| b.1),
                    detail: bounded_detail(detail),
                },
            )))?;
        }
        if let Some(epoch) = self.active.take() {
            self.historical.push((epoch.context, epoch.seq));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "order_stream_tests.rs"]
mod order_writes_v1;
