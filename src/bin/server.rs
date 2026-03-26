use axum::{extract::State, routing::post, Json, Router};
use bal_daemon::*;
use ethers::types::{Address, Bytes, Transaction, U256, H256};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::task::JoinSet;
use tracing::{error, info};

#[derive(Debug, Deserialize)]
struct RpcTransaction {
    from: Address,
    to: Option<Address>,
    data: Bytes,
    gas: U256,
    #[serde(rename = "gasPrice")]
    gas_price: Option<U256>,
    value: U256,
}

#[derive(Debug, Deserialize)]
struct GenerateBalRequest {
    transactions: Vec<RpcTransaction>,
    block_number: U256,
}

#[derive(Debug, Serialize)]
struct GenerateBalResponse {
    bep592_payload: String,
    eip7928_payload: String,
    accounts: usize,
    reads: usize,
    writes: usize,
    size_bytes: usize,
    fresh: bool,
}

struct AppState {
    http_client: reqwest::Client,
    http_url: &'static str,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let state = Arc::new(AppState {
        http_client: reqwest::Client::new(),
        http_url: "https://bsc-testnet.nodereal.io/v1/379e86e230114573aaa4a30d84d76b3e",
    });

    let app = Router::new()
        .route("/generate_bal", post(generate_bal_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:7337").await.unwrap();
    info!("BAL-daemon server listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn generate_bal_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<GenerateBalRequest>,
) -> Result<Json<GenerateBalResponse>, String> {
    let block_number = req.block_number.as_u64();
    let total_transactions = req.transactions.len();
    
    let mut tx_tasks = JoinSet::new();
    for (index, rpc_tx) in req.transactions.into_iter().enumerate() {
        let mut tx = Transaction::default();
        tx.from = rpc_tx.from;
        tx.to = rpc_tx.to;
        tx.input = rpc_tx.data;
        tx.gas = rpc_tx.gas;
        tx.gas_price = rpc_tx.gas_price;
        tx.value = rpc_tx.value;

        tx_tasks.spawn(simulate_transaction(
            state.http_client.clone(),
            state.http_url,
            block_number,
            tx,
            index as u32,
            None,
        ));
    }

    let mut simulations = Vec::with_capacity(total_transactions);
    while let Some(join_result) = tx_tasks.join_next().await {
        match join_result {
            Ok(Ok(Some(simulation))) => simulations.push(simulation),
            Ok(Err(e)) => {
                error!("Simulation error: {}", e);
                return Err(format!("Simulation failed: {}", e));
            }
            Err(e) => {
                error!("Task join error: {}", e);
                return Err(format!("Task failed: {}", e));
            }
            _ => {}
        }
    }

    let head = HeadContext {
        number: block_number,
        hash: H256::zero(), // We don't have the hash in the request, BEP-592 uses it
    };

    let (bal, _total_slots, total_reads, total_writes) = merge_simulations(head, simulations);
    
    let bep592 = bal.rlp_encode();
    let eip7928 = bal.eip7928_encode();

    Ok(Json(GenerateBalResponse {
        bep592_payload: format!("0x{}", hex::encode(&bep592)),
        eip7928_payload: format!("0x{}", hex::encode(&eip7928)),
        accounts: bal.accounts.len(),
        reads: total_reads,
        writes: total_writes,
        size_bytes: bep592.len(),
        fresh: true,
    }))
}
