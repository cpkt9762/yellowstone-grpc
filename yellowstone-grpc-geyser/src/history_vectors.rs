//! Frozen §7.7 inputs, produced with the actual generated prost schema.
use {
    super::history_protocol as hp, crate::order_protocol as op, base64::Engine, prost::Message,
    serde_json::json, yellowstone_grpc_proto::prelude::*,
};
type Rows = Vec<(String, &'static str, Vec<u8>)>;
fn old<T: Message + Default>(name: &str) -> T {
    let rows: serde_json::Value = serde_json::from_str(include_str!("history_vectors_legacy.json"))
        .expect("fixed 4ba920c93 vectors");
    let row = rows
        .as_array()
        .expect("legacy rows")
        .iter()
        .find(|x| x["name"] == name)
        .expect("legacy name");
    let raw = base64::engine::general_purpose::STANDARD
        .decode(row["raw"].as_str().expect("legacy base64"))
        .expect("legacy bytes");
    T::decode(raw.as_slice()).expect("legacy protobuf")
}
fn uuid(s: &str) -> Vec<u8> {
    uuid::Uuid::parse_str(s)
        .expect("frozen UUID")
        .as_bytes()
        .to_vec()
}
fn u(oneof: subscribe_update::UpdateOneof, history: bool) -> SubscribeUpdate {
    SubscribeUpdate {
        filters: vec![if history {
            hp::CAPABILITY
        } else {
            op::CAPABILITY
        }
        .into()],
        update_oneof: Some(oneof),
        created_at: Some(prost_types::Timestamp::default()),
    }
}
fn hash<T: Message>(v: &T) -> Vec<u8> {
    op::sha256(&v.encode_to_vec())
}
fn ld<T: Message>(v: &T) -> Vec<u8> {
    let mut d = op::OrderedDigest::default();
    d.append(&v.encode_to_vec()).expect("length framed");
    d.finish()
}
fn row<T: Message>(rows: &mut Rows, name: &str, typ: &'static str, value: &T) {
    rows.push((name.into(), typ, value.encode_to_vec()));
}
struct Round {
    q: SubscribeRequest,
    owner: SubscribeUpdate,
    a: SubscribeUpdate,
    ha: SubscribeUpdate,
    confirmed: SubscribeUpdate,
    meta: SubscribeUpdate,
    os: SubscribeUpdate,
    hs: SubscribeUpdate,
    oe: SubscribeUpdate,
    he: SubscribeUpdate,
    context: OrderContext,
    kd: Vec<u8>,
}
fn round(n: u64, previous: Option<(&Round, Vec<u8>, Vec<u8>)>) -> Round {
    let mut keys = vec![vec![0; 32]];
    if n == 2 {
        keys.push(
            crate::block_reconstruction_v2::MUST_HAVE_SYSVAR_ACCOUNTS[0]
                .to_bytes()
                .to_vec(),
        );
    }
    keys.sort();
    let kd = op::keys_digest(&keys).expect("frozen key set");
    let slot = 99 + n;
    let bank = 999 + n;
    let context = OrderContext {
        protocol_version: Some(1),
        source_stream_id: uuid("33333333-3333-4333-8333-333333333333"),
        source_epoch: uuid(if n == 1 {
            "44444444-4444-4444-8444-444444444444"
        } else {
            "55555555-5555-4555-8555-555555555555"
        }),
        request_version: Some(n),
    };
    let mut request: SubscribeRequest = old("request");
    let o = request.order_writes.as_mut().expect("legacy order request");
    o.tracked_keys = keys.clone();
    o.request_version = Some(n);
    o.previous_epoch = previous
        .as_ref()
        .map(|(r, _, _)| r.context.source_epoch.clone());
    o.request_sha256 = op::request_digest(o);
    let mut h = SubscribeRequestTrackedHistory {
        protocol_version: Some(1),
        request_version: Some(n),
        previous_epoch: o.previous_epoch.clone(),
        tracked_keys: keys.clone(),
        request_sha256: Vec::new(),
        order_request_sha256: o.request_sha256.clone(),
    };
    h.request_sha256 = hp::request_digest(&h);
    request.tracked_account_history = Some(h.clone());
    let mut owner: SubscribeUpdate = old("owner");
    let Some(subscribe_update::UpdateOneof::OwnerRejectExemption(body)) = &mut owner.update_oneof
    else {
        panic!("legacy owner");
    };
    body.accounts = keys
        .iter()
        .map(|k| OwnerRejectExemptAccount {
            pubkey: bs58::encode(k).into_string(),
            active_from_slot: 0,
        })
        .collect();
    let cover = keys
        .iter()
        .enumerate()
        .map(|(i, k)| OrderKeyCoverage {
            pubkey: k.clone(),
            active_from_slot: Some(if i == 0 { 100 } else { 101 }),
            owner_exempt_active_from_slot: Some(0),
        })
        .collect::<Vec<_>>();
    let mut a: SubscribeUpdate = old("ack");
    let Some(subscribe_update::UpdateOneof::OrderCoverageAck(body)) = &mut a.update_oneof else {
        panic!("legacy ack");
    };
    body.context = Some(context.clone());
    body.request_sha256 = o.request_sha256.clone();
    body.tracked_keys_sha256 = kd.clone();
    body.keys = cover.clone();
    body.active_from_slot = Some(slot);
    body.observed_callback_slot_high_watermark = Some(slot - 1);
    body.owner_exemption_update_sha256 = hash(&owner);
    body.previous_epoch = previous
        .as_ref()
        .map(|(r, _, _)| r.context.source_epoch.clone());
    body.previous_epoch_seal_sha256 = previous.as_ref().map(|(_, h, _)| h.clone());
    let ha = u(
        subscribe_update::UpdateOneof::TrackedHistoryAck(SubscribeUpdateTrackedHistoryAck {
            context: Some(context.clone()),
            request_sha256: h.request_sha256,
            order_coverage_ack_sha256: hash(&a),
            tracked_keys_sha256: kd.clone(),
            keys: cover,
            observed_callback_slot_high_watermark: Some(slot - 1),
            registration_generation: Some(n),
            previous_epoch: previous
                .as_ref()
                .map(|(r, _, _)| r.context.source_epoch.clone()),
            previous_history_epoch_seal_sha256: previous.as_ref().map(|(_, _, h)| h.clone()),
            capabilities: Some(hp::capabilities()),
            switch_slot: Some(slot),
            source_limits: Some(hp::limits(&Default::default(), 4194304, 16384)),
        }),
        true,
    );
    let mut confirmed: SubscribeUpdate = old("confirmed_zero");
    let Some(subscribe_update::UpdateOneof::Slot(body)) = &mut confirmed.update_oneof else {
        panic!("legacy slot");
    };
    body.slot = slot;
    body.parent = Some(slot - 1);
    body.bank_id = Some(bank);
    let mut meta: SubscribeUpdate = old("meta");
    let Some(subscribe_update::UpdateOneof::BlockMeta(body)) = &mut meta.update_oneof else {
        panic!("legacy meta");
    };
    body.slot = slot;
    body.parent_slot = slot - 1;
    body.bank_id = bank;
    body.executed_transaction_count = 0;
    let mut os: SubscribeUpdate = old("zero_slot_seal");
    let Some(subscribe_update::UpdateOneof::OrderSlotSeal(body)) = &mut os.update_oneof else {
        panic!("legacy slot seal");
    };
    body.context = Some(context.clone());
    body.slot = Some(slot);
    body.bank_id = Some(bank);
    body.confirmed_update_sha256 = hash(&confirmed);
    body.block_meta_update_sha256 = hash(&meta);
    let empty = op::sha256(&[]);
    let hs = u(
        subscribe_update::UpdateOneof::TrackedAccountBankSeal(
            SubscribeUpdateTrackedHistoryBankSeal {
                context: Some(context.clone()),
                slot: Some(slot),
                bank_id: Some(bank),
                event_count: Some(0),
                first_history_seq: None,
                last_history_seq: None,
                ordered_events_sha256: empty.clone(),
                order_slot_seal_sha256: hash(&os),
                confirmed_update_sha256: hash(&confirmed),
                block_meta_update_sha256: hash(&meta),
                covered_keys_sha256: kd.clone(),
                null_writer_count: Some(0),
                signed_writer_count: Some(0),
                complete_bank: Some(true),
                observed_callback_count: Some(0),
            },
        ),
        true,
    );
    let mut oe: SubscribeUpdate = old("empty_epoch_seal");
    let Some(subscribe_update::UpdateOneof::OrderEpochSeal(body)) = &mut oe.update_oneof else {
        panic!("legacy epoch seal");
    };
    body.context = Some(context.clone());
    body.slot_seal_count = Some(1);
    body.ordered_slot_seals_sha256 = ld(&os);
    body.reason = Some(if n == 1 { 1 } else { 2 });
    body.tracked_keys_sha256 = kd.clone();
    let he = u(
        subscribe_update::UpdateOneof::TrackedHistoryEpochSeal(
            SubscribeUpdateTrackedHistoryEpochSeal {
                context: Some(context.clone()),
                event_count: Some(0),
                last_history_seq: None,
                ordered_events_sha256: empty,
                bank_seal_count: Some(1),
                ordered_bank_seals_sha256: ld(&hs),
                reason: Some(if n == 1 { 1 } else { 2 }),
                tracked_keys_sha256: kd.clone(),
                order_epoch_seal_sha256: hash(&oe),
                end_slot_exclusive: Some(slot + 1),
            },
        ),
        true,
    );
    Round {
        q: request,
        owner,
        a,
        ha,
        confirmed,
        meta,
        os,
        hs,
        oe,
        he,
        context,
        kd,
    }
}
fn core() -> Rows {
    let one = round(1, None);
    let two = round(2, Some((&one, hash(&one.oe), hash(&one.he))));
    let mut rows = Vec::new();
    row(&mut rows, "Q1", "geyser.SubscribeRequest", &one.q);
    for (name, value) in [
        ("owner1", &one.owner),
        ("A1", &one.a),
        ("HA1", &one.ha),
        ("confirmed1", &one.confirmed),
        ("meta1", &one.meta),
        ("OS1", &one.os),
        ("HS1", &one.hs),
    ] {
        row(&mut rows, name, "geyser.SubscribeUpdate", value);
    }
    row(&mut rows, "Q2", "geyser.SubscribeRequest", &two.q);
    for (name, value) in [
        ("OE1", &one.oe),
        ("HE1", &one.he),
        ("owner2", &two.owner),
        ("A2", &two.a),
        ("HA2", &two.ha),
        ("confirmed2", &two.confirmed),
        ("meta2", &two.meta),
        ("OS2", &two.os),
        ("HS2", &two.hs),
    ] {
        row(&mut rows, name, "geyser.SubscribeUpdate", value);
    }
    let mut close: SubscribeRequest = old("close");
    let o = close.order_close.as_mut().expect("legacy close");
    o.context = Some(two.context.clone());
    o.tracked_keys_sha256 = two.kd.clone();
    close.tracked_history_close = Some(SubscribeRequestTrackedHistoryClose {
        context: Some(two.context),
        tracked_keys_sha256: two.kd,
    });
    row(&mut rows, "QC", "geyser.SubscribeRequest", &close);
    row(&mut rows, "OE2", "geyser.SubscribeUpdate", &two.oe);
    row(&mut rows, "HE2", "geyser.SubscribeUpdate", &two.he);
    rows
}
#[test]
fn tracked_account_history_v1_canonical_paired_zero_replace_close() {
    let rows = core();
    assert_eq!(rows.len(), 21);
    let a2 = SubscribeUpdate::decode(rows.iter().find(|r| r.0 == "A2").expect("A2").2.as_slice())
        .expect("A2 decode");
    assert!(
        matches!(a2.update_oneof,Some(subscribe_update::UpdateOneof::OrderCoverageAck(a)) if a.active_from_slot==Some(101))
    );
    for (name, typ, raw) in &rows {
        if *typ == "geyser.SubscribeUpdate" {
            let decoded = SubscribeUpdate::decode(raw.as_slice()).expect("generated decode");
            assert_eq!(decoded.encode_to_vec(), *raw, "{name}");
            if decoded.filters == [hp::CAPABILITY] {
                let custom = hp::wrap(
                    decoded.update_oneof.expect("history variant"),
                    decoded.created_at.expect("timestamp presence"),
                );
                assert_eq!(custom.encode_to_vec(), *raw, "custom encoder {name}");
                assert_eq!(custom.encoded_len(), raw.len());
            }
        } else {
            let decoded = SubscribeRequest::decode(raw.as_slice()).expect("request decode");
            assert_eq!(decoded.encode_to_vec(), *raw);
        }
    }
    if let Ok(path) = std::env::var("TRACKED_HISTORY_VECTOR_DIR") {
        let root = std::path::Path::new(&path);
        std::fs::create_dir_all(root).expect("vector output directory");
        let mut manifest = Vec::new();
        let mut pack = Vec::new();
        for (name, typ, raw) in rows {
            let offset = pack.len() + 8;
            pack.extend_from_slice(&(raw.len() as u64).to_le_bytes());
            pack.extend_from_slice(&raw);
            std::fs::write(root.join(format!("{name}.bin")), &raw).expect("canonical vector bytes");
            manifest.push(json!({"name":name,"type":typ,"bytes":raw.len(),"sha256":op::sha256(&raw).iter().map(|b|format!("{b:02x}")).collect::<String>(),"storage":{"pack":0,"offset":offset,"length":raw.len()}}));
        }
        std::fs::write(root.join("pack.bin"), pack).expect("pack bytes");
        std::fs::write(root.join("manifest.json"),serde_json::to_vec_pretty(&json!({"family":"r1_paired_zero_replace_close","evidence_level":"emitted_output_synthetic","expected":"codec/hash correct; formal admission rejected (controls-only isolated core)","vectors":manifest})).expect("manifest JSON")).expect("vector manifest");
    }
}
