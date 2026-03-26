use bal_daemon::*;
use ethers::providers::{Middleware, Provider, Http};
use ethers::types::{BlockId, H256};
use std::time::Instant;
use tokio::task::JoinSet;
use tracing::{error, info};

const NODE_REAL_URL: &str = "https://bsc-testnet.nodereal.io/v1/379e86e230114573aaa4a30d84d76b3e";

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

    let provider = Provider::<Http>::try_from(NODE_REAL_URL)?;
    let http_client = reqwest::Client::new();

    info!("Starting load test on NodeReal BSC Testnet...");

    let mut target_blocks = Vec::new();
    let mut current_num = provider.get_block_number().await?.as_u64();

    while target_blocks.len() < 5 {
        info!("Fetching block {}...", current_num);
        let block = provider
            .get_block_with_txs(BlockId::from(current_num))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Block not found"))?;

        let has_contract_interactions = block.transactions.iter().any(|tx| {
            !tx.input.is_empty()
        });

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
        let txs = block.transactions;
        let tx_count = txs.len();

        info!("Simulating block #{} ({} txs)...", block_num, tx_count);

        let start = Instant::now();
        let mut tx_tasks = JoinSet::new();

        for (index, tx) in txs.into_iter().enumerate() {
            let client = http_client.clone();
            tx_tasks.spawn(async move {
                simulate_mined_transaction(client, NODE_REAL_URL, tx.hash, index as u32).await
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
                Ok(Err(e)) => {
                    error!("Simulation error in block #{}: {}", block_num, e);
                    fail_count += 1;
                }
                Err(e) => {
                    error!("Task join error in block #{}: {}", block_num, e);
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

    print_summary_table(all_stats);

    Ok(())
}

fn print_summary_table(stats: Vec<BlockStats>) {
    println!("\n┌──────────┬────────┬─────────┬────────┬──────────┬──────────┬──────────┬──────────┐");
    println!("│ Block    │ Txs    │ Success │ Fail   │ Time(ms) │ TPS      │ Accounts │ Slots    │");
    println!("├──────────┼────────┼─────────┼────────┼──────────┼──────────┼──────────┼──────────┤");

    let mut total_txs = 0;
    let mut total_success = 0;
    let mut total_fail = 0;
    let mut total_time_ms = 0;
    let mut total_accounts = 0;
    let mut total_slots = 0;

    for s in &stats {
        let tps = if s.time_ms > 0 {
            (s.tx_count as f64 / (s.time_ms as f64 / 1000.0))
        } else {
            0.0
        };

        println!(
            "│ {:<8} │ {:<6} │ {:<7} │ {:<6} │ {:<8} │ {:<8.2} │ {:<8} │ {:<8} │",
            s.number, s.tx_count, s.success_count, s.fail_count, s.time_ms, tps, s.accounts, s.slots
        );

        total_txs += s.tx_count;
        total_success += s.success_count;
        total_fail += s.fail_count;
        total_time_ms += s.time_ms;
        total_accounts += s.accounts;
        total_slots += s.slots;
    }

    println!("├──────────┼────────┼─────────┼────────┼──────────┼──────────┼──────────┼──────────┤");
    let avg_tps = if total_time_ms > 0 {
        (total_txs as f64 / (total_time_ms as f64 / 1000.0))
    } else {
        0.0
    };
    println!(
        "│ TOTAL    │ {:<6} │ {:<7} │ {:<6} │ {:<8} │ {:<8.2} │ {:<8} │ {:<8} │",
        total_txs, total_success, total_fail, total_time_ms, avg_tps, total_accounts, total_slots
    );
    println!("└──────────┴────────┴─────────┴────────┴──────────┴──────────┴──────────┴──────────┘\n");
}
