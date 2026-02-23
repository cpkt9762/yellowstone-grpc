use {
    anyhow::Result,
    base64::Engine,
    log::info,
    solana_pubkey::Pubkey,
    std::{env, path::PathBuf, sync::Arc, time::Duration},
    tokio::{fs, sync::mpsc, time::interval},
    yellowstone_grpc_geyser::{
        config::ConfigGrpc,
        grpc::GrpcService,
    },
    yellowstone_grpc_proto::{
        prelude::SlotStatus,
        plugin::message::{Message, MessageAccount, MessageAccountInfo, MessageSlot},
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

    let unix_socket_path = PathBuf::from(&args[1]);
    info!("Starting gRPC server on Unix socket: {:?}", unix_socket_path);

    // Remove existing socket file if it exists
    if unix_socket_path.exists() {
        fs::remove_file(&unix_socket_path).await?;
        info!("Removed existing socket file");
    }

    // Create a minimal config for testing
    let config_grpc = ConfigGrpc {
        address: None,
        unix_socket_path: Some(unix_socket_path.clone()),
        tls_config: None,
        compression: Default::default(),
        server_http2_adaptive_window: None,
        server_http2_keepalive_interval: None,
        server_http2_keepalive_timeout: None,
        server_initial_connection_window_size: None,
        server_initial_stream_window_size: None,
        max_decoding_message_size: 4 * 1024 * 1024,
        snapshot_plugin_channel_capacity: None,
        snapshot_client_channel_capacity: 50_000_000,
        channel_capacity: 100_000,
        unary_concurrency_limit: 100,
        unary_disabled: false,
        x_token: None,
        replay_stored_slots: 0,
        filter_name_size_limit: 128,
        filter_names_size_limit: 4096,
        filter_names_cleanup_interval: std::time::Duration::from_secs(1),
        filter_limits: Default::default(),
    };

    // Validate configuration
    config_grpc.validate()?;

    // Create gRPC service
    let (_snapshot_tx, messages_tx, shutdown) = GrpcService::create(
        Default::default(), // tokio config
        config_grpc,
        None, // debug clients
        false, // is_reload
    )
    .await?;

    // Start fake data generator
    let fake_data_shutdown = Arc::clone(&shutdown);
    tokio::spawn(async move {
        generate_fake_data(messages_tx, fake_data_shutdown).await;
    });

    info!("✅ gRPC server started successfully on Unix socket: {:?}", unix_socket_path);
    info!("Server is ready to accept connections. Will run for 120 seconds for testing...");

    // Wait for 120 seconds for testing
    tokio::time::sleep(tokio::time::Duration::from_secs(120)).await;
    info!("Test period completed, stopping server...");

    // Notify shutdown
    shutdown.notify_waiters();

    // Clean up socket file
    if unix_socket_path.exists() {
        fs::remove_file(&unix_socket_path).await?;
        info!("Cleaned up socket file");
    }

    info!("Server stopped");
    Ok(())
}

async fn generate_fake_data(
    messages_tx: mpsc::UnboundedSender<yellowstone_grpc_proto::plugin::message::Message>,
    shutdown: Arc<tokio::sync::Notify>,
) {
    use yellowstone_grpc_proto::plugin::message::{Message, MessageAccount, MessageSlot};

    let mut interval = interval(Duration::from_secs(2));
    let mut slot_counter = 100_000_000u64;
    let mut account_counter = 0u32;

    info!("🚀 Starting fake data generator...");

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                info!("📴 Fake data generator shutting down...");
                break;
            }
            _ = interval.tick() => {
                slot_counter += 1;
                account_counter += 1;

                // Generate fake slot message
                let slot_message = Message::Slot(MessageSlot {
                    slot: slot_counter,
                    parent: Some(slot_counter - 1),
                    status: SlotStatus::SlotProcessed.into(),
                    created_at: prost_types::Timestamp::from(std::time::SystemTime::now()),
                    dead_error: None,
                });

                // Generate fake account message
                let fake_pubkey = generate_fake_pubkey(account_counter);
                let fake_owner = generate_fake_owner(account_counter);
                let fake_data = generate_fake_account_data(account_counter);

                let account_message = Message::Account(MessageAccount {
                    account: Arc::new(MessageAccountInfo {
                        pubkey: Pubkey::try_from(fake_pubkey.as_slice()).unwrap_or_default(),
                        lamports: 1_000_000 + (account_counter as u64 * 100_000),
                        owner: Pubkey::try_from(fake_owner.as_slice()).unwrap_or_default(),
                        executable: account_counter % 10 == 0, // Every 10th account is executable
                        rent_epoch: 350 + (account_counter as u64 % 10),
                        data: fake_data,
                        write_version: account_counter as u64,
                        txn_signature: None,
                    }),
                    slot: slot_counter,
                    is_startup: false,
                    created_at: prost_types::Timestamp::from(std::time::SystemTime::now()),
                });

                // Send updates
                if let Err(e) = messages_tx.send(slot_message) {
                    info!("❌ Failed to send slot update: {}", e);
                    break;
                }

                if let Err(e) = messages_tx.send(account_message) {
                    info!("❌ Failed to send account update: {}", e);
                    break;
                }

                info!(
                    "📊 Generated fake data: slot={}, account={}, pubkey={}, owner={}",
                    slot_counter,
                    account_counter,
                    base64::engine::general_purpose::STANDARD.encode(&fake_pubkey),
                    base64::engine::general_purpose::STANDARD.encode(&fake_owner)
                );
            }
        }
    }
}

fn generate_fake_pubkey(counter: u32) -> Vec<u8> {
    let mut pubkey = vec![0u8; 32];
    pubkey[0..4].copy_from_slice(&counter.to_le_bytes());
    pubkey[4] = 0x01; // Mark as fake
    pubkey
}

fn generate_fake_owner(counter: u32) -> Vec<u8> {
    let mut owner = vec![0u8; 32];

    // Simulate different program owners
    match counter % 5 {
        0 => {
            // System Program
            owner[31] = 0x00;
        }
        1 => {
            // Token Program
            owner[0..4].copy_from_slice(&[0x06, 0xdd, 0xf6, 0xe1]);
            owner[4..8].copy_from_slice(&[0x76, 0x52, 0x14, 0x94]);
        }
        2 => {
            // SPL Associated Token Account Program
            owner[0..4].copy_from_slice(&[0x8c, 0x97, 0x25, 0x8f]);
            owner[4..8].copy_from_slice(&[0x4e, 0x24, 0x89, 0xf1]);
        }
        3 => {
            // Stake Program
            owner[0..4].copy_from_slice(&[0x06, 0xa7, 0xd5, 0x17]);
            owner[4..8].copy_from_slice(&[0x18, 0x7b, 0xd1, 0x6c]);
        }
        _ => {
            // Custom program
            owner[0..4].copy_from_slice(&counter.to_be_bytes());
            owner[4] = 0xFF; // Mark as custom
        }
    }

    owner
}

fn generate_fake_account_data(counter: u32) -> Vec<u8> {
    let data_size = match counter % 4 {
        0 => 0,    // Empty account
        1 => 32,   // Small account (like a mint)
        2 => 165,  // Token account size
        _ => 1024, // Large account
    };

    let mut data = vec![0u8; data_size];

    if !data.is_empty() {
        // Fill with some pattern
        data[0..4].copy_from_slice(&counter.to_le_bytes());

        // Add some fake token account data structure
        if data_size >= 32 {
            data[4..8].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // Magic bytes
            data[8..16].copy_from_slice(&(counter as u64 * 1000).to_le_bytes()); // Amount
        }

        // Fill rest with pattern
        for i in 16..data.len() {
            data[i] = ((counter + i as u32) % 256) as u8;
        }
    }

    data
}
