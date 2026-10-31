//! Paired R1 B state. All output runs after the original A sender in client_loop.
use {
    crate::{
        history_protocol as hp,
        order_protocol::{self as op, sha256, OrderedDigest},
        order_stream::BankGroup,
        plugin::{
            filter::{
                message::{FilteredUpdate, FilteredUpdateOneof},
                FilterAccountsDataSlice,
            },
            message::{Message as SourceMessage, SlotStatus},
            order_permissions::Permissions,
        },
        tracked_history::{Bank, HistoryRegistry, HistoryWatch, Plan},
    },
    anyhow::{ensure, Context, Result},
    prost::Message,
    std::{
        collections::{BTreeSet, HashSet, VecDeque},
        sync::Arc,
        time::{Duration, Instant},
    },
    yellowstone_grpc_proto::prelude::*,
};
struct Epoch {
    context: OrderContext,
    plan: Plan,
    request: SubscribeRequestTrackedHistory,
    order_request: SubscribeRequestOrderWrites,
    owner_hash: Vec<u8>,
    acked: bool,
    order_seq: u64,
    history_seq: u64,
    orders: OrderedDigest,
    versions: OrderedDigest,
    order_seals: OrderedDigest,
    history_seals: OrderedDigest,
    bank_count: u64,
}
struct Group {
    bank: BankGroup,
    transactions: Vec<FilteredUpdate>,
    meta: Option<Vec<u8>>,
    confirmed: Option<Vec<u8>>,
    bytes: usize,
    waiting_since: Instant,
}
struct Ended {
    context: OrderContext,
    generation: u64,
    order_seq: u64,
    history_seq: u64,
    reported: bool,
}
struct Failure {
    reason: OrderGapReason,
    detail: String,
    bank: Option<(Bank, u64)>,
}
#[derive(Default)]
pub struct HistorySession {
    pub ever: bool,
    stream: Option<Vec<u8>>,
    watch: Option<HistoryWatch>,
    active: Option<Epoch>,
    pending: Option<Epoch>,
    current: Option<Group>,
    ready: VecDeque<Group>,
    ended: Vec<Ended>,
    delivered: BTreeSet<Bank>,
    closing: Option<u64>,
    started: Option<Instant>,
    stopped: bool,
    failure: Option<Failure>,
    queued_bytes: usize,
    limits: crate::plugin::filter::limits::FilterLimitsOrder,
}
impl HistorySession {
    fn stream_id(&mut self) -> Vec<u8> {
        self.stream
            .get_or_insert_with(|| uuid::Uuid::new_v4().as_bytes().to_vec())
            .clone()
    }
    pub fn reject(
        &mut self,
        raw: &SubscribeRequest,
        detail: &str,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        self.reject_reason(raw, OrderRejectReason::InvalidRequest, detail, send)
    }
    pub fn reject_reason(
        &mut self,
        raw: &SubscribeRequest,
        reason: OrderRejectReason,
        detail: &str,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        self.ever = true;
        let h = raw.tracked_account_history.as_ref();
        send(hp::control(
            subscribe_update::UpdateOneof::TrackedHistoryRejected(
                SubscribeUpdateTrackedHistoryRejected {
                    source_stream_id: self.stream_id(),
                    request_version: h.and_then(|r| r.request_version),
                    request_sha256: h
                        .filter(|r| r.request_sha256.len() == 32)
                        .map(|r| r.request_sha256.clone()),
                    reason: Some(reason as i32),
                    detail: op::bounded_detail(detail),
                },
            ),
        ))
    }
    pub fn request(
        &mut self,
        raw: &SubscribeRequest,
        owner: &FilteredUpdate,
        registry: &Arc<HistoryRegistry>,
        watermark: &std::sync::atomic::AtomicU64,
        limits: &crate::plugin::filter::limits::FilterLimitsOrder,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        self.ever = true;
        if self.pending.is_some() || self.closing.is_some() {
            return self.reject_reason(
                raw,
                OrderRejectReason::PendingExists,
                "another paired control is pending",
                send,
            );
        }
        if raw
            .tracked_account_history
            .as_ref()
            .is_some_and(|r| r.protocol_version != Some(1))
            || raw
                .order_writes
                .as_ref()
                .is_some_and(|r| r.protocol_version != Some(1))
        {
            return self.reject_reason(
                raw,
                OrderRejectReason::UnsupportedCapability,
                "unsupported paired protocol version",
                send,
            );
        }
        // Reserve coverage, digest and owner-ack workspace before B clones.
        let workspace = owner
            .encoded_len()
            .checked_mul(16)
            .and_then(|n| n.checked_add(op::MAX_TRACKED_KEYS * 1024))
            .context("history registration workspace size overflow")?;
        let _workspace = match registry.reserve(workspace) {
            Ok(lease) => lease,
            Err(error) => return self.reject(raw, &error.to_string(), send),
        };
        if raw
            .order_writes
            .as_ref()
            .is_some_and(|r| crate::order_stream::validate_coverage(raw, &r.tracked_keys).is_err())
        {
            return self.reject_reason(
                raw,
                OrderRejectReason::ScopeCoverageInvalid,
                "paired request lacks complete live Confirmed coverage",
                send,
            );
        }
        let result = (|| -> Result<Epoch> {
            hp::validate_pair(raw)?;
            ensure!(
                !self.stopped && self.pending.is_none() && self.closing.is_none(),
                "paired B stopped or control pending"
            );
            ensure!(
                limits.max_watchers > 0
                    && limits.max_watchers <= 32
                    && limits.max_transactions_per_bank > 0
                    && limits.max_transactions_per_bank <= 65536
                    && limits.max_banks_per_stream > 0
                    && limits.max_banks_per_stream <= 16384
                    && limits.max_notifications > 0
                    && limits.max_notifications <= 1024
                    && limits.queue_bytes > 0
                    && limits.queue_bytes <= 4194304
                    && limits.control_bytes >= 4096
                    && limits.control_bytes <= 16384,
                "paired B configured limits exceed protocol hard limits"
            );
            let h = raw
                .tracked_account_history
                .as_ref()
                .context("history registration missing")?;
            let o = raw
                .order_writes
                .as_ref()
                .context("order registration missing")?;
            if let Some(old) = &self.active {
                ensure!(
                    h.previous_epoch.as_ref() == Some(&old.context.source_epoch)
                        && h.request_version
                            == old.context.request_version.and_then(|v| v.checked_add(1)),
                    "history previous epoch/version mismatch"
                );
                ensure!(
                    h.tracked_keys.len() > old.plan.keys.len()
                        && old
                            .plan
                            .keys
                            .iter()
                            .all(|k| h.tracked_keys.binary_search(&k.pubkey).is_ok()),
                    "history scope must strictly expand"
                );
            } else {
                ensure!(
                    h.request_version == Some(1) && h.previous_epoch.is_none(),
                    "history must start with paired epoch one"
                );
            }
            let update = owner.as_subscribe_update();
            let Some(subscribe_update::UpdateOneof::OwnerRejectExemption(exemption)) =
                update.update_oneof
            else {
                anyhow::bail!("actual owner ack missing");
            };
            let keys = h
                .tracked_keys
                .iter()
                .map(|key| -> Result<_> {
                    let account = exemption
                        .accounts
                        .iter()
                        .find(|a| bs58::decode(&a.pubkey).into_vec().is_ok_and(|k| k == *key))
                        .context("owner ack lacks history key")?;
                    Ok(OrderKeyCoverage {
                        pubkey: key.clone(),
                        active_from_slot: None,
                        owner_exempt_active_from_slot: Some(account.active_from_slot),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let plan = if let Some(watch) = &self.watch {
                watch.replace_with(keys, || watermark.load(std::sync::atomic::Ordering::SeqCst))?
            } else {
                let (watch, plan) = registry
                    .register_with(keys, || watermark.load(std::sync::atomic::Ordering::SeqCst))?;
                self.watch = Some(watch);
                plan
            };
            Ok(Epoch {
                context: OrderContext {
                    protocol_version: Some(1),
                    source_stream_id: self.stream_id(),
                    source_epoch: uuid::Uuid::new_v4().as_bytes().to_vec(),
                    request_version: h.request_version,
                },
                plan,
                request: h.clone(),
                order_request: o.clone(),
                owner_hash: sha256(&owner.encode_to_vec()),
                acked: false,
                order_seq: 0,
                history_seq: 0,
                orders: OrderedDigest::default(),
                versions: OrderedDigest::default(),
                order_seals: OrderedDigest::default(),
                history_seals: OrderedDigest::default(),
                bank_count: 0,
            })
        })();
        match result {
            Ok(epoch) => {
                self.limits = limits.clone();
                self.pending = Some(epoch);
                self.started = Some(Instant::now());
                self.progress(send)
            }
            Err(e) => self.reject(raw, &e.to_string(), send),
        }
    }
    pub fn close(
        &mut self,
        raw: &SubscribeRequest,
        watermark: &std::sync::atomic::AtomicU64,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        let valid = (|| -> Result<()> {
            let o = raw
                .order_close
                .as_ref()
                .context("half close: order missing")?;
            let h = raw
                .tracked_history_close
                .as_ref()
                .context("half close: history missing")?;
            let only = SubscribeRequest {
                order_close: Some(o.clone()),
                tracked_history_close: Some(h.clone()),
                ..Default::default()
            };
            ensure!(
                *raw == only
                    && o.context == h.context
                    && o.tracked_keys_sha256 == h.tracked_keys_sha256,
                "invalid paired close envelope"
            );
            let e = self.active.as_ref().context("paired epoch absent")?;
            ensure!(
                !self.stopped
                    && self.pending.is_none()
                    && self.closing.is_none()
                    && o.context.as_ref() == Some(&e.context)
                    && o.tracked_keys_sha256 == op::keys_digest(&e.request.tracked_keys)?,
                "paired close context or lifecycle mismatch"
            );
            self.closing = Some(
                self.watch
                    .as_ref()
                    .context("watch missing")?
                    .close_with(|| watermark.load(std::sync::atomic::Ordering::SeqCst))?,
            );
            self.started = Some(Instant::now());
            Ok(())
        })();
        if let Err(e) = valid {
            return self.reject(raw, &e.to_string(), send);
        }
        self.progress(send)
    }
    pub fn coverage_changed(
        &mut self,
        raw: &SubscribeRequest,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        if [self.active.as_ref(), self.pending.as_ref()]
            .into_iter()
            .flatten()
            .any(|e| crate::order_stream::validate_coverage(raw, &e.request.tracked_keys).is_err())
        {
            self.fail_with(
                OrderGapReason::SubscriptionProtocol,
                "A replacement removed paired B coverage",
                send,
            )?;
        }
        Ok(())
    }
    pub fn begin(
        &mut self,
        bank: BankGroup,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        if self.current.is_some() {
            return self.fail_with(
                OrderGapReason::IncompleteBank,
                "nested paired Bank group",
                send,
            );
        }
        let covered = self.epoch_for(bank.slot).is_some();
        if covered && !bank.valid {
            return self.fail_with(
                if bank.resource_limit {
                    OrderGapReason::ResourceLimit
                } else if bank.source_after_seal {
                    OrderGapReason::SourceAfterSeal
                } else {
                    OrderGapReason::SourceCountMismatch
                },
                "invalid source Bank count/index/signature",
                send,
            );
        }
        self.current = Some(Group {
            bank,
            transactions: Vec::new(),
            meta: None,
            confirmed: None,
            bytes: 0,
            waiting_since: Instant::now(),
        });
        Ok(())
    }
    fn epoch_for(&self, slot: u64) -> Option<&Epoch> {
        if self.closing.is_some_and(|end| slot >= end) {
            return None;
        }
        self.pending
            .as_ref()
            .filter(|e| slot >= e.plan.switch)
            .or(self.active.as_ref().filter(|e| slot >= e.plan.switch))
    }
    pub(crate) fn observe_sent(
        &mut self,
        source: &SourceMessage,
        updates: &[FilteredUpdate],
        queue: &crate::grpc::LazyOrderQueue,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        if let SourceMessage::Slot(slot) = source {
            if slot.status == SlotStatus::Dead && self.epoch_for(slot.slot).is_some() {
                return self.fail_with(
                    OrderGapReason::IncompleteBank,
                    "Dead in promised paired coverage",
                    send,
                );
            }
        }
        if self.stopped {
            return Ok(());
        }
        let Some(group) = self.current.as_ref() else {
            if matches!(source,SourceMessage::Transaction(t) if self.epoch_for(t.slot).is_some()) {
                return self.fail_with(OrderGapReason::IncompleteBank, "partial paired Bank", send);
            }
            return Ok(());
        };
        let Some(epoch) = self.epoch_for(group.bank.slot) else {
            return Ok(());
        };
        let mut transaction = None;
        if let SourceMessage::Transaction(tx) = source {
            ensure!(
                tx.slot == group.bank.slot && tx.bank_id == group.bank.bank_id,
                "transaction outside paired Bank"
            );
            let keys = epoch
                .plan
                .keys
                .iter()
                .filter(|k| k.active_from_slot.is_some_and(|a| a <= tx.slot))
                .map(|k| <[u8; 32]>::try_from(k.pubkey.as_slice()))
                .collect::<std::result::Result<HashSet<_>, _>>()?;
            let permissions = tx.transaction.order_permissions();
            if permissions.affects(&keys) {
                // Only the compact fields are cloned into B. Full A logs,
                // balances and pre-account payloads never consume B credit.
                let estimate = tx
                    .transaction
                    .transaction
                    .encoded_len()
                    .checked_add(
                        tx.transaction
                            .meta
                            .err
                            .as_ref()
                            .map_or(0, |e| e.encoded_len()),
                    )
                    .and_then(|n| n.checked_add(512))
                    .context("history transaction size overflow")?;
                let estimate = tx
                    .transaction
                    .meta
                    .loaded_writable_addresses
                    .iter()
                    .chain(tx.transaction.meta.loaded_readonly_addresses.iter())
                    .try_fold(estimate, |n, key| {
                        n.checked_add(key.len())
                            .and_then(|n| n.checked_add(16))
                            .context("history loaded-key size overflow")
                    })?;
                let lease = self
                    .watch
                    .as_ref()
                    .context("history watch missing")?
                    .registry
                    .reserve(
                        estimate
                            .checked_mul(16)
                            .and_then(|n| n.checked_add(4096))
                            .context("history transaction charge overflow")?,
                    )?;
                let credit = queue.reserve_deferred(estimate)?;
                let mut update = op::wrap(
                    subscribe_update::UpdateOneof::TransactionOrder(
                        SubscribeUpdateTransactionOrder {
                            context: Some(epoch.context.clone()),
                            source_seq: Some(0),
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
                        },
                    ),
                    tx.created_at,
                );
                update.history_lease = Some(lease);
                update.deferred_credit = Some(credit);
                transaction = Some(update);
            }
        }
        let group = self.current.as_mut().context("paired group missing")?;
        if let Some(update) = transaction {
            let charge = update
                .encoded_len()
                .checked_mul(3)
                .and_then(|n| n.checked_add(512))
                .context("paired queue charge overflow")?;
            ensure!(
                self.queued_bytes
                    .checked_add(charge)
                    .is_some_and(|n| n <= self.limits.queue_bytes),
                "paired deferred queue limit"
            );
            self.queued_bytes += charge;
            group.bytes += charge;
            group.transactions.push(update);
        }
        for update in updates {
            match &update.message {
                FilteredUpdateOneof::BlockMeta(meta)
                    if meta.slot == group.bank.slot && meta.bank_id == group.bank.bank_id =>
                {
                    group.meta = Some(sha256(&update.encode_to_vec()))
                }
                FilteredUpdateOneof::Slot(slot)
                    if slot.slot == group.bank.slot
                        && slot.bank_id == Some(group.bank.bank_id)
                        && slot.status == SlotStatus::Confirmed =>
                {
                    group.confirmed = Some(sha256(&update.encode_to_vec()))
                }
                _ => {}
            }
        }
        Ok(())
    }
    pub fn end(
        &mut self,
        bank: Bank,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        if self.stopped {
            self.current = None;
            return self.tick(send);
        }
        if let Some(mut group) = self.current.take() {
            ensure!(
                (group.bank.slot, group.bank.bank_id) == bank,
                "paired Bank end mismatch"
            );
            if self.epoch_for(bank.0).is_some() {
                ensure!(
                    group.meta.is_some() && group.confirmed.is_some(),
                    "paired Bank missing own Confirmed/meta"
                );
                ensure!(
                    self.ready.len() < self.limits.max_banks_per_stream,
                    "paired ready Bank limit"
                );
                group.waiting_since = Instant::now();
                self.ready.push_back(group);
            }
        } else if self.epoch_for(bank.0).is_some() {
            return self.fail_with(
                OrderGapReason::IncompleteBank,
                "paired partial Bank end",
                send,
            );
        }
        self.progress(send)
    }
    pub fn tick(&mut self, send: &mut impl FnMut(FilteredUpdate) -> Result<()>) -> Result<()> {
        if let Some(w) = &self.watch {
            if let Err(error) = w.check() {
                self.fail_error(&error, send)?;
            }
        }
        if self
            .started
            .is_some_and(|t| t.elapsed() >= Duration::from_secs(15))
            && !self.stopped
        {
            self.fail_with(
                OrderGapReason::IncompleteBank,
                "paired cutover/close incomplete Bank deadline",
                send,
            )?;
        }
        if self.stopped {
            return self.retry_gaps(send);
        }
        self.progress(send)
    }
    fn progress(&mut self, send: &mut impl FnMut(FilteredUpdate) -> Result<()>) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        if let Some(w) = &self.watch {
            w.check()?;
        }
        // Drain old-side ready banks; new-side banks stay bounded until both old seals.
        loop {
            let cutoff = self
                .pending
                .as_ref()
                .map(|e| e.plan.switch)
                .or(self.closing)
                .unwrap_or(u64::MAX);
            let position = self.ready.iter().position(|g| {
                g.bank.slot < cutoff
                    && self
                        .active
                        .as_ref()
                        .is_some_and(|e| g.bank.slot >= e.plan.switch)
            });
            let Some(i) = position else {
                break;
            };
            let group = self.ready.get(i).context("ready Bank missing")?;
            if group.waiting_since.elapsed() >= Duration::from_secs(15) {
                return self.fail_with(
                    OrderGapReason::IncompleteBank,
                    "paired ingress fence deadline",
                    send,
                );
            }
            if !self
                .watch
                .as_ref()
                .context("watch missing")?
                .ready((group.bank.slot, group.bank.bank_id))?
            {
                // A has already been broadcast. Poll only B's bounded ready
                // queue; never wait for callback guards on the source/A path.
                break;
            }
            let group = self.ready.remove(i).context("ready Bank missing")?;
            self.emit_bank(group, send)?;
        }
        if let Some(next) = self.pending.as_ref() {
            let cutoff = next.plan.switch;
            if self.active.is_some()
                && self
                    .watch
                    .as_ref()
                    .context("watch missing")?
                    .pending(cutoff)?
            {
                return Ok(());
            }
            let previous = self.active.take();
            let previous_epoch = previous.as_ref().map(|e| e.context.source_epoch.clone());
            let (old_order, old_history) = if let Some(e) = previous {
                let hashes = self.seal_epoch(e, OrderEpochSealReason::Replaced, cutoff, send)?;
                (Some(hashes.0), Some(hashes.1))
            } else {
                (None, None)
            };
            let mut epoch = self.pending.take().context("pending history missing")?;
            let order = op::control(subscribe_update::UpdateOneof::OrderCoverageAck(
                SubscribeUpdateOrderCoverageAck {
                    context: Some(epoch.context.clone()),
                    request_sha256: epoch.order_request.request_sha256.clone(),
                    tracked_keys_sha256: op::keys_digest(&epoch.request.tracked_keys)?,
                    keys: epoch.plan.keys.clone(),
                    active_from_slot: epoch
                        .plan
                        .keys
                        .iter()
                        .filter_map(|k| k.active_from_slot)
                        .max(),
                    previous_epoch: previous_epoch.clone(),
                    previous_epoch_seal_sha256: old_order,
                    observed_callback_slot_high_watermark: epoch.plan.switch.checked_sub(1),
                    owner_exemption_update_sha256: epoch.owner_hash.clone(),
                    capabilities: Some(op::capabilities()),
                },
            ));
            let order_hash = sha256(&order.encode_to_vec());
            send(order)?;
            send(hp::control(
                subscribe_update::UpdateOneof::TrackedHistoryAck(
                    SubscribeUpdateTrackedHistoryAck {
                        context: Some(epoch.context.clone()),
                        request_sha256: epoch.request.request_sha256.clone(),
                        order_coverage_ack_sha256: order_hash,
                        tracked_keys_sha256: op::keys_digest(&epoch.request.tracked_keys)?,
                        keys: epoch.plan.keys.clone(),
                        observed_callback_slot_high_watermark: epoch.plan.switch.checked_sub(1),
                        registration_generation: Some(epoch.plan.generation),
                        previous_epoch,
                        previous_history_epoch_seal_sha256: old_history,
                        capabilities: Some(hp::capabilities()),
                        switch_slot: Some(epoch.plan.switch),
                        source_limits: Some(hp::limits(
                            &self
                                .watch
                                .as_ref()
                                .context("watch missing")?
                                .registry
                                .limits,
                            self.limits.queue_bytes,
                            self.limits.control_bytes,
                        )),
                    },
                ),
            ))?;
            epoch.acked = true;
            self.active = Some(epoch);
            self.started = None;
            return self.progress(send);
        }
        if let Some(cutoff) = self.closing {
            if !self
                .watch
                .as_ref()
                .context("watch missing")?
                .pending(cutoff)?
            {
                if let Some(e) = self.active.take() {
                    self.seal_epoch(e, OrderEpochSealReason::NormalEnd, cutoff, send)?;
                }
                self.closing = None;
                self.started = None;
                self.stopped = true;
            }
        }
        Ok(())
    }
    fn emit_bank(
        &mut self,
        mut group: Group,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        let bank = (group.bank.slot, group.bank.bank_id);
        ensure!(
            self.delivered.insert(bank),
            "duplicate authoritative paired Bank"
        );
        let epoch = self
            .active
            .as_mut()
            .context("paired active epoch missing")?;
        let first_order = epoch.order_seq;
        let mut orders = OrderedDigest::default();
        for mut update in group.transactions.drain(..) {
            if let FilteredUpdateOneof::Order(raw) = &mut update.message {
                if let Some(subscribe_update::UpdateOneof::TransactionOrder(order)) =
                    &mut raw.update_oneof
                {
                    order.source_seq = Some(epoch.order_seq);
                }
            }
            let bytes = update.encode_to_vec();
            send(update)?;
            orders.append(&bytes)?;
            epoch.orders.append(&bytes)?;
            epoch.order_seq = epoch
                .order_seq
                .checked_add(1)
                .context("paired order sequence exhausted")?;
        }
        let count = epoch.order_seq - first_order;
        let confirmed = group.confirmed.context("paired Confirmed missing")?;
        let meta = group.meta.context("paired BlockMeta missing")?;
        let order = op::control(subscribe_update::UpdateOneof::OrderSlotSeal(
            SubscribeUpdateOrderSlotSeal {
                context: Some(epoch.context.clone()),
                slot: Some(bank.0),
                bank_id: Some(bank.1),
                event_count: Some(count),
                first_source_seq: (count > 0).then_some(first_order),
                last_source_seq: if count > 0 {
                    epoch.order_seq.checked_sub(1)
                } else {
                    None
                },
                ordered_events_sha256: orders.finish(),
                confirmed_update_sha256: confirmed.clone(),
                block_meta_update_sha256: meta.clone(),
                observed_transaction_count: Some(group.bank.observed),
                executed_transaction_count: Some(group.bank.executed),
                complete_bank: Some(true),
            },
        ));
        let order_raw = order.encode_to_vec();
        send(order)?;
        epoch.order_seals.append(&order_raw)?;
        let (versions, observed) = self
            .watch
            .as_ref()
            .context("history watch missing")?
            .versions(bank, &epoch.plan)?;
        let first_history = epoch.history_seq;
        let mut digest = OrderedDigest::default();
        let mut null = 0u64;
        for (bank_seq, version) in versions.iter().enumerate() {
            null += u64::from(version.account.account.txn_signature.is_none());
            let estimate = version
                .account
                .account
                .data
                .len()
                .checked_add(1024)
                .context("history account size overflow")?;
            let lease = self
                .watch
                .as_ref()
                .context("history watch missing")?
                .registry
                .reserve(
                    estimate
                        .checked_mul(16)
                        .and_then(|n| n.checked_add(4096))
                        .context("history account charge overflow")?,
                )?;
            let account = FilteredUpdate::new_empty(FilteredUpdateOneof::account(
                Arc::clone(&version.account),
                FilterAccountsDataSlice::default(),
            ))
            .as_subscribe_update();
            let Some(subscribe_update::UpdateOneof::Account(account)) = account.update_oneof else {
                anyhow::bail!("account encoder returned wrong variant");
            };
            let mut update = hp::wrap(
                subscribe_update::UpdateOneof::TrackedAccountVersion(
                    SubscribeUpdateTrackedAccountVersion {
                        context: Some(epoch.context.clone()),
                        history_seq: Some(epoch.history_seq),
                        bank_seq: Some(bank_seq as u64),
                        account: Some(account),
                        callback_ordinal: Some(version.ordinal),
                    },
                ),
                version.account.created_at,
            );
            update.history_lease = Some(lease);
            let raw = update.encode_to_vec();
            ensure!(
                raw.len() <= self.limits.queue_bytes,
                "history version exceeds queue credit"
            );
            send(update)?;
            digest.append(&raw)?;
            epoch.versions.append(&raw)?;
            epoch.history_seq = epoch
                .history_seq
                .checked_add(1)
                .context("history sequence exhausted")?;
        }
        let covered = epoch
            .plan
            .keys
            .iter()
            .filter(|k| k.active_from_slot.is_some_and(|a| a <= bank.0))
            .map(|k| k.pubkey.clone())
            .collect::<Vec<_>>();
        let count = epoch.history_seq - first_history;
        let seal = hp::control(subscribe_update::UpdateOneof::TrackedAccountBankSeal(
            SubscribeUpdateTrackedHistoryBankSeal {
                context: Some(epoch.context.clone()),
                slot: Some(bank.0),
                bank_id: Some(bank.1),
                event_count: Some(count),
                first_history_seq: (count > 0).then_some(first_history),
                last_history_seq: if count > 0 {
                    epoch.history_seq.checked_sub(1)
                } else {
                    None
                },
                ordered_events_sha256: digest.finish(),
                order_slot_seal_sha256: sha256(&order_raw),
                confirmed_update_sha256: confirmed,
                block_meta_update_sha256: meta,
                covered_keys_sha256: op::keys_digest(&covered)?,
                null_writer_count: Some(null),
                signed_writer_count: Some(count - null),
                complete_bank: Some(true),
                observed_callback_count: Some(observed),
            },
        ));
        let raw = seal.encode_to_vec();
        send(seal)?;
        epoch.history_seals.append(&raw)?;
        epoch.bank_count = epoch
            .bank_count
            .checked_add(1)
            .context("history Bank count exhausted")?;
        self.watch
            .as_ref()
            .context("watch missing")?
            .complete(bank)?;
        self.queued_bytes = self.queued_bytes.saturating_sub(group.bytes);
        Ok(())
    }
    fn seal_epoch(
        &mut self,
        e: Epoch,
        reason: OrderEpochSealReason,
        cutoff: u64,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        self.watch.as_ref().context("watch missing")?.check()?;
        self.watch
            .as_ref()
            .context("watch missing")?
            .seal_through(cutoff)?;
        let keys = op::keys_digest(&e.request.tracked_keys)?;
        let order = op::control(subscribe_update::UpdateOneof::OrderEpochSeal(
            SubscribeUpdateOrderEpochSeal {
                context: Some(e.context.clone()),
                event_count: Some(e.order_seq),
                last_source_seq: e.order_seq.checked_sub(1),
                ordered_events_sha256: e.orders.finish(),
                slot_seal_count: Some(e.bank_count),
                ordered_slot_seals_sha256: e.order_seals.finish(),
                reason: Some(reason as i32),
                tracked_keys_sha256: keys.clone(),
            },
        ));
        let order_hash = sha256(&order.encode_to_vec());
        send(order)?;
        let history = hp::control(subscribe_update::UpdateOneof::TrackedHistoryEpochSeal(
            SubscribeUpdateTrackedHistoryEpochSeal {
                context: Some(e.context.clone()),
                event_count: Some(e.history_seq),
                last_history_seq: e.history_seq.checked_sub(1),
                ordered_events_sha256: e.versions.finish(),
                bank_seal_count: Some(e.bank_count),
                ordered_bank_seals_sha256: e.history_seals.finish(),
                reason: Some(reason as i32),
                tracked_keys_sha256: keys,
                order_epoch_seal_sha256: order_hash.clone(),
                end_slot_exclusive: Some(cutoff),
            },
        ));
        let history_hash = sha256(&history.encode_to_vec());
        send(history)?;
        self.ended.push(Ended {
            context: e.context,
            generation: e.plan.generation,
            order_seq: e.order_seq,
            history_seq: e.history_seq,
            reported: false,
        });
        Ok((order_hash, history_hash))
    }
    pub fn fail(
        &mut self,
        detail: &str,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        self.fail_with(OrderGapReason::ResourceLimit, detail, send)
    }
    pub fn fail_error(
        &mut self,
        error: &anyhow::Error,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        let reason = error
            .downcast_ref::<crate::tracked_history::HistoryFault>()
            .map_or(OrderGapReason::ResourceLimit, |fault| fault.reason);
        let at = if let (Some(fault), Some(watch)) = (
            error.downcast_ref::<crate::tracked_history::HistoryFault>(),
            self.watch.as_ref(),
        ) {
            if let Some(bank) = fault.bank {
                watch
                    .generation_for(bank)?
                    .map(|generation| (bank, generation))
            } else {
                None
            }
        } else {
            None
        };
        self.fail_at(reason, &error.to_string(), at, send)
    }
    pub const fn has_failed(&self) -> bool {
        self.failure.is_some()
    }
    pub fn fail_with(
        &mut self,
        reason: OrderGapReason,
        detail: &str,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        self.fail_at(reason, detail, None, send)
    }
    fn fail_at(
        &mut self,
        reason: OrderGapReason,
        detail: &str,
        at: Option<(Bank, u64)>,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        if let Some(watch) = &self.watch {
            watch.stop()?;
        }
        self.failure.get_or_insert_with(|| Failure {
            reason,
            detail: op::bounded_detail(detail),
            bank: at,
        });
        self.stopped = true;
        self.current = None;
        self.ready.clear();
        self.queued_bytes = 0;
        if let Some(e) = self.active.take() {
            self.ended.push(Ended {
                context: e.context,
                generation: e.plan.generation,
                order_seq: e.order_seq,
                history_seq: e.history_seq,
                reported: false,
            });
        }
        if let Some(e) = self.pending.take() {
            self.ended.push(Ended {
                context: e.context,
                generation: e.plan.generation,
                order_seq: e.order_seq,
                history_seq: e.history_seq,
                reported: false,
            });
        }
        self.retry_gaps(send)
    }
    fn retry_gaps(&mut self, send: &mut impl FnMut(FilteredUpdate) -> Result<()>) -> Result<()> {
        let Some(Failure {
            reason,
            detail,
            bank: at,
        }) = &self.failure
        else {
            return Ok(());
        };
        for e in self.ended.iter_mut().filter(|e| !e.reported) {
            let bank = at
                .filter(|(_, generation)| *generation == e.generation)
                .map(|(bank, _)| bank);
            send(op::control(subscribe_update::UpdateOneof::OrderGap(
                SubscribeUpdateOrderGap {
                    context: Some(e.context.clone()),
                    next_source_seq: Some(e.order_seq),
                    reason: Some(*reason as i32),
                    slot: bank.map(|b| b.0),
                    bank_id: bank.map(|b| b.1),
                    detail: detail.clone(),
                },
            )))?;
            send(hp::control(
                subscribe_update::UpdateOneof::TrackedHistoryGap(
                    SubscribeUpdateTrackedHistoryGap {
                        context: Some(e.context.clone()),
                        next_history_seq: Some(e.history_seq),
                        reason: Some(*reason as i32),
                        slot: bank.map(|b| b.0),
                        bank_id: bank.map(|b| b.1),
                        detail: detail.clone(),
                    },
                ),
            ))?;
            e.reported = true;
        }
        Ok(())
    }
}
