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
    assert!(q.pending_gap.lock().expect("pending lock").is_some());
    a.try_send(Ok(FilteredUpdate::new_empty(FilteredUpdateOneof::ping())))
        .expect("A has original capacity");
    for _ in 0..queued {
        read(&mut rx).await;
    }
    q.flush();
    assert!(q.pending_gap.lock().expect("pending lock").is_none());
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
    let b = OrderQueue::new(&stream, &Default::default());
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
