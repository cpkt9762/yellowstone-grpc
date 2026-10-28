use {
    cargo_lock::Lockfile,
    sha2::{Digest, Sha256},
    std::collections::HashSet,
    tonic_prost_build::manual::{Builder, Method, Service},
};

fn main() -> anyhow::Result<()> {
    vergen::Emitter::default()
        .add_instructions(&vergen::BuildBuilder::all_build()?)?
        .add_instructions(&vergen::RustcBuilder::all_rustc()?)?
        .emit()?;

    // vergen git version does not looks cool
    println!(
        "cargo:rustc-env=GIT_VERSION={}",
        git_version::git_version!()
    );

    // Build metadata is derived from this exact worktree and schema.
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let output = std::process::Command::new("git").args(args).output()?;
        anyhow::ensure!(output.status.success(), "git metadata command failed");
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    let sha = git(&["rev-parse", "HEAD"])?;
    anyhow::ensure!(
        sha.len() == 40
            && sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid full git SHA"
    );
    println!("cargo:rustc-env=ORDER_GIT_SHA={sha}");
    let git_dir = git(&["rev-parse", "--absolute-git-dir"])?;
    println!("cargo:rerun-if-changed={git_dir}/HEAD");
    let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let head = std::fs::read_to_string(format!("{git_dir}/HEAD"))?;
    if let Some(reference) = head.trim().strip_prefix("ref: ") {
        println!("cargo:rerun-if-changed={common}/{reference}");
        println!("cargo:rerun-if-changed={common}/packed-refs");
    }
    let schemas = ["geyser.proto", "solana-storage.proto"];
    let mut entries = Vec::new();
    for name in schemas {
        let path = format!("../yellowstone-grpc-proto/proto/{name}");
        println!("cargo:rerun-if-changed={path}");
        let bytes = std::fs::read(&path)?;
        let hash = format!("{:x}", Sha256::digest(bytes));
        entries.push(format!("{{\"name\":\"{name}\",\"sha256\":\"{hash}\"}}"));
    }
    let canonical = format!("[{}]", entries.join(","));
    println!(
        "cargo:rustc-env=ORDER_PROTO_SHA256={:x}",
        Sha256::digest(canonical.as_bytes())
    );

    // Extract packages version
    let lockfile = Lockfile::load("../Cargo.lock")?;
    println!(
        "cargo:rustc-env=SOLANA_SDK_VERSION={}",
        // this is used to set the solana version number
        // this used to refer `solana-sdk`
        // since it was deprecated
        // this now referes `agave-geyser-plugin-interface`
        get_pkg_version(&lockfile, "solana-transaction-status")
    );
    println!(
        "cargo:rustc-env=YELLOWSTONE_GRPC_PROTO_VERSION={}",
        get_pkg_version(&lockfile, "yellowstone-grpc-proto")
    );

    // build tonic server with custom FilteredUpdate output type
    let geyser_service = Service::builder()
        .name("Geyser")
        .package("geyser")
        .method(
            Method::builder()
                .name("subscribe")
                .route_name("Subscribe")
                .input_type("crate::order_protocol::OrderSubscribeRequest")
                .output_type("crate::plugin::filter::message::FilteredUpdate")
                .codec_path("crate::order_protocol::OrderCodec")
                .client_streaming()
                .server_streaming()
                .build(),
        )
        .method(
            Method::builder()
                .name("subscribe_deshred")
                .route_name("SubscribeDeshred")
                .input_type("yellowstone_grpc_proto::geyser::SubscribeDeshredRequest")
                .output_type("crate::plugin::filter::message::FilteredUpdateDeshred")
                .codec_path("tonic_prost::ProstCodec")
                .client_streaming()
                .server_streaming()
                .build(),
        )
        .method(
            Method::builder()
                .name("subscribe_gossip")
                .route_name("SubscribeGossip")
                .input_type("yellowstone_grpc_proto::geyser::SubscribeGossipRequest")
                .output_type("yellowstone_grpc_proto::geyser::SubscribeUpdateGossip")
                .codec_path("tonic_prost::ProstCodec")
                .server_streaming()
                .build(),
        )
        .method(
            Method::builder()
                .name("subscribe_first_available_slot")
                .route_name("SubscribeReplayInfo")
                .input_type("yellowstone_grpc_proto::geyser::SubscribeReplayInfoRequest")
                .output_type("yellowstone_grpc_proto::geyser::SubscribeReplayInfoResponse")
                .codec_path("tonic_prost::ProstCodec")
                .build(),
        )
        .method(
            Method::builder()
                .name("ping")
                .route_name("Ping")
                .input_type("yellowstone_grpc_proto::geyser::PingRequest")
                .output_type("yellowstone_grpc_proto::geyser::PongResponse")
                .codec_path("tonic_prost::ProstCodec")
                .build(),
        )
        .method(
            Method::builder()
                .name("get_latest_blockhash")
                .route_name("GetLatestBlockhash")
                .input_type("yellowstone_grpc_proto::geyser::GetLatestBlockhashRequest")
                .output_type("yellowstone_grpc_proto::geyser::GetLatestBlockhashResponse")
                .codec_path("tonic_prost::ProstCodec")
                .build(),
        )
        .method(
            Method::builder()
                .name("get_block_height")
                .route_name("GetBlockHeight")
                .input_type("yellowstone_grpc_proto::geyser::GetBlockHeightRequest")
                .output_type("yellowstone_grpc_proto::geyser::GetBlockHeightResponse")
                .codec_path("tonic_prost::ProstCodec")
                .build(),
        )
        .method(
            Method::builder()
                .name("get_slot")
                .route_name("GetSlot")
                .input_type("yellowstone_grpc_proto::geyser::GetSlotRequest")
                .output_type("yellowstone_grpc_proto::geyser::GetSlotResponse")
                .codec_path("tonic_prost::ProstCodec")
                .build(),
        )
        .method(
            Method::builder()
                .name("is_blockhash_valid")
                .route_name("IsBlockhashValid")
                .input_type("yellowstone_grpc_proto::geyser::IsBlockhashValidRequest")
                .output_type("yellowstone_grpc_proto::geyser::IsBlockhashValidResponse")
                .codec_path("tonic_prost::ProstCodec")
                .build(),
        )
        .method(
            Method::builder()
                .name("get_version")
                .route_name("GetVersion")
                .input_type("yellowstone_grpc_proto::geyser::GetVersionRequest")
                .output_type("yellowstone_grpc_proto::geyser::GetVersionResponse")
                .codec_path("tonic_prost::ProstCodec")
                .build(),
        )
        .build();
    Builder::new()
        .build_client(false)
        .compile(&[geyser_service]);

    Ok(())
}

fn get_pkg_version(lockfile: &Lockfile, pkg_name: &str) -> String {
    lockfile
        .packages
        .iter()
        .filter(|pkg| pkg.name.as_str() == pkg_name)
        .map(|pkg| pkg.version.to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",")
}
