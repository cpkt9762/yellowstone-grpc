use super::*;
use crate::{
    order_protocol::request_digest,
    order_stream::{BankGroup, GroupBoundary},
    plugin::filter::fixtures,
};
use prost::Message as _;
use yellowstone_grpc_proto::prelude::{
    subscribe_update::UpdateOneof, SubscribeRequestFilterAccounts,
    SubscribeRequestFilterBlocksMeta, SubscribeRequestFilterSlots,
    SubscribeRequestFilterTransactions, SubscribeRequestOrderClose, SubscribeRequestOrderWrites,
};
fn request(b: bool) -> SubscribeRequest {
    let key = Pubkey::new_from_array([1; 32]).to_string();
    let mut r = SubscribeRequest {
        commitment: Some(1),
        accounts: HashMap::from([(
            "accounts".into(),
            SubscribeRequestFilterAccounts {
                account: vec![key],
                include_owner_rejected: Some(true),
                ..Default::default()
            },
        )]),
        transactions: HashMap::from([(
            "full".into(),
            SubscribeRequestFilterTransactions {
                include_pre_accounts: Some(true),
                ..Default::default()
            },
        )]),
        transactions_status: HashMap::from([(
            "status".into(),
            SubscribeRequestFilterTransactions::default(),
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
    if b {
        let mut order = SubscribeRequestOrderWrites {
            protocol_version: Some(1),
            request_version: Some(1),
            tracked_keys: vec![vec![1; 32]],
            ..Default::default()
        };
        order.request_sha256 = request_digest(&order);
        r.order_writes = Some(order);
    }
    r
}
fn filter(r: &SubscribeRequest) -> Filter {
    Filter::new(r, &FilterLimits::default(), &mut fixtures::filter_names()).expect("valid filter")
}
type Client = (
    mpsc::UnboundedSender<Option<(Option<u64>, Filter)>>,
    LoadAwareReceiver<TonicResult<FilteredUpdate>>,
    tokio::task::JoinHandle<()>,
);
fn spawn(broadcast: SubscriberChannels) -> Client {
    let (tx, rx) = mpsc::unbounded_channel();
    let (out, stream) = load_aware_channel(1024, WeightScales::default());
    let session = ClientSession::new(
        0,
        Some("order-test".into()),
        "order-test".into(),
        CancellationToken::new(),
        None,
    );
    let handle = tokio::spawn(GrpcService::client_loop(
        session,
        out,
        rx,
        None,
        broadcast,
        None,
        TaskTracker::new(),
        None,
    ));
    (tx, stream, handle)
}
async fn read(rx: &mut LoadAwareReceiver<TonicResult<FilteredUpdate>>) -> FilteredUpdate {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("client response deadline")
        .expect("stream open")
        .expect("A transport healthy")
}
async fn install(
    c: &mut Client,
    r: &SubscribeRequest,
) -> Option<yellowstone_grpc_proto::prelude::SubscribeUpdateOrderCoverageAck> {
    c.0.send(Some((None, filter(r)))).expect("request accepted");
    assert!(matches!(
        read(&mut c.1).await.message,
        FilteredUpdateOneof::OwnerRejectExemption(_)
    ));
    if r.order_writes.is_some() {
        let Some(UpdateOneof::OrderCoverageAck(ack)) =
            read(&mut c.1).await.as_subscribe_update().update_oneof
        else {
            panic!("B ack");
        };
        Some(ack)
    } else {
        None
    }
}
fn group_content(slot: u64, large: bool) -> Vec<Message> {
    let keys = vec![
        Pubkey::new_from_array([1; 32]),
        Pubkey::new_from_array([2; 32]),
        Pubkey::new_from_array([3; 32]),
    ];
    let mut a = fixtures::message_account(fixtures::account_info(
        keys[0],
        keys[1],
        vec![7; 8],
        1,
        None,
    ));
    Arc::make_mut(&mut a).slot = slot;
    Arc::make_mut(&mut a).bank_id = Some(slot);
    let mut tx = fixtures::message_transaction(
        solana_signature::Signature::from([9; 64]),
        keys,
        false,
        Default::default(),
    );
    let txm = Arc::make_mut(&mut tx);
    txm.slot = slot;
    txm.bank_id = slot;
    txm.transaction.index = 0;
    txm.transaction.transaction = yellowstone_grpc_proto::prelude::Transaction {
        signatures: vec![vec![9; 64]],
        message: Some(yellowstone_grpc_proto::prelude::Message {
            header: Some(yellowstone_grpc_proto::prelude::MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            }),
            account_keys: vec![vec![1; 32], vec![2; 32], vec![3; 32]],
            recent_blockhash: vec![0; 32],
            instructions: vec![yellowstone_grpc_proto::prelude::CompiledInstruction {
                program_id_index: 2,
                accounts: vec![0, 1],
                data: if large { vec![5; 4096] } else { vec![] },
            }],
            ..Default::default()
        }),
    };
    vec![Message::Account(a), Message::Transaction(tx)]
}
fn begin(b: &SubscriberChannels, slot: u64, large: bool) {
    b.observe_callback(slot);
    b.order_integrity.register_bank((slot, slot));
    b.send_group(
        CommitmentLevel::Confirmed,
        Arc::new(group_content(slot, large)),
        Some(GroupBoundary::Begin(BankGroup {
            slot,
            bank_id: slot,
            observed: 1,
            executed: 1,
            valid: true,
            resource_limit: false,
            source_after_seal: false,
        })),
    );
}
fn meta(b: &SubscriberChannels, slot: u64) {
    let mut m = fixtures::message_block_meta(slot);
    Arc::make_mut(&mut m).block_meta.bank_id = slot;
    Arc::make_mut(&mut m).block_meta.executed_transaction_count = 1;
    b.send(
        CommitmentLevel::Confirmed,
        Arc::new(vec![Message::BlockMeta(m)]),
    );
}
fn end(b: &SubscriberChannels, slot: u64) {
    let m = fixtures::message_slot(slot, SlotStatus::Confirmed);
    b.send_group(
        CommitmentLevel::Confirmed,
        Arc::new(vec![Message::Slot(Arc::new(m))]),
        Some(GroupBoundary::End {
            slot,
            bank_id: slot,
        }),
    );
}
#[tokio::test]
async fn old_wire_projection_and_full_status_order_are_byte_identical_with_b_on() {
    let b = SubscriberChannels::new(64, 64, 64);
    let mut off = spawn(b.clone());
    let mut on = spawn(b.clone());
    install(&mut off, &request(false)).await;
    install(&mut on, &request(true)).await;
    begin(&b, 10, false);
    // An independent interslot producer interleaves between the original batches.
    b.send(
        CommitmentLevel::Confirmed,
        Arc::new(vec![Message::Slot(Arc::new(fixtures::message_slot(
            11,
            SlotStatus::CreatedBank,
        )))]),
    );
    meta(&b, 10);
    end(&b, 10);
    let mut old_off = Vec::new();
    for _ in 0..6 {
        old_off.push(read(&mut off.1).await.encode_to_vec());
    }
    let mut old_on = Vec::new();
    let mut kinds = Vec::new();
    for _ in 0..8 {
        let u = read(&mut on.1).await;
        let wire = u.as_subscribe_update();
        kinds.push(match wire.update_oneof {
            Some(UpdateOneof::TransactionOrder(_)) => "order",
            Some(UpdateOneof::OrderSlotSeal(_)) => "seal",
            Some(UpdateOneof::Transaction(_)) => "full",
            Some(UpdateOneof::TransactionStatus(_)) => "status",
            _ => "old",
        });
        if !matches!(u.message, FilteredUpdateOneof::Order(_)) {
            old_on.push(u.encode_to_vec());
        }
    }
    assert_eq!(
        old_on, old_off,
        "all legacy filters, timestamps and metadata retain exact wire bytes"
    );
    assert_eq!(&kinds[1..4], &["full", "status", "order"]);
    assert_eq!(kinds.last(), Some(&"seal"));
    off.2.abort();
    on.2.abort();
}
#[tokio::test]
async fn b_credit_failure_keeps_a_alive_and_never_sends_normal_seal() {
    let mut b = SubscriberChannels::new(64, 64, 64);
    b.order_integrity = Arc::new(crate::order_integrity::OrderIntegrity::new(
        crate::plugin::filter::limits::FilterLimitsOrder {
            queue_bytes: 700,
            ..Default::default()
        },
    ));
    let mut c = spawn(b.clone());
    install(&mut c, &request(true)).await;
    begin(&b, 10, true);
    meta(&b, 10);
    end(&b, 10);
    let mut gap = false;
    let mut old = 0;
    for _ in 0..6 {
        let u = read(&mut c.1).await;
        match u.as_subscribe_update().update_oneof {
            Some(UpdateOneof::OrderGap(g)) => {
                assert_eq!(g.reason, Some(6));
                gap = true;
            }
            Some(UpdateOneof::OrderSlotSeal(_) | UpdateOneof::OrderEpochSeal(_)) => {
                panic!("B cannot seal after failure")
            }
            _ => old += 1,
        }
    }
    assert!(gap);
    assert_eq!(old, 5);
    assert!(!c.2.is_finished());
    b.send(
        CommitmentLevel::Confirmed,
        Arc::new(vec![Message::Slot(Arc::new(fixtures::message_slot(
            12,
            SlotStatus::CreatedBank,
        )))]),
    );
    assert!(matches!(
        read(&mut c.1).await.message,
        FilteredUpdateOneof::Slot(_)
    ));
    c.2.abort();
}
#[tokio::test]
async fn closed_epoch_is_revoked_by_late_source_notice_while_a_continues() {
    let b = SubscriberChannels::new(64, 64, 64);
    let mut c = spawn(b.clone());
    let ack = install(&mut c, &request(true)).await.expect("ack");
    begin(&b, 10, false);
    meta(&b, 10);
    end(&b, 10);
    for _ in 0..7 {
        read(&mut c.1).await;
    }
    let close = SubscribeRequest {
        order_close: Some(SubscribeRequestOrderClose {
            context: ack.context.clone(),
            tracked_keys_sha256: ack.tracked_keys_sha256,
        }),
        ..Default::default()
    };
    c.0.send(Some((None, Filter::order_control(close))))
        .expect("close request");
    assert!(matches!(
        read(&mut c.1).await.as_subscribe_update().update_oneof,
        Some(UpdateOneof::OrderEpochSeal(_))
    ));
    b.order_integrity.note_late((10, 10)); // No following broadcast: idle deadline observes the sticky notice.
    let Some(UpdateOneof::OrderGap(gap)) = read(&mut c.1).await.as_subscribe_update().update_oneof
    else {
        panic!("historical gap");
    };
    assert_eq!(gap.reason, Some(7));
    assert_eq!(gap.context, ack.context);
    b.send(
        CommitmentLevel::Confirmed,
        Arc::new(vec![Message::Slot(Arc::new(fixtures::message_slot(
            12,
            SlotStatus::CreatedBank,
        )))]),
    );
    assert!(matches!(
        read(&mut c.1).await.message,
        FilteredUpdateOneof::Slot(_)
    ));
    c.2.abort();
}
#[tokio::test]
async fn saturated_control_reserve_retries_revocation_without_normal_seal_or_a_starvation() {
    let (a, mut rx) = load_aware_channel(1, WeightScales::default());
    let q = OrderQueue::new(
        &a,
        &crate::plugin::filter::limits::FilterLimitsOrder {
            control_bytes: 4096,
            ..Default::default()
        },
    );
    let reject = crate::order_protocol::control(UpdateOneof::OrderRejected(
        yellowstone_grpc_proto::prelude::SubscribeUpdateOrderRejected {
            source_stream_id: vec![1; 16],
            reason: Some(1),
            detail: "x".repeat(1024),
            ..Default::default()
        },
    ));
    let mut queued = 0;
    while q.send(reject.clone()).is_ok() {
        queued += 1;
    }
    assert!(queued > 0);
    let gap = crate::order_protocol::control(UpdateOneof::OrderGap(
        yellowstone_grpc_proto::prelude::SubscribeUpdateOrderGap {
            next_source_seq: Some(0),
            reason: Some(7),
            detail: "late".repeat(256),
            ..Default::default()
        },
    ));
    q.send(gap).expect("bounded pending revocation");
    assert!(!q.pending_gap.lock().expect("pending lock").is_empty());
    a.try_send(Ok(FilteredUpdate::new_empty(FilteredUpdateOneof::ping())))
        .expect("A has original capacity");
    for _ in 0..queued {
        read(&mut rx).await;
    }
    q.flush();
    assert!(q.pending_gap.lock().expect("pending lock").is_empty());
    assert!(matches!(
        read(&mut rx).await.message,
        FilteredUpdateOneof::Ping
    ));
    assert!(matches!(
        read(&mut rx).await.as_subscribe_update().update_oneof,
        Some(UpdateOneof::OrderGap(_))
    ));
}

#[tokio::test]
async fn replace_and_close_at_each_original_batch_gap_keep_bank_atomic() {
    for closing in [false, true] {
        for gap in 0..3 {
            let b = SubscriberChannels::new(64, 64, 64);
            let mut c = spawn(b.clone());
            let initial = install(&mut c, &request(true)).await.expect("initial ack");
            if gap > 0 {
                begin(&b, 10, false);
                for _ in 0..4 {
                    read(&mut c.1).await;
                }
                if gap == 2 {
                    meta(&b, 10);
                    read(&mut c.1).await;
                }
            }
            if closing {
                c.0.send(Some((
                    None,
                    Filter::order_control(SubscribeRequest {
                        order_close: Some(SubscribeRequestOrderClose {
                            context: initial.context.clone(),
                            tracked_keys_sha256: initial.tracked_keys_sha256.clone(),
                        }),
                        ..Default::default()
                    }),
                )))
                .expect("close");
                for _ in 0..16 {
                    tokio::task::yield_now().await;
                }
            } else {
                let mut r = request(true);
                let keys = vec![vec![1; 32], vec![2; 32]];
                r.accounts
                    .get_mut("accounts")
                    .expect("accounts")
                    .account
                    .push(Pubkey::new_from_array([2; 32]).to_string());
                let order = r.order_writes.as_mut().expect("order request");
                order.request_version = Some(2);
                order.previous_epoch = Some(
                    initial
                        .context
                        .as_ref()
                        .expect("context")
                        .source_epoch
                        .clone(),
                );
                order.tracked_keys = keys;
                order.request_sha256 = request_digest(order);
                c.0.send(Some((None, filter(&r)))).expect("replace");
                assert!(matches!(
                    read(&mut c.1).await.message,
                    FilteredUpdateOneof::OwnerRejectExemption(_)
                ));
            }
            if gap > 0 {
                if gap == 1 {
                    meta(&b, 10);
                }
                end(&b, 10);
                for i in 0..(if gap == 1 { 3 } else { 2 }) {
                    let u = read(&mut c.1).await;
                    if i == (if gap == 1 { 2 } else { 1 }) {
                        assert!(
                            matches!(
                                u.as_subscribe_update().update_oneof,
                                Some(UpdateOneof::OrderSlotSeal(_))
                            ),
                            "old group must seal before any scope control"
                        );
                    }
                }
            }
            let Some(UpdateOneof::OrderEpochSeal(seal)) =
                read(&mut c.1).await.as_subscribe_update().update_oneof
            else {
                panic!("old epoch seal at gap {gap}");
            };
            assert_eq!(seal.reason, Some(if closing { 2 } else { 1 }));
            if !closing {
                let Some(UpdateOneof::OrderCoverageAck(ack)) =
                    read(&mut c.1).await.as_subscribe_update().update_oneof
                else {
                    panic!("new full-scope ack");
                };
                assert_eq!(ack.keys.len(), 2);
                assert_eq!(
                    ack.keys[0].active_from_slot,
                    initial.keys[0].active_from_slot
                );
                assert_eq!(
                    ack.previous_epoch,
                    initial.context.as_ref().map(|c| c.source_epoch.clone())
                );
            }
            begin(&b, 20, false);
            meta(&b, 20);
            end(&b, 20);
            let mut seq = None;
            for _ in 0..(if closing { 5 } else { 7 }) {
                let u = read(&mut c.1).await;
                if let Some(UpdateOneof::TransactionOrder(order)) =
                    u.as_subscribe_update().update_oneof
                {
                    seq = order.source_seq;
                }
                if closing {
                    assert!(
                        !matches!(u.message, FilteredUpdateOneof::Order(_)),
                        "A continues after B close"
                    );
                }
            }
            if !closing {
                assert_eq!(seq, Some(0));
            }
            c.2.abort();
        }
    }
}

#[tokio::test]
async fn startup_rejects_b_explicitly_and_preserves_independent_a_filter() {
    let (tx, mut requests) = mpsc::unbounded_channel();
    let (stream, mut rx) = load_aware_channel(1024, WeightScales::default());
    let b = LazyOrderQueue::new(&stream, &Default::default());
    let cancellation = CancellationToken::new();
    let mut session = ClientSession::new(
        0,
        Some("snapshot".into()),
        "snapshot".into(),
        cancellation.clone(),
        None,
    );
    let (snapshot_tx, snapshot_rx) = crossbeam_channel::unbounded();
    drop(snapshot_tx);
    tx.send(Some((None, filter(&request(true)))))
        .expect("startup request");
    GrpcService::client_loop_snapshot(
        &mut session,
        stream,
        &mut requests,
        snapshot_rx,
        cancellation,
        &b,
    )
    .await
    .expect("A snapshot still completes");
    assert!(matches!(
        read(&mut rx).await.as_subscribe_update().update_oneof,
        Some(UpdateOneof::OrderRejected(_))
    ));
    assert!(!session.order.has_active());
    let account = group_content(10, false).remove(0);
    assert_eq!(
        session
            .filter
            .get_updates(&account, Some(CommitmentLevel::Confirmed))
            .len(),
        1,
        "A account subscription remains installed"
    );
}

#[tokio::test]
async fn legacy_latest_request_coalescing_is_preserved_without_b() {
    let b = SubscriberChannels::new(64, 64, 64);
    let (tx, requests) = mpsc::unbounded_channel();
    let (stream, mut rx) = load_aware_channel(64, WeightScales::default());
    let session = ClientSession::new(
        0,
        Some("coalesce".into()),
        "coalesce".into(),
        CancellationToken::new(),
        None,
    );
    // Queue both before starting the consumer; the legacy branch must install only the latest.
    tx.send(Some((None, filter(&request(false)))))
        .expect("first request");
    let mut latest = request(false);
    latest
        .accounts
        .get_mut("accounts")
        .expect("account filter")
        .account = vec![Pubkey::new_from_array([2; 32]).to_string()];
    tx.send(Some((None, filter(&latest))))
        .expect("latest request");
    let handle = tokio::spawn(GrpcService::client_loop(
        session,
        stream,
        requests,
        None,
        b.clone(),
        None,
        TaskTracker::new(),
        None,
    ));
    let Some(UpdateOneof::OwnerRejectExemption(owner)) =
        read(&mut rx).await.as_subscribe_update().update_oneof
    else {
        panic!("latest filter owner ack");
    };
    assert_eq!(owner.accounts.len(), 1);
    assert_eq!(
        owner.accounts[0].pubkey,
        Pubkey::new_from_array([2; 32]).to_string()
    );
    b.send(
        CommitmentLevel::Confirmed,
        Arc::new(vec![Message::Slot(Arc::new(fixtures::message_slot(
            12,
            SlotStatus::CreatedBank,
        )))]),
    );
    assert!(
        matches!(read(&mut rx).await.message, FilteredUpdateOneof::Slot(_)),
        "no intermediate filter owner ack remains queued"
    );
    handle.abort();
}

#[tokio::test]
async fn same_slot_competing_bank_ids_are_not_overwritten_in_seals() {
    let b = SubscriberChannels::new(64, 64, 64);
    let mut c = spawn(b.clone());
    install(&mut c, &request(true)).await;
    for bank_id in [100, 101] {
        b.observe_callback(10);
        b.order_integrity.register_bank((10, bank_id));
        b.send_group(
            CommitmentLevel::Confirmed,
            Arc::new(vec![]),
            Some(GroupBoundary::Begin(BankGroup {
                slot: 10,
                bank_id,
                observed: 0,
                executed: 0,
                valid: true,
                resource_limit: false,
                source_after_seal: false,
            })),
        );
        let mut m = fixtures::message_block_meta(10);
        Arc::make_mut(&mut m).block_meta.bank_id = bank_id;
        b.send(
            CommitmentLevel::Confirmed,
            Arc::new(vec![Message::BlockMeta(m)]),
        );
        let mut slot = fixtures::message_slot(10, SlotStatus::Confirmed);
        slot.bank_id = Some(bank_id);
        b.send_group(
            CommitmentLevel::Confirmed,
            Arc::new(vec![Message::Slot(Arc::new(slot))]),
            Some(GroupBoundary::End { slot: 10, bank_id }),
        );
        for _ in 0..2 {
            read(&mut c.1).await;
        }
        let Some(UpdateOneof::OrderSlotSeal(seal)) =
            read(&mut c.1).await.as_subscribe_update().update_oneof
        else {
            panic!("separate Bank seal");
        };
        assert_eq!(seal.slot, Some(10));
        assert_eq!(seal.bank_id, Some(bank_id));
        assert_eq!(seal.event_count, Some(0));
    }
    // Two conflicting confirmed banks remain explicit evidence for the reader to reject;
    // source envelopes cannot silently collapse them into a slot-only entry.
    c.2.abort();
}

#[tokio::test]
async fn broadcast_lag_is_transport_failure_and_cannot_be_normal_b_end() {
    let b = SubscriberChannels::new(1, 1, 1);
    let mut c = spawn(b.clone());
    install(&mut c, &request(true)).await;
    for slot in 10..14 {
        b.send(
            CommitmentLevel::Confirmed,
            Arc::new(vec![Message::Slot(Arc::new(fixtures::message_slot(
                slot,
                SlotStatus::CreatedBank,
            )))]),
        );
    }
    let failure = tokio::time::timeout(Duration::from_secs(2), c.1.recv())
        .await
        .expect("lag is observable")
        .expect("transport status")
        .expect_err("old A lag policy reports transport error");
    assert!(failure.message().contains("lagged"));
    tokio::time::timeout(Duration::from_secs(2), c.2)
        .await
        .expect("client exits after real broadcast lag")
        .expect("client task returns normally");
}

#[tokio::test]
async fn invalid_source_bank_gaps_b_and_keeps_original_a_tail() {
    let b = SubscriberChannels::new(64, 64, 64);
    let mut c = spawn(b.clone());
    install(&mut c, &request(true)).await;
    b.observe_callback(10);
    b.order_integrity.register_bank((10, 10));
    b.send_group(
        CommitmentLevel::Confirmed,
        Arc::new(group_content(10, false)),
        Some(GroupBoundary::Begin(BankGroup {
            slot: 10,
            bank_id: 10,
            observed: 2,
            executed: 1,
            valid: false,
            resource_limit: false,
            source_after_seal: false,
        })),
    );
    meta(&b, 10);
    end(&b, 10);
    let mut gap = false;
    let mut old = 0;
    for _ in 0..6 {
        let update = read(&mut c.1).await;
        match update.as_subscribe_update().update_oneof {
            Some(UpdateOneof::OrderGap(g)) => {
                gap = true;
                assert_eq!(g.reason, Some(2));
            }
            Some(
                UpdateOneof::OrderSlotSeal(_)
                | UpdateOneof::OrderEpochSeal(_)
                | UpdateOneof::TransactionOrder(_),
            ) => panic!("invalid Bank cannot produce complete coverage"),
            _ => old += 1,
        }
    }
    assert!(gap);
    assert_eq!(old, 5);
    assert!(!c.2.is_finished());
    c.2.abort();
}

#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../tests/fixtures/order_source/revision_baseline_reconstruction.rs"]
mod revision_baseline;
const REVISION_TIMESTAMP: Timestamp = Timestamp {
    seconds: 1700000000,
    nanos: 123,
};
fn revision_callbacks(slot: u64, bank_id: u64) -> Vec<Message> {
    let mut messages = Vec::new();
    for key in crate::block_reconstruction_v2::MUST_HAVE_SYSVAR_ACCOUNTS {
        let mut a = fixtures::simple_message_account(key, Pubkey::default());
        let m = Arc::make_mut(&mut a);
        m.slot = slot;
        m.bank_id = Some(bank_id);
        m.created_at = REVISION_TIMESTAMP;
        messages.push(Message::Account(a));
    }
    let mut created = fixtures::message_slot(slot, SlotStatus::CreatedBank);
    created.bank_id = Some(bank_id);
    created.created_at = REVISION_TIMESTAMP;
    messages.push(Message::Slot(Arc::new(created)));
    for mut m in group_content(slot, false) {
        match &mut m {
            Message::Account(a) => {
                let a = Arc::make_mut(a);
                a.bank_id = Some(bank_id);
                a.created_at = REVISION_TIMESTAMP;
            }
            Message::Transaction(t) => {
                let t = Arc::make_mut(t);
                t.bank_id = bank_id;
                t.created_at = REVISION_TIMESTAMP;
            }
            _ => {}
        }
        messages.push(m);
    }
    let mut entry = fixtures::message_entry(slot, 0);
    let e = Arc::make_mut(&mut entry);
    e.bank_id = bank_id;
    e.created_at = REVISION_TIMESTAMP;
    e.executed_transaction_count = 1;
    messages.push(Message::Entry(entry));
    let mut meta = fixtures::message_block_meta(slot);
    let m = Arc::make_mut(&mut meta);
    m.created_at = REVISION_TIMESTAMP;
    m.block_meta.bank_id = bank_id;
    m.block_meta.executed_transaction_count = 1;
    m.block_meta.entries_count = 1;
    messages.push(Message::BlockMeta(meta));
    for status in [
        SlotStatus::Processed,
        SlotStatus::Confirmed,
        SlotStatus::Finalized,
    ] {
        let mut m = fixtures::message_slot(slot, status);
        m.bank_id = Some(bank_id);
        m.created_at = REVISION_TIMESTAMP;
        messages.push(Message::Slot(Arc::new(m)));
    }
    messages
}
async fn revision_baseline_loop(
    mut rx: mpsc::UnboundedReceiver<BlockReconstructionMessage>,
    broadcast: SubscriberChannels,
) {
    let mut machine = revision_baseline::BlockMachineStorage::new(16);
    while let Some(batch) = rx.recv().await {
        match batch {
            BlockReconstructionMessage::Single(m) => machine.add(m),
            BlockReconstructionMessage::Batch(ms) => {
                for m in ms.iter() {
                    machine.add(m.clone());
                }
            }
        }
        while let Some((u, frozen)) = machine.pop_ready_block() {
            let level = match u.commitment {
                solana_commitment_config::CommitmentLevel::Processed => CommitmentLevel::Processed,
                solana_commitment_config::CommitmentLevel::Confirmed => CommitmentLevel::Confirmed,
                solana_commitment_config::CommitmentLevel::Finalized => CommitmentLevel::Finalized,
            };
            if level != CommitmentLevel::Processed {
                broadcast.send(level, frozen.messages());
            }
            let mut messages = vec![Message::Block(frozen.get_message_block())];
            if let Some(footer) = frozen.get_block_footer() {
                messages.push(Message::BlockFooter(footer));
            }
            messages.push(Message::BlockMeta(frozen.get_block_meta()));
            broadcast.send(level, Arc::new(messages));
            let status = match level {
                CommitmentLevel::Processed => SlotStatus::Processed,
                CommitmentLevel::Confirmed => SlotStatus::Confirmed,
                CommitmentLevel::Finalized => SlotStatus::Finalized,
            };
            let m = Arc::new(vec![Message::Slot(Arc::new(MessageSlot {
                slot: u.slot,
                parent: u.parent_slot,
                status,
                dead_error: None,
                bank_id: Some(u.bank_id),
                created_at: REVISION_TIMESTAMP,
            }))]);
            for level in [
                CommitmentLevel::Processed,
                CommitmentLevel::Confirmed,
                CommitmentLevel::Finalized,
            ] {
                broadcast.send(level, Arc::clone(&m));
            }
        }
    }
}
#[tokio::test]
async fn revision_r1_zero_b_callback_pipeline_matches_3a3d1cd_without_order_work() {
    async fn run(
        baseline: bool,
        shared_filter: Filter,
    ) -> (Vec<Vec<u8>>, Arc<crate::order_integrity::OrderIntegrity>) {
        let mut b = SubscriberChannels::new(64, 64, 64);
        b.revision_timestamp = Some(REVISION_TIMESTAMP);
        let audit = Arc::clone(&b.order_integrity);
        let mut client = spawn(b.clone());
        client
            .0
            .send(Some((None, shared_filter)))
            .expect("A filter");
        read(&mut client.1).await;
        let (tx, rx) = mpsc::unbounded_channel();
        let (reconstruct_tx, reconstruct_rx) = mpsc::unbounded_channel();
        let ingress = tokio::spawn(GrpcService::geyser_loop(
            crate::stream::tokio::BatchStreamUnboundedReceiver::new(rx),
            b.clone(),
            reconstruct_tx,
        ));
        let reconstruct = if baseline {
            tokio::spawn(revision_baseline_loop(reconstruct_rx, b.clone()))
        } else {
            tokio::spawn(GrpcService::block_reconstruction_loop(
                crate::stream::tokio::BatchStreamUnboundedReceiver::new(reconstruct_rx),
                b.clone(),
                None,
                None,
                16,
            ))
        };
        for m in revision_callbacks(10, 10) {
            tx.send(m).expect("callback queue");
        }
        drop(tx);
        let mut result = Vec::new();
        loop {
            let u = read(&mut client.1).await;
            let done = matches!(&u.message,FilteredUpdateOneof::Slot(s)if s.slot==10 && s.status==SlotStatus::Finalized);
            result.push(u.encode_to_vec());
            if done {
                break;
            }
        }
        ingress.await.expect("ingress");
        reconstruct.abort();
        client.2.abort();
        (result, audit)
    }
    let f = filter(&request(false));
    let (reference, _) = run(true, f.clone()).await;
    let (actual, audit) = run(false, f).await;
    assert_eq!(
        actual, reference,
        "callback queue to reconstruction to A wire must match frozen 3a3d1cd"
    );
    assert_eq!(
        audit.revision_locks.load(Ordering::Relaxed),
        0,
        "zero B: no integrity locks"
    );
    assert_eq!(
        audit.revision_bank_scans.load(Ordering::Relaxed),
        0,
        "zero B: no Bank validation scan"
    );
    assert_eq!(
        audit.revision_client_work.load(Ordering::Relaxed),
        0,
        "zero B: no B queue/timer/client scheduling"
    );
}

fn revision_request(
    keys: &[u8],
    previous: Option<&yellowstone_grpc_proto::prelude::OrderContext>,
) -> SubscribeRequest {
    let mut r = request(previous.is_some());
    r.accounts.get_mut("accounts").expect("accounts").account = keys
        .iter()
        .map(|k| Pubkey::new_from_array([*k; 32]).to_string())
        .collect();
    if let Some(context) = previous {
        let order = r.order_writes.as_mut().expect("B request");
        order.request_version = context.request_version.and_then(|v| v.checked_add(1));
        order.previous_epoch = Some(context.source_epoch.clone());
        order.tracked_keys = keys.iter().map(|k| vec![*k; 32]).collect();
        order.request_sha256 = request_digest(order);
    }
    r
}
fn revision_tracked_filter(r: &SubscribeRequest) -> Filter {
    // The real inbound stream retains subsequent A requests once B was requested.
    let mut f = filter(r);
    f.order_source = Some(Arc::new(r.clone()));
    f
}
#[tokio::test]
async fn revision_r2_pending_scope_removed_by_installed_a_filter_is_rejected() {
    for second_b in [false, true] {
        let b = SubscriberChannels::new(64, 64, 64);
        let mut c = spawn(b.clone());
        let initial = install(&mut c, &request(true)).await.expect("initial");
        begin(&b, 10, false);
        for _ in 0..4 {
            read(&mut c.1).await;
        }
        let replace = revision_request(&[1, 2], initial.context.as_ref());
        let pending_hash = replace
            .order_writes
            .as_ref()
            .expect("replace")
            .request_sha256
            .clone();
        c.0.send(Some((None, revision_tracked_filter(&replace))))
            .expect("pending replace");
        assert!(matches!(
            read(&mut c.1).await.message,
            FilteredUpdateOneof::OwnerRejectExemption(_)
        ));
        let next = if second_b {
            revision_request(&[1, 3], initial.context.as_ref())
        } else {
            revision_request(&[1], None)
        };
        c.0.send(Some((None, revision_tracked_filter(&next))))
            .expect("A removes pending K1");
        let mut seen = Vec::new();
        loop {
            let u = read(&mut c.1).await;
            let owner = matches!(u.message, FilteredUpdateOneof::OwnerRejectExemption(_));
            seen.push(u);
            if owner {
                break;
            }
        }
        meta(&b, 10);
        end(&b, 10);
        b.send(
            CommitmentLevel::Confirmed,
            Arc::new(vec![Message::Slot(Arc::new(fixtures::message_slot(
                20,
                SlotStatus::CreatedBank,
            )))]),
        );
        loop {
            let u = read(&mut c.1).await;
            let done = matches!(&u.message,FilteredUpdateOneof::Slot(s)if s.slot==20);
            seen.push(u);
            if done {
                break;
            }
        }
        assert!(!seen.iter().any(|u|matches!(u.as_subscribe_update().update_oneof,Some(UpdateOneof::OrderCoverageAck(a))if a.context.as_ref().and_then(|c|c.request_version)==Some(2))),"pending K1 scope cannot activate after installed A removes its coverage");
        assert!(seen.iter().any(|u|matches!(u.as_subscribe_update().update_oneof,Some(UpdateOneof::OrderRejected(r))if r.reason==Some(3) && r.request_sha256==Some(pending_hash.clone()))),"cancelled pending request needs its own explicit rejection");
        if second_b {
            let hash = next
                .order_writes
                .as_ref()
                .expect("second B")
                .request_sha256
                .clone();
            assert!(seen.iter().any(|u|matches!(u.as_subscribe_update().update_oneof,Some(UpdateOneof::OrderRejected(r))if r.reason==Some(4) && r.request_sha256==Some(hash.clone()))),"second concurrent B remains PENDING_EXISTS");
        }
        let close = SubscribeRequest {
            order_close: Some(SubscribeRequestOrderClose {
                context: initial.context,
                tracked_keys_sha256: initial.tracked_keys_sha256,
            }),
            ..Default::default()
        };
        c.0.send(Some((None, Filter::order_control(close))))
            .expect("old scope still usable");
        assert!(
            matches!(read(&mut c.1).await.as_subscribe_update().update_oneof,Some(UpdateOneof::OrderEpochSeal(s))if s.reason==Some(2))
        );
        c.2.abort();
    }
}
#[tokio::test]
async fn revision_r2_pending_ack_binds_latest_installed_owner_registry() {
    let b = SubscriberChannels::new(64, 64, 64);
    let mut c = spawn(b.clone());
    let initial = install(&mut c, &request(true)).await.expect("initial");
    begin(&b, 10, false);
    for _ in 0..4 {
        read(&mut c.1).await;
    }
    let replace = revision_request(&[1, 2], initial.context.as_ref());
    c.0.send(Some((None, revision_tracked_filter(&replace))))
        .expect("replace");
    read(&mut c.1).await;
    let a = revision_request(&[1, 2, 3], None);
    c.0.send(Some((None, revision_tracked_filter(&a))))
        .expect("retained pending keys");
    let owner = read(&mut c.1).await;
    assert!(matches!(
        owner.message,
        FilteredUpdateOneof::OwnerRejectExemption(_)
    ));
    meta(&b, 10);
    end(&b, 10);
    for _ in 0..4 {
        read(&mut c.1).await;
    }
    let Some(UpdateOneof::OrderCoverageAck(ack)) =
        read(&mut c.1).await.as_subscribe_update().update_oneof
    else {
        panic!("new ack");
    };
    assert_eq!(
        ack.owner_exemption_update_sha256,
        crate::order_protocol::sha256(&owner.encode_to_vec()),
        "activation must attest the current installed owner registry"
    );
    c.2.abort();
}

async fn revision_sealed_history(
    q: &OrderQueue,
    rx: &mut LoadAwareReceiver<TonicResult<FilteredUpdate>>,
) -> (
    crate::order_stream::OrderSession,
    Arc<crate::order_integrity::OrderIntegrity>,
    Vec<yellowstone_grpc_proto::prelude::OrderContext>,
) {
    let registry = Arc::new(crate::order_integrity::OrderIntegrity::new(
        Default::default(),
    ));
    let mut order = crate::order_stream::OrderSession::default();
    order.register_watch(&registry).expect("watch");
    let mut client = ClientSession::new(
        0,
        Some("history".into()),
        "history".into(),
        CancellationToken::new(),
        None,
    );
    let acks = std::cell::RefCell::new(Vec::new());
    {
        let mut send = |u: FilteredUpdate| -> anyhow::Result<()> {
            if let Some(UpdateOneof::OrderCoverageAck(ack)) = u.as_subscribe_update().update_oneof {
                acks.borrow_mut().push(ack);
            }
            q.send(u)
        };
        for (slot, keys) in [(10, vec![1]), (20, vec![1, 2])] {
            let r = if slot == 10 {
                request(true)
            } else {
                revision_request(
                    &keys,
                    acks.borrow().last().and_then(
                        |a: &yellowstone_grpc_proto::prelude::SubscribeUpdateOrderCoverageAck| {
                            a.context.as_ref()
                        },
                    ),
                )
            };
            let owner = client
                .install_filter(revision_tracked_filter(&r))
                .expect("install")
                .expect("actual owner ack");
            order
                .request(&r, &owner, slot - 10, &mut send)
                .expect("scope");
            registry.register_bank((slot, slot));
            order
                .begin(
                    BankGroup {
                        slot,
                        bank_id: slot,
                        observed: 0,
                        executed: 0,
                        valid: true,
                        resource_limit: false,
                        source_after_seal: false,
                    },
                    &mut send,
                )
                .expect("group");
            let mut meta = fixtures::message_block_meta(slot);
            Arc::make_mut(&mut meta).block_meta.bank_id = slot;
            let m = Message::BlockMeta(meta);
            let updates = client
                .filter
                .get_updates(&m, Some(CommitmentLevel::Confirmed));
            order
                .observe_sent(&m, &updates, &mut send)
                .expect("metadata");
            let m = Message::Slot(Arc::new(fixtures::message_slot(
                slot,
                SlotStatus::Confirmed,
            )));
            let updates = client
                .filter
                .get_updates(&m, Some(CommitmentLevel::Confirmed));
            order
                .observe_sent(&m, &updates, &mut send)
                .expect("Confirmed");
            order.end(slot, slot, slot, &mut send).expect("slot seal");
        }
        let ack = acks.borrow().last().expect("last ack").clone();
        order
            .close(
                &SubscribeRequest {
                    order_close: Some(SubscribeRequestOrderClose {
                        context: ack.context,
                        tracked_keys_sha256: ack.tracked_keys_sha256,
                    }),
                    ..Default::default()
                },
                &mut send,
            )
            .expect("close second historical epoch");
    }
    let mut seals = 0;
    for _ in 0..6 {
        if matches!(
            read(rx).await.as_subscribe_update().update_oneof,
            Some(UpdateOneof::OrderEpochSeal(_))
        ) {
            seals += 1;
        }
    }
    assert_eq!(seals, 2, "two epochs were actually sealed on the queue");
    let contexts = acks
        .into_inner()
        .into_iter()
        .map(|a| a.context.expect("context"))
        .collect();
    (order, registry, contexts)
}
fn revision_fill_control(q: &OrderQueue) -> usize {
    let control = q.control.as_ref().expect("control lane");
    let mut count = 0;
    loop {
        let remaining = 4096 - control.current_weight();
        let mut selected = None;
        for len in (1..=1024).rev() {
            let update = crate::order_protocol::control(UpdateOneof::OrderRejected(
                yellowstone_grpc_proto::prelude::SubscribeUpdateOrderRejected {
                    source_stream_id: vec![1; 16],
                    reason: Some(1),
                    detail: "x".repeat(len),
                    ..Default::default()
                },
            ));
            if update.encoded_len() <= remaining {
                selected = Some(update);
                break;
            }
        }
        let Some(update) = selected else {
            break;
        };
        q.send(update).expect("fill exact control credit");
        count += 1;
    }
    assert!(
        4096 - control.current_weight() < 80,
        "even a small historical gap cannot fit"
    );
    count
}
#[tokio::test]
async fn revision_r3_all_sealed_epochs_are_revoked_after_control_credit_returns() {
    let (a, mut rx) = load_aware_channel(1, WeightScales::default());
    let q = OrderQueue::new(
        &a,
        &crate::plugin::filter::limits::FilterLimitsOrder {
            control_bytes: 4096,
            ..Default::default()
        },
    );
    let (mut order, registry, contexts) = revision_sealed_history(&q, &mut rx).await;
    let filled = revision_fill_control(&q);
    registry.note_late((10, 10));
    registry.note_late((20, 20));
    order
        .late((10, 10), &mut |u| q.send(u))
        .expect("retain E1 revocation");
    order
        .late((20, 20), &mut |u| q.send(u))
        .expect("retain E2 revocation");
    a.try_send(Ok(FilteredUpdate::new_empty(FilteredUpdateOneof::ping())))
        .expect("A original capacity remains available");
    for _ in 0..filled {
        read(&mut rx).await;
    }
    q.flush();
    let mut epochs = std::collections::HashSet::new();
    let mut a_seen = false;
    while let Ok(Some(update)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
        let update = update.expect("healthy stream");
        match update.as_subscribe_update().update_oneof {
            Some(UpdateOneof::OrderGap(g)) => {
                assert_eq!(g.reason, Some(7));
                epochs.insert(g.context.expect("affected context").source_epoch);
            }
            Some(UpdateOneof::Ping(_)) => a_seen = true,
            _ => panic!("unexpected normal seal/update during revocation"),
        }
    }
    assert!(a_seen);
    assert_eq!(
        epochs,
        contexts.into_iter().map(|c| c.source_epoch).collect(),
        "every sealed epoch needs its own explicit revocation; one gap cannot mask another"
    );
}
#[tokio::test]
async fn revision_r3_reported_changes_only_after_gap_is_queued_or_retained() {
    let (a, mut rx) = load_aware_channel(1, WeightScales::default());
    let q = OrderQueue::new(&a, &Default::default());
    let (mut order, registry, contexts) = revision_sealed_history(&q, &mut rx).await;
    registry.note_late((10, 10));
    assert!(order
        .late((10, 10), &mut |_| anyhow::bail!(
            "injected enqueue/retention failure"
        ))
        .is_err());
    order
        .check_deadline(&mut |u| q.send(u))
        .expect("sticky retry");
    let update = tokio::time::timeout(Duration::from_millis(50), rx.recv())
        .await
        .expect("failed enqueue must not consume the source notice")
        .expect("gap")
        .expect("stream");
    assert!(
        matches!(update.as_subscribe_update().update_oneof,Some(UpdateOneof::OrderGap(g))if g.context.as_ref()==Some(&contexts[0]))
    );
}
