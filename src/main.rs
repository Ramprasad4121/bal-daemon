use anyhow::Result;
use ethers::providers::{Middleware, Provider, Ws};
use ethers::types::{Address, H256};
use futures_util::StreamExt;
use rlp::RlpStream;
use std::collections::HashSet;
use tracing::{info, warn};

// BEP-592 structs

#[derive(Debug, Clone)]
struct StorageAccessItem {
    tx_index: u32,
    dirty: bool,
    key: [u8; 32],
}

#[derive(Debug, Clone)]
struct AccountAccessListEncode {
    tx_index: u32,
    address: [u8; 20],
    storage_items: Vec<StorageAccessItem>,
}

#[derive(Debug, Clone)]
struct BlockAccessListEncode {
    version: u32,
    number: u64,
    hash: [u8; 32],
    sign_data: Vec<u8>,
    accounts: Vec<AccountAccessListEncode>,
}

impl StorageAccessItem {
    fn new(tx_index: u32, dirty: bool, key: H256) -> Self {
        Self { tx_index, dirty, key: key.into() }
    }
}

impl AccountAccessListEncode {
    fn new(tx_index: u32, address: Address) -> Self {
        Self { tx_index, address: address.into(), storage_items: Vec::new() }
    }
}

impl BlockAccessListEncode {
    fn new(number: u64, hash: H256) -> Self {
        Self {
            version: 0,
            number,
            hash: hash.into(),
            sign_data: Vec::new(),
            accounts: Vec::new(),
        }
    }

    fn rlp_encode(&self) -> Vec<u8> {
        let mut s = RlpStream::new();
        s.begin_list(5);
        s.append(&self.version);
        s.append(&self.number);
        s.append(&self.hash.as_ref());
        s.append(&self.sign_data.as_slice());
        s.begin_list(self.accounts.len());
        for acc in &self.accounts {
            s.begin_list(3);
            s.append(&acc.tx_index);
            s.append(&acc.address.as_ref());
            s.begin_list(acc.storage_items.len());
            for item in &acc.storage_items {
                s.begin_list(3);
                s.append(&item.tx_index);
                s.append(&(item.dirty as u8));
                s.append(&item.key.as_ref());
            }
        }
        s.out().to_vec()
    }
}

// Parse prestateTracer diffMode result into BEP-592 account/storage entries

fn parse_slot(s: &str) -> Option<[u8; 32]> {
    let hex_str = s.trim_start_matches("0x");
    let padded = format!("{:0>64}", hex_str);
    let bytes = hex::decode(&padded).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Some(arr)
}

fn parse_prestate_diff(
    result: &serde_json::Value,
    tx_index: u32,
) -> Vec<(Address, Vec<StorageAccessItem>)> {
    let mut output: Vec<(Address, Vec<StorageAccessItem>)> = Vec::new();

    let empty_map = serde_json::Map::new();
    let pre_map = result
        .get("pre")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty_map);
    let post_map = result
        .get("post")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty_map);

    // Collect all addresses from pre and post
    let mut addresses: HashSet<String> = HashSet::new();
    for k in pre_map.keys() {
        addresses.insert(k.clone());
    }
    for k in post_map.keys() {
        addresses.insert(k.clone());
    }

    for addr_str in &addresses {
        let addr_bytes = match hex::decode(addr_str.trim_start_matches("0x")) {
            Ok(b) if b.len() == 20 => b,
            _ => continue,
        };
        let mut addr_arr = [0u8; 20];
        addr_arr.copy_from_slice(&addr_bytes);
        let address = Address::from(addr_arr);

        let pre_storage = pre_map
            .get(addr_str)
            .and_then(|v| v.get("storage"))
            .and_then(|v| v.as_object());
        let post_storage = post_map
            .get(addr_str)
            .and_then(|v| v.get("storage"))
            .and_then(|v| v.as_object());

        let mut items: Vec<StorageAccessItem> = Vec::new();
        let mut seen: HashSet<[u8; 32]> = HashSet::new();

        // Slots in post = written (dirty = true)
        if let Some(post_slots) = post_storage {
            for slot_str in post_slots.keys() {
                if let Some(key) = parse_slot(slot_str) {
                    if seen.insert(key) {
                        items.push(StorageAccessItem::new(tx_index, true, H256::from(key)));
                    }
                }
            }
        }

        // Slots in pre only = read (dirty = false)
        if let Some(pre_slots) = pre_storage {
            for slot_str in pre_slots.keys() {
                if let Some(key) = parse_slot(slot_str) {
                    if seen.insert(key) {
                        items.push(StorageAccessItem::new(tx_index, false, H256::from(key)));
                    }
                }
            }
        }

        if !items.is_empty() {
            output.push((address, items));
        }
    }

    output
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let ws_url = "wss://bsc-testnet.nodereal.io/ws/v1/379e86e230114573aaa4a30d84d76b3e";
    let http_url = "https://bsc-testnet.nodereal.io/v1/379e86e230114573aaa4a30d84d76b3e";

    info!("Connecting to {}", ws_url);
    let provider = Provider::<Ws>::connect(ws_url).await?;
    info!("Connected. Waiting for new heads...");

    let http_client = reqwest::Client::new();
    let mut stream = provider.subscribe_blocks().await?;

    while let Some(block) = stream.next().await {
        let block_number = match block.number {
            Some(n) => n.as_u64(),
            None => {
                warn!("Block missing number");
                continue;
            }
        };
        let block_hash = match block.hash {
            Some(h) => h,
            None => {
                warn!("Block missing hash");
                continue;
            }
        };

        info!("New head: #{} {:#x}", block_number, block_hash);

        // Fetch one pending transaction
        let pending_tx = match provider
            .get_block_with_txs(ethers::types::BlockNumber::Pending)
            .await
        {
            Ok(Some(b)) if !b.transactions.is_empty() => b.transactions[0].clone(),
            Ok(_) => {
                info!("Mempool empty");
                continue;
            }
            Err(e) => {
                warn!("Pending block error: {}", e);
                continue;
            }
        };

        info!("Simulating: {:#x}", pending_tx.hash);

        // Build call object from pending tx
        let call_obj = serde_json::json!({
            "from": format!("{:#x}", pending_tx.from),
            "to": pending_tx.to.map(|a| format!("{:#x}", a)),
            "data": format!("0x{}", hex::encode(pending_tx.input.as_ref())),
            "gas": format!("{:#x}", pending_tx.gas),
            "gasPrice": pending_tx.gas_price.map(|g| format!("{:#x}", g)),
            "value": format!("{:#x}", pending_tx.value),
        });

        // Use prestateTracer with diffMode to get storage reads and writes
        let rpc_body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_traceCall",
            "params": [
                call_obj,
                format!("{:#x}", block_number),
                {
                    "tracer": "prestateTracer",
                    "tracerConfig": { "diffMode": true }
                }
            ],
            "id": 1
        });

        let resp = match http_client.post(http_url).json(&rpc_body).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("HTTP request failed: {}", e);
                continue;
            }
        };

        let resp_json: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!("Response parse failed: {}", e);
                continue;
            }
        };

        let result_val = match resp_json.get("result") {
            Some(v) => v.clone(),
            None => {
                warn!(
                    "No result field. Response: {}",
                    serde_json::to_string(&resp_json).unwrap_or_default()
                );
                continue;
            }
        };

        let accounts_data = parse_prestate_diff(&result_val, 0);

        if accounts_data.is_empty() {
            info!("No storage accesses in this tx, waiting for next head");
            continue;
        }

        // Build BEP-592 payload
        let mut bal = BlockAccessListEncode::new(block_number, block_hash);
        let mut total_reads = 0usize;
        let mut total_writes = 0usize;

        for (address, items) in accounts_data {
            total_reads += items.iter().filter(|i| !i.dirty).count();
            total_writes += items.iter().filter(|i| i.dirty).count();
            let mut account = AccountAccessListEncode::new(0, address);
            account.storage_items = items;
            bal.accounts.push(account);
        }

        let encoded = bal.rlp_encode();

        info!("─────────────────────────────────");
        info!("BEP-592 payload generated");
        info!("Block:    #{} {:#x}", block_number, block_hash);
        info!("Tx:       {:#x}", pending_tx.hash);
        info!("Accounts: {}", bal.accounts.len());
        info!("Reads:    {}", total_reads);
        info!("Writes:   {}", total_writes);
        info!("Size:     {} bytes", encoded.len());
        info!("Hex:      {}", hex::encode(&encoded));
        info!("─────────────────────────────────");
        break;
    }

    info!("Done.");
    Ok(())
}