#![allow(clippy::large_enum_variant)]

pub mod cuckoo;

pub mod geyser {
    #![allow(clippy::clone_on_ref_ptr)]
    #![allow(clippy::missing_const_for_fn)]

    #[cfg(feature = "tonic")]
    include!(concat!(env!("OUT_DIR"), "/geyser.rs"));
    #[cfg(not(feature = "tonic"))]
    include!(concat!(env!("OUT_DIR"), "/no-tonic/geyser.rs"));
}

pub mod solana {
    #![allow(clippy::missing_const_for_fn)]

    pub mod storage {
        pub mod confirmed_block {
            #[cfg(feature = "tonic")]
            include!(concat!(
                env!("OUT_DIR"),
                "/solana.storage.confirmed_block.rs"
            ));
            #[cfg(not(feature = "tonic"))]
            include!(concat!(
                env!("OUT_DIR"),
                "/no-tonic/solana.storage.confirmed_block.rs"
            ));
        }
    }
}

pub mod prelude {
    pub use super::{geyser::*, solana::storage::confirmed_block::*};
}

#[cfg(feature = "tonic")]
pub use tonic;
pub use {prost, prost_types};

#[cfg(test)]
mod owner_reject_tests {
    use {super::prelude::*, prost::Message};

    #[test]
    fn owner_reject_proto_roundtrip() {
        for include_owner_rejected in [None, Some(false), Some(true)] {
            let request = SubscribeRequestFilterAccounts {
                account: vec!["11111111111111111111111111111111".to_owned()],
                include_owner_rejected,
                ..Default::default()
            };
            assert_eq!(
                SubscribeRequestFilterAccounts::decode(request.encode_to_vec().as_slice())
                    .expect("decode owner reject opt-in"),
                request
            );
        }
        let update = SubscribeUpdate {
            update_oneof: Some(subscribe_update::UpdateOneof::OwnerRejectExemption(
                SubscribeUpdateOwnerRejectExemption {
                    accounts: vec![OwnerRejectExemptAccount {
                        pubkey: "11111111111111111111111111111111".to_owned(),
                        active_from_slot: 42,
                    }],
                    owner_reject: vec!["11111111111111111111111111111111".to_owned()],
                },
            )),
            ..Default::default()
        };
        assert_eq!(
            SubscribeUpdate::decode(update.encode_to_vec().as_slice())
                .expect("decode owner reject acknowledgement"),
            update
        );
    }
}
