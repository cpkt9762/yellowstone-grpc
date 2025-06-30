use {
    anyhow::Result,
    base64::{engine::general_purpose, Engine as _},
    futures::stream::StreamExt,
    log::info,
    std::{collections::HashMap, env, time::Duration},
    tokio::{net::UnixStream, time::timeout},
    tonic::{
        transport::{Channel, Endpoint, Uri},
        Request,
    },
    tower::service_fn,
    hyper_util::rt::TokioIo,
    yellowstone_grpc_proto::prelude::{
        geyser_client::GeyserClient, PingRequest,
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
        SubscribeRequestFilterAccounts,
    },
};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <unix_socket_path>", args[0]);
        eprintln!("Example: {} /tmp/yellowstone-grpc.sock", args[0]);
        std::process::exit(1);
    }

    let unix_socket_path = &args[1];
    info!("Testing gRPC over Unix socket: {}", unix_socket_path);

    // Create gRPC channel over Unix socket
    let channel = create_unix_channel(unix_socket_path).await?;
    let mut client = GeyserClient::new(channel);

    info!("Testing ping...");
    test_ping(&mut client).await?;

    info!("Testing account subscription...");
    test_account_subscription(&mut client).await?;

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
        .await?;

    Ok(channel)
}

async fn test_ping(client: &mut GeyserClient<Channel>) -> Result<()> {
    let request = Request::new(PingRequest { count: 1 });

    match timeout(Duration::from_secs(5), client.ping(request)).await {
        Ok(response) => {
            let pong = response?.into_inner();
            info!("✅ Ping successful! Count: {}", pong.count);
            Ok(())
        }
        Err(_) => {
            anyhow::bail!("❌ Ping request timed out");
        }
    }
}

async fn test_account_subscription(client: &mut GeyserClient<Channel>) -> Result<()> {
    info!("Setting up account subscription...");

    // Subscribe to all accounts (for testing - in production you'd want to filter)
    let mut accounts_filter = HashMap::new();
    accounts_filter.insert("all_accounts".to_string(), SubscribeRequestFilterAccounts {
        account: vec![], // Empty = subscribe to all accounts
        owner: vec![],   // Empty = any owner
        filters: vec![], // No additional filters
        nonempty_txn_signature: None,
    });

    let subscribe_request = SubscribeRequest {
        slots: HashMap::new(),
        accounts: accounts_filter,
        transactions: HashMap::new(),
        transactions_status: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta: HashMap::new(),
        entry: HashMap::new(),
        commitment: Some(CommitmentLevel::Processed as i32),
        accounts_data_slice: vec![],
        ping: None,
        from_slot: None,
    };

    let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
    request_tx.send(subscribe_request)?;
    info!("✅ Sent account subscription request");

    let request_stream = tokio_stream::wrappers::UnboundedReceiverStream::new(request_rx);
    let response = client.subscribe(request_stream).await?;
    let mut response_stream = response.into_inner();

    info!("🔄 Waiting for account updates (will timeout after 30 seconds)...");

    let mut received_count = 0;
    let max_messages = 10; // Limit to 10 messages for testing

    match timeout(Duration::from_secs(30), async {
        while let Some(message) = response_stream.next().await {
            match message {
                Ok(update) => {
                    match update.update_oneof {
                        Some(UpdateOneof::Account(account_update)) => {
                            if let Some(account_info) = &account_update.account {
                                // Convert pubkey and owner to base64 for display
                                let pubkey_b64 = general_purpose::STANDARD.encode(&account_info.pubkey);
                                let owner_b64 = general_purpose::STANDARD.encode(&account_info.owner);
                                info!(
                                    "📊 Received account update: slot={}, pubkey={}, lamports={}, owner={}, data_len={}, executable={}",
                                    account_update.slot,
                                    pubkey_b64,
                                    account_info.lamports,
                                    owner_b64,
                                    account_info.data.len(),
                                    account_info.executable
                                );

                                // Show first few bytes of data if available
                                if !account_info.data.is_empty() {
                                    let data_preview = if account_info.data.len() > 32 {
                                        format!("{:?}...", &account_info.data[..32])
                                    } else {
                                        format!("{:?}", account_info.data)
                                    };
                                    info!("   📄 Account data preview: {}", data_preview);
                                }
                            } else {
                                info!("📊 Received account update: slot={}, no account info", account_update.slot);
                            }
                            received_count += 1;
                            if received_count >= max_messages {
                                break;
                            }
                        }
                        Some(UpdateOneof::Slot(slot_update)) => {
                            info!("🎰 Received slot update: slot={}", slot_update.slot);
                        }
                        Some(other) => {
                            info!("📦 Received other update: {:?}", other);
                        }
                        None => {
                            info!("📭 Received empty update");
                        }
                    }
                }
                Err(e) => {
                    anyhow::bail!("❌ Error receiving update: {}", e);
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }).await {
        Ok(_) => {
            info!("✅ Successfully received {} account updates", received_count);
            Ok(())
        }
        Err(_) => {
            if received_count > 0 {
                info!("⚠️  Received {} account updates before timeout", received_count);
                Ok(())
            } else {
                info!("⚠️  No account updates received within timeout period (this is expected if no accounts are being updated)");
                Ok(())
            }
        }
    }
}
