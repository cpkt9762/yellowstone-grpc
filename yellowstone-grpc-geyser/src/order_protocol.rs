//! Frozen order_writes_v1 wire primitives. Never used to re-encode legacy traffic.
use {
    crate::plugin::filter::{
        message::{FilteredUpdate, FilteredUpdateOneof},
        name::FilterName,
    },
    anyhow::{ensure, Result},
    prost::Message,
    prost_types::Timestamp,
    sha2::{Digest, Sha256},
    std::time::SystemTime,
    yellowstone_grpc_proto::prelude::*,
};

pub const CAPABILITY: &str = "order_writes_v1";
pub const MAX_TRACKED_KEYS: usize = 1800;

pub fn bounded_detail(value: &str) -> String {
    let mut end = value.len().min(1024);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        "B protocol error".into()
    } else {
        value[..end].to_owned()
    }
}

pub fn sha256(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

/// Incremental length-framed digest; retains no raw history.
#[derive(Clone, Default)]
pub struct OrderedDigest(Sha256);
impl OrderedDigest {
    pub fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.update(u64::try_from(bytes.len())?.to_le_bytes());
        self.0.update(bytes);
        Ok(())
    }
    pub fn finish(&self) -> Vec<u8> {
        self.0.clone().finalize().to_vec()
    }
}

pub fn capabilities() -> OrderCapabilities {
    OrderCapabilities {
        name: Some(CAPABILITY.into()),
        protocol_version: Some(1),
        permission_semantics: Some("hb_upper_v2".into()),
        commitment: Some(1),
        encoder_profile: Some("prost_order_v1".into()),
        bank_scoped: Some(true),
        zero_write_seals: Some(true),
        full_target_unchanged: Some(true),
    }
}

pub fn request_digest(request: &SubscribeRequestOrderWrites) -> Vec<u8> {
    let mut canonical = request.clone();
    canonical.request_sha256.clear();
    sha256(&canonical.encode_to_vec())
}

pub fn keys_digest(keys: &[Vec<u8>]) -> Result<Vec<u8>> {
    validate_keys(keys)?;
    let mut hash = Sha256::new();
    hash.update(u64::try_from(keys.len())?.to_le_bytes());
    for key in keys {
        hash.update(key);
    }
    Ok(hash.finalize().to_vec())
}

pub fn validate_keys(keys: &[Vec<u8>]) -> Result<()> {
    ensure!(
        !keys.is_empty() && keys.len() <= MAX_TRACKED_KEYS,
        "tracked key count outside 1..=1800"
    );
    ensure!(
        keys.iter().all(|key| key.len() == 32),
        "invalid tracked key length"
    );
    ensure!(
        keys.windows(2).all(|pair| pair[0] < pair[1]),
        "keys must be sorted by bytes and unique"
    );
    Ok(())
}

pub fn validate_request(request: &SubscribeRequestOrderWrites) -> Result<()> {
    ensure!(
        request.protocol_version == Some(1),
        "unsupported source protocol version"
    );
    ensure!(
        request.request_version.is_some_and(|v| v > 0),
        "missing request version"
    );
    if let Some(epoch) = &request.previous_epoch {
        validate_uuid(epoch)?;
    }
    validate_keys(&request.tracked_keys)?;
    ensure!(
        request.request_sha256.len() == 32 && request.request_sha256 == request_digest(request),
        "request digest mismatch"
    );
    Ok(())
}

pub fn validate_uuid(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() == 16 && bytes[6] >> 4 == 4 && bytes[8] >> 6 == 2,
        "expected UUID v4"
    );
    Ok(())
}

pub fn validate_context(context: &OrderContext) -> Result<()> {
    ensure!(
        context.protocol_version == Some(1),
        "unsupported context protocol"
    );
    validate_uuid(&context.source_stream_id)?;
    validate_uuid(&context.source_epoch)?;
    ensure!(
        context.request_version.is_some_and(|v| v > 0),
        "missing context request version"
    );
    Ok(())
}

/// Reject unknown fields, duplicates and noncanonical varints before prost loses them.
pub fn decode_request(bytes: &[u8]) -> Result<SubscribeRequestOrderWrites> {
    let request = SubscribeRequestOrderWrites::decode(bytes)?;
    ensure!(
        request.encode_to_vec() == bytes,
        "noncanonical order request encoding"
    );
    validate_request(&request)?;
    Ok(request)
}

pub fn wrap(message: subscribe_update::UpdateOneof, created_at: Timestamp) -> FilteredUpdate {
    let update = SubscribeUpdate {
        filters: vec![CAPABILITY.into()],
        update_oneof: Some(message),
        created_at: Some(created_at),
    };
    FilteredUpdate::new(
        [FilterName::new(CAPABILITY)].into_iter().collect(),
        FilteredUpdateOneof::Order(Box::new(update)),
        created_at,
    )
}

pub fn control(message: subscribe_update::UpdateOneof) -> FilteredUpdate {
    wrap(message, Timestamp::from(SystemTime::now()))
}

#[cfg(test)]
mod order_writes_v1 {
    use super::*;
    #[test]
    fn canonical_request_rejects_presence_versions_duplicates_and_unknown() {
        let mut request = SubscribeRequestOrderWrites {
            protocol_version: Some(1),
            request_version: Some(1),
            tracked_keys: vec![vec![1; 32]],
            ..Default::default()
        };
        request.request_sha256 = request_digest(&request);
        let raw = request.encode_to_vec();
        assert_eq!(decode_request(&raw).expect("canonical request"), request);
        for suffix in [vec![8, 1], vec![48, 0]] {
            let mut bad = raw.clone();
            bad.extend(suffix);
            assert!(decode_request(&bad).is_err());
        }
        request.protocol_version = Some(2);
        assert!(validate_request(&request).is_err());
    }
    #[test]
    fn generated_envelope_is_custom_encoder_and_length() {
        let update = control(subscribe_update::UpdateOneof::OrderGap(
            SubscribeUpdateOrderGap {
                next_source_seq: Some(0),
                reason: Some(6),
                detail: "test".into(),
                ..Default::default()
            },
        ));
        let raw = update.encode_to_vec();
        assert_eq!(raw, update.as_subscribe_update().encode_to_vec());
        assert_eq!(raw.len(), update.encoded_len());
        assert_eq!(OrderedDigest::default().finish(), sha256(&[]));
    }
}

/// Preserve B wire errors as data so a separately valid A request remains usable.
#[derive(Debug)]
pub struct OrderSubscribeRequest {
    pub request: SubscribeRequest,
    pub order_wire_error: Option<String>,
}

pub fn decode_subscription(raw: &[u8]) -> Result<OrderSubscribeRequest> {
    use bytes::Buf;
    use prost::encoding::{decode_key, decode_varint, skip_field, DecodeContext, WireType};
    let request = SubscribeRequest::decode(raw)?;
    let checked = (|| -> Result<()> {
        if request.order_writes.is_none() && request.order_close.is_none() {
            return Ok(());
        }
        let mut input = raw;
        let mut singular = std::collections::HashSet::new();
        while input.has_remaining() {
            let (tag, wire) = decode_key(&mut input)?;
            ensure!(
                (1..=12).contains(&tag) || tag == 1002 || tag == 1003,
                "unknown subscription field"
            );
            if matches!(tag, 6 | 9 | 11 | 1002 | 1003) {
                ensure!(singular.insert(tag), "duplicate subscription singular");
            }
            if tag == 1002 || tag == 1003 {
                ensure!(
                    wire == WireType::LengthDelimited,
                    "invalid B control wire type"
                );
                let len = usize::try_from(decode_varint(&mut input)?)?;
                ensure!(len <= input.len(), "truncated B control");
                let bytes = &input[..len];
                input.advance(len);
                if tag == 1002 {
                    let decoded = SubscribeRequestOrderWrites::decode(bytes)?;
                    ensure!(
                        decoded.encode_to_vec() == bytes,
                        "noncanonical order request encoding"
                    );
                } else {
                    let close = SubscribeRequestOrderClose::decode(bytes)?;
                    ensure!(
                        close.encode_to_vec() == bytes,
                        "noncanonical close encoding"
                    );
                    validate_context(
                        close
                            .context
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("missing close context"))?,
                    )?;
                    ensure!(
                        close.tracked_keys_sha256.len() == 32,
                        "invalid close key hash"
                    );
                }
            } else {
                skip_field(wire, tag, &mut input, DecodeContext::default())?;
            }
        }
        Ok(())
    })();
    Ok(OrderSubscribeRequest {
        request,
        order_wire_error: checked.err().map(|e| e.to_string()),
    })
}

#[derive(Default)]
pub struct OrderCodec {
    _private: (),
}
#[derive(Default)]
pub struct OrderEncoder;
#[derive(Default)]
pub struct OrderDecoder;
impl tonic::codec::Codec for OrderCodec {
    type Encode = FilteredUpdate;
    type Decode = OrderSubscribeRequest;
    type Encoder = OrderEncoder;
    type Decoder = OrderDecoder;
    fn encoder(&mut self) -> Self::Encoder {
        OrderEncoder
    }
    fn decoder(&mut self) -> Self::Decoder {
        OrderDecoder
    }
}
impl tonic::codec::Encoder for OrderEncoder {
    type Item = FilteredUpdate;
    type Error = tonic::Status;
    fn encode(
        &mut self,
        item: Self::Item,
        buf: &mut tonic::codec::EncodeBuf<'_>,
    ) -> std::result::Result<(), Self::Error> {
        item.encode(buf)
            .map_err(|e| tonic::Status::internal(e.to_string()))
    }
}
impl tonic::codec::Decoder for OrderDecoder {
    type Item = OrderSubscribeRequest;
    type Error = tonic::Status;
    fn decode(
        &mut self,
        buf: &mut tonic::codec::DecodeBuf<'_>,
    ) -> std::result::Result<Option<Self::Item>, Self::Error> {
        use bytes::Buf;
        let raw = buf.copy_to_bytes(buf.remaining());
        decode_subscription(&raw)
            .map(Some)
            .map_err(|e| tonic::Status::internal(e.to_string()))
    }
}
