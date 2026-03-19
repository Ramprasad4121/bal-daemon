use anyhow::{anyhow, bail, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{Address, BlockNumber, Transaction, H256};
use reqwest::Client;
use rlp::{Rlp, RlpStream};
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

fn verify_round_trip(payload: &BlockAccessListEncode, encoded: &[u8]) -> Result<()> {
    let decoded = Rlp::new(encoded);

    let version: u32 = decoded.val_at(0)?;
    if version != payload.version {
        bail!(
            "round-trip version mismatch: expected {}, got {}",
            payload.version,
            version
        );
    }

    let number: u64 = decoded.val_at(1)?;
    if number != payload.number {
        bail!(
            "round-trip block number mismatch: expected {}, got {}",
            payload.number,
            number
        );
    }

    let hash: Vec<u8> = decoded.val_at(2)?;
    if hash != payload.hash.to_vec() {
        bail!("round-trip block hash mismatch");
    }

    let sign_data: Vec<u8> = decoded.val_at(3)?;
    if sign_data != payload.sign_data {
        bail!("round-trip sign_data mismatch");
    }

    let accounts = decoded.at(4)?;
    let account_count = accounts.item_count()?;
    if account_count != payload.accounts.len() {
        bail!(
            "round-trip account count mismatch: expected {}, got {}",
            payload.accounts.len(),
            account_count
        );
    }

    for (account_index, expected_account) in payload.accounts.iter().enumerate() {
        let account = accounts.at(account_index)?;

        let tx_index: u32 = account.val_at(0)?;
        if tx_index != expected_account.tx_index {
            bail!(
                "round-trip account tx_index mismatch at index {}: expected {}, got {}",
                account_index,
                expected_account.tx_index,
                tx_index
            );
        }

        let address: Vec<u8> = account.val_at(1)?;
        if address != expected_account.address.to_vec() {
            bail!(
                "round-trip account address mismatch at index {}",
                account_index
            );
        }

        let storage_items = account.at(2)?;
        let storage_count = storage_items.item_count()?;
        if storage_count != expected_account.storage_items.len() {
            bail!(
                "round-trip storage item count mismatch at account {}: expected {}, got {}",
                account_index,
                expected_account.storage_items.len(),
                storage_count
            );
        }

        for (storage_index, expected_item) in expected_account.storage_items.iter().enumerate() {
            let item = storage_items.at(storage_index)?;

            let item_tx_index: u32 = item.val_at(0)?;
            if item_tx_index != expected_item.tx_index {
                bail!(
                    "round-trip storage tx_index mismatch at account {}, item {}: expected {}, got {}",
                    account_index,
                    storage_index,
                    expected_item.tx_index,
                    item_tx_index
                );
            }

            let dirty: u8 = item.val_at(1)?;
            let expected_dirty = expected_item.dirty as u8;
            if dirty != expected_dirty {
                bail!(
                    "round-trip dirty flag mismatch at account {}, item {}: expected {}, got {}",
                    account_index,
                    storage_index,
                    expected_dirty,
                    dirty
                );
            }

            let key: Vec<u8> = item.val_at(2)?;
            if key != expected_item.key.to_vec() {
                bail!(
                    "round-trip storage key mismatch at account {}, item {}",
                    account_index,
                    storage_index
                );
            }
        }
    }

    Ok(())
}

async fn find_contract_interaction(
    provider: &Provider<Http>,
    latest_block_number: u64,
) -> Result<(u64, H256, Transaction)> {
    for depth in 0..=5u64 {
        let block_number = latest_block_number
            .checked_sub(depth)
            .ok_or_else(|| anyhow!("block walkback underflow at depth {}", depth))?;

        let block = provider
            .get_block_with_txs(BlockNumber::Number(block_number.into()))
            .await?
            .ok_or_else(|| anyhow!("block {} not found", block_number))?;

        let block_hash = block
            .hash
            .ok_or_else(|| anyhow!("block {} missing hash", block_number))?;
        let block_number = block
            .number
            .ok_or_else(|| anyhow!("block {} missing number", block_number))?
            .as_u64();

        if let Some(tx) = block
            .transactions
            .iter()
            .find(|tx| !tx.input.as_ref().is_empty())
            .cloned()
        {
            return Ok((block_number, block_hash, tx));
        }
    }

    bail!("no contract interaction found in the latest 6 blocks");
}

async fn run_integration_test() -> Result<()> {
    let rpc_url = "https://bsc-testnet.nodereal.io/v1/379e86e230114573aaa4a30d84d76b3e";
    let http_client = Client::builder().no_proxy().build()?;
    let provider = Provider::new(Http::new_with_client(
        reqwest::Url::parse(rpc_url)?,
        http_client.clone(),
    ))
    .interval(Duration::from_millis(50));

    info!("Connecting to NodeReal BSC testnet at {}", rpc_url);

    let latest_block_number = provider.get_block_number().await?.as_u64();
    let (block_number, block_hash, tx) =
        find_contract_interaction(&provider, latest_block_number).await?;

    info!("Block:    #{} {:#x}", block_number, block_hash);
    info!("Tx:       {:#x}", tx.hash);

    let parent_block_number = block_number
        .checked_sub(1)
        .ok_or_else(|| anyhow!("cannot trace a transaction from genesis block"))?;

    let trace_result = trace_call(
        &http_client,
        rpc_url,
        build_trace_call_body(&tx, parent_block_number),
    )
    .await?;

    let (payload, total_accounts, total_reads, total_writes) =
        build_bal_payload(block_number, block_hash, &trace_result);
    let encoded = payload.rlp_encode();
    let payload_hex = hex::encode(&encoded);
    verify_round_trip(&payload, &encoded)?;

    info!("Accounts: {}", total_accounts);
    info!("Reads:    {}", total_reads);
    info!("Writes:   {}", total_writes);
    info!("Size:     {} bytes", encoded.len());
    info!("Hex:      {}", payload_hex);
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
