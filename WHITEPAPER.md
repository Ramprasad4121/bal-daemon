# BAL Daemon: Automated BEP-592 Payload Generation for BSC Block Builders

**Author:** Ramprasad (@0xramprasad)
**Date:** March 2026
**Related:** https://github.com/bnb-chain/bsc/issues/3596

## Abstract

The adoption of BEP-592 has been hindered by a significant tooling gap, leaving the performance benefits of Block-Level Access Lists (BAL) largely theoretical. The BAL Daemon addresses this by providing an automated pipeline for generating BEP-592 RLP payloads at mempool speeds. By subscribing to new block heads and performing parallel transaction simulations using `debug_traceCall`, the daemon extracts storage access patterns and encodes them into valid BAL payloads. Testnet results demonstrate the system's efficiency, successfully processing 33 transactions with 0 failures and capturing 264 storage slots across 5 blocks, proving its readiness for builder integration.

## 1. Background

### 1.1 BEP-592 and the Fermi Upgrade
BEP-592 introduced Block-Level Access Lists (BAL) to the BNB Smart Chain, shipping with the Fermi hardfork in January 2026. A BAL allows block builders to explicitly list the storage slots that will be accessed by transactions within a block. This enables BSC nodes to pre-fetch these slots from disk before execution begins, significantly reducing I/O wait times. Core team benchmarks indicate that blocks including a valid BAL experience 34.7% less CPU usage and 12.1% higher throughput. Details can be found in [BSC Issue #3596](https://github.com/bnb-chain/bsc/issues/3596).

### 1.2 The Adoption Problem
Despite the clear performance gains, adoption of BEP-592 among builders is near zero. The barrier is the computational overhead of generating the payload itself. To produce a valid BAL, a builder must:
1. Simulate every pending transaction against the latest state root.
2. Trace every `SLOAD` and `SSTORE` operation.
3. Consolidate and RLP-encode the results.
This must be completed within the tight 0.45-second block interval of BSC. Without specialized tooling, builders opt to skip BAL generation to avoid delaying block production.

### 1.3 Why This Matters
The performance improvements offered by the Fermi upgrade remain dormant as long as builders do not include BAL payloads. The infrastructure is in place, but the lack of automation creates a bottleneck that prevents the network from reaching its full potential. Bridging this gap is essential for scaling BSC.

## 2. Technical Design

### 2.1 Staleness at 0.45-Second Block Times
In an environment with 0.45-second block times, staleness is a primary challenge. A simulation started on block $N$ might finish after block $N+1$ has already been propagated, making the access list invalid for the current state. The BAL Daemon handles this using a `CancellationToken` pattern. When a new head is detected, any in-progress simulations are immediately aborted. Furthermore, the daemon performs a final hash verification: if the current head hash at the time of encoding does not match the hash used at the start of simulation, the results are discarded as stale.

### 2.2 Parallel Transaction Simulation
To meet the latency requirements, the daemon utilizes Rust's `tokio` runtime to simulate all pending transactions in parallel. Each transaction is handled by a separate task, ensuring that the total simulation time is limited by the slowest individual transaction rather than the sum of all transactions. Results are then merged into a unified data structure, deduplicating slots by address and promoting "dirty" flags if any transaction performed a write operation.

### 2.3 prestateTracer and Storage Tracing
The daemon employs the `debug_traceCall` RPC method with the `prestateTracer` in `diffMode`. This approach is highly efficient as it provides a map of storage slots modified ("post" map) and slots only read ("pre" map). This eliminates the need for expensive opcode parsing or manual state tracking, allowing the daemon to focus on the high-level access patterns required by BEP-592.

### 2.4 BEP-592 RLP Encoding
The core of the daemon is the RLP encoder that implements the `BlockAccessListEncode` structure. This follows the exact specification used in the BSC core codebase (`core/types/block.go`), ensuring that the generated payloads are natively compatible with BSC nodes.

### 2.5 EIP-7928 Compatibility
The BAL Daemon also includes support for EIP-7928, which proposes a similar access list mechanism for Ethereum. While BEP-592 and EIP-7928 differ in their RLP structures and specific field requirements, they share the same underlying simulation logic. By maintaining a dual-encoder architecture, the daemon provides a seamless migration path should BSC align closer with EIP-7928 in future upgrades.

## 3. Results

### 3.1 Integration Test
The integration test verifies the daemon's ability to interact with the BSC Chapel testnet. It successfully identifies recent contract interactions, traces them, and generates a valid RLP payload.
- **Example Run:**
  - **Block:** #96583654
  - **Accounts:** 2
  - **Writes:** 10
  - **Size:** 459 bytes
  - **Result:** PASS

### 3.2 Load Test
A load test conducted against the Chapel testnet (via NodeReal) demonstrates sustained performance:
| Block | Txs | Success | Fail | Time(ms) | TPS | Accounts | Slots |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| 97927748 | 10 | 10 | 0 | 667 | 14.99 | 12 | 109 |
| 97927747 | 8 | 8 | 0 | 256 | 31.25 | 8 | 57 |
| 97927746 | 8 | 8 | 0 | 176 | 45.45 | 9 | 64 |
| 97927745 | 3 | 3 | 0 | 169 | 17.75 | 4 | 21 |
| 97927744 | 4 | 4 | 0 | 186 | 21.51 | 3 | 13 |
| **TOTAL** | **33** | **33** | **0** | **1454** | **22.70** | **36** | **264** |

### 3.3 Staleness Behavior
Logs indicate that the aggressive block times on testnet frequently trigger the cancellation logic. This confirms that the daemon correctly prioritizes data freshness over completeness, avoiding the production of invalid payloads that would be rejected by the network.

## 4. Limitations and Future Work

### 4.1 RPC Requirements
The reliance on `debug_traceCall` with `prestateTracer` necessitates access to a debug-enabled RPC endpoint. While available on testnets and via paid providers, production environments should ideally run the daemon alongside a local BSC node to minimize latency and eliminate RPC costs.

### 4.2 Mainnet Testing
Due to the requirement for debug-enabled RPCs, mainnet validation is pending. High-traffic DeFi workloads on mainnet are expected to result in larger access lists and higher contention, which will provide a more rigorous test of the merging and encoding logic.

### 4.3 Builder Integration
While the JSON-RPC server is functional, the final step is the end-to-end integration with a production block builder. This will involve the builder calling the `/generate_bal` endpoint as part of its block assembly process.

### 4.4 EIP-7928 Migration
The EIP-7928 encoder is implemented but lacks validation against a live node supporting that specific standard. Future work will include testing against compatible testnets as they become available.

## 5. Conclusion

The BAL Daemon provides the necessary bridge between the BEP-592 specification and its practical application. By automating the complex task of simulation and encoding, it enables builders to finally realize the performance gains of the Fermi upgrade. The system is robust, handles the realities of 0.45s block times, and is ready for the next phase of mainnet testing and builder adoption.
