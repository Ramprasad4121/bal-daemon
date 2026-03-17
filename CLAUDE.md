


# bal-daemon

A Rust daemon that generates BEP-592 Block-Level Access List payloads for BSC block builders.

## What this is

BEP-592 is live on BSC since the Fermi hardfork (January 2026). When a builder includes a
BAL payload with a block, the node pre-fetches all required storage slots before execution.
The core team's benchmarks show 34.7% less CPU usage when BAL is present. Adoption is near
zero because no tooling exists to generate these payloads automatically. This daemon fixes that.

## What it does

1. Connects to a BSC node via WebSocket
2. Subscribes to new heads
3. On each new head, fetches pending transactions from the mempool
4. Simulates each transaction using debug_traceCall to trace SLOAD and SSTORE operations
5. Encodes the result as a valid BEP-592 RLP payload (BlockAccessListEncode)
6. Outputs the payload for the builder to attach to the block bid

## BEP-592 payload structure

```rust
struct BlockAccessListEncode {
    version: u32,           // 0 for current version
    number: u64,            // block number
    hash: [u8; 32],         // block hash
    sign_data: Vec<u8>,     // validator signature (empty in daemon output, builder signs)
    accounts: Vec<AccountAccessListEncode>,
}

struct AccountAccessListEncode {
    tx_index: u32,          // first tx in block that accessed this account
    address: [u8; 20],
    storage_items: Vec<StorageAccessItem>,
}

struct StorageAccessItem {
    tx_index: u32,          // first tx in block that accessed this slot
    dirty: bool,            // true = SSTORE (write), false = SLOAD (read)
    key: [u8; 32],          // storage slot key
}
```

## Staleness problem

BSC post-Fermi produces a new block every 0.45 seconds. A cached access set generated
against block N's state root is potentially wrong by block N+1. The daemon must:
- Cancel in-progress simulations when a new head arrives
- Discard any access set generated against a stale state root
- Re-simulate against the new head's state root immediately

## RLP encoding note

BEP-592 uses RLP encoding. EIP-7928 (the future consensus-critical version) uses a
different structure — separate storage_reads and storage_changes, post-transaction values,
balance/nonce/code tracking, strict lexicographic ordering, no SignData. The simulation
core should be kept separate from the encoder so switching to EIP-7928 output is an
encoder swap, not a rewrite.

## Tech stack

- Rust (async, tokio)
- ethers-rs for BSC node communication
- rlp crate for BEP-592 encoding
- tokio CancellationToken for staleness handling

## Target network

BSC testnet for development: wss://bsc-testnet-rpc.publicnode.com
BSC mainnet for production: wss://bsc-rpc.publicnode.com

## Current goal

Minimal working daemon: one transaction, one correct BEP-592 payload, end to end.
Not the full thing yet. Prove the encode/decode cycle works first.

## What success looks like

The daemon prints a hex-encoded RLP payload and a summary:
- Block number and hash
- Number of accounts in the access list
- Number of storage slots (reads vs writes)
- Whether the state root was fresh or stale at encode time

