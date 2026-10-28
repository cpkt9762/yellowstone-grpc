//! Per-stream B state, driven exclusively by the original client_loop sender.
use {
    crate::{
        order_protocol::*,
        plugin::{
            filter::message::FilteredUpdate,
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
}
impl BankGroup {
    pub fn from_block(block: &MessageBlock) -> Self {
        let mut indices = HashSet::new();
        let mut signatures = HashSet::new();
        let executed = block.meta.executed_transaction_count;
        let observed = block.transactions.len() as u64;
        let valid = observed == executed
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
    stream: Vec<u8>,
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
            stream: uuid::Uuid::new_v4().as_bytes().to_vec(),
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
            .any(|f| f.interslot_updates != Some(false))
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
    pub fn ever_requested(&self) -> bool {
        self.ever
    }
    pub fn has_active(&self) -> bool {
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
                source_stream_id: self.stream.clone(),
                request_version: request.and_then(|r| r.request_version),
                request_sha256: request
                    .filter(|r| r.request_sha256.len() == 32)
                    .map(|r| r.request_sha256.clone()),
                reason: Some(reason as i32),
                detail: detail.chars().take(512).collect(),
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
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        let previous = self.active.take();
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
            source_stream_id: self.stream.clone(),
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
    pub fn close(
        &mut self,
        raw: &SubscribeRequest,
        send: &mut impl FnMut(FilteredUpdate) -> Result<()>,
    ) -> Result<()> {
        let Some(close) = &raw.order_close else {
            return Ok(());
        };
        let mut only = SubscribeRequest::default();
        only.order_close = Some(close.clone());
        let valid = *raw == only
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
        self.closing = Some(Instant::now());
        if self.group.is_none() {
            self.finish_close(send)?;
        }
        Ok(())
    }
    fn finish_close(&mut self, send: &mut impl FnMut(FilteredUpdate) -> Result<()>) -> Result<()> {
        if let Some(epoch) = self.active.take() {
            send(Self::epoch_seal(&epoch, OrderEpochSealReason::NormalEnd))?;
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
                OrderGapReason::SourceCountMismatch,
                Some((bank.slot, bank.bank_id)),
                "bank count/index/signature mismatch",
                send,
            )?;
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
            if slot.status == SlotStatus::Dead && self.active.is_some() {
                self.fail(
                    OrderGapReason::IncompleteBank,
                    None,
                    "Dead in active coverage",
                    send,
                )?;
            }
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
            match &update.as_subscribe_update().update_oneof {
                Some(subscribe_update::UpdateOneof::BlockMeta(meta))
                    if meta.slot == group.bank.slot && meta.bank_id == group.bank.bank_id =>
                {
                    group.meta_hash = Some(sha256(&update.encode_to_vec()))
                }
                Some(subscribe_update::UpdateOneof::Slot(slot))
                    if slot.slot == group.bank.slot
                        && slot.bank_id == Some(group.bank.bank_id)
                        && slot.status == 1 =>
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
        if let Some(epoch) = self.active.take() {
            send(control(subscribe_update::UpdateOneof::OrderGap(
                SubscribeUpdateOrderGap {
                    context: Some(epoch.context),
                    next_source_seq: Some(epoch.seq),
                    reason: Some(reason as i32),
                    slot: bank.map(|b| b.0),
                    bank_id: bank.map(|b| b.1),
                    detail: detail.chars().take(512).collect(),
                },
            )))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod order_writes_v1 {
    use super::*;
    use crate::plugin::{
        filter::{fixtures, message::FilteredUpdateOneof},
        message::MessageSlot,
    };
    pub fn request(
        keys: Vec<Vec<u8>>,
        version: u64,
        previous: Option<Vec<u8>>,
    ) -> (SubscribeRequest, FilteredUpdate) {
        let mut order = SubscribeRequestOrderWrites {
            protocol_version: Some(1),
            request_version: Some(version),
            previous_epoch: previous,
            tracked_keys: keys.clone(),
            ..Default::default()
        };
        order.request_sha256 = request_digest(&order);
        let accounts = keys
            .iter()
            .map(|key| bs58::encode(key).into_string())
            .collect::<Vec<_>>();
        let req = SubscribeRequest {
            commitment: Some(1),
            order_writes: Some(order),
            accounts: HashMap::from([(
                "accounts".into(),
                SubscribeRequestFilterAccounts {
                    account: accounts.clone(),
                    include_owner_rejected: Some(true),
                    ..Default::default()
                },
            )]),
            slots: HashMap::from([(
                "slots".into(),
                SubscribeRequestFilterSlots {
                    interslot_updates: Some(true),
                    ..Default::default()
                },
            )]),
            blocks_meta: HashMap::from([("meta".into(), SubscribeRequestFilterBlocksMeta {})]),
            ..Default::default()
        };
        let owner = FilteredUpdate::new_empty(FilteredUpdateOneof::OwnerRejectExemption(
            SubscribeUpdateOwnerRejectExemption {
                accounts: accounts
                    .into_iter()
                    .map(|pubkey| OwnerRejectExemptAccount {
                        pubkey,
                        active_from_slot: 1,
                    })
                    .collect(),
                owner_reject: vec![],
            },
        ));
        (req, owner)
    }
    fn send_group(session: &mut OrderSession, slot: u64, out: &mut Vec<FilteredUpdate>) {
        session
            .begin(
                BankGroup {
                    slot,
                    bank_id: slot,
                    observed: 0,
                    executed: 0,
                    valid: true,
                },
                &mut |u| {
                    out.push(u);
                    Ok(())
                },
            )
            .expect("begin");
        let mut meta = fixtures::message_block_meta(slot);
        Arc::make_mut(&mut meta).block_meta.bank_id = slot;
        let m = SourceMessage::BlockMeta(Arc::clone(&meta));
        let update = FilteredUpdate::new_empty(FilteredUpdateOneof::BlockMeta(meta));
        session
            .observe_sent(&m, &[update], &mut |u| {
                out.push(u);
                Ok(())
            })
            .expect("meta");
        let slot_msg = MessageSlot {
            slot,
            parent: Some(slot - 1),
            status: SlotStatus::Confirmed,
            dead_error: None,
            created_at: Default::default(),
            bank_id: Some(slot),
        };
        let m = SourceMessage::Slot(Arc::new(slot_msg.clone()));
        let update = FilteredUpdate::new_empty(FilteredUpdateOneof::slot(slot_msg));
        session
            .observe_sent(&m, &[update], &mut |u| {
                out.push(u);
                Ok(())
            })
            .expect("confirmed");
        session
            .end(slot, slot, slot, &mut |u| {
                out.push(u);
                Ok(())
            })
            .expect("end");
    }
    #[test]
    fn zero_write_seal_close_and_full_scope_ack() {
        let mut session = OrderSession::default();
        let mut out = Vec::new();
        let (req, owner) = request(vec![vec![1; 32]], 1, None);
        session
            .request(&req, &owner, 3, &mut |u| {
                out.push(u);
                Ok(())
            })
            .expect("request");
        let ack = out[0].as_subscribe_update();
        let Some(subscribe_update::UpdateOneof::OrderCoverageAck(ack)) = ack.update_oneof else {
            panic!("ack");
        };
        assert_eq!(ack.active_from_slot, Some(4));
        assert_eq!(ack.keys.len(), 1);
        assert_eq!(
            ack.owner_exemption_update_sha256,
            sha256(&owner.encode_to_vec())
        );
        send_group(&mut session, 5, &mut out);
        let Some(subscribe_update::UpdateOneof::OrderSlotSeal(seal)) =
            out[1].as_subscribe_update().update_oneof
        else {
            panic!("zero seal");
        };
        assert_eq!(seal.event_count, Some(0));
        assert_eq!(seal.first_source_seq, None);
        assert_eq!(seal.ordered_events_sha256, sha256(&[]));
        let close = SubscribeRequest {
            order_close: Some(SubscribeRequestOrderClose {
                context: ack.context,
                tracked_keys_sha256: ack.tracked_keys_sha256,
            }),
            ..Default::default()
        };
        session
            .close(&close, &mut |u| {
                out.push(u);
                Ok(())
            })
            .expect("close");
        let Some(subscribe_update::UpdateOneof::OrderEpochSeal(seal)) =
            out[2].as_subscribe_update().update_oneof
        else {
            panic!("epoch seal");
        };
        assert_eq!(seal.reason, Some(2));
        assert_eq!(seal.slot_seal_count, Some(1));
        assert!(!session.has_active());
    }
    #[test]
    fn replace_waits_for_group_and_pending_is_not_coalesced() {
        let mut s = OrderSession::default();
        let mut out = Vec::new();
        let (req, owner) = request(vec![vec![1; 32]], 1, None);
        s.request(&req, &owner, 3, &mut |u| {
            out.push(u);
            Ok(())
        })
        .expect("initial");
        let old = s
            .active
            .as_ref()
            .expect("active")
            .context
            .source_epoch
            .clone();
        // A warmup group is still a switching barrier.
        s.begin(
            BankGroup {
                slot: 2,
                bank_id: 2,
                observed: 0,
                executed: 0,
                valid: true,
            },
            &mut |u| {
                out.push(u);
                Ok(())
            },
        )
        .expect("warmup");
        let (req, owner) = request(vec![vec![1; 32], vec![2; 32]], 2, Some(old.clone()));
        s.request(&req, &owner, 6, &mut |u| {
            out.push(u);
            Ok(())
        })
        .expect("pending");
        assert_eq!(out.len(), 1);
        s.request(&req, &owner, 6, &mut |u| {
            out.push(u);
            Ok(())
        })
        .expect("rejected second pending");
        assert!(matches!(
            out[1].as_subscribe_update().update_oneof,
            Some(subscribe_update::UpdateOneof::OrderRejected(_))
        ));
        s.end(2, 2, 8, &mut |u| {
            out.push(u);
            Ok(())
        })
        .expect("switch");
        assert!(matches!(
            out[2].as_subscribe_update().update_oneof,
            Some(subscribe_update::UpdateOneof::OrderEpochSeal(_))
        ));
        let Some(subscribe_update::UpdateOneof::OrderCoverageAck(ack)) =
            out[3].as_subscribe_update().update_oneof
        else {
            panic!("replacement ack");
        };
        assert_eq!(ack.previous_epoch, Some(old));
        assert_eq!(ack.keys[0].active_from_slot, Some(4));
        assert_eq!(ack.keys[1].active_from_slot, Some(9));
    }
}
