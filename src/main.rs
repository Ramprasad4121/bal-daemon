use anyhow::Result;
use bal_daemon::*;
use ethers::providers::{Middleware, Provider, Ws};
use ethers::types::{BlockNumber, H256};
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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
            info!("simulation cancelled: stale head");
            counters.record_stale();
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
            Some(cancellation.clone()),
        ));
    }

    let mut simulations = Vec::with_capacity(total_transactions);
    while !tx_tasks.is_empty() {
        tokio::select! {
            _ = cancellation.cancelled() => {
                tx_tasks.abort_all();
                while let Some(_) = tx_tasks.join_next().await {}
                info!("simulation cancelled: stale head");
                counters.record_stale();
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
                        info!("simulation cancelled: stale head");
                        counters.record_stale();
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
        info!("stale access set discarded");
        counters.record_stale();
        return Ok(());
    }

    let (bal, total_slots, total_reads, total_writes) = merge_simulations(head, simulations);

    if *current_head_rx.borrow() != Some(head.hash) {
        info!("stale access set discarded");
        counters.record_stale();
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
