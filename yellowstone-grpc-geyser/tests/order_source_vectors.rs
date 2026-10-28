//! Real t6 decoded-message inputs; new output and negative mutations are synthetic.
use {
    prost::Message,
    std::{collections::HashSet, path::PathBuf},
    yellowstone_grpc_geyser::{
        order_protocol::*, plugin::order_permissions::Permissions, version::GrpcVersionInfo,
    },
    yellowstone_grpc_proto::prelude::*,
};
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/order_source")
}
fn manifest() -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(root().join("manifest.json")).expect("vector manifest"))
        .expect("manifest JSON")
}
fn input(row: &serde_json::Value) -> SubscribeUpdate {
    let raw =
        std::fs::read(root().join(row["file"].as_str().expect("input file"))).expect("input bytes");
    assert_eq!(
        format!("{:x}", sha2::Sha256::digest(&raw)),
        row["sha256"].as_str().expect("hash")
    );
    SubscribeUpdate::decode(raw.as_slice()).expect("real decoded-message input")
}
use sha2::Digest;
fn info(
    update: &SubscribeUpdate,
) -> (&SubscribeUpdateTransaction, &SubscribeUpdateTransactionInfo) {
    let Some(subscribe_update::UpdateOneof::Transaction(tx)) = &update.update_oneof else {
        panic!("full input");
    };
    (tx, tx.transaction.as_ref().expect("full transaction info"))
}
fn decode_key(value: &str) -> [u8; 32] {
    bs58::decode(value)
        .into_vec()
        .expect("base58 key")
        .try_into()
        .expect("32 byte key")
}
#[test]
fn real_legacy_v0_v1_permissions_and_independent_synthetic_golden_bytes() {
    let m = manifest();
    let rows = m["vectors"].as_array().expect("vectors");
    assert_eq!(rows.len(), 6);
    for row in rows {
        assert_eq!(row["source_input"], "source_input_real");
        assert_eq!(row["emitted_output"], "emitted_output_synthetic");
        let update = input(row);
        let (outer, i) = info(&update);
        let tx = i.transaction.as_ref().expect("transaction");
        let meta = i.meta.as_ref().expect("meta");
        let Permissions::Known(keys) = Permissions::classify(&i.signature, tx, meta) else {
            panic!("known real fixture");
        };
        let expected = row["expected_writable"]
            .as_array()
            .expect("expected keys")
            .iter()
            .map(|k| decode_key(k.as_str().expect("key")))
            .collect::<HashSet<_>>();
        assert_eq!(
            keys.into_iter().collect::<HashSet<_>>(),
            expected,
            "{}",
            row["file"]
        );
        let context = OrderContext {
            protocol_version: Some(1),
            source_stream_id: uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111")
                .expect("fixed stream")
                .as_bytes()
                .to_vec(),
            source_epoch: uuid::Uuid::parse_str("22222222-2222-4222-8222-222222222222")
                .expect("fixed epoch")
                .as_bytes()
                .to_vec(),
            request_version: Some(1),
        };
        let order = wrap(
            subscribe_update::UpdateOneof::TransactionOrder(SubscribeUpdateTransactionOrder {
                context: Some(context),
                source_seq: Some(0),
                slot: Some(outer.slot),
                bank_id: Some(outer.bank_id),
                index: Some(i.index),
                signature: i.signature.clone(),
                is_vote: Some(i.is_vote),
                transaction: Some(tx.clone()),
                err: meta.err.clone(),
                loaded_writable: meta.loaded_writable_addresses.clone(),
                loaded_readonly: meta.loaded_readonly_addresses.clone(),
                permissions_state: Some(1),
            }),
            update.created_at.expect("original timestamp"),
        );
        let bytes = order.encode_to_vec();
        let golden =
            std::fs::read(root().join(row["synthetic_order_file"].as_str().expect("golden")))
                .expect("independently encoded golden");
        assert_eq!(bytes, golden, "{}", row["file"]);
        assert_eq!(bytes.len(), order.encoded_len());
        assert_eq!(order.as_subscribe_update().encode_to_vec(), bytes);
        assert_eq!(
            SubscribeUpdate::decode(bytes.as_slice()).expect("new proto roundtrip"),
            order.as_subscribe_update()
        );
    }
}
#[test]
fn real_get_version_has_nested_git_and_new_version_has_exact_capability() {
    let raw =
        std::fs::read(root().join("get-version-real.json")).expect("original GetVersion string");
    let real: serde_json::Value = serde_json::from_slice(&raw).expect("real version JSON");
    assert_eq!(real["version"]["git"], "geyser-v13.2.1-103-g3a3d1cd");
    assert!(real.get("git").is_none());
    let current =
        serde_json::to_value(GrpcVersionInfo::default()).expect("new actual wire structure");
    assert!(current.get("git").is_none());
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("local revision evidence");
    assert!(head.status.success());
    assert_eq!(
        current["version"]["git_sha"]
            .as_str()
            .expect("built revision"),
        String::from_utf8(head.stdout)
            .expect("revision UTF-8")
            .trim()
    );
    assert_eq!(
        current["version"]["git_sha"]
            .as_str()
            .expect("full SHA")
            .len(),
        40
    );
    assert_eq!(
        current["version"]["proto_schema_sha256"]
            .as_str()
            .expect("schema hash")
            .len(),
        64
    );
    assert_eq!(
        current["version"]["capabilities"][0],
        serde_json::json!({"name":"order_writes_v1","protocol_version":1,"permission_semantics":"hb_upper_v2","commitment":1,"encoder_profile":"prost_order_v1","bank_scoped":true,"zero_write_seals":true,"full_target_unchanged":true})
    );
}
#[test]
fn synthetic_structural_mutations_are_unknown_scope_all() {
    let m = manifest();
    let u = input(&m["vectors"][0]);
    let (_, i) = info(&u);
    let original = i.transaction.clone().expect("transaction");
    let meta = i.meta.clone().expect("meta");
    for mutation in 0..7 {
        let mut tx = original.clone();
        let msg = tx.message.as_mut().expect("message");
        let mut mm = meta.clone();
        match mutation {
            0 => {
                msg.account_keys[1] = msg.account_keys[0].clone();
            }
            1 => {
                msg.instructions[0].program_id_index = 256;
            }
            2 => {
                msg.header.as_mut().expect("header").num_required_signatures = 256;
            }
            3 => {
                msg.versioned = true;
                msg.config = Some(TransactionConfig::default());
                mm.loaded_writable_addresses.push(vec![7; 32]);
            }
            4 => {
                mm.loaded_writable_addresses
                    .push(msg.account_keys[0].clone());
            }
            5 => {
                msg.instructions[0].accounts.push(255);
            }
            _ => {
                tx.signatures[0].pop();
            }
        }
        let p = Permissions::classify(&i.signature, &tx, &mm);
        assert!(matches!(p, Permissions::Unknown(_)), "mutation {mutation}");
        assert!(p.affects(&HashSet::from([[8; 32]])));
    }
}

#[test]
fn runtime_sdk_and_transaction_view_writable_are_subsets_with_explicit_reserved_harness() {
    use {
        agave_transaction_view::{
            resolved_transaction_view::ResolvedTransactionView, sanitize::SanitizeConfig,
            transaction_view::TransactionView,
        },
        solana_message::{
            v0::LoadedAddresses, SanitizedMessage, SanitizedVersionedMessage, SimpleAddressLoader,
            VersionedMessage,
        },
        solana_pubkey::Pubkey,
        solana_svm_transaction::svm_message::SVMMessage,
    };
    let m = manifest();
    // These are explicit test parameters, not an assertion about any Bank's active features.
    let limits = SanitizeConfig {
        min_requested_heap_size: 32768,
        max_requested_heap_size: 262144,
        max_instructions: 64,
        max_accounts_per_instruction: 255,
    };
    for row in m["vectors"].as_array().expect("vectors") {
        let u = input(row);
        let (_, i) = info(&u);
        let tx = i.transaction.as_ref().expect("transaction");
        let meta = i.meta.as_ref().expect("meta");
        let native = yellowstone_grpc_geyser::plugin::convert_from::create_tx_versioned(tx.clone())
            .expect("native transaction");
        let Permissions::Known(upper) = Permissions::classify(&i.signature, tx, meta) else {
            panic!("known real input");
        };
        let upper = upper.into_iter().collect::<HashSet<_>>();
        let loaded = LoadedAddresses {
            writable: meta
                .loaded_writable_addresses
                .iter()
                .map(|k| Pubkey::try_from(k.as_slice()).expect("loaded write"))
                .collect(),
            readonly: meta
                .loaded_readonly_addresses
                .iter()
                .map(|k| Pubkey::try_from(k.as_slice()).expect("loaded read"))
                .collect(),
        };
        let raw = wincode::serialize(&native).expect("native wire serialization");
        let view =
            TransactionView::try_new_sanitized(raw.as_slice(), &limits).expect("sanitized view");
        for reserved in [
            HashSet::new(),
            native
                .message
                .static_account_keys()
                .iter()
                .copied()
                .collect(),
        ] {
            let sanitized = SanitizedMessage::try_new(
                SanitizedVersionedMessage::try_from(native.message.clone())
                    .expect("sanitized version"),
                SimpleAddressLoader::Enabled(loaded.clone()),
                &reserved,
            )
            .expect("runtime SDK message");
            let resolved = ResolvedTransactionView::try_new(
                view.clone(),
                matches!(native.message, VersionedMessage::V0(_)).then(|| loaded.clone()),
                &reserved,
            )
            .expect("runtime resolved view");
            for (index, key) in sanitized.account_keys().iter().enumerate() {
                assert_eq!(
                    sanitized.is_writable(index),
                    resolved.is_writable(index),
                    "two runtime paths agree"
                );
                if sanitized.is_writable(index) {
                    assert!(
                        upper.contains(&key.to_bytes()),
                        "runtime writable cannot escape upper bound"
                    );
                }
            }
        }
    }
}

#[test]
fn independent_request_golden_and_legacy_subscription_keep_a_fields() {
    let bytes = std::fs::read(root().join("order-request-synthetic.bin"))
        .expect("independent canonical request");
    let request = decode_request(&bytes).expect("frozen canonical request");
    assert_eq!(request.request_version, Some(1));
    assert_eq!(request.tracked_keys, vec![vec![1; 32]]);
    let legacy =
        std::fs::read(root().join("subscribe-request-real.bin")).expect("real original request");
    let envelope = decode_subscription(&legacy).expect("legacy subscribe decode");
    assert!(envelope.order_wire_error.is_none());
    assert!(envelope.request.order_writes.is_none());
    let mut with_b = envelope.request.clone();
    with_b.order_writes = Some(request);
    let raw = with_b.encode_to_vec();
    let decoded = decode_subscription(&raw).expect("new request envelope");
    assert!(decoded.order_wire_error.is_none());
    assert_eq!(decoded.request.accounts, envelope.request.accounts);
    assert_eq!(decoded.request.transactions, envelope.request.transactions);
    // A remains decodable even when B has duplicate singular/unknown fields.
    let mut bad = raw;
    bad.extend_from_slice(&[0xb0, 0x3e, 1]);
    let decoded = decode_subscription(&bad).expect("independently valid A retained");
    assert!(decoded.order_wire_error.is_some());
    assert_eq!(decoded.request.accounts, envelope.request.accounts);
}
