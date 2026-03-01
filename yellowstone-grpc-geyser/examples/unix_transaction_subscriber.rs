use {
    anyhow::{Context, Result},
    bs58,
    futures::stream::StreamExt,
    hyper_util::rt::TokioIo,
    log::{error, info, warn},
    solana_pubkey::Pubkey,
    std::{collections::HashMap, env, str::FromStr, time::Duration},
    tokio::{net::UnixStream, time::timeout},
    tokio_stream,
    tonic::{
        transport::{Channel, Endpoint, Uri},
        Request,
    },
    tower::service_fn,
    yellowstone_grpc_proto::{
        convert_from,
        prelude::{
            geyser_client::GeyserClient, subscribe_update::UpdateOneof, CommitmentLevel,
            PingRequest, SubscribeRequest, SubscribeRequestFilterTransactions,
            SubscribeUpdateTransactionInfo,
        },
    },
};

const TARGET_ACCOUNT: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <unix_socket_path>", args[0]);
        eprintln!("Example: {} /tmp/yellowstone-grpc.sock", args[0]);
        eprintln!();
        eprintln!("This client subscribes to transactions involving account:");
        eprintln!("  {}", TARGET_ACCOUNT);
        std::process::exit(1);
    }

    let unix_socket_path = &args[1];
    info!("🚀 Starting Transaction Subscriber via Unix Socket");
    info!("📡 Socket path: {}", unix_socket_path);
    info!("🎯 Target account: {}", TARGET_ACCOUNT);

    // Validate the target account address
    let target_pubkey =
        Pubkey::from_str(TARGET_ACCOUNT).context("Invalid target account address")?;
    info!("✅ Target account validated: {}", target_pubkey);

    // Create gRPC channel over Unix socket
    let channel = create_unix_channel(unix_socket_path).await?;
    let mut client = GeyserClient::new(channel);

    info!("🔍 Testing connection...");
    test_ping(&mut client).await?;

    info!("📊 Starting transaction subscription...");
    subscribe_to_transactions(&mut client, &target_pubkey).await?;

    Ok(())
}

async fn create_unix_channel(unix_socket_path: &str) -> Result<Channel> {
    let uri = Uri::from_static("http://[::]:50051");
    let path = unix_socket_path.to_string();

    let channel = Endpoint::from(uri)
        .connect_with_connector(service_fn(move |_: Uri| {
            let path = path.clone();
            async move {
                let stream = UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await
        .context("Failed to connect to Unix socket")?;

    Ok(channel)
}

async fn test_ping(client: &mut GeyserClient<Channel>) -> Result<()> {
    let request = Request::new(PingRequest { count: 1 });

    match timeout(Duration::from_secs(5), client.ping(request)).await {
        Ok(response) => {
            let pong = response?.into_inner();
            info!("✅ Connection successful! Ping count: {}", pong.count);
            Ok(())
        }
        Err(_) => {
            anyhow::bail!("❌ Connection failed: ping request timed out");
        }
    }
}

async fn subscribe_to_transactions(
    client: &mut GeyserClient<Channel>,
    target_account: &Pubkey,
) -> Result<()> {
    info!("🔧 Setting up transaction subscription filter...");

    // Create transaction filter for the specific account
    let mut transactions_filter = HashMap::new();
    transactions_filter.insert(
        "target_account_txs".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),                                  // Exclude vote transactions
            failed: Some(false),                                // Exclude failed transactions
            signature: None,                                    // No specific signature filter
            account_include: vec![target_account.to_string()],  // Include our target account
            account_exclude: vec![],                            // No accounts to exclude
            account_required: vec![target_account.to_string()], // Require our target account
            account_data_required: vec![],
            max_messages: None,
            account_data_source: None,
            account_data_owner: None,
        },
    );

    let subscribe_request = SubscribeRequest {
        slots: HashMap::new(),
        accounts: HashMap::new(),
        transactions: transactions_filter, // Our transaction filter
        transactions_status: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta: HashMap::new(),
        entry: HashMap::new(),
        commitment: Some(CommitmentLevel::Confirmed as i32), // Use confirmed commitment
        accounts_data_slice: vec![],
        ping: None,
        from_slot: None,
    };

    info!("📡 Sending subscription request...");
    let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
    request_tx.send(subscribe_request)?;

    let request_stream = tokio_stream::wrappers::UnboundedReceiverStream::new(request_rx);
    let response = client.subscribe(request_stream).await?;
    let mut stream = response.into_inner();

    info!("🎉 Subscription established! Waiting for transactions...");
    info!(
        "📋 Filter: account_include=[{}], vote=false, failed=false",
        target_account
    );

    let mut transaction_count = 0;
    let start_time = std::time::Instant::now();

    while let Some(message) = stream.next().await {
        match message {
            Ok(update) => {
                if let Some(update_oneof) = update.update_oneof {
                    match update_oneof {
                        UpdateOneof::Transaction(transaction_update) => {
                            if let Some(transaction_info) = transaction_update.transaction {
                                transaction_count += 1;
                                handle_transaction_update(
                                    transaction_info,
                                    transaction_count,
                                    transaction_update.slot,
                                )
                                .await?;
                            }
                        }
                        UpdateOneof::Ping(_) => {
                            let elapsed = start_time.elapsed();
                            info!(
                                "💓 Ping received (uptime: {:.1}s, txs: {})",
                                elapsed.as_secs_f64(),
                                transaction_count
                            );
                        }
                        other => {
                            warn!(
                                "🤔 Received unexpected update type: {:?}",
                                std::mem::discriminant(&other)
                            );
                        }
                    }
                }
            }
            Err(e) => {
                error!("❌ Stream error: {}", e);
                anyhow::bail!("Subscription stream failed: {}", e);
            }
        }
    }

    info!(
        "📊 Subscription ended. Total transactions processed: {}",
        transaction_count
    );
    Ok(())
}

async fn handle_transaction_update(
    transaction_info: SubscribeUpdateTransactionInfo,
    count: u64,
    slot: u64,
) -> Result<()> {
    // Extract basic transaction information
    let signature_bytes = &transaction_info.signature;
    let signature_str = bs58::encode(signature_bytes).into_string();

    let is_vote = transaction_info.is_vote;

    info!("🔥 Transaction #{}: {}", count, signature_str);
    info!("   📍 Slot: {}", slot);
    info!("   🗳️  Vote: {}", is_vote);
    info!("   📊 Index: {}", transaction_info.index);

    // Display basic transaction information
    info!("   ✅ Successfully received transaction involving target account");

    // Try to get additional details if available
    if log::log_enabled!(log::Level::Debug) {
        match convert_from::create_tx_with_meta(transaction_info.clone()) {
            Ok(_tx_with_meta) => {
                info!("   📄 Transaction details parsed successfully");
            }
            Err(e) => {
                warn!("   ⚠️  Failed to parse transaction details: {}", e);
            }
        }
    }

    println!(""); // Add spacing between transactions
    Ok(())
}
