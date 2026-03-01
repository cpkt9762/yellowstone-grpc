use {
    futures::stream::StreamExt,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, env},
    yellowstone_grpc_client::GeyserGrpcClient,
    yellowstone_grpc_proto::prelude::{
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
        SubscribeRequestFilterSlots, SubscribeRequestFilterTransactions,
    },
};

const DEX_PROGRAM: &str = "REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2";
const POOL_ACCOUNT: &str = "HDQqC3z3kncjdeyrCBM6v8VE1mbPcn2xbDmWNXzBBKJC";
const INPUT_VAULT: &str = "CU7zThMYxYmdpXukuS7HsD6z3ACDTFC29aucQVLetYH8";
const OUTPUT_VAULT: &str = "GKXHReciKn57FyYYsdQqLVwu2J79UT34bDiwk63Zv3n1";

/// Parse SPL Token account data to get token balance.
/// Layout: mint (32) + owner (32) + amount (8) + ...
fn parse_token_amount(data: &[u8]) -> Option<u64> {
    if data.len() < 72 {
        return None;
    }
    Some(u64::from_le_bytes(data[64..72].try_into().ok()?))
}

/// Parse SPL Token account mint
fn parse_token_mint(data: &[u8]) -> Option<Pubkey> {
    if data.len() < 32 {
        return None;
    }
    Some(Pubkey::from(<[u8; 32]>::try_from(&data[..32]).ok()?))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let endpoint =
        env::var("GEYSER_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:10900".to_string());

    println!("🔗 Connecting to {endpoint}");
    println!("📋 DEX Program:  {DEX_PROGRAM}");
    println!("🏊 Pool Account: {POOL_ACCOUNT}");
    println!("💰 Input Vault:  {INPUT_VAULT}");
    println!("💰 Output Vault: {OUTPUT_VAULT}");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    let mut client = GeyserGrpcClient::build_from_shared(endpoint)?
        .connect()
        .await?;

    let request = SubscribeRequest {
        slots: HashMap::from([(
            "slots".to_string(),
            SubscribeRequestFilterSlots {
                filter_by_commitment: Some(true),
                interslot_updates: None,
            },
        )]),
        transactions: HashMap::from([(
            "dex_txs".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                account_include: vec![DEX_PROGRAM.to_string()],
                account_exclude: vec![],
                account_required: vec![],
                // Request pool state + both vaults
                account_data_required: vec![
                    POOL_ACCOUNT.to_string(),
                    INPUT_VAULT.to_string(),
                    OUTPUT_VAULT.to_string(),
                ],
                account_data_owner: Some(DEX_PROGRAM.to_string()),
                account_data_source: Some("previous".to_string()),
                signature: None,
                max_messages: None,
            },
        )]),
        ..Default::default()
    };

    println!("📡 Subscribing...");
    let (_, mut stream) = client.subscribe_with_request(Some(request)).await?;

    let pool_pubkey: Pubkey = POOL_ACCOUNT.parse()?;
    let input_vault_pubkey: Pubkey = INPUT_VAULT.parse()?;
    let output_vault_pubkey: Pubkey = OUTPUT_VAULT.parse()?;
    let mut tx_count = 0u64;

    while let Some(msg) = stream.next().await {
        let msg = msg?;
        let Some(update) = msg.update_oneof else {
            continue;
        };

        match update {
            UpdateOneof::Transaction(tx_update) => {
                tx_count += 1;
                let tx = tx_update.transaction.as_ref().unwrap();
                let sig_str = bs58::encode(&tx.signature).into_string();

                println!("\n🔔 TX #{tx_count}: {sig_str}");
                println!("   Slot: {}", tx_update.slot);

                if tx_update.accounts.is_empty() {
                    println!("   ⏳ finalized_accounts: empty (not yet finalized)");
                } else {
                    println!(
                        "   ✅ finalized_accounts: {} accounts",
                        tx_update.accounts.len()
                    );

                    let mut input_balance: Option<u64> = None;
                    let mut output_balance: Option<u64> = None;

                    for acct in &tx_update.accounts {
                        let pubkey = Pubkey::from(
                            <[u8; 32]>::try_from(&acct.pubkey[..32]).unwrap_or_default(),
                        );
                        let owner = Pubkey::from(
                            <[u8; 32]>::try_from(&acct.owner[..32]).unwrap_or_default(),
                        );

                        if pubkey == pool_pubkey {
                            println!(
                                "   🎯 POOL STATE: data_len={} write_version={}",
                                acct.data.len(),
                                acct.write_version
                            );
                        } else if pubkey == input_vault_pubkey {
                            let amount = parse_token_amount(&acct.data);
                            let mint = parse_token_mint(&acct.data);
                            input_balance = amount;
                            println!(
                                "   💰 INPUT VAULT:  balance={} mint={} (owner: {})",
                                amount.map_or("N/A".to_string(), |a| a.to_string()),
                                mint.map_or("N/A".to_string(), |m| m.to_string()),
                                owner,
                            );
                        } else if pubkey == output_vault_pubkey {
                            let amount = parse_token_amount(&acct.data);
                            let mint = parse_token_mint(&acct.data);
                            output_balance = amount;
                            println!(
                                "   💰 OUTPUT VAULT: balance={} mint={} (owner: {})",
                                amount.map_or("N/A".to_string(), |a| a.to_string()),
                                mint.map_or("N/A".to_string(), |m| m.to_string()),
                                owner,
                            );
                        } else {
                            println!(
                                "      📦 {} (owner: {}, data_len: {})",
                                pubkey,
                                owner,
                                acct.data.len()
                            );
                        }
                    }

                    // Summary if both vaults found
                    if input_balance.is_some() || output_balance.is_some() {
                        println!("   ╔══════════════════════════════════════════╗");
                        println!(
                            "   ║ Input Vault:  {:>20} tokens  ║",
                            input_balance.map_or("--".to_string(), |b| b.to_string())
                        );
                        println!(
                            "   ║ Output Vault: {:>20} tokens  ║",
                            output_balance.map_or("--".to_string(), |b| b.to_string())
                        );
                        println!("   ╚══════════════════════════════════════════╝");
                    }
                }

                println!("   📈 Stats: {tx_count} txs total");
            }
            UpdateOneof::Slot(slot) => {
                if slot.status == CommitmentLevel::Finalized as i32 {
                    println!("🔒 Finalized slot: {}", slot.slot);
                }
            }
            _ => {}
        }
    }

    Ok(())
}
