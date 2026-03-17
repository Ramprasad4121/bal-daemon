# BAL Daemon — Design Document

**Author:** Ramprasad (@0xramprasad)  
**Related issue:** [bnb-chain/bsc#3596](https://github.com/bnb-chain/bsc/issues/3596)  
**Status:** Pre-implementation research

---

## What this document covers

Before writing any code, I went through two problems Lucas flagged in his review:

1. Simulation staleness — the state root changes every 0.45 seconds post-Fermi, so how does the daemon avoid generating stale access sets
2. RLP encoding differences between BEP-592 and EIP-7928 — they look similar but are not identical, and getting this wrong early causes pain later

This document records what I found and how I plan to handle both.

---

## Part 1: Staleness Strategy

### How the node currently uses BAL

Reading `core/state_prefetcher.go`, the flow is:

1. A block arrives with a BAL attached
2. `PrefetchBAL()` parses the BAL into a `BlockAccessListPrefetch` indexed by transaction position
3. 8 goroutines pre-fetch snapshot cache (`PrefetchBALSnapshot`)
4. 8 more goroutines pre-fetch MPT trie nodes (`PrefetchBALTrie`)
5. Both use an `interruptChan` so they can be cancelled immediately if the block goes stale

The node already expects interruption. The daemon needs the same discipline.

### The actual staleness problem

The node currently generates BAL as a byproduct of executing the block — it already knows the exact state. My daemon has to predict the access set from pending mempool transactions before the block is built, against a state root that changes every 0.45 seconds.

If the state root moves one block forward between simulation and block propagation, a storage slot that changed in that block will produce a wrong access set. The node's prefetcher loads the wrong data. Best case: unnecessary rollbacks. Worst case: invalid block.

### How I found the subscription mechanism

In `eth/handler.go`:

```go
headCh := make(chan core.ChainHeadEvent, chainHeadChanSize)
headSub := chain.SubscribeChainHeadEvent(headCh)
```

Every new head fires a `ChainHeadEvent`. The daemon mirrors this via the BSC node's WebSocket RPC using `eth_subscribe newHeads`.

### The staleness strategy

**On every new head:**
- Update `currentStateRoot` to the new block's state root
- Close the active `interruptChan` — this cancels all in-progress simulations
- Open a new `interruptChan`
- Invalidate all cached access sets generated against the previous state root
- Re-simulate pending mempool transactions against the new root

**On reorg:**
A reorg means the parent hash changes. When a new head arrives with a different parent than expected, flush the entire cache and re-simulate from scratch against the new head's state root.

**Pseudocode:**

```
on NewHeadEvent(block):
    if block.parentHash != currentHead.hash:
        flushEntireCache()          // reorg detected
    else:
        invalidateCacheForSlots()   // normal new head
    
    close(interruptChan)            // cancel in-progress simulations
    interruptChan = make(chan struct{})
    currentStateRoot = block.stateRoot
    
    go simulatePending(mempool.pending(), currentStateRoot, interruptChan)
```

**One thing I'm not sure about yet:** how aggressively to re-simulate. Simulating the entire mempool on every new head at 0.45s intervals is expensive. I'll probably need to prioritise transactions that are likely to be included in the next block (high gas price, low nonce) and simulate those first. I'll figure this out once I have a working baseline.

---

## Part 2: RLP Encoding Differences

### BEP-592 structure

```go
type StorageAccessItem struct {
    TxIndex uint32      // first tx in block that accessed this slot
    Dirty   bool        // true = written, false = read-only
    Key     common.Hash
}

type AccountAccessListEncode struct {
    TxIndex      uint32
    Address      common.Address
    StorageItems []StorageAccessItem
}

type BlockAccessListEncode struct {
    Version  uint32
    Number   uint64
    Hash     common.Hash
    SignData []byte      // 65-byte validator signature
    Accounts []AccountAccessListEncode
}
```

### EIP-7928 structure

```python
AccountChanges = [
    Address,
    List[SlotChanges],      # storage writes: slot -> [[block_access_index, new_value]]
    List[StorageKey],       # storage reads (read-only keys, no values)
    List[BalanceChange],    # [[block_access_index, post_balance]]
    List[NonceChange],      # [[block_access_index, new_nonce]]
    List[CodeChange]        # [[block_access_index, new_code]]
]

BlockAccessList = List[AccountChanges]
```

### Divergence table

| Field | BEP-592 | EIP-7928 |
|---|---|---|
| Consensus | Non-consensus, optional | Consensus-critical, block invalid if BAL mismatches |
| Header | Not in header | `block_access_list_hash` in block header |
| Transport | Attached to block body | Sent separately via Engine API |
| Storage reads vs writes | Single `StorageAccessItem` with `Dirty bool` | Separated: `storage_changes` for writes, `storage_reads` for reads |
| Post-execution values | Not stored — keys only | Stored for every write |
| Balance / nonce / code | Not tracked | Fully tracked per-transaction |
| Transaction index | `TxIndex uint32` — first tx that accessed the slot | `BlockAccessIndex uint16` — every tx that caused a change; 0 = pre-execution, 1..n = txs, n+1 = post-execution |
| Signature | 65-byte `SignData` | No signature field |
| Size limit | 1MB hard cap | Gas-budget based: `bal_items <= block_gas_limit / 2000` |
| Ordering | Not specified | Strict: addresses lexicographic, slots lexicographic, changes ascending by index |
| Validation | None — bad BAL falls back to serial execution | Full validation — block rejected if generated BAL doesn't match |

### What this means for implementation

The simulation core — tracing SLOAD and SSTORE per transaction — is the same work for both formats. The difference is in the encoder.

For BEP-592 output:
- Flatten to one `StorageAccessItem` per slot using `Dirty bool`
- Record only the first `TxIndex` that accessed the slot
- Sign with `SignData`
- No ordering requirement

For EIP-7928 output (future milestone):
- Separate reads and writes explicitly
- Record post-transaction values for every write
- Track balance, nonce, and code changes per transaction
- Sort addresses lexicographically, slots lexicographically, changes by index ascending
- No signature

I'll build the simulation layer once and write two encoders. Switching from BEP-592 to EIP-7928 becomes an encoder swap, not a rewrite.

---

## What comes next

With these two problems understood, the next step is a minimal working daemon:

1. Subscribe to new heads via `eth_subscribe newHeads`
2. On each new head, simulate one pending transaction against the latest state root
3. Trace SLOAD and SSTORE operations
4. Output a valid BEP-592 RLP payload
5. Verify the node accepts it

Once one transaction works end to end, I'll scale to full mempool simulation with the staleness strategy above.

I'll post the repo link on the issue once this is working.
