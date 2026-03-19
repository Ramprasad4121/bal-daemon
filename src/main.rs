use anyhow::{bail, Result};
use ethers::providers::{Middleware, Provider, Ws};
use ethers::types::{Address, BlockNumber, Transaction, H256};
use futures_util::StreamExt;
use rlp::RlpStream;
use std::collections::{BTreeMap, HashSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
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

#[derive(Debug, Clone, Copy)]
struct HeadContext {
    number: u64,
    hash: H256,
}

#[derive(Debug, Default)]
struct SimulationCounters {
    fresh: AtomicU64,
    stale: AtomicU64,
}

#[derive(Debug)]
struct SimulatedTransaction {
    tx_index: u32,
    accounts: Vec<(Address, Vec<StorageAccessItem>)>,
}

#[derive(Debug)]
struct AccountAccumulator {
    tx_index: u32,
    address: Address,
    storage_items: BTreeMap<[u8; 32], StorageAccessItem>,
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

impl SimulationCounters {
    fn snapshot(&self) -> (u64, u64) {
        (
            self.fresh.load(Ordering::Relaxed),
            self.stale.load(Ordering::Relaxed),
        )
    }

    fn record_fresh(&self) -> (u64, u64) {
        self.fresh.fetch_add(1, Ordering::Relaxed);
        self.snapshot()
    }

    fn record_stale(&self) -> (u64, u64) {
        self.stale.fetch_add(1, Ordering::Relaxed);
        self.snapshot()
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

fn build_trace_call_body(pending_tx: &Transaction, block_number: u64) -> serde_json::Value {
    let call_obj = serde_json::json!({
        "from": format!("{:#x}", pending_tx.from),
        "to": pending_tx.to.map(|a| format!("{:#x}", a)),
        "data": format!("0x{}", hex::encode(pending_tx.input.as_ref())),
        "gas": format!("{:#x}", pending_tx.gas),
        "gasPrice": pending_tx.gas_price.map(|g| format!("{:#x}", g)),
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
    http_client: &reqwest::Client,
    http_url: &str,
    rpc_body: serde_json::Value,
) -> Result<serde_json::Value> {
    let resp = http_client
        .post(http_url)
        .json(&rpc_body)
        .send()
        .await?
        .error_for_status()?;

    let resp_json: serde_json::Value = resp.json().await?;

    if let Some(error) = resp_json.get("error") {
        bail!(
            "debug_traceCall returned error: {}",
            serde_json::to_string(error).unwrap_or_default()
        );
    }

    let Some(result) = resp_json.get("result") else {
        bail!(
            "No result field. Response: {}",
            serde_json::to_string(&resp_json).unwrap_or_default()
        );
    };

    Ok(result.clone())
}

fn log_simulation_totals(counters: &SimulationCounters) {
    let (fresh, stale) = counters.snapshot();
    info!("simulation totals: fresh={}, stale={}", fresh, stale);
}

fn log_stale_simulation(counters: &SimulationCounters, message: &'static str) {
    info!("{}", message);
    counters.record_stale();
    log_simulation_totals(counters);
}

async fn simulate_transaction(
    http_client: reqwest::Client,
    http_url: &'static str,
    block_number: u64,
    pending_tx: Transaction,
    tx_index: u32,
    cancellation: CancellationToken,
) -> Result<Option<SimulatedTransaction>> {
    let rpc_body = build_trace_call_body(&pending_tx, block_number);

    let result_val = tokio::select! {
        _ = cancellation.cancelled() => return Ok(None),
        result = trace_call(&http_client, http_url, rpc_body) => result?,
    };

    Ok(Some(SimulatedTransaction {
        tx_index,
        accounts: parse_prestate_diff(&result_val, tx_index),
    }))
}

fn merge_simulations(
    head: HeadContext,
    simulations: Vec<SimulatedTransaction>,
) -> (BlockAccessListEncode, usize, usize, usize) {
    let mut merged_accounts: BTreeMap<[u8; 20], AccountAccumulator> = BTreeMap::new();

    for simulation in simulations {
        for (address, items) in simulation.accounts {
            let address_key: [u8; 20] = address.into();
            let account =
                merged_accounts
                    .entry(address_key)
                    .or_insert_with(|| AccountAccumulator {
                        tx_index: simulation.tx_index,
                        address,
                        storage_items: BTreeMap::new(),
                    });

            account.tx_index = account.tx_index.min(simulation.tx_index);

            for item in items {
                if let Some(existing) = account.storage_items.get_mut(&item.key) {
                    existing.tx_index = existing.tx_index.min(item.tx_index);
                    existing.dirty |= item.dirty;
                } else {
                    account.storage_items.insert(item.key, item);
                }
            }
        }
    }

    let mut bal = BlockAccessListEncode::new(head.number, head.hash);
    let mut total_slots = 0usize;
    let mut total_reads = 0usize;
    let mut total_writes = 0usize;

    for account in merged_accounts.into_values() {
        let storage_items: Vec<_> = account.storage_items.into_values().collect();
        total_slots += storage_items.len();
        total_reads += storage_items.iter().filter(|item| !item.dirty).count();
        total_writes += storage_items.iter().filter(|item| item.dirty).count();

        let mut merged_account = AccountAccessListEncode::new(account.tx_index, account.address);
        merged_account.storage_items = storage_items;
        bal.accounts.push(merged_account);
    }

    (bal, total_slots, total_reads, total_writes)
}

async fn simulate_for_head(
    provider: Provider<Ws>,
    http_client: reqwest::Client,
    http_url: &'static str,
    head: HeadContext,
    current_head_rx: watch::Receiver<Option<H256>>,
    cancellation: CancellationToken,
    counters: Arc<SimulationCounters>,
) -> Result<()> {
    let pending_block = tokio::select! {
        _ = cancellation.cancelled() => {
            log_stale_simulation(&counters, "simulation cancelled: stale head");
            return Ok(());
        }
        pending_block = provider.get_block_with_txs(BlockNumber::Pending) => pending_block,
    };

    let pending_tx = match pending_block {
        Ok(Some(block)) if !block.transactions.is_empty() => block.transactions,
        Ok(_) => {
            info!("Mempool empty");
            return Ok(());
        }
        Err(e) => {
            warn!("Pending block error: {}", e);
            return Ok(());
        }
    };

    let total_transactions = pending_tx.len();
    info!("Simulating {} pending txs in parallel", total_transactions);

    let mut tx_tasks = JoinSet::new();
    for (index, tx) in pending_tx.into_iter().enumerate() {
        let tx_index = match u32::try_from(index) {
            Ok(tx_index) => tx_index,
            Err(_) => {
                warn!("Pending tx index overflow: {}", index);
                return Ok(());
            }
        };

        tx_tasks.spawn(simulate_transaction(
            http_client.clone(),
            http_url,
            head.number,
            tx,
            tx_index,
            cancellation.clone(),
        ));
    }

    let mut simulations = Vec::with_capacity(total_transactions);
    while !tx_tasks.is_empty() {
        tokio::select! {
            _ = cancellation.cancelled() => {
                tx_tasks.abort_all();
                while let Some(_) = tx_tasks.join_next().await {}
                log_stale_simulation(&counters, "simulation cancelled: stale head");
                return Ok(());
            }
            join_result = tx_tasks.join_next() => {
                let Some(join_result) = join_result else {
                    break;
                };

                match join_result {
                    Ok(Ok(Some(simulation))) => simulations.push(simulation),
                    Ok(Ok(None)) => {
                        tx_tasks.abort_all();
                        while let Some(_) = tx_tasks.join_next().await {}
                        log_stale_simulation(&counters, "simulation cancelled: stale head");
                        return Ok(());
                    }
                    Ok(Err(e)) => {
                        tx_tasks.abort_all();
                        while let Some(_) = tx_tasks.join_next().await {}
                        warn!("transaction simulation failed: {}", e);
                        return Ok(());
                    }
                    Err(e) => {
                        tx_tasks.abort_all();
                        while let Some(_) = tx_tasks.join_next().await {}
                        warn!("transaction task join error: {}", e);
                        return Ok(());
                    }
                }
            }
        }
    }

    if *current_head_rx.borrow() != Some(head.hash) {
        log_stale_simulation(&counters, "stale access set discarded");
        return Ok(());
    }

    let (bal, total_slots, total_reads, total_writes) = merge_simulations(head, simulations);

    if *current_head_rx.borrow() != Some(head.hash) {
        log_stale_simulation(&counters, "stale access set discarded");
        return Ok(());
    }

    let (fresh, stale) = counters.record_fresh();
    if bal.accounts.is_empty() {
        info!(
            "No storage accesses across {} pending txs, waiting for next head",
            total_transactions
        );
        info!("simulation totals: fresh={}, stale={}", fresh, stale);
        return Ok(());
    }

    let encoded = bal.rlp_encode();

    info!("─────────────────────────────────");
    info!("BEP-592 payload generated");
    info!("Block:    #{} {:#x}", head.number, head.hash);
    info!("Txs:      {}", total_transactions);
    info!("Accounts: {}", bal.accounts.len());
    info!("Slots:    {}", total_slots);
    info!("Reads:    {}", total_reads);
    info!("Writes:   {}", total_writes);
    info!("Fresh:    {}", fresh);
    info!("Stale:    {}", stale);
    info!("Size:     {} bytes", encoded.len());
    info!("Hex:      {}", hex::encode(&encoded));
    info!("─────────────────────────────────");

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

    let ws_url = "wss://bsc-testnet.nodereal.io/ws/v1/379e86e230114573aaa4a30d84d76b3e";
    let http_url = "https://bsc-testnet.nodereal.io/v1/379e86e230114573aaa4a30d84d76b3e";

    info!("Connecting to {}", ws_url);
    let provider = Provider::<Ws>::connect(ws_url).await?;
    info!("Connected. Waiting for new heads...");

    let http_client = reqwest::Client::new();
    let mut stream = provider.subscribe_blocks().await?;
    let counters = Arc::new(SimulationCounters::default());
    let (current_head_tx, current_head_rx) = watch::channel::<Option<H256>>(None);
    let mut active_simulation: Option<CancellationToken> = None;

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
        let _ = current_head_tx.send(Some(block_hash));

        let cancellation = CancellationToken::new();
        if let Some(previous) = active_simulation.take() {
            previous.cancel();
        }
        active_simulation = Some(cancellation.clone());

        let head = HeadContext {
            number: block_number,
            hash: block_hash,
        };
        let provider = provider.clone();
        let http_client = http_client.clone();
        let current_head_rx = current_head_rx.clone();
        let counters = Arc::clone(&counters);

        tokio::spawn(async move {
            if let Err(e) = simulate_for_head(
                provider,
                http_client,
                http_url,
                head,
                current_head_rx,
                cancellation,
                counters,
            )
            .await
            {
                warn!("simulation task failed: {}", e);
            }
        });
    }

    if let Some(active) = active_simulation.take() {
        active.cancel();
    }

    info!("Head subscription ended.");
    Ok(())
}
