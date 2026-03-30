# bal-daemon

A Rust daemon that generates BEP-592 Block-Level Access List payloads for BSC block builders.

BEP-592 shipped with the Fermi hardfork in January 2026. When a builder includes a BAL with a block, the BSC node pre-fetches all required storage slots before execution starts. The core team's own benchmarks show 34.7% less CPU usage and 12.1% higher throughput when BAL is present. Adoption is near zero because no tooling exists to generate valid payloads at mempool speed. This daemon fixes that.

## The problem in one paragraph

To generate a valid BEP-592 RLP payload, a builder has to simulate every pending transaction against the latest state root, trace every SLOAD and SSTORE, and encode the result correctly — all before the next block arrives at 0.45-second intervals. Without automation, builders skip it entirely and fall back to serial execution. The performance gains from BEP-592 stay theoretical.

## How it works

The daemon subscribes to new heads via WebSocket. On each new head it spawns parallel simulation tasks — one per pending transaction — using `debug_traceCall` with `prestateTracer` in `diffMode`. Each task traces storage reads and writes for its transaction. The results are merged into a single `BlockAccessListEncode` and RLP-encoded into a BEP-592 payload.

Staleness is handled explicitly. If a new head arrives while simulation is running, a `CancellationToken` fires and all in-progress tasks are discarded. Before encoding, the daemon checks that the block hash at simulation start still matches the current head. If it does not match, the access set is thrown out. Fresh vs stale counts are logged per run.

The daemon also includes an EIP-7928 encoder alongside BEP-592. Same simulation core, different output format. When EIP-7928 becomes consensus-critical, switching encoders is a one-line change.

## Architecture

```text
       ┌─────────────────────────────────────────────────────────┐
       │                    NewHeadEvent (WS)                    │
       └────────────────────────────┬────────────────────────────┘
                                    │
                    ┌───────────────┴───────────────-----┐
                    │  cancel previous CancellationToken │
                    ├───────────────────────────────-----┤
                    │  fetch pending txs (mempool)       │
                    └───────────────┬───────────────-----┘
                                    │
                    ┌───────────────┴───────────────----┐
                    │  spawn parallel simulation tasks  │
                    └───────────────┬───────────────----┘
                                    │
            ┌───────────────────────┴───────────────────────┐
            │       debug_traceCall (prestateTracer)        │
            │     extracts storage reads/writes per tx      │
            └───────────────────────┬───────────────────────┘
                                    │
                    ┌───────────────┴──────────────--------─┐
                    │       merge results by address        │
                    │   dedup slots, set tx_index, flags    │
                    └───────────────┬───────────────--------┘
                                    │
                    ┌───────────────┴───────────────----┐
                    │    staleness check (head hash)    │
                    └───────────────┬──────────────----─┘
                                    │
                ┌───────────────────┴───────────────────┐
                │                                       │
        [ FRESH ]                                 [ STALE ]
                │                                       │
      ┌─────────┴─────────┐                     ┌───────┴───────┐
      │  RLP encode       │                     │    discard    │
      │  output BEP-592   │                     │               │
      └───────────────────┘                     └───────────────┘
```

## Testnet results

Load test against BSC Chapel testnet (NodeReal):

| Block | Txs | Success | Fail | Time(ms) | TPS | Accounts | Slots |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| 97927748 | 10 | 10 | 0 | 667 | 14.99 | 12 | 109 |
| 97927747 | 8 | 8 | 0 | 256 | 31.25 | 8 | 57 |
| 97927746 | 8 | 8 | 0 | 176 | 45.45 | 9 | 64 |
| 97927745 | 3 | 3 | 0 | 169 | 17.75 | 4 | 21 |
| 97927744 | 4 | 4 | 0 | 186 | 21.51 | 3 | 13 |
| **TOTAL** | **33** | **33** | **0** | **1454** | **22.70** | **36** | **264** |

Integration test against real BSC Chapel testnet contract interactions:

*   **Block:** #96583654
*   **Tx:** 0xd026db...
*   **Accounts:** 2
*   **Writes:** 10
*   **Size:** 459 bytes
*   **Result:** PASS

Related issue: https://github.com/bnb-chain/bsc/issues/3596

## Requirements

- Rust 1.80+
- A BSC node RPC with `debug_traceCall` enabled (NodeReal, QuickNode, or local node)
- WebSocket endpoint for new head subscription
- Environment file (.env) for RPC URLs

## Run
```bash
# Clone
git clone https://github.com/Ramprasad4121/bal-daemon.git
cd bal-daemon

# Create .env with your RPC endpoints
echo "WS_URL=wss://..." > .env
echo "HTTP_URL=https://..." >> .env

# Run the daemon
cargo run --bin bal-daemon

# Run the JSON-RPC server on port 7337
cargo run --bin server

# Run tests
cargo test

# Run integration test against BSC Chapel testnet
cargo run --bin integration_test

# Run load test
cargo run --bin load_test
```

## JSON-RPC interface

The server binary exposes one endpoint:
`POST /generate_bal`
Content-Type: application/json
```json
{
"transactions": [
{
"from": "0x...",
"to": "0x...",
"data": "0x...",
"gas": "0x...",
"gasPrice": "0x...",
"value": "0x..."
}
],
"block_number": "0x..."
}
```
Response:
```json
{
  "bep592_payload": "0x...",
  "eip7928_payload": "0x...",
  "accounts": 3,
  "reads": 5,
  "writes": 10,
  "size_bytes": 459,
  "fresh": true
}
```

## Status

- New head subscription and staleness cancellation — done
- Multi-transaction parallel simulation — done
- BEP-592 RLP encoding — done and unit tested
- EIP-7928 encoder — done
- JSON-RPC server — done
- Integration test against real BSC Chapel testnet — passing
- Load test: 33 txs, 0 failures, 264 storage slots

## What is left

- Mainnet testing — requires a debug-enabled paid RPC or local BSC node
- Builder integration — getting the daemon running alongside a real BSC block builder
- Scheduler BEP — once benchmark data exists from running at scale

## Author

Ramprasad
github.com/Ramprasad4121
x.com/0xramprasad

Related: https://github.com/bnb-chain/bsc/issues/3596
