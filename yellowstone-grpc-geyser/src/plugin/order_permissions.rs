//! hb_upper_v2: requested write locks minus proven program demotion only.
use {
    anyhow::{ensure, Context, Result},
    std::collections::HashSet,
    yellowstone_grpc_proto::prelude::{Transaction, TransactionStatusMeta},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permissions {
    Known(Vec<[u8; 32]>),
    /// Explicitly covers every tracked key; never infer readonly from a partial parse.
    Unknown(String),
}

impl Permissions {
    pub fn classify(signature: &[u8], tx: &Transaction, meta: &TransactionStatusMeta) -> Self {
        match writable_keys(signature, tx, meta) {
            Ok(keys) => Self::Known(keys),
            Err(error) => Self::Unknown(error.to_string()),
        }
    }
    pub fn affects(&self, tracked: &HashSet<[u8; 32]>) -> bool {
        match self {
            Self::Known(keys) => keys.iter().any(|key| tracked.contains(key)),
            Self::Unknown(_) => true,
        }
    }
}

fn writable_keys(
    signature: &[u8],
    tx: &Transaction,
    meta: &TransactionStatusMeta,
) -> Result<Vec<[u8; 32]>> {
    let message = tx.message.as_ref().context("missing transaction message")?;
    let header = message.header.as_ref().context("missing message header")?;
    let signed = usize::from(u8::try_from(header.num_required_signatures)?);
    let readonly_signed = usize::from(u8::try_from(header.num_readonly_signed_accounts)?);
    let readonly_unsigned = usize::from(u8::try_from(header.num_readonly_unsigned_accounts)?);
    let n = message.account_keys.len();
    ensure!(
        signed > 0
            && readonly_signed < signed
            && signed
                .checked_add(readonly_unsigned)
                .is_some_and(|v| v <= n),
        "invalid header counts"
    );
    ensure!(
        tx.signatures.len() == signed && tx.signatures.iter().all(|s| s.len() == 64),
        "invalid signatures"
    );
    ensure!(
        tx.signatures.first().is_some_and(|s| s == signature),
        "info signature differs from first signature"
    );
    ensure!(
        message.recent_blockhash.len() == 32,
        "invalid blockhash/lifetime length"
    );
    let writable_loaded = &meta.loaded_writable_addresses;
    let readonly_loaded = &meta.loaded_readonly_addresses;
    let lookups = &message.address_table_lookups;
    if !message.versioned || message.config.is_some() {
        ensure!(
            lookups.is_empty() && writable_loaded.is_empty() && readonly_loaded.is_empty(),
            "legacy/V1 cannot have lookups or loaded keys"
        );
        ensure!(
            message.versioned || message.config.is_none(),
            "legacy cannot have V1 config"
        );
    } else {
        let mut w = 0usize;
        let mut r = 0usize;
        for lookup in lookups {
            ensure!(lookup.account_key.len() == 32, "invalid lookup key");
            ensure!(
                !lookup.writable_indexes.is_empty() || !lookup.readonly_indexes.is_empty(),
                "empty lookup"
            );
            w = w
                .checked_add(lookup.writable_indexes.len())
                .context("lookup count overflow")?;
            r = r
                .checked_add(lookup.readonly_indexes.len())
                .context("lookup count overflow")?;
        }
        ensure!(
            w == writable_loaded.len() && r == readonly_loaded.len(),
            "lookup/loaded count mismatch"
        );
    }
    let total = n
        .checked_add(writable_loaded.len())
        .and_then(|v| v.checked_add(readonly_loaded.len()))
        .context("resolved key count overflow")?;
    ensure!(total <= 256, "too many resolved keys");
    let keys = message
        .account_keys
        .iter()
        .chain(writable_loaded)
        .chain(readonly_loaded)
        .map(|key| <[u8; 32]>::try_from(key.as_slice()).context("invalid account key length"))
        .collect::<Result<Vec<_>>>()?;
    ensure!(keys.len() <= 256, "too many resolved keys");
    let unique: HashSet<_> = keys.iter().copied().collect();
    ensure!(unique.len() == keys.len(), "duplicate resolved key");
    let mut invoked = HashSet::new();
    for instruction in &message.instructions {
        let program = usize::try_from(instruction.program_id_index)?;
        ensure!(program > 0 && program < n, "invalid program index");
        ensure!(
            instruction
                .accounts
                .iter()
                .all(|i| usize::from(*i) < keys.len()),
            "invalid account index"
        );
        invoked.insert(program);
    }
    // SDK constant is shared by all message versions. Loader presence anywhere
    // disables program demotion, including loaded readonly keys. Reserved keys
    // intentionally remain in this upper bound: the Bank feature set is unavailable.
    let loader = solana_sdk_ids::bpf_loader_upgradeable::id().to_bytes();
    let has_loader = unique.contains(&loader);
    let writable_signed_end = signed - readonly_signed;
    let writable_unsigned_end = n - readonly_unsigned;
    let writable_loaded_end = n
        .checked_add(writable_loaded.len())
        .context("key count overflow")?;
    Ok(keys
        .into_iter()
        .enumerate()
        .filter_map(|(i, key)| {
            let requested = i < writable_signed_end
                || (i >= signed && i < writable_unsigned_end)
                || (i >= n && i < writable_loaded_end);
            (requested && (has_loader || !invoked.contains(&i))).then_some(key)
        })
        .collect())
}

#[cfg(test)]
mod order_writes_v1 {
    use super::*;
    use yellowstone_grpc_proto::prelude::{
        CompiledInstruction, Message, MessageHeader, TransactionConfig,
    };
    fn transaction() -> Transaction {
        Transaction {
            signatures: vec![vec![9; 64]],
            message: Some(Message {
                header: Some(MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                }),
                account_keys: vec![vec![1; 32], vec![2; 32], vec![3; 32]],
                recent_blockhash: vec![0; 32],
                instructions: vec![CompiledInstruction {
                    program_id_index: 2,
                    accounts: vec![0, 1],
                    data: vec![],
                }],
                ..Default::default()
            }),
        }
    }
    #[test]
    fn permissions_apply_program_demotion_to_all_versions_and_keep_reserved_upper_bound() {
        let mut tx = transaction();
        for (versioned, config) in [
            (false, None),
            (true, None),
            (true, Some(TransactionConfig::default())),
        ] {
            let m = tx.message.as_mut().expect("message");
            m.versioned = versioned;
            m.config = config;
            assert_eq!(
                Permissions::classify(&[9; 64], &tx, &TransactionStatusMeta::default()),
                Permissions::Known(vec![[1; 32], [2; 32]])
            );
        }
        tx.message.as_mut().expect("message").account_keys[1] =
            solana_sdk_ids::bpf_loader_upgradeable::id()
                .to_bytes()
                .to_vec();
        let Permissions::Known(keys) =
            Permissions::classify(&[9; 64], &tx, &TransactionStatusMeta::default())
        else {
            panic!("known loader transaction");
        };
        assert!(keys.contains(&[3; 32]));
    }
    #[test]
    fn malformed_never_becomes_readonly() {
        let mut tx = transaction();
        tx.message.as_mut().expect("message").account_keys[1] = vec![1; 32];
        let p = Permissions::classify(&[9; 64], &tx, &TransactionStatusMeta::default());
        assert!(matches!(p, Permissions::Unknown(_)));
        assert!(p.affects(&HashSet::from([[8; 32]])));
    }
}
