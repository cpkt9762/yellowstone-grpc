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
                resource_limit: false,
                source_after_seal: false,
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
            resource_limit: false,
            source_after_seal: false,
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
#[test]
fn partial_group_warmup_missing_end_and_sequence_overflow_fail_closed() {
    let mut s = OrderSession::default();
    let mut out = Vec::new();
    let (r, o) = request(vec![vec![1; 32]], 1, None);
    s.request(&r, &o, 3, &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("initial");
    s.begin(
        BankGroup {
            slot: 2,
            bank_id: 2,
            observed: 0,
            executed: 0,
            valid: true,
            resource_limit: false,
            source_after_seal: false,
        },
        &mut |u| {
            out.push(u);
            Ok(())
        },
    )
    .expect("warmup");
    s.end(2, 2, 3, &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("warmup ends without fake seal");
    assert_eq!(out.len(), 1);
    s.begin(
        BankGroup {
            slot: 5,
            bank_id: 5,
            observed: 0,
            executed: 0,
            valid: true,
            resource_limit: false,
            source_after_seal: false,
        },
        &mut |u| {
            out.push(u);
            Ok(())
        },
    )
    .expect("begin");
    s.group.as_mut().expect("group").started = Instant::now() - Duration::from_secs(16);
    s.check_deadline(&mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("deadline gap");
    assert!(matches!(
        out[1].as_subscribe_update().update_oneof,
        Some(subscribe_update::UpdateOneof::OrderGap(_))
    ));
    assert!(!s.has_active());
    let mut s = OrderSession::default();
    let mut out = Vec::new();
    s.request(&r, &o, 3, &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("initial");
    s.end(5, 5, 5, &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("partial gap");
    assert!(!s.has_active());
    assert!(matches!(
        out[1].as_subscribe_update().update_oneof,
        Some(subscribe_update::UpdateOneof::OrderGap(_))
    ));
}
#[test]
fn source_bank_integrity_checks_counts_index_and_signature_without_dedup() {
    let keys = fixtures::deterministic_pubkeys(1, 3);
    let mut tx = fixtures::message_transaction(
        solana_signature::Signature::from([9; 64]),
        keys,
        false,
        Default::default(),
    );
    Arc::make_mut(&mut tx).transaction.index = 0;
    let mut block = fixtures::message_block(10, vec![Arc::clone(&tx)], vec![], vec![]);
    let b = Arc::make_mut(&mut block);
    Arc::make_mut(&mut b.meta)
        .block_meta
        .executed_transaction_count = 1;
    assert!(BankGroup::from_block(&block).valid);
    Arc::make_mut(&mut block).transactions.push(Arc::clone(&tx));
    assert!(
        !BankGroup::from_block(&block).valid,
        "extra callback cannot be deduped into success"
    );
    Arc::make_mut(&mut Arc::make_mut(&mut block).meta)
        .block_meta
        .executed_transaction_count = 2;
    assert!(
        !BankGroup::from_block(&block).valid,
        "matching counts do not cure duplicate identity/index"
    );
    let mut tx2 = (*tx).clone();
    tx2.transaction.index = 1;
    Arc::make_mut(&mut block).transactions[1] = Arc::new(tx2);
    assert!(
        !BankGroup::from_block(&block).valid,
        "duplicate signature is independently rejected"
    );
    assert!(
        BankGroup::with_limit(&block, 1).resource_limit,
        "validation memory has its own bound"
    );
}
#[test]
fn digest_detects_delete_duplicate_swap_and_zero_seal_deletion() {
    fn digest(items: &[&[u8]]) -> Vec<u8> {
        let mut h = OrderedDigest::default();
        for i in items {
            h.append(i).expect("frame");
        }
        h.finish()
    }
    let expected = digest(&[b"order0", b"order1", b"order2"]);
    for altered in [
        vec![b"order0".as_slice(), b"order2"],
        vec![b"order0", b"order1", b"order1", b"order2"],
        vec![b"order1", b"order0", b"order2"],
    ] {
        assert_ne!(expected, digest(&altered));
    }
    assert_ne!(
        digest(&[b"zero_slot_seal", b"nonzero_slot_seal"]),
        digest(&[b"nonzero_slot_seal"])
    );
}

#[test]
fn failed_vote_unknown_and_readonly_preserve_raw_and_epoch_sequence() {
    use crate::plugin::message::MessageTransaction;
    use solana_pubkey::Pubkey;
    use solana_signature::Signature;
    let mut s = OrderSession::default();
    let mut out = Vec::new();
    let (r, o) = request(vec![vec![1; 32]], 1, None);
    s.request(&r, &o, 0, &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("request");
    let keys = vec![
        Pubkey::new_from_array([1; 32]),
        Pubkey::new_from_array([2; 32]),
    ];
    let mut failed = fixtures::message_transaction(
        Signature::from([9; 64]),
        keys,
        true,
        TransactionStatusMeta {
            err: Some(TransactionError { err: vec![] }),
            ..Default::default()
        },
    );
    Arc::make_mut(&mut failed).transaction.index = 0;
    let mut unknown = (*failed).clone();
    unknown.transaction.signature = Signature::from([10; 64]);
    unknown.transaction.transaction.signatures[0] = vec![10; 64];
    unknown.transaction.index = 1;
    unknown
        .transaction
        .transaction
        .message
        .as_mut()
        .expect("message")
        .account_keys[1] = vec![1; 32];
    let keys = vec![
        Pubkey::new_from_array([2; 32]),
        Pubkey::new_from_array([1; 32]),
    ];
    let mut readonly =
        fixtures::message_transaction(Signature::from([11; 64]), keys, false, Default::default());
    let read = Arc::make_mut(&mut readonly);
    read.transaction.index = 2;
    read.transaction
        .transaction
        .message
        .as_mut()
        .expect("message")
        .header
        .as_mut()
        .expect("header")
        .num_readonly_unsigned_accounts = 1;
    let mut block = fixtures::message_block(
        100,
        vec![
            Arc::clone(&failed),
            Arc::new(unknown.clone()),
            Arc::clone(&readonly),
        ],
        vec![],
        vec![],
    );
    Arc::make_mut(&mut Arc::make_mut(&mut block).meta)
        .block_meta
        .executed_transaction_count = 3;
    Arc::make_mut(&mut Arc::make_mut(&mut block).meta)
        .block_meta
        .bank_id = 100;
    s.begin(BankGroup::from_block(&block), &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("begin");
    for tx in [Arc::clone(&failed), Arc::new(unknown), readonly] {
        s.observe_sent(&SourceMessage::Transaction(tx), &[], &mut |u| {
            out.push(u);
            Ok(())
        })
        .expect("source order");
    }
    assert_eq!(
        out.len(),
        3,
        "ack plus one failed vote and one unknown; no readonly order"
    );
    let Some(subscribe_update::UpdateOneof::TransactionOrder(first)) =
        out[1].as_subscribe_update().update_oneof
    else {
        panic!("first order");
    };
    assert_eq!(first.source_seq, Some(0));
    assert_eq!(first.index, Some(0));
    assert_eq!(first.is_vote, Some(true));
    assert_eq!(first.err, Some(TransactionError { err: vec![] }));
    assert_eq!(
        first.transaction,
        Some(failed.transaction.transaction.clone())
    );
    let Some(subscribe_update::UpdateOneof::TransactionOrder(second)) =
        out[2].as_subscribe_update().update_oneof
    else {
        panic!("second order");
    };
    assert_eq!(second.source_seq, Some(1));
    assert_eq!(second.permissions_state, Some(2));
    s.active.as_mut().expect("active").seq = u64::MAX;
    let tx: Arc<MessageTransaction> = failed;
    assert!(s
        .observe_sent(&SourceMessage::Transaction(tx), &[], &mut |u| {
            out.push(u);
            Ok(())
        })
        .is_err());
    assert_eq!(out.len(), 3, "overflow cannot enqueue a duplicate sequence");
    s.fail(
        OrderGapReason::ResourceLimit,
        Some((100, 100)),
        "sequence overflow",
        &mut |u| {
            out.push(u);
            Ok(())
        },
    )
    .expect("overflow gap");
}

fn revision_r4_feed_bank(
    source: &mut crate::block_reconstruction_v2::BlockMachineStorage,
    slot: u64,
    expected: u64,
    status: Option<SlotStatus>,
) {
    use solana_pubkey::Pubkey;
    let mut created = fixtures::message_slot(slot, SlotStatus::CreatedBank);
    created.bank_id = Some(slot);
    source.add(SourceMessage::Slot(Arc::new(created)));
    for key in crate::block_reconstruction_v2::MUST_HAVE_SYSVAR_ACCOUNTS {
        let mut a = fixtures::simple_message_account(key, Pubkey::default());
        let m = Arc::make_mut(&mut a);
        m.slot = slot;
        m.bank_id = Some(slot);
        source.add(SourceMessage::Account(a));
    }
    let mut meta = fixtures::message_block_meta(slot);
    let m = Arc::make_mut(&mut meta);
    m.block_meta.bank_id = slot;
    m.block_meta.executed_transaction_count = expected;
    source.add(SourceMessage::BlockMeta(meta));
    if let Some(status) = status {
        source.add(SourceMessage::Slot(Arc::new(fixtures::message_slot(
            slot, status,
        ))));
    }
}
fn revision_r4_source(
    registry: &Arc<crate::order_integrity::OrderIntegrity>,
) -> (
    crate::block_reconstruction_v2::BlockMachineStorage,
    Arc<crate::block_reconstruction_v2::FrozenBank>,
) {
    revision_r4_source_mode(registry, Some(SlotStatus::Confirmed))
}
fn revision_r4_source_mode(
    registry: &Arc<crate::order_integrity::OrderIntegrity>,
    status: Option<SlotStatus>,
) -> (
    crate::block_reconstruction_v2::BlockMachineStorage,
    Arc<crate::block_reconstruction_v2::FrozenBank>,
) {
    let mut source = crate::block_reconstruction_v2::BlockMachineStorage::new(16);
    source.order_integrity = Some(Arc::clone(registry));
    revision_r4_feed_bank(&mut source, 20, 1, status);
    revision_r4_feed_bank(&mut source, 21, 0, Some(SlotStatus::Confirmed));
    let mut ready = None;
    while let Some((u, frozen)) = source.pop_ready_block() {
        if u.slot == 21 && u.commitment == solana_commitment_config::CommitmentLevel::Confirmed {
            ready = Some(frozen);
        }
    }
    (
        source,
        ready.expect("Y really freezes/confirms despite incomplete X"),
    )
}
fn revision_r4_start(
    s: &mut OrderSession,
    frozen: &crate::block_reconstruction_v2::FrozenBank,
    out: &mut Vec<FilteredUpdate>,
) {
    let limits = crate::plugin::filter::limits::FilterLimitsOrder::default();
    s.begin(BankGroup::from_frozen(frozen, &limits), &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("native group begin");
    for message in frozen.messages().iter() {
        s.observe_sent(message, &[], &mut |u| {
            out.push(u);
            Ok(())
        })
        .expect("native group content");
    }
}
fn revision_r4_end(
    s: &mut OrderSession,
    frozen: &crate::block_reconstruction_v2::FrozenBank,
    out: &mut Vec<FilteredUpdate>,
) {
    let meta = frozen.get_block_meta();
    let slot = meta.slot;
    let bank_id = meta.bank_id;
    let message = SourceMessage::BlockMeta(Arc::clone(&meta));
    let update = FilteredUpdate::new_empty(FilteredUpdateOneof::BlockMeta(meta));
    s.observe_sent(&message, &[update], &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("native metadata");
    let message = MessageSlot {
        slot,
        parent: Some(slot - 1),
        status: SlotStatus::Confirmed,
        dead_error: None,
        bank_id: Some(bank_id),
        created_at: Default::default(),
    };
    let source = SourceMessage::Slot(Arc::new(message.clone()));
    let update = FilteredUpdate::new_empty(FilteredUpdateOneof::slot(message));
    s.observe_sent(&source, &[update], &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("native Confirmed");
    s.end(slot, bank_id, 21, &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("native end");
}
#[test]
fn revision_r4_close_cannot_seal_around_another_native_incomplete_bank() {
    let registry = Arc::new(crate::order_integrity::OrderIntegrity::new(
        Default::default(),
    ));
    let mut s = OrderSession::default();
    s.register_watch(&registry).expect("watch");
    let mut out = Vec::new();
    let (r, o) = request(vec![vec![1; 32]], 1, None);
    s.request(&r, &o, 0, &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("scope");
    let (_source, y) = revision_r4_source(&registry);
    assert!(s
        .watch
        .as_ref()
        .expect("watch")
        .pending(1)
        .expect("X pending"));
    revision_r4_start(&mut s, &y, &mut out);
    let active = s.active.as_ref().expect("active");
    let close = SubscribeRequest {
        order_close: Some(SubscribeRequestOrderClose {
            context: Some(active.context.clone()),
            tracked_keys_sha256: active.key_hash.clone(),
        }),
        ..Default::default()
    };
    s.close(&close, &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("close inside Y");
    revision_r4_end(&mut s, &y, &mut out);
    assert!(!out.iter().any(|u|matches!(u.as_subscribe_update().update_oneof,Some(subscribe_update::UpdateOneof::OrderEpochSeal(seal))if seal.reason==Some(2))),"Y ending cannot erase native X's incomplete coverage");
    assert!(s.closing.is_some());
    s.closing = Some(Instant::now() - Duration::from_secs(16));
    s.check_deadline(&mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("fixed total close deadline");
    assert!(out.iter().any(|u|matches!(u.as_subscribe_update().update_oneof,Some(subscribe_update::UpdateOneof::OrderGap(g))if g.reason==Some(3))));
    assert!(!s.has_active());
}
#[test]
fn revision_r4_replace_waits_for_all_native_pending_banks() {
    let registry = Arc::new(crate::order_integrity::OrderIntegrity::new(
        Default::default(),
    ));
    let mut s = OrderSession::default();
    s.register_watch(&registry).expect("watch");
    let mut out = Vec::new();
    let (r, o) = request(vec![vec![1; 32]], 1, None);
    s.request(&r, &o, 0, &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("scope");
    let (mut source, y) = revision_r4_source(&registry);
    revision_r4_start(&mut s, &y, &mut out);
    let epoch = s
        .active
        .as_ref()
        .expect("active")
        .context
        .source_epoch
        .clone();
    let (r, o) = request(vec![vec![1; 32], vec![2; 32]], 2, Some(epoch));
    s.request(&r, &o, 21, &mut |u| {
        out.push(u);
        Ok(())
    })
    .expect("pending replace");
    revision_r4_end(&mut s, &y, &mut out);
    assert!(!out.iter().any(|u|matches!(u.as_subscribe_update().update_oneof,Some(subscribe_update::UpdateOneof::OrderEpochSeal(seal))if seal.reason==Some(1))),"REPLACED is also a normal sealing exit and cannot skip X");
    assert!(s.has_pending());
    let mut tx = fixtures::message_transaction(
        solana_signature::Signature::from([9; 64]),
        vec![solana_pubkey::Pubkey::new_from_array([1; 32])],
        false,
        Default::default(),
    );
    let m = Arc::make_mut(&mut tx);
    m.slot = 20;
    m.bank_id = 20;
    m.transaction.index = 0;
    source.add(SourceMessage::Transaction(tx));
    let mut x = None;
    while let Some((u, frozen)) = source.pop_ready_block() {
        if u.slot == 20 && u.commitment == solana_commitment_config::CommitmentLevel::Confirmed {
            x = Some(frozen);
        }
    }
    let x = x.expect("missing X transaction completes the real source Bank");
    revision_r4_start(&mut s, &x, &mut out);
    revision_r4_end(&mut s, &x, &mut out);
    assert!(!s.has_pending());
    assert!(out.iter().any(|u|matches!(u.as_subscribe_update().update_oneof,Some(subscribe_update::UpdateOneof::OrderCoverageAck(ack))if ack.context.as_ref().and_then(|c|c.request_version)==Some(2))));
}

#[test]
fn revision_r4_finalized_and_inherited_confirmation_also_block_close() {
    for status in [Some(SlotStatus::Finalized), None] {
        let registry = Arc::new(crate::order_integrity::OrderIntegrity::new(
            Default::default(),
        ));
        let mut s = OrderSession::default();
        s.register_watch(&registry).expect("watch");
        let mut out = Vec::new();
        let (r, o) = request(vec![vec![1; 32]], 1, None);
        s.request(&r, &o, 0, &mut |u| {
            out.push(u);
            Ok(())
        })
        .expect("scope");
        let (_source, y) = revision_r4_source_mode(&registry, status);
        revision_r4_start(&mut s, &y, &mut out);
        let active = s.active.as_ref().expect("active");
        let close = SubscribeRequest {
            order_close: Some(SubscribeRequestOrderClose {
                context: Some(active.context.clone()),
                tracked_keys_sha256: active.key_hash.clone(),
            }),
            ..Default::default()
        };
        s.close(&close, &mut |u| {
            out.push(u);
            Ok(())
        })
        .expect("close");
        revision_r4_end(&mut s, &y, &mut out);
        assert!(
            !out.iter().any(|u| matches!(
                u.as_subscribe_update().update_oneof,
                Some(subscribe_update::UpdateOneof::OrderEpochSeal(_))
            )),
            "native Finalized/inherited Confirmed cannot hide incomplete ancestor X"
        );
        assert!(s.closing.is_some());
        s.closing = Some(Instant::now() - Duration::from_secs(16));
        s.check_deadline(&mut |u| {
            out.push(u);
            Ok(())
        })
        .expect("deadline");
        assert!(!s.has_active());
    }
}
