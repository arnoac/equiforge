use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use crate::core::params::*;
use crate::core::types::*;
use crate::network::NodeState;
use crate::wallet;
use crate::miner;
use crate::network;
use crate::core::params::COINBASE_MATURITY;

pub const RPC_PORT_OFFSET: u16 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcRequest { pub method: String, #[serde(default)] pub params: serde_json::Value, #[serde(default)] pub id: u64 }
#[derive(Debug, Serialize, Deserialize)]
pub struct RpcResponse { pub result: Option<serde_json::Value>, pub error: Option<RpcError>, pub id: u64 }
#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError { pub code: i32, pub message: String }

fn success(id: u64, result: serde_json::Value) -> RpcResponse { RpcResponse { result: Some(result), error: None, id } }
fn error(id: u64, code: i32, msg: &str) -> RpcResponse { RpcResponse { result: None, error: Some(RpcError { code, message: msg.to_string() }), id } }

pub async fn start_rpc_server(state: Arc<NodeState>, rpc_port: u16) {
    let addr = format!("0.0.0.0:{}", rpc_port);
    let listener = match TcpListener::bind(&addr).await { Ok(l) => l, Err(e) => { tracing::error!("Failed to bind RPC on {}: {}", addr, e); return; } };
    tracing::info!("🌐 RPC server on http://0.0.0.0:{}", rpc_port);
    loop {
        match listener.accept().await {
            Ok((stream, _)) => { let state = state.clone(); tokio::spawn(async move { handle_http(stream, state).await }); }
            Err(e) => tracing::error!("RPC accept error: {}", e),
        }
    }
}

async fn handle_http(mut stream: tokio::net::TcpStream, state: Arc<NodeState>) {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.is_err() { return; }

    if request_line.starts_with("GET") {
        let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
        loop { let mut line = String::new(); if reader.read_line(&mut line).await.is_err() { break; } if line.trim().is_empty() { break; } }
        if path == "/snapshot" || path == "/snapshot.bin" {
            tracing::info!("📸 Snapshot download requested");
            let chain = state.chain.read().await;
            let height = chain.height;
            let mut data: Vec<u8> = Vec::new();
            data.extend_from_slice(&1u32.to_le_bytes());
            data.extend_from_slice(&height.to_le_bytes());
            data.extend_from_slice(&((height + 1) as u64).to_le_bytes());
            let genesis_hash = chain.genesis_hash();
            data.extend_from_slice(&genesis_hash);
            for h in 0..=height {
                if let Some(block) = chain.block_at_height(h) {
                    let encoded = bincode::serialize(block).unwrap();
                    data.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
                    data.extend_from_slice(&encoded);
                }
            }
            drop(chain);
            use std::io::Write as IoWrite;
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(&data).unwrap();
            let compressed = encoder.finish().unwrap();
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"snapshot.bin\"\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n", compressed.len());
            let _ = writer.write_all(response.as_bytes()).await;
            let _ = writer.write_all(&compressed).await;
            return;
        }
        let html = include_str!("explorer.html");
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{}", html.len(), html);
        let _ = writer.write_all(response.as_bytes()).await;
        return;
    }
    if request_line.starts_with("OPTIONS") {
        loop { let mut line = String::new(); if reader.read_line(&mut line).await.is_err() { break; } if line.trim().is_empty() { break; } }
        let _ = writer.write_all(b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: POST, GET, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Max-Age: 86400\r\n\r\n").await;
        return;
    }
    let mut content_length: usize = 0;
    loop {
        let mut header_line = String::new();
        if reader.read_line(&mut header_line).await.is_err() { return; }
        let trimmed = header_line.trim();
        if trimmed.is_empty() { break; }
        let lower = trimmed.to_lowercase();
        if let Some(val) = lower.strip_prefix("content-length:") { content_length = val.trim().parse().unwrap_or(0); }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 { if reader.read_exact(&mut body).await.is_err() { return; } }
    let response = match serde_json::from_slice::<RpcRequest>(&body) {
        Ok(req) => handle_rpc(req, &state).await,
        Err(e) => error(0, -32700, &format!("parse error: {}", e)),
    };
    let response_json = serde_json::to_string(&response).unwrap();
    let http_response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\n\r\n{}", response_json.len(), response_json);
    let _ = writer.write_all(http_response.as_bytes()).await;
}

async fn handle_rpc(req: RpcRequest, state: &Arc<NodeState>) -> RpcResponse {
    match req.method.as_str() {
        "getinfo" | "getblockchaininfo" => {
            let chain = state.chain.read().await;
            let peers = state.peers.read().await;
            let mempool = state.mempool.lock().await;
            let sb = state.scoreboard.lock().await;
            let height = chain.height;
            let mut total_supply: u64 = 0;
            for h in 0..=height { total_supply += block_reward(h); }
            let tip = chain.tip_header();
            let avg_block_time = if height >= 10 {
                if let Some(older) = chain.block_at_height(height.saturating_sub(10)) {
                    let dt = tip.timestamp.saturating_sub(older.header.timestamp);
                    if dt > 0 { dt as f64 / 10.0 } else { TARGET_BLOCK_TIME as f64 }
                } else { TARGET_BLOCK_TIME as f64 }
            } else { TARGET_BLOCK_TIME as f64 };
            let diff = chain.next_difficulty();
            let est_hashes = estimated_hashes_for_difficulty(diff);
            let hashrate = if avg_block_time > 0.0 { est_hashes as f64 / avg_block_time } else { 0.0 };
            success(req.id, json!({
                "height": height, "tip": hex::encode(chain.tip),
                "difficulty": diff, "fractional_difficulty": chain.fractional_difficulty(),
                "utxos": chain.utxo_set.len(), "known_blocks": chain.total_known_blocks(),
                "peers": peers.len(), "mempool": mempool.len(), "banned": sb.ban_count(),
                "block_reward": block_reward(height) as f64 / COIN as f64,
                "persistent": chain.is_persistent(),
                "total_supply": total_supply as f64 / COIN as f64,
                "max_supply": MAX_SUPPLY as f64 / COIN as f64,
                "avg_block_time": avg_block_time, "hashrate": hashrate,
                "last_block_time": tip.timestamp,
                "network": if is_testnet() { "testnet" } else { "mainnet" },
            }))
        }
        "getblockcount" | "getheight" => { let chain = state.chain.read().await; success(req.id, json!(chain.height)) }
        "getbestblockhash" => { let chain = state.chain.read().await; success(req.id, json!(hex::encode(chain.tip))) }
        "getbalance" => {
            let address = req.params.get(0).or_else(|| req.params.get("address")).and_then(|v| v.as_str()).unwrap_or("");
            if address.is_empty() { return error(req.id, -32602, "missing address parameter"); }
            match wallet::address_to_pubkey_hash(address) {
                Some(hash) => { let chain = state.chain.read().await; let balance = chain.utxo_set.balance_of(&hash);
                    success(req.id, json!({"address": address, "balance": balance as f64 / COIN as f64, "balance_base": balance})) }
                None => error(req.id, -32602, "invalid address"),
            }
        }
        "listunspent" => {
            let address = req.params.get(0).or_else(|| req.params.get("address")).and_then(|v| v.as_str()).unwrap_or("");
            match wallet::address_to_pubkey_hash(address) {
                Some(hash) => {
                    let chain = state.chain.read().await;
                    let utxos: Vec<serde_json::Value> = chain.utxo_set.utxos_for(&hash).iter().map(|(op, e)| json!({
                        "txid": hex::encode(op.txid), "vout": op.vout, "amount": e.output.amount as f64 / COIN as f64,
                        "amount_base": e.output.amount, "height": e.height, "coinbase": e.is_coinbase,
                        "confirmations": chain.height - e.height + 1,
                    })).collect();
                    success(req.id, json!(utxos))
                }
                None => error(req.id, -32602, "invalid address"),
            }
        }
        "gettx" => {
            let txid_str = req.params.get(0).or_else(|| req.params.get("txid")).and_then(|v| v.as_str()).unwrap_or("");
            if txid_str.len() != 64 { return error(req.id, -32602, "invalid txid"); }
            let chain = state.chain.read().await;
            for h in (0..=chain.height).rev() {
                if let Some(block) = chain.block_at_height(h) {
                    for (tx_idx, tx) in block.transactions.iter().enumerate() {
                        let this_txid = hex::encode(tx.hash());
                        if this_txid != txid_str { continue; }
                        let inputs: Vec<serde_json::Value> = if tx.is_coinbase() {
                            vec![json!({"type":"coinbase","amount": tx.total_output() as f64 / COIN as f64})]
                        } else {
                            tx.inputs.iter().map(|inp| {
                                let prev_txid = hex::encode(inp.previous_output.txid);
                                let (mut pa, mut paddr) = (0u64, String::new());
                                'outer: for ph in (0..=chain.height).rev() {
                                    if let Some(pb) = chain.block_at_height(ph) {
                                        for ptx in &pb.transactions {
                                            if hex::encode(ptx.hash()) == prev_txid {
                                                if let Some(out) = ptx.outputs.get(inp.previous_output.vout as usize) {
                                                    pa = out.amount; paddr = wallet::pubkey_hash_to_address(&out.pubkey_hash);
                                                } break 'outer;
                                            }
                                        }
                                    }
                                }
                                json!({"txid":prev_txid,"vout":inp.previous_output.vout,"amount":pa as f64/COIN as f64,"address":paddr})
                            }).collect()
                        };
                        let outputs: Vec<serde_json::Value> = tx.outputs.iter().enumerate().map(|(vout, out)| {
                            let addr = wallet::pubkey_hash_to_address(&out.pubkey_hash);
                            let op = OutPoint { txid: tx.hash(), vout: vout as u32 };
                            let spent = !chain.utxo_set.contains(&op);
                            json!({"vout":vout,"amount":out.amount as f64/COIN as f64,"address":addr,"spent":spent})
                        }).collect();
                        let it: f64 = inputs.iter().filter_map(|i| i.get("amount").and_then(|a| a.as_f64())).sum();
                        let ot: f64 = outputs.iter().filter_map(|o| o.get("amount").and_then(|a| a.as_f64())).sum();
                        return success(req.id, json!({
                            "txid":txid_str,"block_hash":hex::encode(block.header.hash()),
                            "block_height":h,"tx_index":tx_idx,"timestamp":block.header.timestamp,
                            "confirmations":chain.height-h+1,"is_coinbase":tx.is_coinbase(),
                            "inputs":inputs,"outputs":outputs,"input_total":it,"output_total":ot,
                            "fee":if tx.is_coinbase(){0.0}else{it-ot},"size":tx.size(),
                        }));
                    }
                }
            }
            error(req.id, -32602, "transaction not found")
        }
         "getaddress" => {
            let address = req.params.get(0).or_else(|| req.params.get("address")).and_then(|v| v.as_str()).unwrap_or("");
            match wallet::address_to_pubkey_hash(address) {
                Some(hash) => {
                    let chain = state.chain.read().await;

                    // ── UTXO-based balances ──
                    let utxos = chain.utxo_set.utxos_for(&hash);
                    let mut total_balance: u64 = 0;
                    let mut immature_balance: u64 = 0;
                    let mut mature_balance: u64 = 0;

                    for (_op, entry) in &utxos {
                        let amount = entry.output.amount;
                        total_balance += amount;
                        let confs = chain.height.saturating_sub(entry.height);
                        if entry.is_coinbase && confs < COINBASE_MATURITY {
                            immature_balance += amount;
                        } else {
                            mature_balance += amount;
                        }
                    }

                    // ── Build a lookup: txid → Vec<TxOutput> for resolving input provenance ──
                    // We need this to figure out if a transaction INPUT was spending from our address.
                    // A spent UTXO no longer exists in the UTXO set, so we scan block history.
                    let mut output_owner: std::collections::HashMap<(Hash256, u32), (Hash256, u64)> = std::collections::HashMap::new();
                    // Key: (txid, vout) → Value: (pubkey_hash, amount)

                    // First pass: build the output ownership map
                    for h in 0..=chain.height {
                        if let Some(block) = chain.block_at_height(h) {
                            for tx in &block.transactions {
                                let txid = tx.hash();
                                for (vout, out) in tx.outputs.iter().enumerate() {
                                    output_owner.insert((txid, vout as u32), (out.pubkey_hash, out.amount));
                                }
                            }
                        }
                    }

                    // ── Second pass: scan transactions for involvement ──
                    let mut txs: Vec<serde_json::Value> = Vec::new();
                    let mut tx_count: u64 = 0;
                    let mut total_received: u64 = 0;
                    let mut total_sent: u64 = 0;

                    for h in (0..=chain.height).rev() {
                        if let Some(block) = chain.block_at_height(h) {
                            for tx in &block.transactions {
                                let mut received: u64 = 0;
                                let mut sent: u64 = 0;

                                // Check outputs → received
                                for out in &tx.outputs {
                                    if out.pubkey_hash == hash {
                                        received += out.amount;
                                    }
                                }

                                // Check inputs → sent (skip coinbase which has no real inputs)
                                if !tx.is_coinbase() {
                                    for inp in &tx.inputs {
                                        if let Some((owner, amount)) = output_owner.get(&(inp.previous_output.txid, inp.previous_output.vout)) {
                                            if *owner == hash {
                                                sent += amount;
                                            }
                                        }
                                    }
                                }

                                if received > 0 || sent > 0 {
                                    tx_count += 1;
                                    total_received += received;
                                    total_sent += sent;
                                    let net = received as i64 - sent as i64;
                                    if txs.len() < 50 {
                                        txs.push(json!({
                                            "txid": hex::encode(tx.hash()),
                                            "block_height": h,
                                            "timestamp": block.header.timestamp,
                                            "received": received as f64 / COIN as f64,
                                            "sent": sent as f64 / COIN as f64,
                                            "net": net as f64 / COIN as f64,
                                            "is_coinbase": tx.is_coinbase(),
                                        }));
                                    }
                                }
                            }
                        }
                    }

                    // ── UTXO list with maturity info ──
                    let utxo_list: Vec<serde_json::Value> = utxos.iter().map(|(op, e)| {
                        let confs = chain.height.saturating_sub(e.height) + 1;
                        let mature = !e.is_coinbase || confs >= COINBASE_MATURITY;
                        json!({
                            "txid": hex::encode(op.txid),
                            "vout": op.vout,
                            "amount": e.output.amount as f64 / COIN as f64,
                            "amount_base": e.output.amount,
                            "height": e.height,
                            "coinbase": e.is_coinbase,
                            "confirmations": confs,
                            "mature": mature,
                        })
                    }).collect();

                    success(req.id, json!({
                        "address": address,
                        // Total balance (all UTXOs)
                        "balance": total_balance as f64 / COIN as f64,
                        "balance_base": total_balance,
                        // Spendable now (mature only)
                        "spendable": mature_balance as f64 / COIN as f64,
                        "spendable_base": mature_balance,
                        // Immature coinbase (< 100 confirmations)
                        "immature": immature_balance as f64 / COIN as f64,
                        "immature_base": immature_balance,
                        // Lifetime totals
                        "total_received": total_received as f64 / COIN as f64,
                        "total_sent": total_sent as f64 / COIN as f64,
                        // Counts
                        "tx_count": tx_count,
                        "utxo_count": utxo_list.len(),
                        "transactions": txs,
                        "utxos": utxo_list,
                    }))
                }
                None => error(req.id, -32602, "invalid address"),
            }
        }
        "sendrawtransaction" => {
            let tx_json = req.params.get(0).or_else(|| req.params.get("tx"));
            match tx_json {
                Some(tx_val) => match serde_json::from_value::<Transaction>(tx_val.clone()) {
                    Ok(tx) => { let chain = state.chain.read().await; let mut mempool = state.mempool.lock().await;
                        match mempool.validate_and_add(tx.clone(), &chain) {
                            Ok(txid) => { drop(mempool); drop(chain); let _ = state.tx_tx.send(tx); success(req.id, json!({"txid":hex::encode(txid),"status":"accepted"})) }
                            Err(reason) => error(req.id, -32000, &format!("rejected: {}", reason)),
                        }
                    }
                    Err(e) => error(req.id, -32602, &format!("invalid transaction: {}", e)),
                },
                None => error(req.id, -32602, "missing tx parameter"),
            }
        }
        "getmempool" => {
            let mempool = state.mempool.lock().await;
            let entries: Vec<serde_json::Value> = mempool.get_pending_with_fees().iter().map(|(tx, fee, fee_rate)| json!({
                "txid":hex::encode(tx.hash()),"size":tx.size(),"fee":*fee as f64/COIN as f64,"fee_base":fee,"fee_rate":fee_rate,
            })).collect();
            success(req.id, json!({"size":entries.len(),"transactions":entries}))
        }
        "getpeerinfo" => {
            let peers = state.peers.read().await;
            let peer_list: Vec<serde_json::Value> = peers.values().map(|p| json!({
                "address":p.address,"listen_address":p.listen_address,"version":p.version,
                "best_height":p.best_height,"last_seen":p.last_seen,
            })).collect();
            success(req.id, json!(peer_list))
        }
        "getblock" => {
            let hash_str = req.params.get(0).or_else(|| req.params.get("hash")).and_then(|v| v.as_str()).unwrap_or("");
            let chain = state.chain.read().await;
            if let Ok(height) = hash_str.parse::<u64>() {
                if let Some(block) = chain.block_at_height(height) { return success(req.id, block_to_json(block, &chain)); }
            }
            if let Ok(hash_bytes) = hex::decode(hash_str) {
                if hash_bytes.len() == 32 {
                    let mut hash = [0u8; 32]; hash.copy_from_slice(&hash_bytes);
                    if let Some(header) = chain.header(&hash) {
                        if let Some(block) = chain.block_at_height(header.height) { return success(req.id, block_to_json(block, &chain)); }
                    }
                }
            }
            error(req.id, -32602, "block not found")
        }
        "getmininginfo" => {
            let chain = state.chain.read().await; let diff = chain.next_difficulty();
            success(req.id, json!({"height":chain.height+1,"difficulty":diff,"fractional_difficulty":chain.fractional_difficulty(),
                "estimated_hashes":estimated_hashes_for_difficulty(diff),"block_reward":block_reward(chain.height+1) as f64/COIN as f64}))
        }
        "getrichlist" => {
            let count = req.params.get(0).and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            let chain = state.chain.read().await;
            let mut balances: std::collections::HashMap<Hash256, u64> = std::collections::HashMap::new();
            for (_op, entry) in chain.utxo_set.iter() { *balances.entry(entry.output.pubkey_hash).or_insert(0) += entry.output.amount; }
            let mut sorted: Vec<(Hash256, u64)> = balances.into_iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(&a.1));
            let list: Vec<serde_json::Value> = sorted.iter().take(count).enumerate().map(|(rank, (hash, amount))| json!({
                "rank":rank+1,"address":wallet::pubkey_hash_to_address(hash),"balance":*amount as f64/COIN as f64,"balance_base":amount,
            })).collect();
            success(req.id, json!({"total_addresses":sorted.len(),"addresses":list}))
        }
         "getblocktemplate" => {
            let miner_hex = req.params.get(0).and_then(|v| v.as_str()).unwrap_or("");
            let miner_hash: Hash256 = if miner_hex.len() == 64 {
                match hex::decode(miner_hex) {
                    Ok(bytes) if bytes.len() == 32 => {
                        let mut h = [0u8; 32];
                        h.copy_from_slice(&bytes);
                        h
                    }
                    _ => return error(req.id, -32602, "invalid miner_pubkey_hash (need 64 hex chars)"),
                }
            } else {
                return error(req.id, -32602, "params: [\"miner_pubkey_hash_hex\"]");
            };

            let chain = state.chain.read().await;
            let mp = state.mempool.lock().await;
            let pending = mp.get_pending();
            drop(mp);

            let cfg = miner::MinerConfig {
                miner_pubkey_hash: miner_hash,
                community_fund_hash: [0xCF; 32],
                threads: 1,
                miner_tag: String::new(),
            };
            let template = miner::create_block_template(&chain, &pending, &cfg);
            let difficulty = chain.next_difficulty();
            drop(chain);

            // Serialize transactions so external miner can reconstruct the block
            let txs_hex: Vec<String> = template.transactions.iter()
                .map(|tx| hex::encode(bincode::serialize(tx).unwrap()))
                .collect();

            success(req.id, json!({
                "height": template.header.height,
                "prev_hash": hex::encode(template.header.prev_hash),
                "merkle_root": hex::encode(template.header.merkle_root),
                "timestamp": template.header.timestamp,
                "difficulty_target": template.header.difficulty_target,
                "version": template.header.version,
                "network_difficulty": difficulty,
                "block_reward": block_reward(template.header.height) as f64 / COIN as f64,
                "tx_count": template.transactions.len(),
                "transactions_hex": txs_hex,
                // The header as a single hex blob for easy hashing
                "header_hex": hex::encode(bincode::serialize(&template.header).unwrap()),
            }))
        }

        // ─── submitblock ───────────────────────────────────────────
        // Submit a mined block to the network.
        // Params: [header_hex, nonce] OR [block_hex]
        //
        // Option A (recommended for pools):
        //   ["header_hex_from_getblocktemplate", nonce_u64, ["tx1_hex", "tx2_hex", ...]]
        //   Pool takes the header_hex from getblocktemplate, the winning nonce,
        //   and the same tx list, reconstructs the block, and submits.
        //
        // Option B (full block):
        //   [block_hex]
        //   Full bincode-serialized Block as hex.
        "submitblock" => {
            let block: Block = if req.params.is_array() && req.params.as_array().map(|a| a.len()).unwrap_or(0) >= 3 {
                // Option A: header_hex + nonce + transactions_hex array
                let header_hex = match req.params.get(0).and_then(|v| v.as_str()) {
                    Some(h) => h,
                    None => return error(req.id, -32602, "params[0]: header_hex string"),
                };
                let nonce = match req.params.get(1).and_then(|v| v.as_u64()) {
                    Some(n) => n,
                    None => return error(req.id, -32602, "params[1]: nonce (u64)"),
                };
                let txs_hex = match req.params.get(2).and_then(|v| v.as_array()) {
                    Some(arr) => arr,
                    None => return error(req.id, -32602, "params[2]: transactions_hex array"),
                };

                // Decode header
                let header_bytes = match hex::decode(header_hex) {
                    Ok(b) => b,
                    Err(_) => return error(req.id, -32602, "invalid header_hex"),
                };
                let mut header: BlockHeader = match bincode::deserialize(&header_bytes) {
                    Ok(h) => h,
                    Err(_) => return error(req.id, -32602, "failed to decode header"),
                };
                header.nonce = nonce;

                // Decode transactions
                let mut transactions: Vec<Transaction> = Vec::new();
                for (i, tx_val) in txs_hex.iter().enumerate() {
                    let tx_hex = match tx_val.as_str() {
                        Some(s) => s,
                        None => return error(req.id, -32602, &format!("tx[{}] not a string", i)),
                    };
                    let tx_bytes = match hex::decode(tx_hex) {
                        Ok(b) => b,
                        Err(_) => return error(req.id, -32602, &format!("tx[{}] invalid hex", i)),
                    };
                    let tx: Transaction = match bincode::deserialize(&tx_bytes) {
                        Ok(t) => t,
                        Err(_) => return error(req.id, -32602, &format!("tx[{}] decode failed", i)),
                    };
                    transactions.push(tx);
                }

                Block { header, transactions }
            } else if let Some(block_hex) = req.params.get(0).and_then(|v| v.as_str()) {
                // Option B: full block hex
                let block_bytes = match hex::decode(block_hex) {
                    Ok(b) => b,
                    Err(_) => return error(req.id, -32602, "invalid block hex"),
                };
                match bincode::deserialize(&block_bytes) {
                    Ok(b) => b,
                    Err(_) => return error(req.id, -32602, "failed to decode block"),
                }
            } else {
                return error(req.id, -32602,
                    "params: [header_hex, nonce, [tx_hex...]] or [block_hex]");
            };

            // Verify PoW before accepting
            if !block.header.meets_difficulty() {
                return error(req.id, -32010, "block does not meet difficulty target");
            }

            let block_hash = hex::encode(block.header.hash());
            let height = block.header.height;

            // Submit to network (same path as solo mining and pool)
            network::broadcast_block(&state, block).await;

            success(req.id, json!({
                "accepted": true,
                "hash": block_hash,
                "height": height,
            }))
        }
        _ => error(req.id, -32601, &format!("method '{}' not found", req.method)),
    }
}

fn block_to_json(block: &Block, chain: &crate::core::chain::Chain) -> serde_json::Value {
    let hash = block.header.hash(); let height = block.header.height;
    let miner_addr = if !block.transactions.is_empty() && !block.transactions[0].outputs.is_empty() {
        wallet::pubkey_hash_to_address(&block.transactions[0].outputs[0].pubkey_hash)
    } else { String::from("unknown") };
    let coinbase_output = block.transactions.get(0).map(|tx| tx.total_output()).unwrap_or(0);
    let reward = block_reward(height);
    let fees = coinbase_output.saturating_sub(reward);
    let prev_time = if height > 0 { chain.block_at_height(height-1).map(|b| b.header.timestamp).unwrap_or(0) } else { 0 };
    let block_time_delta = if prev_time > 0 { block.header.timestamp - prev_time } else { 0 };
    let miner_tag = block.transactions[0].coinbase_tag();
    let txs: Vec<serde_json::Value> = block.transactions.iter().enumerate().map(|(i, tx)| {
        let output_total: u64 = tx.outputs.iter().map(|o| o.amount).sum();
        let recipients: Vec<serde_json::Value> = tx.outputs.iter().map(|out| json!({
            "address": wallet::pubkey_hash_to_address(&out.pubkey_hash), "amount": out.amount as f64 / COIN as f64,
        })).collect();
        json!({"txid":hex::encode(tx.hash()),"index":i,"is_coinbase":tx.is_coinbase(),"input_count":tx.inputs.len(),
            "output_count":tx.outputs.len(),"output_total":output_total as f64/COIN as f64,"size":tx.size(),"recipients":recipients})
    }).collect();
    json!({"hash":hex::encode(hash),"height":height,"version":block.header.version,
        "prev_hash":hex::encode(block.header.prev_hash),"merkle_root":hex::encode(block.header.merkle_root),
        "timestamp":block.header.timestamp,"difficulty":block.header.difficulty_target,"nonce":block.header.nonce,
        "tx_count":block.transactions.len(),"transactions":txs,"size":block.size(),"miner":miner_addr,
        "reward":reward as f64/COIN as f64,"fees":fees as f64/COIN as f64,"block_time":block_time_delta,
        "confirmations":chain.height-height+1, "miner_tag": miner_tag})
}

// ─── RPC Client ────────────────────────────────────────────────────
pub fn rpc_call(port: u16, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let request = RpcRequest { method: method.to_string(), params, id: 1 };
    let body = serde_json::to_string(&request).unwrap();
    use std::io::{Read, Write}; use std::net::TcpStream;
    let addr = format!("127.0.0.1:{}", port);
    let mut stream = TcpStream::connect(&addr).map_err(|_| format!("cannot connect to node RPC at {}. Is the node running?", addr))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).ok();
    let http_request = format!("POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
    stream.write_all(http_request.as_bytes()).map_err(|e| format!("write error: {}", e))?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| format!("read error: {}", e))?;
    let response_str = String::from_utf8_lossy(&response);
    let body_start = response_str.find("\r\n\r\n").ok_or("invalid HTTP response")?;
    let json_body = &response_str[body_start + 4..];
    let rpc_response: RpcResponse = serde_json::from_str(json_body).map_err(|e| format!("JSON parse error: {}", e))?;
    if let Some(err) = rpc_response.error { return Err(format!("RPC error {}: {}", err.code, err.message)); }
    rpc_response.result.ok_or("empty result".to_string())
}
pub fn try_rpc_call(port: u16, method: &str, params: serde_json::Value) -> Option<serde_json::Value> { rpc_call(port, method, params).ok() }
