use {
    anyhow::Context,
    clap::Parser,
    futures::{sink::SinkExt, stream::StreamExt},
    log::{error, info},
    serde_json::json,
    solana_account::Account,
    solana_account_decoder::{encode_ui_account, UiAccountEncoding},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_transaction_status::UiTransactionEncoding,
    std::{collections::HashMap, env, path::PathBuf, str::FromStr},
    tokio::fs,
    yellowstone_grpc_client::GeyserGrpcClient,
    yellowstone_grpc_geyser::plugin::convert_from,
    yellowstone_grpc_proto::prelude::{
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
        SubscribeRequestFilterTransactions, SubscribeRequestPing,
    },
};

#[derive(Debug, Clone, Parser)]
#[clap(
    author,
    version,
    about = "Dump a single transaction with pre-account data via gRPC"
)]
struct Args {
    #[clap(short, long, default_value_t = String::from("http://127.0.0.1:10000"))]
    endpoint: String,

    #[clap(long)]
    x_token: Option<String>,

    #[clap(long)]
    program_id: String,

    #[clap(long, default_value = "./dump")]
    output_dir: PathBuf,

    #[clap(long, default_value = "processed")]
    commitment: String,

    #[clap(long, default_value_t = false)]
    include_vote: bool,

    /// Only capture successful transactions (skip failed ones)
    #[clap(long, default_value_t = false)]
    success_only: bool,

    #[clap(long, default_value_t = false)]
    include_pre_accounts: bool,

    #[clap(long, default_value_t = 1024 * 1024 * 1024)]
    max_decoding_message_size: usize,
}

fn parse_commitment(s: &str) -> anyhow::Result<CommitmentLevel> {
    match s.to_lowercase().as_str() {
        "processed" => Ok(CommitmentLevel::Processed),
        "confirmed" => Ok(CommitmentLevel::Confirmed),
        "finalized" => Ok(CommitmentLevel::Finalized),
        _ => anyhow::bail!("invalid commitment: {s}"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env::set_var(
        env_logger::DEFAULT_FILTER_ENV,
        env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info".into()),
    );
    env_logger::init();

    let args = Args::parse();
    let commitment = parse_commitment(&args.commitment)?;
    let _ = Pubkey::from_str(&args.program_id).context("invalid --program-id")?;

    info!("Connecting to {}", args.endpoint);
    let mut client = GeyserGrpcClient::build_from_shared(args.endpoint.clone())?
        .x_token(args.x_token.clone())?
        .max_decoding_message_size(args.max_decoding_message_size)
        .connect()
        .await
        .context("failed to connect")?;

    match client.get_version().await {
        Ok(v) => info!("Server version: {:?}", v),
        Err(e) => info!("get_version failed (non-fatal): {e}"),
    }

    let include_pre = if args.include_pre_accounts {
        Some(true)
    } else {
        None
    };
    info!(
        "Subscribing: program={} commitment={} include_pre_accounts={:?}",
        args.program_id, args.commitment, include_pre
    );

    let mut transactions = HashMap::new();
    transactions.insert(
        "dump".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(args.include_vote),
            failed: None,
            signature: None,
            account_include: vec![args.program_id.clone()],
            account_exclude: vec![],
            account_required: vec![],
            include_pre_accounts: include_pre,
        },
    );

    let request = SubscribeRequest {
        transactions,
        commitment: Some(commitment as i32),
        ..Default::default()
    };

    let (mut subscribe_tx, mut stream) = client.subscribe_with_request(Some(request)).await?;
    info!("Stream opened, waiting for first matching transaction...");

    while let Some(message) = stream.next().await {
        match message {
            Ok(msg) => match msg.update_oneof {
                Some(UpdateOneof::Transaction(tx_update)) => {
                    let slot = tx_update.slot;
                    let tx_info = tx_update
                        .transaction
                        .ok_or_else(|| anyhow::anyhow!("no transaction in update"))?;

                    let signature = Signature::try_from(tx_info.signature.as_slice())
                        .context("invalid signature")?;
                    let sig_str = signature.to_string();

                    if args.success_only {
                        let is_err = tx_info
                            .meta
                            .as_ref()
                            .map(|m| m.err.is_some())
                            .unwrap_or(false);
                        if is_err {
                            info!("[slot={slot}] skipped (failed): {sig_str}");
                            continue;
                        }
                    }

                    info!("[slot={slot}] captured: {sig_str}");

                    let pre_accounts = tx_info
                        .meta
                        .as_ref()
                        .map(|m| m.pre_accounts_data.clone())
                        .unwrap_or_default();

                    let tx_dir = args.output_dir.join(&sig_str);
                    let accounts_dir = tx_dir.join("accounts");
                    fs::create_dir_all(&accounts_dir)
                        .await
                        .context("create output dirs")?;

                    let mut saved = 0u32;
                    let mut skipped = 0u32;
                    for pa in &pre_accounts {
                        if pa.executable {
                            skipped += 1;
                            continue;
                        }
                        let pubkey =
                            Pubkey::try_from(pa.pubkey.as_slice()).context("bad pubkey")?;
                        let owner = Pubkey::try_from(pa.owner.as_slice()).context("bad owner")?;

                        let account = Account {
                            lamports: pa.lamports,
                            data: pa.data.clone(),
                            owner,
                            executable: pa.executable,
                            rent_epoch: pa.rent_epoch,
                        };

                        let ui_account = encode_ui_account(
                            &pubkey,
                            &account,
                            UiAccountEncoding::Base64,
                            None,
                            None,
                        );
                        let keyed = json!({
                            "pubkey": pubkey.to_string(),
                            "account": ui_account,
                        });

                        let path = accounts_dir.join(format!("{pubkey}.json"));
                        fs::write(&path, serde_json::to_string_pretty(&keyed)?)
                            .await
                            .with_context(|| format!("write account {pubkey}"))?;
                        saved += 1;
                        info!("  account: {pubkey} ({} bytes)", pa.data.len());
                    }

                    let tx_clone = tx_info.clone();
                    let tx_with_meta = convert_from::create_tx_with_meta(tx_info)
                        .map_err(|e| anyhow::anyhow!("{e}"))
                        .context("convert tx")?;
                    let encoded_base64 = tx_with_meta
                        .encode(UiTransactionEncoding::Base64, Some(u8::MAX), true)
                        .context("encode base64")?;
                    fs::write(
                        tx_dir.join("tx_base64.json"),
                        serde_json::to_string_pretty(&encoded_base64)?,
                    )
                    .await
                    .context("write tx_base64.json")?;
                    info!("  saved tx_base64.json");

                    let tx_with_meta2 = convert_from::create_tx_with_meta(tx_clone)
                        .map_err(|e| anyhow::anyhow!("{e}"))
                        .context("convert tx for parsed")?;
                    let encoded_parsed = tx_with_meta2
                        .encode(UiTransactionEncoding::JsonParsed, Some(u8::MAX), true)
                        .context("encode jsonParsed")?;
                    fs::write(
                        tx_dir.join("tx_parsed.json"),
                        serde_json::to_string_pretty(&encoded_parsed)?,
                    )
                    .await
                    .context("write tx_parsed.json")?;
                    info!("  saved tx_parsed.json");

                    info!("=== Dump Complete ===");
                    info!("  signature : {sig_str}");
                    info!("  slot      : {slot}");
                    info!("  output    : {}", tx_dir.display());
                    info!("  accounts  : {saved} saved, {skipped} executable skipped");
                    info!("  tx files  : tx_base64.json, tx_parsed.json");

                    return Ok(());
                }
                Some(UpdateOneof::Ping(_)) => {
                    subscribe_tx
                        .send(SubscribeRequest {
                            ping: Some(SubscribeRequestPing { id: 1 }),
                            ..Default::default()
                        })
                        .await?;
                }
                Some(UpdateOneof::Pong(_)) => {}
                Some(UpdateOneof::Slot(_)) => {}
                Some(other) => {
                    info!("ignoring update: {}", update_type_name(&other));
                }
                None => {
                    error!("empty update_oneof");
                    break;
                }
            },
            Err(error) => {
                error!("stream error: {error:?}");
                break;
            }
        }
    }

    anyhow::bail!("stream closed before receiving a transaction")
}

fn update_type_name(update: &UpdateOneof) -> &'static str {
    match update {
        UpdateOneof::Account(_) => "account",
        UpdateOneof::Slot(_) => "slot",
        UpdateOneof::Transaction(_) => "transaction",
        UpdateOneof::TransactionStatus(_) => "transaction_status",
        UpdateOneof::Block(_) => "block",
        UpdateOneof::BlockMeta(_) => "block_meta",
        UpdateOneof::Entry(_) => "entry",
        UpdateOneof::Ping(_) => "ping",
        UpdateOneof::Pong(_) => "pong",
    }
}
