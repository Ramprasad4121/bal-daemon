use rlp::Rlp;
use ethers::providers::{Provider, Http};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. verify_bal_payload test
    let payload_hex = "f88c808405c1b83ba04c0e24ed5378f9a7a8695528591e82798e37c6f62b906b39bd6ca98df29f188180f862f8608094d98afa5e340816a637bd886d16e82f9c2106bb21f848e38001a00bf32f4da38320366071ac3236c564e6dbc88a40d525d63d09ef1b94ae5913abe38001a090a3916c6b57b86ca63923538a8e760694b59e976b381e1139befab04fb2d3d0";
    let bytes = hex::decode(payload_hex)?;
    let rlp = Rlp::new(&bytes);
    
    let mut accounts = 0;
    let mut reads = 0;
    let mut writes = 0;
    let mut error = None;
    let mut valid = true;

    if !rlp.is_list() || rlp.item_count()? != 5 {
        valid = false;
        error = Some("Invalid RLP list length".to_string());
    } else {
        match rlp.val_at::<u32>(0) {
            Ok(version) if version == 0 => {
                match rlp.at(4) {
                    Ok(acc_list) => {
                        accounts = acc_list.item_count()?;
                        for i in 0..accounts {
                            let acc = acc_list.at(i)?;
                            let items = acc.at(2)?;
                            for j in 0..items.item_count()? {
                                let item = items.at(j)?;
                                let dirty: u8 = item.val_at(1)?;
                                if dirty == 1 { writes += 1; } else { reads += 1; }
                            }
                        }
                    }
                    Err(_) => {
                        valid = false;
                        error = Some("Missing accounts list".to_string());
                    }
                }
            }
            Ok(version) => {
                valid = false;
                error = Some(format!("Invalid version: {}", version));
            }
            Err(_) => {
                valid = false;
                error = Some("Invalid version format".to_string());
            }
        }
    }

    println!("verify_bal_payload output:");
    println!("{}", serde_json::to_string_pretty(&json!({
        "valid": valid,
        "accounts": accounts,
        "reads": reads,
        "writes": writes,
        "error": error
    }))?);

    // 2. get_block_bal_stats test
    let block_number = 96583654;
    let provider = Provider::<Http>::try_from("https://bsc-testnet.nodereal.io/v1/379e86e230114573aaa4a30d84d76b3e")?;
    
    let block_json: serde_json::Value = provider.request("eth_getBlockByNumber", (format!("{:#x}", block_number), false)).await?;
    
    let has_bal = block_json.get("accessList").is_some();
    let accounts_count = block_json.get("accessList").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    
    println!("\nget_block_bal_stats output:");
    println!("{}", serde_json::to_string_pretty(&json!({
        "has_bal": has_bal,
        "accounts": accounts_count,
        "storage_slots": 0, // Simplified for this test
        "payload_size_bytes": 0 // Simplified for this test
    }))?);

    Ok(())
}
