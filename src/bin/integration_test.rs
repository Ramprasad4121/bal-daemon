// Run a local BSC testnet node with debug methods enabled using Docker:
// docker run -p 8545:8545 -p 8546:8546 ghcr.io/bnb-chain/bsc:latest \
//   --http --http.api eth,debug,net,web3 \
//   --ws --ws.api eth,debug,net,web3 \
//   --datadir /data --testnet

use anyhow::{anyhow, bail, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{Address, BlockNumber, Transaction, H256};
use reqwest::Client;
use rlp::RlpStream;
use std::collections::HashSet;
use std::time::Duration;
use tracing::{error, info};

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
        Self {
            tx_index,
            dirty,
            key: key.into(),
        }
    }
}

impl AccountAccessListEncode {
    fn new(tx_index: u32, address: Address) -> Self {
        Self {
            tx_index,
            address: address.into(),
            storage_items: Vec::new(),
        }
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
    let mut output = Vec::new();

    let empty_map = serde_json::Map::new();
    let pre_map = result
        .get("pre")
        .and_then(|value| value.as_object())
        .unwrap_or(&empty_map);
    let post_map = result
        .get("post")
        .and_then(|value| value.as_object())
        .unwrap_or(&empty_map);

    let mut addresses = HashSet::new();
    for key in pre_map.keys() {
        addresses.insert(key.clone());
    }
    for key in post_map.keys() {
        addresses.insert(key.clone());
    }

    for addr_str in &addresses {
        let addr_bytes = match hex::decode(addr_str.trim_start_matches("0x")) {
            Ok(bytes) if bytes.len() == 20 => bytes,
            _ => continue,
        };
        let mut addr_arr = [0u8; 20];
        addr_arr.copy_from_slice(&addr_bytes);
        let address = Address::from(addr_arr);

        let pre_storage = pre_map
            .get(addr_str)
            .and_then(|value| value.get("storage"))
            .and_then(|value| value.as_object());
        let post_storage = post_map
            .get(addr_str)
            .and_then(|value| value.get("storage"))
            .and_then(|value| value.as_object());

        let mut items = Vec::new();
        let mut seen = HashSet::new();

        if let Some(post_slots) = post_storage {
            for slot_str in post_slots.keys() {
                if let Some(key) = parse_slot(slot_str) {
                    if seen.insert(key) {
                        items.push(StorageAccessItem::new(tx_index, true, H256::from(key)));
                    }
                }
            }
        }

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

fn build_trace_call_body(pending_tx: &Transaction, block_number: u64) -> serde_json::Value {
    let call_obj = serde_json::json!({
        "from": format!("{:#x}", pending_tx.from),
        "to": pending_tx.to.map(|address| format!("{:#x}", address)),
        "data": format!("0x{}", hex::encode(pending_tx.input.as_ref())),
        "gas": format!("{:#x}", pending_tx.gas),
        "gasPrice": pending_tx.gas_price.map(|gas| format!("{:#x}", gas)),
        "value": format!("{:#x}", pending_tx.value),
    });

    serde_json::json!({
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
    })
}

async fn trace_call(
    http_client: &Client,
    http_url: &str,
    rpc_body: serde_json::Value,
) -> Result<serde_json::Value> {
    let response = http_client
        .post(http_url)
        .json(&rpc_body)
        .send()
        .await?
        .error_for_status()?;

    let response_json: serde_json::Value = response.json().await?;
    if let Some(error) = response_json.get("error") {
        bail!(
            "debug_traceCall returned error: {}",
            serde_json::to_string(error).unwrap_or_default()
        );
    }

    let Some(result) = response_json.get("result") else {
        bail!(
            "missing result field in debug_traceCall response: {}",
            serde_json::to_string(&response_json).unwrap_or_default()
        );
    };

    Ok(result.clone())
}

fn build_bal_payload(
    block_number: u64,
    block_hash: H256,
    result: &serde_json::Value,
) -> (BlockAccessListEncode, usize, usize, usize) {
    let accounts_data = parse_prestate_diff(result, 0);
    let mut bal = BlockAccessListEncode::new(block_number, block_hash);
    let mut total_accounts = 0usize;
    let mut total_reads = 0usize;
    let mut total_writes = 0usize;

    for (address, items) in accounts_data {
        total_accounts += 1;
        total_reads += items.iter().filter(|item| !item.dirty).count();
        total_writes += items.iter().filter(|item| item.dirty).count();

        let mut account = AccountAccessListEncode::new(0, address);
        account.storage_items = items;
        bal.accounts.push(account);
    }

    (bal, total_accounts, total_reads, total_writes)
}

async fn run_integration_test() -> Result<()> {
    let rpc_url = "http://localhost:8545";
    let http_client = Client::builder().no_proxy().build()?;
    let provider = Provider::new(Http::new_with_client(
        reqwest::Url::parse(rpc_url)?,
        http_client.clone(),
    ))
    .interval(Duration::from_millis(50));

    info!("Connecting to local BSC node at {}", rpc_url);

    let latest_block = provider
        .get_block(BlockNumber::Latest)
        .await?
        .ok_or_else(|| anyhow!("latest block not found"))?;
    let block_number = latest_block
        .number
        .ok_or_else(|| anyhow!("latest block missing number"))?
        .as_u64();
    let block_hash = latest_block
        .hash
        .ok_or_else(|| anyhow!("latest block missing hash"))?;

    info!("Latest block: #{} {:#x}", block_number, block_hash);

    let pending_block = provider.get_block_with_txs(BlockNumber::Pending).await?;
    let pending_tx = match pending_block {
        Some(block) if !block.transactions.is_empty() => block.transactions[0].clone(),
        _ => bail!("no pending transactions in mempool; submit one and rerun integration_test"),
    };

    info!("Pending tx selected: {:#x}", pending_tx.hash);

    let trace_result = trace_call(
        &http_client,
        rpc_url,
        build_trace_call_body(&pending_tx, block_number),
    )
    .await?;

    let (payload, total_accounts, total_reads, total_writes) =
        build_bal_payload(block_number, block_hash, &trace_result);
    let encoded = payload.rlp_encode();
    let payload_hex = hex::encode(&encoded);

    let block_response: Option<serde_json::Value> = provider
        .request("eth_getBlockByHash", (block_hash, false))
        .await?;
    if block_response.is_none() {
        bail!("eth_getBlockByHash returned null for latest block hash");
    }

    info!("Payload hex: {}", payload_hex);
    info!(
        "Summary: accounts={}, storage_slots={}, reads={}, writes={}",
        total_accounts,
        total_reads + total_writes,
        total_reads,
        total_writes
    );
    info!("PASS");

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run_integration_test().await {
        Ok(()) => Ok(()),
        Err(err) => {
            error!("FAIL: {}", err);
            Err(err)
        }
    }
}
