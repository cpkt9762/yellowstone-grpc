use {serde::Serialize, std::env};

#[derive(Debug, Serialize)]
pub struct Version {
    pub package: &'static str,
    pub version: &'static str,
    pub proto: &'static str,
    pub solana: &'static str,
    pub git_sha: &'static str,
    pub proto_schema_sha256: &'static str,
    pub capabilities: [OrderCapabilityJson; 1],
    pub git: &'static str,
    pub rustc: &'static str,
    pub buildts: &'static str,
}

pub const VERSION: Version = Version {
    package: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    proto: env!("YELLOWSTONE_GRPC_PROTO_VERSION"),
    solana: env!("SOLANA_SDK_VERSION"),
    git_sha: env!("ORDER_GIT_SHA"),
    proto_schema_sha256: env!("ORDER_PROTO_SHA256"),
    capabilities: [ORDER_CAPABILITY],
    git: env!("GIT_VERSION"),
    rustc: env!("VERGEN_RUSTC_SEMVER"),
    buildts: env!("VERGEN_BUILD_TIMESTAMP"),
};

#[derive(Debug, Serialize)]
pub struct GrpcVersionInfoExtra {
    hostname: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GrpcVersionInfo {
    version: Version,
    extra: GrpcVersionInfoExtra,
}

impl Default for GrpcVersionInfo {
    fn default() -> Self {
        Self {
            version: VERSION,
            extra: GrpcVersionInfoExtra {
                hostname: hostname::get()
                    .ok()
                    .and_then(|name| name.into_string().ok()),
            },
        }
    }
}

// JSON mirrors every field of the wire capability, under version.capabilities.
#[derive(Debug, Serialize)]
pub struct OrderCapabilityJson {
    name: &'static str,
    protocol_version: u32,
    permission_semantics: &'static str,
    commitment: i32,
    encoder_profile: &'static str,
    bank_scoped: bool,
    zero_write_seals: bool,
    full_target_unchanged: bool,
}
pub const ORDER_CAPABILITY: OrderCapabilityJson = OrderCapabilityJson {
    name: "order_writes_v1",
    protocol_version: 1,
    permission_semantics: "hb_upper_v2",
    commitment: 1,
    encoder_profile: "prost_order_v1",
    bank_scoped: true,
    zero_write_seals: true,
    full_target_unchanged: true,
};

#[cfg(test)]
mod tracked_account_history_v1 {
    #[test]
    fn advertises_exact_history_capability() {
        let value = serde_json::to_value(&super::VERSION).expect("version JSON");
        assert_eq!(value["history_capabilities"][0]["name"], "tracked_account_history_v1");
        assert_eq!(value["history_capabilities"][0]["semantics"], "committed_store_all_v1");
        assert_eq!(value["history_capabilities"][0]["legacy_projection_unchanged"], true);
    }
}
