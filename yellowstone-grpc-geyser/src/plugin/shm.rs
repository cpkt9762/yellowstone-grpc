use {
    solana_shm_accounts::{error::ShmError, writer::ShmWriter},
    std::collections::HashSet,
};

pub struct ShmSubscriptionState {
    pub accounts: HashSet<[u8; 32]>,
    pub writer: Option<ShmWriter>,
}

impl std::fmt::Debug for ShmSubscriptionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmSubscriptionState")
            .field("accounts", &self.accounts)
            .field("writer", &self.writer.is_some())
            .finish()
    }
}

impl ShmSubscriptionState {
    pub fn empty() -> Self {
        Self {
            accounts: HashSet::new(),
            writer: None,
        }
    }
}

impl Default for ShmSubscriptionState {
    fn default() -> Self {
        Self::empty()
    }
}

pub fn validate_subscription(
    writer: &ShmWriter,
    pubkeys: &[[u8; 32]],
) -> Result<(), Vec<[u8; 32]>> {
    let missing: Vec<[u8; 32]> = pubkeys
        .iter()
        .filter(|pk| writer.lookup(pk).is_none())
        .copied()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

pub fn open_and_validate_shm(shm_path: &str, pubkeys: &[[u8; 32]]) -> Result<ShmWriter, ShmError> {
    let writer = ShmWriter::open(shm_path)?;
    let missing: Vec<[u8; 32]> = pubkeys
        .iter()
        .filter(|pk| writer.lookup(pk).is_none())
        .copied()
        .collect();
    if !missing.is_empty() {
        for pk in &missing {
            log::warn!("pubkey not found in shm offset table: {:?}", pk);
        }
    }
    Ok(writer)
}
