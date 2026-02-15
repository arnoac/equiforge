# EquiForge

A fair, accessible, ASIC-resistant blockchain built in Rust.

EquiForge uses **EquiHash-X**, a memory-hard proof-of-work algorithm requiring 4 MB per hash with 64 mixing iterations, making dedicated mining hardware impractical while keeping CPUs and GPUs competitive. Anyone with a computer can mine.

---

## Network Parameters

| Parameter | Value |
|-----------|-------|
| Max Supply | 42,000,000 EQF |
| Block Reward | 50 EQF (47.5 miner + 2.5 community fund) |
| Block Time | 90 seconds |
| Halving Interval | 2,103,840 blocks (~6 years) |
| Difficulty Adjustment | Every block (60-block rolling window) |
| Max Block Size | 4 MB |
| Coinbase Maturity | 100 blocks |
| P2P Port | 9333 (mainnet) / 19333 (testnet) |
| RPC/Explorer Port | 9334 (mainnet) / 19334 (testnet) |
| PoW Algorithm | EquiHash-X v1 (memory-hard, 4 MB/hash) |

---

## Quick Start

### 1. Install

```bash
git clone https://github.com/yourusername/equiforge.git
cd equiforge
cargo build --release
```

The binary is at `./target/release/equiforge` (Linux/macOS) or `.\target\release\equiforge.exe` (Windows).

### 2. Initialize

```bash
equiforge init --testnet
```

This creates your wallet and data directory. Your wallet address will be displayed — save it.

### 3. Run a Node

```bash
equiforge node --testnet
```

Your node will connect to seed nodes, sync the blockchain, and begin participating in the network. The built-in block explorer is available at `http://localhost:19334`.

---

## Mining

EquiForge offers three ways to mine, from simplest to most advanced.

### Method 1: Solo Mining (Easiest)

Mine directly on your own node. You find blocks yourself and keep the full miner reward (47.5 EQF per block). Simple but you may wait a long time between blocks as difficulty increases.

```bash
equiforge node --mine --testnet
```

**Options:**

| Flag | Description |
|------|-------------|
| `--mine` or `-m` | Enable mining |
| `--threads N` or `-t N` | CPU threads to use (default: all cores) |
| `--miner-tag "name"` | Identity tag embedded in blocks you mine (max 32 chars) |

**Examples:**

```bash
# Mine with all CPU cores
equiforge node --mine --testnet

# Mine with 4 threads and a custom tag
equiforge node --mine -t 4 --miner-tag "MyRig" --testnet

# Mine and connect to a specific peer
equiforge node --mine --connect 44.55.66.77:19333 --testnet
```

Your mined blocks will show your tag in the block explorer. Solo mining requires running a full node and storing the entire blockchain.

---

### Method 2: Pool Mining (Recommended)

Connect to a mining pool and earn rewards proportional to your contributed hashrate. No blockchain download required — just point your miner at a pool and start earning.

**This is the best option for most miners.** You earn steady, smaller payouts instead of waiting for rare solo blocks.

#### Connecting to a Pool

```bash
equiforge pool-mine \
  --pool 129.80.239.237:19335 \
  --address YOUR_WALLET_ADDRESS \
  --threads 4
```

**Options:**

| Flag | Description |
|------|-------------|
| `--pool HOST:PORT` | Pool server address (can specify multiple for failover) |
| `--address ADDR` | Your wallet address for payouts |
| `--threads N` or `-t N` | CPU threads to use (default: 1) |
| `--worker NAME` | Worker name to identify this machine (default: hostname) |

#### Multi-Pool Failover

Specify multiple pools for automatic failover. The miner probes latency to each pool on startup, connects to the fastest one, and automatically switches to the next if the current pool goes down.

```bash
equiforge pool-mine \
  --pool 129.80.239.237:19335 \
  --pool 44.55.66.77:19335 \
  --pool 88.99.11.22:19335 \
  --address YOUR_WALLET_ADDRESS \
  --worker my-desktop \
  -t 4
```

**Output:**

```
⛏️  EquiForge Pool Miner
   Pools:   3 configured
   Worker:  my-desktop
   Threads: 4

🌐 Pool latency probe:
     23ms  44.55.66.77:19335 ← best
    142ms  129.80.239.237:19335
    ---    88.99.11.22:19335 (unreachable)

🔗 Connecting to 44.55.66.77:19335 (23ms latency)...
✅ Connected to 44.55.66.77:19335
📋 Job #1: height=138 diff=10/14
📤 Share #1: nonce=8827361 zeros=11/10
✅ Accepted (pool total: 1, est: 128.0 H/s, session: 16s)
```

If a pool goes down mid-mining, you'll see:

```
❌ 44.55.66.77:19335 — connection reset
🔄 Failing over to next pool...
🔗 Connecting to 129.80.239.237:19335 (142ms latency)...
✅ Connected to 129.80.239.237:19335
```

#### Getting a Wallet Address

If you only want to pool mine (no full node), you can generate a wallet without syncing:

```bash
equiforge init --testnet
equiforge wallet show --testnet
```

Copy your `eq1q...` address and use it with `--address`.

---

### Method 3: Running Your Own Pool (Advanced)

Run a pool server that other miners can connect to. This requires running a full node. You earn a pool operator fee (default 1%) from all blocks your pool finds.

```bash
equiforge node --mine --pool --pool-port 19335 --miner-tag "MyPool" --testnet
```

**Options:**

| Flag | Description |
|------|-------------|
| `--pool` | Enable the built-in pool server |
| `--pool-port PORT` | Port for miners to connect to (default: 9334 mainnet / 19335 testnet) |
| `--miner-tag "name"` | Pool identity embedded in blocks (shows as `pool:name` in explorer) |

#### Pool Server Requirements

- **A public IP or VPS** — Miners must be able to reach your pool server from the internet.
- **Open the pool port** — Ensure your firewall allows inbound TCP on the pool port.
- **Stable internet** — The pool server must stay connected to the P2P network.

#### Setting Up on a VPS (Recommended)

1. **Provision a VPS** — Oracle Cloud Free Tier, AWS, DigitalOcean, etc.

2. **Build and initialize:**
   ```bash
   git clone https://github.com/yourusername/equiforge.git
   cd equiforge
   cargo build --release
   ./target/release/equiforge init --testnet
   ```

3. **Open firewall ports:**
   ```bash
   # P2P port + Pool port
   sudo ufw allow 19333/tcp
   sudo ufw allow 19335/tcp
   ```

4. **Run the pool:**
   ```bash
   ./target/release/equiforge node --mine --pool --pool-port 19335 --miner-tag "MyPool-US" --testnet
   ```

5. **Tell miners to connect:**
   ```
   equiforge pool-mine --pool YOUR_VPS_IP:19335 --address THEIR_ADDRESS -t 4
   ```

#### Pool Architecture

The pool server runs inside the node process. It shares the node's blockchain state directly (no RPC overhead). When the pool finds a block:

1. Pool operator's address receives the block reward
2. Pool distributes payouts to miners via regular transactions based on PPLNS (Pay Per Last N Shares)
3. The block's coinbase contains the pool's identity tag (e.g., `pool:MyPool-US`)

#### Opening Ports on Windows (for local testing)

If running a pool on your home PC:

```powershell
# Windows Firewall (run PowerShell as Admin)
New-NetFirewallRule -DisplayName "EquiForge Pool" -Direction Inbound -Port 19335 -Protocol TCP -Action Allow
```

Then configure port forwarding on your router: External port `19335` → your PC's local IP → port `19335` TCP.

> **Note:** You cannot connect to your own public IP from inside your network (hairpin NAT limitation). Use `127.0.0.1:19335` for local testing. Miners outside your network can connect to your public IP normally.

---

## Wallet

### View Your Wallet

```bash
equiforge wallet show --testnet
```

### Generate a New Address

```bash
equiforge wallet new-address --testnet
```

### Check Balance

```bash
# All wallet addresses
equiforge balance --testnet

# Specific address
equiforge balance eq1qz2sgf... --testnet
```

### Send EQF

```bash
equiforge send --to eq1qRECIPIENT --amount 10.5 --testnet
```

The default fee is 0.0001 EQF. Customize with `--fee`:

```bash
equiforge send --to eq1qRECIPIENT --amount 10.5 --fee 0.001 --testnet
```

### Encrypt Your Wallet

```bash
# Encrypt
equiforge wallet encrypt --password "your-passphrase" --testnet

# Decrypt
equiforge wallet decrypt --password "your-passphrase" --testnet

# Run with encrypted wallet
equiforge node --mine --password "your-passphrase" --testnet
```

---

## Node Operations

### Blockchain Info

```bash
equiforge info --testnet
```

### Connected Peers

```bash
equiforge peers --testnet
```

### Snapshots (Fast Sync)

Export the blockchain for others to bootstrap quickly:

```bash
# Export
equiforge export-snapshot --output chain.bin --testnet

# Import
equiforge import-snapshot --input chain.bin --testnet
```

### Custom Port

```bash
equiforge node --port 29333 --testnet
```

---

## Block Explorer

Every running node includes a built-in block explorer. Open your browser to:

- **Testnet:** `http://localhost:19334`
- **Mainnet:** `http://localhost:9334`

The explorer shows:

- Real-time blocks with miner identity tags
- Transaction details with input/output tracking
- Address pages with spendable/immature balance breakdown
- Network statistics (hashrate, difficulty, peer count)
- UTXO details with coinbase maturity status
- Mempool viewer

---

## RPC API

The node exposes a JSON-RPC API on the RPC port (P2P port + 1).

### Example Request

```bash
curl -s http://127.0.0.1:19334 \
  -d '{"method":"getinfo","params":[],"id":1}'
```

### Available Methods

| Method | Params | Description |
|--------|--------|-------------|
| `getinfo` | `[]` | Node status, height, difficulty, peers |
| `getblock` | `[height]` or `[hash]` | Block details with transactions |
| `gettx` | `[txid]` | Transaction details |
| `getbalance` | `[address]` | Address balance |
| `getaddress` | `[address]` | Full address info: balance, UTXOs, tx history |
| `getmempool` | `[]` | Pending transactions |
| `getpeerinfo` | `[]` | Connected peer details |
| `getrichlist` | `[]` | Top addresses by balance |
| `getblocktemplate` | `[pubkey_hash_hex]` | Block template for external mining |
| `submitblock` | `[header_hex, nonce, [tx_hex...]]` | Submit a mined block |

### External Pool Integration

Third-party pool operators can use `getblocktemplate` and `submitblock` to build pools without modifying the node:

```bash
# Get a block template
curl -s http://127.0.0.1:19334 \
  -d '{"method":"getblocktemplate","params":["YOUR_PUBKEY_HASH_HEX"],"id":1}'

# Submit a mined block
curl -s http://127.0.0.1:19334 \
  -d '{"method":"submitblock","params":["HEADER_HEX","NONCE","TX_HEX_ARRAY"],"id":1}'
```

---

## Testnet vs Mainnet

| | Testnet | Mainnet |
|---|---------|---------|
| Flag | `--testnet` | *(none)* |
| P2P Port | 19333 | 9333 |
| RPC Port | 19334 | 9334 |
| Data Directory | `equiforge_testnet/` | `equiforge_data/` |
| Seed Node | 129.80.239.237:19333 | 129.80.239.237:9333 |

Always use `--testnet` while the network is in testing. Mainnet will launch with a fresh genesis block.

---

## Building from Source

### Requirements

- Rust 1.75+ (install from [rustup.rs](https://rustup.rs))
- A C compiler (gcc/clang/MSVC)

### Build

```bash
git clone https://github.com/yourusername/equiforge.git
cd equiforge
cargo build --release
```

### Run Tests

```bash
cargo test
```

### Test Mining (No Network)

Mine blocks in memory to verify everything works:

```bash
equiforge test-mine 10
```

---

## Mining Performance Reference

EquiHash-X is intentionally slow per hash (~100-200 H/s per core) to keep mining fair across hardware:

| Hardware | Approximate Hashrate |
|----------|---------------------|
| 1 CPU core | ~100-200 H/s |
| 4 CPU cores | ~400-800 H/s |
| 8 CPU cores | ~800-1600 H/s |
| 16 CPU cores | ~1600-3200 H/s |

At current testnet difficulty, blocks are found approximately every 90 seconds across all miners combined.

---

## Project Structure

```
src/
├── core/
│   ├── types.rs        # Block, Transaction, Hash types
│   ├── chain.rs        # Blockchain state, validation, difficulty
│   └── params.rs       # Network parameters, rewards, halving
├── crypto/             # Ed25519 signatures, key derivation
├── miner/              # Block template creation, parallel mining
├── network/            # P2P gossip, peer management, sync
├── pool/
│   ├── mod.rs          # Pool server, PPLNS rewards, shared protocol
│   └── pool_miner.rs   # Lightweight pool miner client
├── pow/                # EquiHash-X proof-of-work algorithm
├── rpc/
│   ├── mod.rs          # JSON-RPC server
│   └── explorer.html   # Built-in block explorer
├── storage/            # Block persistence, UTXO set
├── wallet/             # Key management, address encoding
├── lib.rs
└── main.rs             # CLI entry point
```

---

## License

[Your license here]

---

## Links

- **Block Explorer:** http://129.80.239.237:19334
- **Testnet Seed Node:** 129.80.239.237:19333
- **Testnet Pool:** 129.80.239.237:19335