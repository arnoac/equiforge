use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::core::params::*;
use crate::core::types::*;
use crate::network::NodeState;
use crate::wallet;

/// Default RPC port (P2P port + 1)
pub const RPC_PORT_OFFSET: u16 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcRequest {
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub id: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcResponse {
    pub result: Option<serde_json::Value>,
    pub error: Option<RpcError>,
    pub id: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

fn success(id: u64, result: serde_json::Value) -> RpcResponse {
    RpcResponse { result: Some(result), error: None, id }
}

fn error(id: u64, code: i32, msg: &str) -> RpcResponse {
    RpcResponse { result: None, error: Some(RpcError { code, message: msg.to_string() }), id }
}

/// Start the RPC HTTP server
pub async fn start_rpc_server(state: Arc<NodeState>, rpc_port: u16) {
    let addr = format!("0.0.0.0:{}", rpc_port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("❌ RPC server failed to bind {}: {}", addr, e);
            return;
        }
    };

    tracing::info!("🌐 RPC server on http://{}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    handle_http(stream, state).await;
                });
            }
            Err(e) => {
                tracing::error!("RPC accept error: {}", e);
            }
        }
    }
}

/// Handle a single HTTP connection
async fn handle_http(mut stream: tokio::net::TcpStream, state: Arc<NodeState>) {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);

    // Read HTTP request line
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.is_err() { return; }

    // Check if it's a GET request (serve explorer UI or snapshot)
    if request_line.starts_with("GET") {
        // Parse the path
        let path = request_line.split_whitespace().nth(1).unwrap_or("/");

        // Drain headers
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.is_err() { break; }
            if line.trim().is_empty() { break; }
        }

        if path == "/snapshot" || path == "/snapshot.bin" {
            // Stream chain snapshot as gzip-compressed binary
            tracing::info!("📸 Snapshot download requested");
            let chain = state.chain.read().await;
            let height = chain.height;

            // Build snapshot data
            let mut data: Vec<u8> = Vec::new();
            data.extend_from_slice(&1u32.to_le_bytes()); // version
            data.extend_from_slice(&height.to_le_bytes());
            data.extend_from_slice(&((height + 1) as u64).to_le_bytes()); // block_count
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

            // Compress
            use std::io::Write as IoWrite;
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(&data).unwrap();
            let compressed = encoder.finish().unwrap();

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"snapshot.bin\"\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n",
                compressed.len()
            );
            let _ = writer.write_all(response.as_bytes()).await;
            let _ = writer.write_all(&compressed).await;
            tracing::info!("📸 Snapshot sent: {} blocks, {:.1} MB compressed", height + 1, compressed.len() as f64 / 1_048_576.0);
            return;
        }

        let html = explorer_html();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{}",
            html.len(), html
        );
        let _ = writer.write_all(response.as_bytes()).await;
        return;
    }

    // OPTIONS: CORS preflight
    if request_line.starts_with("OPTIONS") {
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.is_err() { break; }
            if line.trim().is_empty() { break; }
        }
        let response = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: POST, GET, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Max-Age: 86400\r\n\r\n";
        let _ = writer.write_all(response.as_bytes()).await;
        return;
    }

    // POST: JSON-RPC
    let mut content_length: usize = 0;
    loop {
        let mut header_line = String::new();
        if reader.read_line(&mut header_line).await.is_err() { return; }
        let trimmed = header_line.trim();
        if trimmed.is_empty() { break; }
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            content_length = val.trim().parse().unwrap_or(0);
        }
        if let Some(val) = trimmed.strip_prefix("content-length:") {
            content_length = val.trim().parse().unwrap_or(0);
        }
    }

    // Read body
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }
    }

    // Parse JSON-RPC request
    let response = match serde_json::from_slice::<RpcRequest>(&body) {
        Ok(req) => handle_rpc(req, &state).await,
        Err(e) => error(0, -32700, &format!("parse error: {}", e)),
    };

    // Send HTTP response
    let response_json = serde_json::to_string(&response).unwrap();
    let http_response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\n\r\n{}",
        response_json.len(),
        response_json,
    );

    let _ = writer.write_all(http_response.as_bytes()).await;
}

/// Handle a JSON-RPC request
async fn handle_rpc(req: RpcRequest, state: &Arc<NodeState>) -> RpcResponse {
    match req.method.as_str() {
        "getinfo" | "getblockchaininfo" => {
            let chain = state.chain.read().await;
            let peers = state.peers.read().await;
            let mempool = state.mempool.lock().await;
            let sb = state.scoreboard.lock().await;

            success(req.id, json!({
                "height": chain.height,
                "tip": hex::encode(chain.tip),
                "difficulty": chain.next_difficulty(),
                "fractional_difficulty": chain.fractional_difficulty(),
                "utxos": chain.utxo_set.len(),
                "known_blocks": chain.total_known_blocks(),
                "peers": peers.len(),
                "mempool": mempool.len(),
                "banned": sb.ban_count(),
                "block_reward": block_reward(chain.height) as f64 / COIN as f64,
                "persistent": chain.is_persistent(),
            }))
        }

        "getblockcount" | "getheight" => {
            let chain = state.chain.read().await;
            success(req.id, json!(chain.height))
        }

        "getbestblockhash" => {
            let chain = state.chain.read().await;
            success(req.id, json!(hex::encode(chain.tip)))
        }

        "getbalance" => {
            let address = req.params.get(0)
                .or_else(|| req.params.get("address"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if address.is_empty() {
                return error(req.id, -32602, "missing address parameter");
            }

            match wallet::address_to_pubkey_hash(address) {
                Some(hash) => {
                    let chain = state.chain.read().await;
                    let balance = chain.utxo_set.balance_of(&hash);
                    success(req.id, json!({
                        "address": address,
                        "balance": balance as f64 / COIN as f64,
                        "balance_base": balance,
                    }))
                }
                None => error(req.id, -32602, "invalid address"),
            }
        }

        "listunspent" => {
            let address = req.params.get(0)
                .or_else(|| req.params.get("address"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match wallet::address_to_pubkey_hash(address) {
                Some(hash) => {
                    let chain = state.chain.read().await;
                    let utxos: Vec<serde_json::Value> = chain.utxo_set.utxos_for(&hash)
                        .iter()
                        .map(|(outpoint, entry)| json!({
                            "txid": hex::encode(outpoint.txid),
                            "vout": outpoint.vout,
                            "amount": entry.output.amount as f64 / COIN as f64,
                            "amount_base": entry.output.amount,
                            "height": entry.height,
                            "coinbase": entry.is_coinbase,
                        }))
                        .collect();
                    success(req.id, json!(utxos))
                }
                None => error(req.id, -32602, "invalid address"),
            }
        }

        "sendrawtransaction" => {
            let tx_json = req.params.get(0)
                .or_else(|| req.params.get("tx"));

            match tx_json {
                Some(tx_val) => {
                    match serde_json::from_value::<Transaction>(tx_val.clone()) {
                        Ok(tx) => {
                            let chain = state.chain.read().await;
                            let mut mempool = state.mempool.lock().await;
                            match mempool.validate_and_add(tx.clone(), &chain) {
                                Ok(txid) => {
                                    drop(mempool);
                                    drop(chain);
                                    let _ = state.tx_tx.send(tx);
                                    success(req.id, json!({
                                        "txid": hex::encode(txid),
                                        "status": "accepted",
                                    }))
                                }
                                Err(reason) => {
                                    error(req.id, -32000, &format!("rejected: {}", reason))
                                }
                            }
                        }
                        Err(e) => error(req.id, -32602, &format!("invalid transaction: {}", e)),
                    }
                }
                None => error(req.id, -32602, "missing tx parameter"),
            }
        }

        "getmempool" => {
            let mempool = state.mempool.lock().await;
            let entries: Vec<serde_json::Value> = mempool.get_pending_with_fees()
                .iter()
                .map(|(tx, fee, fee_rate)| json!({
                    "txid": hex::encode(tx.hash()),
                    "size": tx.size(),
                    "fee": *fee as f64 / COIN as f64,
                    "fee_base": fee,
                    "fee_rate": fee_rate,
                }))
                .collect();
            success(req.id, json!({
                "size": entries.len(),
                "transactions": entries,
            }))
        }

        "getpeerinfo" => {
            let peers = state.peers.read().await;
            let peer_list: Vec<serde_json::Value> = peers.values()
                .map(|p| json!({
                    "address": p.address,
                    "version": p.version,
                    "best_height": p.best_height,
                    "last_seen": p.last_seen,
                }))
                .collect();
            success(req.id, json!(peer_list))
        }

        "getblock" => {
            let hash_str = req.params.get(0)
                .or_else(|| req.params.get("hash"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let chain = state.chain.read().await;

            // Try as height first
            if let Ok(height) = hash_str.parse::<u64>() {
                if let Some(block) = chain.block_at_height(height) {
                    return success(req.id, block_to_json(block));
                }
            }

            // Try as hash
            if let Ok(hash_bytes) = hex::decode(hash_str) {
                if hash_bytes.len() == 32 {
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(&hash_bytes);
                    if let Some(header) = chain.header(&hash) {
                        if let Some(block) = chain.block_at_height(header.height) {
                            return success(req.id, block_to_json(block));
                        }
                    }
                }
            }

            error(req.id, -32602, "block not found")
        }

        "getmininginfo" => {
            let chain = state.chain.read().await;
            let diff = chain.next_difficulty();
            success(req.id, json!({
                "height": chain.height + 1,
                "difficulty": diff,
                "fractional_difficulty": chain.fractional_difficulty(),
                "estimated_hashes": crate::core::types::estimated_hashes_for_difficulty(diff),
                "block_reward": block_reward(chain.height + 1) as f64 / COIN as f64,
            }))
        }

        _ => error(req.id, -32601, &format!("method '{}' not found", req.method)),
    }
}

fn block_to_json(block: &crate::core::types::Block) -> serde_json::Value {
    let txids: Vec<String> = block.transactions.iter()
        .map(|tx| hex::encode(tx.hash()))
        .collect();

    json!({
        "hash": hex::encode(block.header.hash()),
        "height": block.header.height,
        "version": block.header.version,
        "prev_hash": hex::encode(block.header.prev_hash),
        "merkle_root": hex::encode(block.header.merkle_root),
        "timestamp": block.header.timestamp,
        "difficulty": block.header.difficulty_target,
        "nonce": block.header.nonce,
        "tx_count": block.transactions.len(),
        "txids": txids,
        "size": block.size(),
    })
}

// ─── RPC Client (for CLI commands to query running node) ────────────

/// Send an RPC request to a running node and return the result
pub fn rpc_call(port: u16, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let request = RpcRequest {
        method: method.to_string(),
        params,
        id: 1,
    };

    let body = serde_json::to_string(&request).unwrap();

    // Simple blocking HTTP POST using std::net
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let addr = format!("127.0.0.1:{}", port);
    let mut stream = TcpStream::connect(&addr)
        .map_err(|_| format!("cannot connect to node RPC at {}. Is the node running?", addr))?;

    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).ok();

    let http_request = format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );

    stream.write_all(http_request.as_bytes())
        .map_err(|e| format!("write error: {}", e))?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response)
        .map_err(|e| format!("read error: {}", e))?;

    // Parse HTTP response - find the JSON body after \r\n\r\n
    let response_str = String::from_utf8_lossy(&response);
    let body_start = response_str.find("\r\n\r\n")
        .ok_or("invalid HTTP response")?;
    let json_body = &response_str[body_start + 4..];

    let rpc_response: RpcResponse = serde_json::from_str(json_body)
        .map_err(|e| format!("JSON parse error: {}", e))?;

    if let Some(err) = rpc_response.error {
        return Err(format!("RPC error {}: {}", err.code, err.message));
    }

    rpc_response.result.ok_or("empty result".to_string())
}

/// Try to call the running node's RPC. Returns None if node isn't running.
pub fn try_rpc_call(port: u16, method: &str, params: serde_json::Value) -> Option<serde_json::Value> {
    rpc_call(port, method, params).ok()
}

/// Generate the block explorer HTML page
fn explorer_html() -> String {
    r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>EquiForge Explorer</title>
<link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;500;600;700&family=Outfit:wght@300;400;500;600;700&display=swap" rel="stylesheet">
<style>
:root {
  --bg-primary: #06080d;
  --bg-secondary: #0c1018;
  --bg-card: #111720;
  --bg-card-hover: #161d2a;
  --border: #1c2435;
  --border-focus: #3b82f6;
  --text-primary: #e8ecf4;
  --text-secondary: #7a8599;
  --text-muted: #4a5568;
  --accent: #3b82f6;
  --accent-glow: rgba(59,130,246,0.15);
  --green: #22c55e;
  --green-dim: rgba(34,197,94,0.12);
  --amber: #f59e0b;
  --amber-dim: rgba(245,158,11,0.12);
  --red: #ef4444;
  --red-dim: rgba(239,68,68,0.12);
  --cyan: #06b6d4;
  --cyan-dim: rgba(6,182,212,0.1);
}
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:'Outfit',sans-serif;background:var(--bg-primary);color:var(--text-primary);min-height:100vh;overflow-x:hidden}
.mono{font-family:'JetBrains Mono',monospace}

/* ─── Header ─── */
.header{
  background:var(--bg-secondary);
  border-bottom:1px solid var(--border);
  padding:0 32px;
  height:64px;
  display:flex;
  align-items:center;
  justify-content:space-between;
  position:sticky;top:0;z-index:100;
  backdrop-filter:blur(12px);
}
.header-left{display:flex;align-items:center;gap:16px}
.logo{
  font-size:20px;font-weight:700;letter-spacing:-0.5px;
  background:linear-gradient(135deg,#3b82f6,#06b6d4);
  -webkit-background-clip:text;-webkit-text-fill-color:transparent;
}
.logo-icon{font-size:22px;filter:none;-webkit-text-fill-color:initial}
.net-badge{
  font-size:11px;font-weight:600;letter-spacing:0.5px;
  padding:3px 10px;border-radius:20px;
  background:var(--green-dim);color:var(--green);
  text-transform:uppercase;
}
.header-right{display:flex;align-items:center;gap:12px}
.live-dot{width:8px;height:8px;border-radius:50%;background:var(--green);animation:pulse 2s infinite}
@keyframes pulse{0%,100%{opacity:1}50%{opacity:0.4}}
.live-label{font-size:12px;color:var(--text-secondary);font-weight:500}

/* ─── Nav Tabs ─── */
.nav{
  display:flex;gap:2px;padding:0 32px;
  background:var(--bg-secondary);
  border-bottom:1px solid var(--border);
}
.nav-tab{
  padding:12px 20px;font-size:13px;font-weight:500;
  color:var(--text-secondary);cursor:pointer;
  border-bottom:2px solid transparent;
  transition:all 0.2s;
}
.nav-tab:hover{color:var(--text-primary)}
.nav-tab.active{color:var(--accent);border-bottom-color:var(--accent)}

/* ─── Container ─── */
.container{max-width:1200px;margin:0 auto;padding:24px 32px}
@media(max-width:768px){.container{padding:16px}.header{padding:0 16px}.nav{padding:0 16px}}

/* ─── Search ─── */
.search-wrap{position:relative;margin-bottom:28px}
.search-wrap input{
  width:100%;
  background:var(--bg-card);border:1px solid var(--border);
  border-radius:12px;padding:14px 18px 14px 44px;
  color:var(--text-primary);font-size:14px;font-family:'Outfit',sans-serif;
  transition:border-color 0.2s,box-shadow 0.2s;
}
.search-wrap input:focus{outline:none;border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-glow)}
.search-wrap input::placeholder{color:var(--text-muted)}
.search-icon{position:absolute;left:16px;top:50%;transform:translateY(-50%);color:var(--text-muted);font-size:16px}

/* ─── Stats Grid ─── */
.stats{display:grid;grid-template-columns:repeat(4,1fr);gap:12px;margin-bottom:28px}
@media(max-width:900px){.stats{grid-template-columns:repeat(2,1fr)}}
@media(max-width:500px){.stats{grid-template-columns:1fr}}
.stat{
  background:var(--bg-card);border:1px solid var(--border);
  border-radius:12px;padding:18px 20px;
  transition:border-color 0.2s,transform 0.15s;
}
.stat:hover{border-color:var(--border-focus);transform:translateY(-1px)}
.stat-label{font-size:11px;font-weight:600;text-transform:uppercase;letter-spacing:0.8px;color:var(--text-muted);margin-bottom:8px}
.stat-value{font-size:26px;font-weight:700;letter-spacing:-0.5px;line-height:1}
.stat-sub{font-size:12px;color:var(--text-secondary);margin-top:6px}
.stat-value.blue{color:var(--accent)}
.stat-value.green{color:var(--green)}
.stat-value.cyan{color:var(--cyan)}
.stat-value.amber{color:var(--amber)}

/* ─── Cards ─── */
.card{
  background:var(--bg-card);border:1px solid var(--border);
  border-radius:12px;margin-bottom:16px;overflow:hidden;
}
.card-head{
  padding:16px 20px;
  border-bottom:1px solid var(--border);
  display:flex;justify-content:space-between;align-items:center;
}
.card-head h2{font-size:15px;font-weight:600;color:var(--text-primary)}
.card-head .count{font-size:12px;color:var(--text-secondary);font-weight:500}
.card-body{padding:0}

/* ─── Table ─── */
table{width:100%;border-collapse:collapse}
thead th{
  text-align:left;font-size:11px;font-weight:600;
  text-transform:uppercase;letter-spacing:0.6px;
  color:var(--text-muted);padding:12px 20px;
  border-bottom:1px solid var(--border);
  background:var(--bg-secondary);
  position:sticky;top:0;
}
tbody td{
  padding:14px 20px;border-bottom:1px solid var(--border);
  font-size:13px;color:var(--text-secondary);
  transition:background 0.15s;
}
tbody tr{cursor:pointer;transition:background 0.15s}
tbody tr:hover td{background:var(--bg-card-hover)}
tbody tr:last-child td{border-bottom:none}
td.mono-cell{font-family:'JetBrains Mono',monospace;font-size:12px}

/* ─── Hash / Address ─── */
.hash-link{
  color:var(--accent);font-family:'JetBrains Mono',monospace;
  font-size:12px;cursor:pointer;
  transition:color 0.15s;text-decoration:none;
}
.hash-link:hover{color:#60a5fa;text-decoration:underline}
.full-hash{
  font-family:'JetBrains Mono',monospace;font-size:12px;
  color:var(--text-secondary);word-break:break-all;
  background:var(--bg-secondary);padding:8px 12px;border-radius:8px;
  display:inline-block;
}

/* ─── Badges ─── */
.badge{
  display:inline-flex;align-items:center;gap:4px;
  padding:4px 10px;border-radius:6px;
  font-size:11px;font-weight:600;
}
.badge-green{background:var(--green-dim);color:var(--green)}
.badge-blue{background:var(--accent-glow);color:var(--accent)}
.badge-amber{background:var(--amber-dim);color:var(--amber)}
.badge-cyan{background:var(--cyan-dim);color:var(--cyan)}

/* ─── Detail View (Block/Address) ─── */
.detail-grid{display:grid;grid-template-columns:160px 1fr;gap:0}
.detail-grid .dl{display:contents}
.detail-grid .dt{
  padding:12px 20px;font-size:12px;font-weight:600;
  color:var(--text-muted);text-transform:uppercase;letter-spacing:0.5px;
  border-bottom:1px solid var(--border);
  display:flex;align-items:center;
}
.detail-grid .dd{
  padding:12px 20px;font-size:13px;
  color:var(--text-secondary);
  border-bottom:1px solid var(--border);
  display:flex;align-items:center;word-break:break-all;
}
@media(max-width:600px){
  .detail-grid{grid-template-columns:1fr}
  .detail-grid .dt{padding-bottom:2px;border-bottom:none}
  .detail-grid .dd{padding-top:2px}
}

/* ─── Back Button ─── */
.back-btn{
  display:inline-flex;align-items:center;gap:6px;
  padding:8px 16px;border-radius:8px;
  background:var(--bg-card);border:1px solid var(--border);
  color:var(--text-secondary);font-size:13px;font-weight:500;
  cursor:pointer;transition:all 0.2s;margin-bottom:20px;
}
.back-btn:hover{border-color:var(--accent);color:var(--text-primary)}

/* ─── Peer Cards ─── */
.peer-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(280px,1fr));gap:12px;padding:16px 20px}
.peer-card{
  background:var(--bg-secondary);border:1px solid var(--border);
  border-radius:10px;padding:14px 16px;
}
.peer-card .peer-addr{font-family:'JetBrains Mono',monospace;font-size:12px;color:var(--accent);margin-bottom:8px}
.peer-card .peer-meta{display:flex;gap:16px;font-size:12px;color:var(--text-muted)}
.peer-card .peer-meta span{display:flex;align-items:center;gap:4px}

/* ─── Loading / Error ─── */
#loading{text-align:center;color:var(--text-muted);padding:60px 20px;font-size:14px}
#loading .spinner{
  width:32px;height:32px;border:3px solid var(--border);
  border-top-color:var(--accent);border-radius:50%;
  animation:spin 0.8s linear infinite;
  margin:0 auto 16px;
}
@keyframes spin{to{transform:rotate(360deg)}}
.error-banner{
  background:var(--red-dim);border:1px solid rgba(239,68,68,0.3);
  border-radius:10px;padding:12px 18px;margin-bottom:16px;
  color:var(--red);font-size:13px;display:none;
}

/* ─── Address balance header ─── */
.addr-header{
  display:flex;align-items:center;justify-content:space-between;
  flex-wrap:wrap;gap:12px;padding:20px;
}
.addr-bal{font-size:28px;font-weight:700;color:var(--green)}
.addr-bal .unit{font-size:14px;color:var(--text-muted);font-weight:500}

/* ─── Animations ─── */
.fade-in{animation:fadeIn 0.3s ease}
@keyframes fadeIn{from{opacity:0;transform:translateY(8px)}to{opacity:1;transform:translateY(0)}}
</style>
</head>
<body>

<div class="header">
  <div class="header-left">
    <div><span class="logo-icon">⛏️</span> <span class="logo">EquiForge</span></div>
    <div class="net-badge" id="netBadge">Mainnet</div>
  </div>
  <div class="header-right">
    <div class="live-dot"></div>
    <div class="live-label" id="liveLabel">Syncing...</div>
  </div>
</div>

<div class="nav">
  <div class="nav-tab active" data-tab="dashboard" onclick="switchTab('dashboard')">Overview</div>
  <div class="nav-tab" data-tab="blocks" onclick="switchTab('blocks')">Blocks</div>
  <div class="nav-tab" data-tab="peers" onclick="switchTab('peers')">Network</div>
</div>

<div class="container">
  <div class="error-banner" id="error"></div>

  <div class="search-wrap">
    <span class="search-icon">🔍</span>
    <input id="searchInput" placeholder="Search by block height, block hash, or wallet address..." 
           onkeydown="if(event.key==='Enter')search()" autocomplete="off" spellcheck="false">
  </div>

  <div id="content">
    <div id="loading"><div class="spinner"></div>Connecting to node...</div>
  </div>
</div>

<script>
const RPC = window.location.origin;
let currentTab = 'dashboard';
let chainInfo = null;

async function rpc(method, params=[]) {
  const r = await fetch(RPC, {method:'POST', headers:{'Content-Type':'application/json'},
    body: JSON.stringify({method, params, id:Date.now()})});
  const d = await r.json();
  if(d.error) throw new Error(d.error.message);
  return d.result;
}

function short(h){ return h ? h.slice(0,10)+'···'+h.slice(-6) : '—' }
function fmt(v){ return v !== undefined && v !== null ? Number(v).toLocaleString() : '—' }
function fmtEqf(v){ return v !== undefined && v !== null ? parseFloat(v).toFixed(v%1===0?0:4) : '—' }
function timeAgo(ts){
  const s = Math.floor(Date.now()/1000 - ts);
  if(s<60) return s+'s ago';
  if(s<3600) return Math.floor(s/60)+'m ago';
  if(s<86400) return Math.floor(s/3600)+'h ago';
  return Math.floor(s/86400)+'d ago';
}
function fmtTime(ts){ return new Date(ts*1000).toLocaleString() }
function fmtSize(b){
  if(b<1024) return b+' B';
  return (b/1024).toFixed(1)+' KB';
}

function switchTab(tab){
  currentTab = tab;
  document.querySelectorAll('.nav-tab').forEach(t => 
    t.classList.toggle('active', t.dataset.tab === tab));
  refresh();
}

async function refresh(){
  try {
    chainInfo = await rpc('getinfo');
    document.getElementById('liveLabel').textContent = 
      `Block #${chainInfo.height} · ${chainInfo.peers} peers`;

    if(currentTab === 'dashboard') await renderDashboard();
    else if(currentTab === 'blocks') await renderBlocks();
    else if(currentTab === 'peers') await renderPeers();

    document.getElementById('loading').style.display = 'none';
  } catch(e) {
    document.getElementById('loading').innerHTML = 
      '<div class="spinner"></div>Cannot connect to node. Is it running?';
    document.getElementById('loading').style.display = 'block';
  }
}

async function renderDashboard(){
  const info = chainInfo;
  const mining = await rpc('getmininginfo');

  // Calculate estimated hashrate from difficulty
  const estHashes = mining.estimated_hashes || 0;
  const hashrate = estHashes > 0 ? (estHashes / 90).toFixed(0) : '—';

  let html = `<div class="stats fade-in">
    <div class="stat">
      <div class="stat-label">Block Height</div>
      <div class="stat-value blue">${fmt(info.height)}</div>
      <div class="stat-sub">${fmt(info.known_blocks)} total known</div>
    </div>
    <div class="stat">
      <div class="stat-label">Difficulty</div>
      <div class="stat-value amber">${info.fractional_difficulty?.toFixed(2) ?? '—'}</div>
      <div class="stat-sub">${info.difficulty} bits · ~${fmt(estHashes)} hashes</div>
    </div>
    <div class="stat">
      <div class="stat-label">Network</div>
      <div class="stat-value green">${fmt(info.peers)}</div>
      <div class="stat-sub">connected peers</div>
    </div>
    <div class="stat">
      <div class="stat-label">Block Reward</div>
      <div class="stat-value cyan">${fmtEqf(info.block_reward)}</div>
      <div class="stat-sub">EQF per block</div>
    </div>
  </div>`;

  // Recent blocks
  html += `<div class="card fade-in">
    <div class="card-head"><h2>Recent Blocks</h2><span class="count">Latest 15</span></div>
    <div class="card-body"><table><thead><tr>
      <th>Height</th><th>Hash</th><th>Txs</th><th>Size</th><th>Difficulty</th><th>Time</th>
    </tr></thead><tbody id="blockRows">`;

  const height = info.height;
  const start = Math.max(0, height - 14);
  const blockPromises = [];
  for(let h = height; h >= start; h--) blockPromises.push(rpc('getblock',[String(h)]).catch(()=>null));
  const blocks = await Promise.all(blockPromises);

  for(const b of blocks) {
    if(!b) continue;
    html += `<tr onclick="loadBlock('${b.hash}')">
      <td><strong style="color:var(--text-primary)">${b.height}</strong></td>
      <td><span class="hash-link">${short(b.hash)}</span></td>
      <td>${b.tx_count}</td>
      <td class="mono-cell">${fmtSize(b.size)}</td>
      <td><span class="badge badge-amber">${b.difficulty} bits</span></td>
      <td style="color:var(--text-muted)">${timeAgo(b.timestamp)}</td>
    </tr>`;
  }
  html += '</tbody></table></div></div>';

  // Mempool
  try {
    const mp = await rpc('getmempool');
    if(mp.size > 0) {
      html += `<div class="card fade-in">
        <div class="card-head"><h2>Mempool</h2><span class="count">${mp.size} pending</span></div>
        <div class="card-body"><table><thead><tr><th>TXID</th><th>Size</th><th>Fee</th><th>Fee Rate</th></tr></thead><tbody>`;
      for(const tx of mp.transactions.slice(0,10)) {
        html += `<tr><td><span class="hash-link">${short(tx.txid)}</span></td>
          <td class="mono-cell">${tx.size} B</td>
          <td>${fmtEqf(tx.fee)} EQF</td>
          <td class="mono-cell">${tx.fee_rate?.toFixed(2) ?? '—'} sat/B</td></tr>`;
      }
      html += '</tbody></table></div></div>';
    }
  } catch(e){}

  document.getElementById('content').innerHTML = html;
}

async function renderBlocks(){
  const height = chainInfo.height;
  const count = 30;
  const start = Math.max(0, height - count + 1);

  let html = `<div class="card fade-in">
    <div class="card-head"><h2>All Blocks</h2><span class="count">${fmt(height+1)} total</span></div>
    <div class="card-body"><table><thead><tr>
      <th>Height</th><th>Hash</th><th>Txs</th><th>Size</th><th>Difficulty</th><th>Nonce</th><th>Time</th>
    </tr></thead><tbody>`;

  const promises = [];
  for(let h = height; h >= start; h--) promises.push(rpc('getblock',[String(h)]).catch(()=>null));
  const blocks = await Promise.all(promises);

  for(const b of blocks) {
    if(!b) continue;
    html += `<tr onclick="loadBlock('${b.hash}')">
      <td><strong style="color:var(--text-primary)">${b.height}</strong></td>
      <td><span class="hash-link">${short(b.hash)}</span></td>
      <td>${b.tx_count}</td>
      <td class="mono-cell">${fmtSize(b.size)}</td>
      <td><span class="badge badge-amber">${b.difficulty} bits</span></td>
      <td class="mono-cell" style="color:var(--text-muted)">${fmt(b.nonce)}</td>
      <td style="color:var(--text-muted)">${timeAgo(b.timestamp)}</td>
    </tr>`;
  }
  html += '</tbody></table></div></div>';
  document.getElementById('content').innerHTML = html;
}

async function renderPeers(){
  const peers = await rpc('getpeerinfo');
  let html = `<div class="stats fade-in">
    <div class="stat">
      <div class="stat-label">Connected Peers</div>
      <div class="stat-value green">${peers.length}</div>
    </div>
    <div class="stat">
      <div class="stat-label">Protocol Versions</div>
      <div class="stat-value">${[...new Set(peers.map(p=>p.version))].join(', ')}</div>
    </div>
    <div class="stat">
      <div class="stat-label">Max Peer Height</div>
      <div class="stat-value blue">${fmt(Math.max(...peers.map(p=>p.best_height),0))}</div>
    </div>
    <div class="stat">
      <div class="stat-label">UTXOs</div>
      <div class="stat-value cyan">${fmt(chainInfo.utxos)}</div>
    </div>
  </div>`;

  html += `<div class="card fade-in">
    <div class="card-head"><h2>Connected Peers</h2><span class="count">${peers.length} active</span></div>
    <div class="card-body">`;

  if(peers.length > 0) {
    html += '<div class="peer-grid">';
    for(const p of peers.sort((a,b)=>b.best_height-a.best_height)) {
      const seen = p.last_seen ? timeAgo(p.last_seen) : 'unknown';
      html += `<div class="peer-card">
        <div class="peer-addr">${p.address}</div>
        <div class="peer-meta">
          <span>📦 Height ${fmt(p.best_height)}</span>
          <span>🔗 v${p.version}</span>
          <span>🕐 ${seen}</span>
        </div>
      </div>`;
    }
    html += '</div>';
  } else {
    html += '<div style="padding:40px;text-align:center;color:var(--text-muted)">No peers connected</div>';
  }
  html += '</div></div>';
  document.getElementById('content').innerHTML = html;
}

async function loadBlock(hashOrHeight){
  try {
    const b = await rpc('getblock', [hashOrHeight]);
    let html = `<button class="back-btn fade-in" onclick="refresh()">← Back</button>`;

    html += `<div class="card fade-in">
      <div class="card-head">
        <h2>Block #${b.height}</h2>
        <div style="display:flex;gap:8px">
          <span class="badge badge-amber">${b.difficulty} bits</span>
          <span class="badge badge-cyan">${b.tx_count} tx</span>
        </div>
      </div>
      <div class="card-body">
        <div class="detail-grid">
          <div class="dl"><div class="dt">Hash</div><div class="dd"><span class="full-hash">${b.hash}</span></div></div>
          <div class="dl"><div class="dt">Previous</div><div class="dd">${b.height>0?`<span class="hash-link" onclick="loadBlock('${b.prev_hash}')">${b.prev_hash}</span>`:'Genesis Block'}</div></div>
          <div class="dl"><div class="dt">Merkle Root</div><div class="dd"><span style="font-family:'JetBrains Mono',monospace;font-size:12px;color:var(--text-muted);word-break:break-all">${b.merkle_root}</span></div></div>
          <div class="dl"><div class="dt">Timestamp</div><div class="dd">${fmtTime(b.timestamp)} <span style="color:var(--text-muted);margin-left:8px">(${timeAgo(b.timestamp)})</span></div></div>
          <div class="dl"><div class="dt">Nonce</div><div class="dd mono-cell">${fmt(b.nonce)}</div></div>
          <div class="dl"><div class="dt">Size</div><div class="dd">${fmtSize(b.size)}</div></div>
          <div class="dl"><div class="dt">Version</div><div class="dd">${b.version}</div></div>
        </div>
      </div>
    </div>`;

    html += `<div class="card fade-in">
      <div class="card-head"><h2>Transactions</h2><span class="count">${b.tx_count} in block</span></div>
      <div class="card-body"><table><thead><tr><th>#</th><th>Transaction ID</th></tr></thead><tbody>`;
    b.txids.forEach((txid,i) => {
      html += `<tr><td style="color:var(--text-muted);width:40px">${i}</td>
        <td><span class="hash-link" style="font-size:12px">${txid}</span>
        ${i===0?'<span class="badge badge-blue" style="margin-left:8px">coinbase</span>':''}
        </td></tr>`;
    });
    html += '</tbody></table></div></div>';

    // Navigation
    html += '<div style="display:flex;gap:8px;margin-top:8px" class="fade-in">';
    if(b.height > 0) html += `<button class="back-btn" onclick="loadBlock('${b.height-1}')" style="margin:0">← Block #${b.height-1}</button>`;
    html += `<button class="back-btn" onclick="loadBlock('${b.height+1}')" style="margin:0">Block #${b.height+1} →</button>`;
    html += '</div>';

    document.getElementById('content').innerHTML = html;
    document.getElementById('error').style.display = 'none';
    window.scrollTo({top:0,behavior:'smooth'});
  } catch(e) { showError(e.message); }
}

async function loadAddress(addr){
  try {
    const [bal, utxos] = await Promise.all([
      rpc('getbalance', [addr]),
      rpc('listunspent', [addr])
    ]);
    let html = `<button class="back-btn fade-in" onclick="refresh()">← Back</button>`;

    html += `<div class="card fade-in">
      <div class="addr-header">
        <div>
          <div style="font-size:12px;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.5px;margin-bottom:8px">Wallet Address</div>
          <div class="full-hash">${addr}</div>
        </div>
        <div class="addr-bal">${fmtEqf(bal.balance)} <span class="unit">EQF</span></div>
      </div>
      <div class="card-body">
        <div class="detail-grid">
          <div class="dl"><div class="dt">Balance</div><div class="dd">${fmtEqf(bal.balance)} EQF</div></div>
          <div class="dl"><div class="dt">Raw Balance</div><div class="dd mono-cell">${fmt(bal.balance_base)} base units</div></div>
          <div class="dl"><div class="dt">UTXOs</div><div class="dd">${utxos.length} unspent outputs</div></div>
        </div>
      </div>
    </div>`;

    if(utxos.length > 0) {
      html += `<div class="card fade-in">
        <div class="card-head"><h2>Unspent Outputs</h2><span class="count">${utxos.length} UTXOs</span></div>
        <div class="card-body"><table><thead><tr><th>TXID</th><th>Output</th><th>Amount</th><th>Block</th><th>Type</th></tr></thead><tbody>`;
      for(const u of utxos.sort((a,b)=>b.height-a.height)) {
        html += `<tr>
          <td><span class="hash-link">${short(u.txid)}</span></td>
          <td class="mono-cell">${u.vout}</td>
          <td><strong style="color:var(--green)">${fmtEqf(u.amount)} EQF</strong></td>
          <td><span class="hash-link" onclick="loadBlock('${u.height}')">#${u.height}</span></td>
          <td>${u.coinbase?'<span class="badge badge-cyan">⛏ mined</span>':'<span class="badge badge-blue">transfer</span>'}</td>
        </tr>`;
      }
      html += '</tbody></table></div></div>';
    }

    document.getElementById('content').innerHTML = html;
    document.getElementById('error').style.display = 'none';
    window.scrollTo({top:0,behavior:'smooth'});
  } catch(e) { showError(e.message); }
}

function search(){
  const q = document.getElementById('searchInput').value.trim();
  if(!q) return;
  if(/^\d+$/.test(q)) loadBlock(q);
  else if(q.length===64 && /^[0-9a-f]+$/i.test(q)) loadBlock(q);
  else loadAddress(q);
}

function showError(msg){
  const el = document.getElementById('error');
  el.textContent = '⚠ ' + msg;
  el.style.display = 'block';
  setTimeout(() => el.style.display = 'none', 5000);
}

// Init
refresh();
setInterval(()=>{ if(currentTab==='dashboard') refresh() }, 15000);
</script>
</body>
</html>"##.to_string()
}