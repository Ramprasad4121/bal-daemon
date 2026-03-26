use anyhow::{bail, Result};
use ethers::types::{Address, Transaction, H256};
use rlp::RlpStream;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;

// BEP-592 / EIP-7928 structs

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StorageAccessItem {
    pub tx_index: u32,
    pub dirty: bool,
    pub key: [u8; 32],
    pub value: [u8; 32],
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AccountAccessListEncode {
    pub tx_index: u32,
    pub address: [u8; 20],
    pub storage_items: Vec<StorageAccessItem>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockAccessListEncode {
    pub version: u32,
    pub number: u64,
    pub hash: [u8; 32],
    pub sign_data: Vec<u8>,
    pub accounts: Vec<AccountAccessListEncode>,
}

#[derive(Debug, Clone, Copy)]
pub struct HeadContext {
    pub number: u64,
    pub hash: H256,
}

#[derive(Debug, Default)]
pub struct SimulationCounters {
    pub fresh: AtomicU64,
    pub stale: AtomicU64,
}

#[derive(Debug)]
pub struct SimulatedTransaction {
    pub tx_index: u32,
    pub accounts: Vec<(Address, Vec<StorageAccessItem>)>,
}

#[derive(Debug)]
pub struct AccountAccumulator {
    pub tx_index: u32,
    pub address: Address,
    pub storage_items: BTreeMap<[u8; 32], StorageAccessItem>,
}

impl StorageAccessItem {
    pub fn new(tx_index: u32, dirty: bool, key: H256, value: H256) -> Self {
        Self {
            tx_index,
            dirty,
            key: key.into(),
            value: value.into(),
        }
    }
}

impl AccountAccessListEncode {
    pub fn new(tx_index: u32, address: Address) -> Self {
        Self {
            tx_index,
            address: address.into(),
            storage_items: Vec::new(),
        }
    }
}

impl BlockAccessListEncode {
    pub fn new(number: u64, hash: H256) -> Self {
        Self {
            version: 0,
            number,
            hash: hash.into(),
            sign_data: Vec::new(),
            accounts: Vec::new(),
        }
    }

    pub fn rlp_encode(&self) -> Vec<u8> {
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

    pub fn eip7928_encode(&self) -> Vec<u8> {
        let mut s = RlpStream::new();
        let mut sorted_accounts = self.accounts.clone();
        sorted_accounts.sort_by_key(|a| a.address);

        s.begin_list(sorted_accounts.len());
        for acc in &sorted_accounts {
            s.begin_list(6);
            s.append(&acc.address.as_ref());

            let mut storage_items = acc.storage_items.clone();
            storage_items.sort_by_key(|i| i.key);

            let storage_changes: Vec<_> = storage_items.iter().filter(|i| i.dirty).collect();
            let storage_reads: Vec<_> = storage_items.iter().filter(|i| !i.dirty).collect();

            // storage_changes: [[slot: bytes32, [[block_access_index: u16, new_value: bytes32]]]]
            s.begin_list(storage_changes.len());
            for item in storage_changes {
                s.begin_list(2);
                s.append(&item.key.as_ref());
                s.begin_list(1);
                s.begin_list(2);
                s.append(&(item.tx_index as u16));
                s.append(&item.value.as_ref());
            }

            // storage_reads: [bytes32]
            s.begin_list(storage_reads.len());
            for item in storage_reads {
                s.append(&item.key.as_ref());
            }

            s.begin_list(0); // balance_changes
            s.begin_list(0); // nonce_changes
            s.begin_list(0); // code_changes
        }
        s.out().to_vec()
    }
}

impl SimulationCounters {
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.fresh.load(Ordering::Relaxed),
            self.stale.load(Ordering::Relaxed),
        )
    }

    pub fn record_fresh(&self) -> (u64, u64) {
        self.fresh.fetch_add(1, Ordering::Relaxed);
        self.snapshot()
    }

    pub fn record_stale(&self) -> (u64, u64) {
        self.stale.fetch_add(1, Ordering::Relaxed);
        self.snapshot()
    }
}

// Parse prestateTracer diffMode result into account/storage entries

pub fn parse_hex_32(s: &str) -> Option<[u8; 32]> {
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

pub fn parse_prestate_diff(
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
            for (slot_str, value_json) in post_slots {
                if let Some(key) = parse_hex_32(slot_str) {
                    if seen.insert(key) {
                        let value = value_json.as_str().and_then(parse_hex_32).unwrap_or([0u8; 32]);
                        items.push(StorageAccessItem::new(tx_index, true, H256::from(key), H256::from(value)));
                    }
                }
            }
        }

        // Slots in pre only = read (dirty = false)
        if let Some(pre_slots) = pre_storage {
            for (slot_str, value_json) in pre_slots {
                if let Some(key) = parse_hex_32(slot_str) {
                    if seen.insert(key) {
                        let value = value_json.as_str().and_then(parse_hex_32).unwrap_or([0u8; 32]);
                        items.push(StorageAccessItem::new(tx_index, false, H256::from(key), H256::from(value)));
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

pub fn build_trace_call_body(pending_tx: &Transaction, block_number: u64) -> serde_json::Value {
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

pub async fn trace_call(
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
            "RPC returned error: {}",
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

pub fn build_trace_transaction_body(tx_hash: H256) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceTransaction",
        "params": [
            format!("{:#x}", tx_hash),
            {
                "tracer": "prestateTracer",
                "tracerConfig": { "diffMode": true }
            }
        ],
        "id": 1
    })
}

pub async fn simulate_transaction(
    http_client: reqwest::Client,
    http_url: &str,
    block_number: u64,
    pending_tx: Transaction,
    tx_index: u32,
    cancellation: Option<CancellationToken>,
) -> Result<Option<SimulatedTransaction>> {
    let rpc_body = build_trace_call_body(&pending_tx, block_number);

    let result_val = if let Some(cancel) = cancellation {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(None),
            result = trace_call(&http_client, http_url, rpc_body) => result?,
        }
    } else {
        trace_call(&http_client, http_url, rpc_body).await?
    };

    Ok(Some(SimulatedTransaction {
        tx_index,
        accounts: parse_prestate_diff(&result_val, tx_index),
    }))
}

pub async fn simulate_mined_transaction(
    http_client: reqwest::Client,
    http_url: &str,
    tx_hash: H256,
    tx_index: u32,
) -> Result<SimulatedTransaction> {
    let rpc_body = build_trace_transaction_body(tx_hash);
    let result_val = trace_call(&http_client, http_url, rpc_body).await?;

    Ok(SimulatedTransaction {
        tx_index,
        accounts: parse_prestate_diff(&result_val, tx_index),
    })
}

pub fn merge_simulations(
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
                    if item.tx_index > existing.tx_index {
                        existing.value = item.value;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rlp::Rlp;

    #[test]
    fn test_rlp_round_trip() {
        let original = BlockAccessListEncode {
            version: 0,
            number: 12_345_678,
            hash: [0xab; 32],
            sign_data: Vec::new(),
            accounts: vec![AccountAccessListEncode {
                tx_index: 0,
                address: [0xcd; 20],
                storage_items: vec![
                    StorageAccessItem {
                        tx_index: 0,
                        dirty: false,
                        key: [0x11; 32],
                        value: [0x00; 32],
                    },
                    StorageAccessItem {
                        tx_index: 0,
                        dirty: true,
                        key: [0x22; 32],
                        value: [0x00; 32],
                    },
                ],
            }],
        };

        let encoded = original.rlp_encode();
        let decoded = Rlp::new(&encoded);

        let version: u32 = decoded
            .val_at(0)
            .expect("failed to decode version from RLP payload");
        assert_eq!(version, 0, "version mismatch after RLP round trip");

        let number: u64 = decoded
            .val_at(1)
            .expect("failed to decode block number from RLP payload");
        assert_eq!(
            number, 12_345_678,
            "block number mismatch after RLP round trip"
        );

        let hash: Vec<u8> = decoded
            .val_at(2)
            .expect("failed to decode block hash from RLP payload");
        assert_eq!(
            hash,
            original.hash.to_vec(),
            "block hash mismatch after RLP round trip"
        );

        let accounts = decoded
            .at(4)
            .expect("failed to decode accounts list from RLP payload");
        assert_eq!(
            accounts
                .item_count()
                .expect("failed to count accounts in RLP"),
            1,
            "accounts length mismatch after RLP round trip"
        );

        let first_account = accounts
            .at(0)
            .expect("failed to decode first account from RLP payload");
        let account_tx_index: u32 = first_account
            .val_at(0)
            .expect("failed to decode account tx_index from RLP payload");
        assert_eq!(
            account_tx_index, 0,
            "account tx_index mismatch after RLP round trip"
        );

        let account_address: Vec<u8> = first_account
            .val_at(1)
            .expect("failed to decode account address from RLP payload");
        assert_eq!(
            account_address,
            original.accounts[0].address.to_vec(),
            "account address mismatch after RLP round trip"
        );

        let storage_items = first_account
            .at(2)
            .expect("failed to decode storage items from RLP payload");
        assert_eq!(
            storage_items
                .item_count()
                .expect("failed to count storage items in RLP"),
            2,
            "storage item count mismatch after RLP round trip"
        );

        let first_storage_item = storage_items
            .at(0)
            .expect("failed to decode first storage item from RLP payload");
        let first_storage_tx_index: u32 = first_storage_item
            .val_at(0)
            .expect("failed to decode first storage tx_index from RLP payload");
        assert_eq!(
            first_storage_tx_index, 0,
            "first storage tx_index mismatch after RLP round trip"
        );
        let first_storage_dirty: u8 = first_storage_item
            .val_at(1)
            .expect("failed to decode first storage dirty flag from RLP payload");
        assert_eq!(
            first_storage_dirty, 0,
            "first storage dirty flag mismatch after RLP round trip"
        );
        let first_storage_key: Vec<u8> = first_storage_item
            .val_at(2)
            .expect("failed to decode first storage key from RLP payload");
        assert_eq!(
            first_storage_key,
            original.accounts[0].storage_items[0].key.to_vec(),
            "first storage key mismatch after RLP round trip"
        );

        let second_storage_item = storage_items
            .at(1)
            .expect("failed to decode second storage item from RLP payload");
        let second_storage_tx_index: u32 = second_storage_item
            .val_at(0)
            .expect("failed to decode second storage tx_index from RLP payload");
        assert_eq!(
            second_storage_tx_index, 0,
            "second storage tx_index mismatch after RLP round trip"
        );
        let second_storage_dirty: u8 = second_storage_item
            .val_at(1)
            .expect("failed to decode second storage dirty flag from RLP payload");
        assert_eq!(
            second_storage_dirty, 1,
            "second storage dirty flag mismatch after RLP round trip"
        );
        let second_storage_key: Vec<u8> = second_storage_item
            .val_at(2)
            .expect("failed to decode second storage key from RLP payload");
        assert_eq!(
            second_storage_key,
            original.accounts[0].storage_items[1].key.to_vec(),
            "second storage key mismatch after RLP round trip"
        );
    }

    #[test]
    fn test_eip7928_encode() {
        let original = BlockAccessListEncode {
            version: 0,
            number: 12_345_678,
            hash: [0xab; 32],
            sign_data: Vec::new(),
            accounts: vec![
                AccountAccessListEncode {
                    tx_index: 1,
                    address: [0xcd; 20],
                    storage_items: vec![
                        StorageAccessItem {
                            tx_index: 1,
                            dirty: false,
                            key: [0x11; 32],
                            value: [0x00; 32],
                        },
                        StorageAccessItem {
                            tx_index: 2,
                            dirty: true,
                            key: [0x22; 32],
                            value: [0x33; 32],
                        },
                    ],
                },
                AccountAccessListEncode {
                    tx_index: 0,
                    address: [0xcc; 20], // Smaller address for sorting test
                    storage_items: vec![],
                },
            ],
        };

        let encoded = original.eip7928_encode();
        let decoded = Rlp::new(&encoded);

        // Should be a list of AccountChanges
        assert_eq!(decoded.item_count().unwrap(), 2);
        
        // First account should be 0xcc (sorted)
        let acc_cc = decoded.at(0).unwrap();
        assert_eq!(acc_cc.val_at::<Vec<u8>>(0).unwrap(), vec![0xcc; 20]);
        
        // Second account should be 0xcd
        let acc_cd = decoded.at(1).unwrap();
        assert_eq!(acc_cd.val_at::<Vec<u8>>(0).unwrap(), vec![0xcd; 20]);
        
        // storage_changes for 0xcd
        let changes = acc_cd.at(1).unwrap();
        assert_eq!(changes.item_count().unwrap(), 1);
        let first_change = changes.at(0).unwrap();
        assert_eq!(first_change.val_at::<Vec<u8>>(0).unwrap(), vec![0x22; 32]);
        let index_vals = first_change.at(1).unwrap();
        let first_index_val = index_vals.at(0).unwrap();
        assert_eq!(first_index_val.val_at::<u16>(0).unwrap(), 2);
        assert_eq!(first_index_val.val_at::<Vec<u8>>(1).unwrap(), vec![0x33; 32]);
        
        // storage_reads for 0xcd
        let reads = acc_cd.at(2).unwrap();
        assert_eq!(reads.item_count().unwrap(), 1);
        assert_eq!(reads.val_at::<Vec<u8>>(0).unwrap(), vec![0x11; 32]);
        
        // Empty fields
        assert_eq!(acc_cd.at(3).unwrap().item_count().unwrap(), 0);
        assert_eq!(acc_cd.at(4).unwrap().item_count().unwrap(), 0);
        assert_eq!(acc_cd.at(5).unwrap().item_count().unwrap(), 0);
    }
}
