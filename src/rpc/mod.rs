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

    // Check if it's a GET request (serve explorer UI)
    if request_line.starts_with("GET") {
        // Drain headers
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.is_err() { break; }
            if line.trim().is_empty() { break; }
        }

        let html = explorer_html();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{}",
            html.len(), html
        );
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
<title>EquiForge Block Explorer</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,monospace;background:#0a0e17;color:#c9d1d9;min-height:100vh}
.header{background:linear-gradient(135deg,#161b22 0%,#0d1117 100%);border-bottom:1px solid #30363d;padding:20px 32px}
.header h1{font-size:24px;color:#58a6ff;font-weight:600}
.header span{color:#8b949e;font-size:13px;margin-left:12px}
.container{max-width:1100px;margin:0 auto;padding:24px}
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px;margin-bottom:24px}
.stat{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:16px}
.stat .label{color:#8b949e;font-size:12px;text-transform:uppercase;letter-spacing:0.5px}
.stat .value{color:#f0f6fc;font-size:22px;font-weight:600;margin-top:4px}
.stat .value.green{color:#3fb950}
.stat .value.blue{color:#58a6ff}
.card{background:#161b22;border:1px solid #30363d;border-radius:8px;margin-bottom:16px;overflow:hidden}
.card-header{padding:12px 16px;border-bottom:1px solid #30363d;display:flex;justify-content:space-between;align-items:center}
.card-header h2{font-size:15px;color:#f0f6fc}
.card-body{padding:16px}
table{width:100%;border-collapse:collapse}
th{text-align:left;color:#8b949e;font-size:12px;text-transform:uppercase;padding:8px 12px;border-bottom:1px solid #30363d}
td{padding:10px 12px;border-bottom:1px solid #21262d;font-size:13px}
tr:hover td{background:#1c2129}
.hash{color:#58a6ff;font-family:monospace;font-size:12px;cursor:pointer}
.hash:hover{text-decoration:underline}
.search{display:flex;gap:8px;margin-bottom:24px}
.search input{flex:1;background:#0d1117;border:1px solid #30363d;border-radius:6px;padding:10px 14px;color:#c9d1d9;font-size:14px;font-family:monospace}
.search input:focus{outline:none;border-color:#58a6ff}
.search button{background:#238636;color:#fff;border:none;border-radius:6px;padding:10px 20px;cursor:pointer;font-weight:600}
.search button:hover{background:#2ea043}
.badge{display:inline-block;padding:2px 8px;border-radius:12px;font-size:11px;font-weight:600}
.badge-green{background:#0d2818;color:#3fb950}
.badge-blue{background:#0c2d6b;color:#58a6ff}
#loading{text-align:center;color:#8b949e;padding:40px}
.block-detail{display:none}
.block-detail.active{display:block}
#error{color:#f85149;padding:12px;display:none}
</style>
</head>
<body>
<div class="header">
  <h1>⛏️ EquiForge <span>Block Explorer</span></h1>
</div>
<div class="container">
  <div class="search">
    <input id="searchInput" placeholder="Search by block height, hash, or address..." onkeydown="if(event.key==='Enter')search()">
    <button onclick="search()">Search</button>
  </div>
  <div id="error"></div>
  <div id="stats" class="stats"></div>
  <div id="content"></div>
  <div id="loading">Loading...</div>
</div>
<script>
const RPC = window.location.origin;
async function rpc(method, params=[]) {
  const r = await fetch(RPC, {method:'POST', headers:{'Content-Type':'application/json'},
    body: JSON.stringify({method, params, id:1})});
  const d = await r.json();
  if(d.error) throw new Error(d.error.message);
  return d.result;
}
function short(h){return h?h.slice(0,12)+'…'+h.slice(-6):''}
function fmt(v){return v !== undefined && v !== null ? v : '—'}
function fmtEqf(base){
  if(!base && base !== 0) return '—';
  return (base/1e8).toFixed(base%1e8===0?0:8).replace(/\.?0+$/,'');
}

async function loadDashboard() {
  try {
    const info = await rpc('getinfo');
    document.getElementById('stats').innerHTML = `
      <div class="stat"><div class="label">Height</div><div class="value blue">${fmt(info.height)}</div></div>
      <div class="stat"><div class="label">Difficulty</div><div class="value">${info.fractional_difficulty?.toFixed(1) ?? '—'}</div></div>
      <div class="stat"><div class="label">UTXOs</div><div class="value">${fmt(info.utxos)}</div></div>
      <div class="stat"><div class="label">Peers</div><div class="value green">${fmt(info.peers)}</div></div>
      <div class="stat"><div class="label">Mempool</div><div class="value">${fmt(info.mempool)}</div></div>
      <div class="stat"><div class="label">Banned</div><div class="value">${fmt(info.banned)}</div></div>
      <div class="stat"><div class="label">Block Reward</div><div class="value">${fmt(info.block_reward)} EQF</div></div>
    `;
    // Load recent blocks
    const height = info.height;
    let html = '<div class="card"><div class="card-header"><h2>Recent Blocks</h2></div><div class="card-body"><table><tr><th>Height</th><th>Hash</th><th>Txs</th><th>Difficulty</th><th>Time</th></tr>';
    const start = Math.max(0, height - 14);
    for(let h = height; h >= start; h--) {
      try {
        const b = await rpc('getblock', [String(h)]);
        const t = new Date(b.timestamp * 1000).toLocaleTimeString();
        html += `<tr onclick="loadBlock('${b.hash}')" style="cursor:pointer">
          <td><strong>${b.height}</strong></td>
          <td><span class="hash">${short(b.hash)}</span></td>
          <td>${b.tx_count}</td><td>${b.difficulty} bits</td><td>${t}</td></tr>`;
      } catch(e) {}
    }
    html += '</table></div></div>';
    document.getElementById('content').innerHTML = html;
    document.getElementById('loading').style.display = 'none';
  } catch(e) {
    document.getElementById('loading').innerHTML = '❌ Cannot connect to node RPC. Is the node running?';
  }
}

async function loadBlock(hashOrHeight) {
  try {
    const b = await rpc('getblock', [hashOrHeight]);
    const time = new Date(b.timestamp * 1000).toLocaleString();
    let html = `<div class="card"><div class="card-header"><h2>Block #${b.height}</h2>
      <span class="badge badge-blue">${b.difficulty} bits</span></div><div class="card-body">
      <table>
      <tr><td><strong>Hash</strong></td><td class="hash">${b.hash}</td></tr>
      <tr><td><strong>Previous</strong></td><td><span class="hash" onclick="loadBlock('${b.prev_hash}')">${b.prev_hash}</span></td></tr>
      <tr><td><strong>Merkle Root</strong></td><td style="font-family:monospace;font-size:12px">${b.merkle_root}</td></tr>
      <tr><td><strong>Timestamp</strong></td><td>${time} (${b.timestamp})</td></tr>
      <tr><td><strong>Nonce</strong></td><td>${b.nonce}</td></tr>
      <tr><td><strong>Transactions</strong></td><td>${b.tx_count}</td></tr>
      <tr><td><strong>Size</strong></td><td>${b.size} bytes</td></tr>
      </table></div></div>`;
    html += '<div class="card"><div class="card-header"><h2>Transactions</h2></div><div class="card-body"><table><tr><th>TXID</th></tr>';
    for(const txid of b.txids) {
      html += `<tr><td class="hash">${txid}</td></tr>`;
    }
    html += '</table></div></div>';
    html += `<button onclick="loadDashboard()" style="background:#30363d;color:#c9d1d9;border:none;border-radius:6px;padding:8px 16px;cursor:pointer;margin-top:8px">← Back</button>`;
    document.getElementById('content').innerHTML = html;
    document.getElementById('error').style.display = 'none';
  } catch(e) {
    showError(e.message);
  }
}

async function loadAddress(addr) {
  try {
    const bal = await rpc('getbalance', [addr]);
    const utxos = await rpc('listunspent', [addr]);
    let html = `<div class="card"><div class="card-header"><h2>Address</h2><span class="badge badge-green">${bal.balance} EQF</span></div><div class="card-body">
      <table><tr><td><strong>Address</strong></td><td class="hash">${addr}</td></tr>
      <tr><td><strong>Balance</strong></td><td>${bal.balance} EQF (${bal.balance_base} base units)</td></tr>
      <tr><td><strong>UTXOs</strong></td><td>${utxos.length}</td></tr></table></div></div>`;
    if(utxos.length > 0) {
      html += '<div class="card"><div class="card-header"><h2>Unspent Outputs</h2></div><div class="card-body"><table><tr><th>TXID</th><th>Vout</th><th>Amount</th><th>Height</th><th>Type</th></tr>';
      for(const u of utxos) {
        html += `<tr><td class="hash">${short(u.txid)}</td><td>${u.vout}</td><td>${u.amount} EQF</td><td>${u.height}</td><td>${u.coinbase?'<span class="badge badge-blue">coinbase</span>':'tx'}</td></tr>`;
      }
      html += '</table></div></div>';
    }
    html += `<button onclick="loadDashboard()" style="background:#30363d;color:#c9d1d9;border:none;border-radius:6px;padding:8px 16px;cursor:pointer;margin-top:8px">← Back</button>`;
    document.getElementById('content').innerHTML = html;
    document.getElementById('error').style.display = 'none';
  } catch(e) {
    showError(e.message);
  }
}

function search() {
  const q = document.getElementById('searchInput').value.trim();
  if(!q) return;
  if(/^\d+$/.test(q)) { loadBlock(q); }
  else if(q.length === 64 && /^[0-9a-f]+$/i.test(q)) { loadBlock(q); }
  else { loadAddress(q); }
}

function showError(msg) {
  const el = document.getElementById('error');
  el.textContent = '❌ ' + msg;
  el.style.display = 'block';
  setTimeout(() => el.style.display = 'none', 5000);
}

loadDashboard();
setInterval(loadDashboard, 15000);
</script>
</body>
</html>"##.to_string()
}
