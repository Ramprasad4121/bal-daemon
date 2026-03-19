# bal-daemon

I am building `bal-daemon` as a small Rust daemon that generates BEP-592 Block-Level Access List payloads for BSC block builders. It watches new heads, simulates pending transactions against the latest state, and emits an RLP-encoded payload that a builder can attach to a block.

## Why It Matters

BEP-592 is already live on BSC, but almost nobody is generating these payloads yet. That means builders leave a performance feature unused even though the protocol is ready for it. I wrote this project to close that gap and make BAL generation practical from a normal daemon instead of custom node code.

## How It Works

- I subscribe to new heads over WebSocket and keep track of the freshest block hash.
- I fetch pending transactions from the mempool and simulate them against the current head.
- I trace storage reads and writes with `debug_traceCall` using `prestateTracer` in `diffMode`.
- I merge the traced accesses into one BEP-592 block payload across all pending transactions I simulate.
- I RLP-encode the result and print a hex payload plus a short summary.

## How To Run It

Right now I keep the RPC endpoints in `src/main.rs`. To run the daemon against NodeReal testnet, I set `ws_url` and `http_url` to my NodeReal endpoints, then start the binary from the repo root:

```bash
cargo run --bin bal-daemon
```

The WebSocket endpoint should look like:

```text
wss://bsc-testnet.nodereal.io/ws/v1/<your-key>
```

The matching HTTP endpoint should use the same key for `debug_traceCall`.

## How To Run Tests

I use the normal Rust test target for fast checks:

```bash
cargo test
```

I use the integration binary for a local RPC check against Anvil or another local node:

```bash
cargo run --bin integration_test
```

For the local integration path, I start Anvil in no-mining mode, submit a pending deployment transaction, and then run the binary so it can trace the mempool entry before anything gets mined.

## Architecture

### Staleness Handling

BSC block times are short enough that an access list can go stale almost immediately. I cancel in-flight simulation work on every new head, keep the latest head hash in shared state, and discard any access set that was produced against an older block hash before I encode the payload.

### Multi-Tx Simulation

The daemon does not stop at one transaction anymore. I fetch all pending transactions from the pending block view, simulate them in parallel, and merge their account and slot accesses into one `BlockAccessListEncode`. If the same slot appears multiple times, I keep the earliest `tx_index` and mark it dirty if any transaction wrote to it.

### RLP Encoding

The payload encoder mirrors the current BEP-592 layout: version, block number, block hash, empty sign data, and then per-account storage access items. I also keep a round-trip test in `src/main.rs` so I can verify the encoded bytes decode back to the expected structure.

## Issue

GitHub issue: https://github.com/bnb-chain/bsc/issues/3596

## Author

Ramprasad (@0xramprasad)  
github.com/Ramprasad4121
