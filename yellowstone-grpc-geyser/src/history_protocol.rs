//! Exact, opt-in tracked history wire primitives.
use {
    crate::{
        order_protocol::{self, sha256},
        plugin::filter::{
            message::{FilteredUpdate, FilteredUpdateOneof},
            name::FilterName,
        },
    },
    anyhow::{ensure, Result},
    prost::Message,
    prost_types::Timestamp,
    yellowstone_grpc_proto::prelude::*,
};
pub const CAPABILITY: &str = "tracked_account_history_v1";
pub fn capabilities() -> HistoryCapabilities {
    HistoryCapabilities {
        name: Some(CAPABILITY.into()),
        protocol_version: Some(1),
        commitment: Some(1),
        encoder_profile: Some("prost_tracked_history_v1".into()),
        bank_scoped: Some(true),
        zero_write_seals: Some(true),
        rollback_included: Some(true),
        null_writer_preserved: Some(true),
        legacy_projection_unchanged: Some(true),
        semantics: Some("committed_store_all_v1".into()),
    }
}
pub const fn limits(
    history: &crate::plugin::filter::limits::FilterLimitsHistory,
    queue: usize,
    control: usize,
) -> TrackedHistoryLimits {
    TrackedHistoryLimits {
        retained_bytes: Some(history.retained_bytes as u64),
        bank_bytes: Some(history.bank_bytes as u64),
        bank_versions: Some(history.bank_versions as u64),
        queue_bytes: Some(queue as u64),
        control_bytes: Some(control as u64),
    }
}
pub fn request_digest(r: &SubscribeRequestTrackedHistory) -> Vec<u8> {
    let mut r = r.clone();
    r.request_sha256.clear();
    sha256(&r.encode_to_vec())
}
pub const fn has_control(r: &SubscribeRequest) -> bool {
    r.tracked_account_history.is_some() || r.tracked_history_close.is_some()
}
pub fn validate_pair(r: &SubscribeRequest) -> Result<()> {
    let h = r
        .tracked_account_history
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing history registration"))?;
    let o = r
        .order_writes
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("history requires paired order registration"))?;
    ensure!(
        r.order_close.is_none() && r.tracked_history_close.is_none(),
        "mixed registration and close"
    );
    order_protocol::validate_request(o)?;
    crate::order_stream::validate_coverage(r, &o.tracked_keys)?;
    ensure!(
        h.protocol_version == Some(1)
            && h.request_version == o.request_version
            && h.previous_epoch == o.previous_epoch
            && h.tracked_keys == o.tracked_keys,
        "history/order scope or version mismatch"
    );
    ensure!(
        h.request_sha256.len() == 32
            && h.request_sha256 == request_digest(h)
            && h.order_request_sha256 == o.request_sha256,
        "history request digest mismatch"
    );
    Ok(())
}
pub fn wrap(oneof: subscribe_update::UpdateOneof, created_at: Timestamp) -> FilteredUpdate {
    let update = SubscribeUpdate {
        filters: vec![CAPABILITY.into()],
        update_oneof: Some(oneof),
        created_at: Some(created_at),
    };
    FilteredUpdate::new(
        [FilterName::new(CAPABILITY)].into_iter().collect(),
        FilteredUpdateOneof::Order(Box::new(update)),
        created_at,
    )
}
pub fn control(oneof: subscribe_update::UpdateOneof) -> FilteredUpdate {
    wrap(oneof, Timestamp::from(std::time::SystemTime::now()))
}
