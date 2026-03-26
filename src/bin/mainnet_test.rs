use bal_daemon::*;
use ethers::providers::{Middleware, Provider, Http, Ws};
use ethers::types::{BlockId, H256, BlockNumber};
use std::time::{Instant, Duration};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use futures_util::StreamExt;
use tracing::{error, info, warn};

const MAINNET_HTTP_URL: &str = "https://bsc-mainnet.nodereal.io/v1/64a9df0874fb4a93b9d0a3849de012d3";
const MAINNET_WSS_URL: &str = "wss://bsc-mainnet.nodereal.io/ws/v1/64a9df0874fb4a93b9d0a3849de012d3";

#[derive(Default)]
struct MainnetCounters {
    rate_limited: AtomicU64,
    retries_success: AtomicU64,
    retries_failed: AtomicU64,
}

struct BlockStats {
    number: u64,
    tx_count: usize,
    success_count: usize,
    fail_count: usize,
    time_ms: u128,
    accounts: usize,
    slots: usize,
    payload_size: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let counters = Arc::new(MainnetCounters::default());
    let provider = Provider::<Http>::try_from(MAINNET_HTTP_URL)?;
    let http_client = reqwest::Client::new();

    info!("--- PHASE 1: Mined Block Load Test (Mainnet) ---");
    
    let mut target_blocks = Vec::new();
    let mut current_num = provider.get_block_number().await?.as_u64();

    while target_blocks.len() < 5 {
        info!("Fetching block {}...", current_num);
        let block = provider
            .get_block_with_txs(BlockId::from(current_num))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Block not found"))?;

        let has_contract_interactions = block.transactions.iter().any(|tx| !tx.input.is_empty());

        if has_contract_interactions {
            target_blocks.push(block);
            info!("Found block with contract interactions: {}", current_num);
        }
        current_num -= 1;
    }

    let mut all_stats = Vec::new();

    for block in target_blocks {
        let block_num = block.number.unwrap().as_u64();
        let block_hash = block.hash.unwrap();
        let mut txs = block.transactions;
        
        // Cap at 20 transactions
        if txs.len() > 20 {
            txs.truncate(20);
        }
        let tx_count = txs.len();

        info!("Simulating block #{} (capped at {} txs with 200ms stagger)...", block_num, tx_count);

        let start = Instant::now();
        let mut tx_tasks = JoinSet::new();

        for (index, tx) in txs.into_iter().enumerate() {
            let client = http_client.clone();
            let counters = Arc::clone(&counters);
            tx_tasks.spawn(async move {
                // Staggered start: index * 200ms
                tokio::time::sleep(Duration::from_millis(index as u64 * 200)).await;
                simulate_mined_with_retry(client, tx.hash, index as u32, counters).await
            });
        }

        let mut simulations = Vec::with_capacity(tx_count);
        let mut success_count = 0;
        let mut fail_count = 0;

        while let Some(join_result) = tx_tasks.join_next().await {
            match join_result {
                Ok(Ok(sim)) => {
                    simulations.push(sim);
                    success_count += 1;
                }
                _ => {
                    fail_count += 1;
                }
            }
        }

        let head = HeadContext {
            number: block_num,
            hash: block_hash,
        };

        let (bal, total_slots, _reads, _writes) = merge_simulations(head, simulations);
        let payload = bal.rlp_encode();
        let duration = start.elapsed();

        all_stats.push(BlockStats {
            number: block_num,
            tx_count,
            success_count,
            fail_count,
            time_ms: duration.as_millis(),
            accounts: bal.accounts.len(),
            slots: total_slots,
            payload_size: payload.len(),
        });
    }

    print_summary_table(all_stats, &counters);

    info!("--- PHASE 2: Live Staleness Measurement (30s) ---");
    measure_staleness(counters).await?;

    Ok(())
}

async fn simulate_mined_with_retry(
    client: reqwest::Client,
    tx_hash: H256,
    index: u32,
    counters: Arc<MainnetCounters>,
) -> anyhow::Result<SimulatedTransaction> {
    let rpc_body = build_trace_transaction_body(tx_hash);
    
    match trace_call(&client, MAINNET_HTTP_URL, rpc_body.clone()).await {
        Ok(res) => Ok(SimulatedTransaction {
            tx_index: index,
            accounts: parse_prestate_diff(&res, index),
        }),
        Err(e) if e.to_string().contains("429") => {
            counters.rate_limited.fetch_add(1, Ordering::Relaxed);
            warn!("Rate limited on tx {:#x}, retrying in 2s...", tx_hash);
            tokio::time::sleep(Duration::from_secs(2)).await;
            
            match trace_call(&client, MAINNET_HTTP_URL, rpc_body).await {
                Ok(res) => {
                    counters.retries_success.fetch_add(1, Ordering::Relaxed);
                    Ok(SimulatedTransaction {
                        tx_index: index,
                        accounts: parse_prestate_diff(&res, index),
                    })
                }
                Err(e) => {
                    counters.retries_failed.fetch_add(1, Ordering::Relaxed);
                    error!("Retry failed for tx {:#x}: {}", tx_hash, e);
                    Err(e)
                }
            }
        }
        Err(e) => Err(e),
    }
}

async fn measure_staleness(mainnet_counters: Arc<MainnetCounters>) -> anyhow::Result<()> {
    let http_client = reqwest::Client::new();
    let sim_counters = Arc::new(SimulationCounters::default());
    let (current_head_tx, current_head_rx) = watch::channel::<Option<H256>>(None);
    
    let start_time = Instant::now();
    let duration = Duration::from_secs(30);
    let mut reconnect_count = 0;

    info!("Listening for new heads on mainnet for 30s with auto-reconnect...");

    while start_time.elapsed() < duration {
        let ws_provider = match Provider::<Ws>::connect(MAINNET_WSS_URL).await {
            Ok(p) => p,
            Err(e) => {
                reconnect_count += 1;
                if reconnect_count > 3 {
                    error!("Max reconnect attempts reached. Ending test early.");
                    break;
                }
                warn!("WS connect failed: {}. Retrying in 3s...", e);
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };

        let mut stream = match ws_provider.subscribe_blocks().await {
            Ok(s) => s,
            Err(e) => {
                warn!("WS subscription failed: {}. Retrying...", e);
                continue;
            }
        };

        let mut active_simulation: Option<CancellationToken> = None;

        loop {
            let elapsed = start_time.elapsed();
            if elapsed >= duration {
                break;
            }

            tokio::select! {
                res = stream.next() => {
                    match res {
                        Some(block) => {
                            let block_number = block.number.unwrap().as_u64();
                            let block_hash = block.hash.unwrap();
                            info!("New head: #{} {:#x}", block_number, block_hash);
                            let _ = current_head_tx.send(Some(block_hash));

                            let cancellation = CancellationToken::new();
                            if let Some(previous) = active_simulation.take() {
                                previous.cancel();
                            }
                            active_simulation = Some(cancellation.clone());

                            let head = HeadContext { number: block_number, hash: block_hash };
                            let ws_provider = ws_provider.clone();
                            let http_client = http_client.clone();
                            let current_head_rx = current_head_rx.clone();
                            let sim_counters = Arc::clone(&sim_counters);
                            let mainnet_counters = Arc::clone(&mainnet_counters);

                            tokio::spawn(async move {
                                let _ = simulate_live_head(
                                    ws_provider,
                                    http_client,
                                    head,
                                    current_head_rx,
                                    cancellation,
                                    sim_counters,
                                    mainnet_counters,
                                ).await;
                            });
                        }
                        None => {
                            warn!("WS stream closed. Reconnecting...");
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
        
        if start_time.elapsed() >= duration {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    let (fresh, stale) = sim_counters.snapshot();
    let total = fresh + stale;
    let rate = if total > 0 { (fresh as f64 / total as f64) * 100.0 } else { 0.0 };

    info!("Staleness Stats:");
    info!("  Total Attempts: {}", total);
    info!("  Fresh Count:    {}", fresh);
    info!("  Stale Count:    {}", stale);
    info!("  Fresh Rate:     {:.2}%", rate);

    Ok(())
}

async fn simulate_live_head(
    provider: Provider<Ws>,
    http_client: reqwest::Client,
    head: HeadContext,
    current_head_rx: watch::Receiver<Option<H256>>,
    cancellation: CancellationToken,
    sim_counters: Arc<SimulationCounters>,
    mainnet_counters: Arc<MainnetCounters>,
) -> anyhow::Result<()> {
    let pending_block = tokio::select! {
        _ = cancellation.cancelled() => {
            sim_counters.record_stale();
            return Ok(());
        }
        res = provider.get_block_with_txs(BlockNumber::Pending) => res?,
    };

    let txs = match pending_block {
        Some(b) if !b.transactions.is_empty() => b.transactions,
        _ => return Ok(()),
    };

    let mut tx_tasks = JoinSet::new();
    for (index, tx) in txs.into_iter().take(20).enumerate() {
        let client = http_client.clone();
        let cancel = cancellation.clone();
        let mainnet_counters = Arc::clone(&mainnet_counters);
        
        tx_tasks.spawn(async move {
            tokio::time::sleep(Duration::from_millis(index as u64 * 200)).await;
            
            let rpc_body = build_trace_call_body(&tx, head.number);
            let mut result = tokio::select! {
                _ = cancel.cancelled() => return None,
                res = trace_call(&client, MAINNET_HTTP_URL, rpc_body.clone()) => res,
            };

            if let Err(ref e) = result {
                if e.to_string().contains("429") {
                    mainnet_counters.rate_limited.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    result = tokio::select! {
                        _ = cancel.cancelled() => return None,
                        res = trace_call(&client, MAINNET_HTTP_URL, rpc_body) => {
                            if res.is_ok() { mainnet_counters.retries_success.fetch_add(1, Ordering::Relaxed); }
                            else { mainnet_counters.retries_failed.fetch_add(1, Ordering::Relaxed); }
                            res
                        },
                    };
                }
            }

            result.ok().map(|_| ())
        });
    }

    while let Some(res) = tx_tasks.join_next().await {
        if res.is_err() || res.unwrap().is_none() {
            if cancellation.is_cancelled() {
                sim_counters.record_stale();
                return Ok(());
            }
        }
    }

    if *current_head_rx.borrow() == Some(head.hash) {
        sim_counters.record_fresh();
    } else {
        sim_counters.record_stale();
    }

    Ok(())
}

fn print_summary_table(stats: Vec<BlockStats>, counters: &MainnetCounters) {
    println!("\n┌──────────┬────────┬─────────┬────────┬──────────┬──────────┬──────────┬──────────┬──────────┐");
    println!("│ Block    │ Txs    │ Success │ Fail   │ Time(ms) │ TPS      │ Accounts │ Slots    │ Size(B)  │");
    println!("├──────────┼────────┼─────────┼────────┼──────────┼──────────┼──────────┼──────────┼──────────┤");

    let mut total_txs = 0;
    let mut total_success = 0;
    let mut total_fail = 0;
    let mut total_time_ms = 0;
    let mut total_accounts = 0;
    let mut total_slots = 0;
    let mut total_size = 0;

    for s in &stats {
        let tps = if s.time_ms > 0 { (s.tx_count as f64 / (s.time_ms as f64 / 1000.0)) } else { 0.0 };
        println!(
            "│ {:<8} │ {:<6} │ {:<7} │ {:<6} │ {:<8} │ {:<8.2} │ {:<8} │ {:<8} │ {:<8} │",
            s.number, s.tx_count, s.success_count, s.fail_count, s.time_ms, tps, s.accounts, s.slots, s.payload_size
        );
        total_txs += s.tx_count;
        total_success += s.success_count;
        total_fail += s.fail_count;
        total_time_ms += s.time_ms;
        total_accounts += s.accounts;
        total_slots += s.slots;
        total_size += s.payload_size;
    }

    println!("├──────────┼────────┼─────────┼────────┼──────────┼──────────┼──────────┼──────────┼──────────┤");
    let avg_tps = if total_time_ms > 0 { (total_txs as f64 / (total_time_ms as f64 / 1000.0)) } else { 0.0 };
    println!(
        "│ TOTAL    │ {:<6} │ {:<7} │ {:<6} │ {:<8} │ {:<8.2} │ {:<8} │ {:<8} │ {:<8} │",
        total_txs, total_success, total_fail, total_time_ms, avg_tps, total_accounts, total_slots, total_size
    );
    println!("└──────────┴────────┴─────────┴────────┴──────────┴──────────┴──────────┴──────────┴──────────┘");
    
    println!("\nRate Limit Stats:");
    println!("  Total 429s:      {}", counters.rate_limited.load(Ordering::Relaxed));
    println!("  Retries Success: {}", counters.retries_success.load(Ordering::Relaxed));
    println!("  Retries Failed:  {}\n", counters.retries_failed.load(Ordering::Relaxed));
}
