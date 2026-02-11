<div align="center">

# ⛏️ EquiForge

**A fair, accessible, ASIC-resistant blockchain built from scratch in Rust.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![Network](https://img.shields.io/badge/Network-LIVE-brightgreen.svg)](#start-mining)

*CPU-mineable • 42M max supply • 90-second blocks • Memory-hard PoW*

</div>

---

## Why EquiForge?

Most cryptocurrencies have been captured by ASIC farms and mining pools, making it impossible for regular people to participate. EquiForge is different:

- **EquiHash-X** — A custom memory-hard proof-of-work algorithm requiring 4 MB RAM per hash. ASICs can't economically compete with CPUs.
- **Fair launch** — No premine, no ICO, no VC allocation. Every coin is mined.
- **Simple** — Download the binary, run one command, start mining. No pool required.

## Start Mining

### Option 1: Download Binary (Recommended)

Download the latest release for your platform from [Releases](https://github.com/equiforge/equiforge/releases):

| Platform | File |
|---|---|
| Windows x64 | `equiforge-windows-x64.exe` |
| Linux x64 | `equiforge-linux-x64` |
| Linux ARM64 | `equiforge-linux-arm64` |

Then run:

```bash
# Initialize (first time only)
./equiforge init

# Start mining — automatically connects to the network
./equiforge node --mine
```

That's it. You're mining EquiForge.

### Option 2: Build from Source

```bash
# Install Rust (if needed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone and build
git clone https://github.com/equiforge/equiforge.git
cd equiforge
cargo build --release

# Initialize and mine
./target/release/equiforge init
./target/release/equiforge node --mine
```

## Block Explorer

Once the node is running, open the built-in block explorer:

**Public explorer:** http://129.80.239.237:9334

**Local explorer:** http://127.0.0.1:9334

## Commands

### Node

```bash
equiforge node --mine                   # Mine with all CPU cores
equiforge node --mine --threads 4       # Mine with 4 threads
equiforge node                          # Run node without mining (relay only)
```

### Wallet

```bash
equiforge balance                       # Show your balance
equiforge balance <ADDRESS>             # Check any address
equiforge send --to <ADDR> --amount 10  # Send 10 EQF
equiforge wallet show                   # List all your addresses
equiforge wallet new-address            # Generate a new receiving address
equiforge wallet encrypt --password X   # Encrypt your wallet
equiforge wallet decrypt --password X   # Remove encryption
```

### Info

```bash
equiforge info                          # Chain status, difficulty, peers
equiforge peers                         # Connected peers
```

### Advanced

```bash
equiforge --data-dir mynode --port 9335 node --mine   # Custom data dir & port
equiforge test-mine 10                                 # Test mine 10 blocks (in-memory)
```

## Multi-Miner Setup

Run multiple miners on the same machine for testing:

```bash
# Miner A (default port 9333)
equiforge node --mine

# Miner B (different port & data dir)
equiforge --data-dir node2 --port 9335 node --mine

# Miner C
equiforge --data-dir node3 --port 9337 node --mine
```

All miners auto-discover each other via the seed node.

## Network

| Property | Value |
|---|---|
| **Seed Node** | `129.80.239.237:9333` |
| **Explorer** | http://129.80.239.237:9334 |
| **P2P Port** | 9333 |
| **RPC Port** | 9334 |

The network uses gossip-based peer discovery. Connect to the seed node and you'll automatically find other miners.

## Chain Parameters

| Parameter | Value |
|---|---|
| Max Supply | **42,000,000 EQF** |
| Block Reward | 50 EQF (halves every ~6 years) |
| Block Time | 90 seconds |
| PoW Algorithm | EquiHash-X (4 MB memory-hard) |
| Difficulty | LWMA (adjusts every block) |
| Signatures | Ed25519 |
| Coinbase Maturity | 100 blocks |
| Max Block Size | 4 MB |
| Community Fund | 5% of block reward |
| Min Transaction Fee | 0.00001 EQF |
| Halving Interval | 2,103,840 blocks (~6 years) |

## EquiHash-X

EquiForge uses a custom proof-of-work algorithm designed for ASIC resistance:

1. **FILL** — Generate a 4 MB scratchpad from the block header using Blake3
2. **MIX** — 64 rounds of memory-hard mixing with data-dependent random reads/writes
3. **SQUEEZE** — Final compression via double SHA-256

Each hash requires 4 MB of fast memory and random access patterns that defeat GPU texture caching and ASIC pipelining. A single CPU core achieves ~50-200 H/s, and GPUs offer minimal speedup due to memory latency.

Building an ASIC with enough on-die SRAM for meaningful parallelism (4 GB for 1000 cores) is economically impractical, keeping mining accessible to anyone with a modern computer.

## Architecture

```
~4,000 lines of Rust

src/
├── core/
│   ├── chain.rs      Blockchain state, UTXO set, validation, reorgs
│   ├── types.rs      Block, Transaction, OutPoint types
│   └── params.rs     Consensus parameters, seed nodes
├── pow/mod.rs        EquiHash-X proof-of-work algorithm
├── miner/mod.rs      Block template creation, parallel mining
├── network/mod.rs    P2P protocol, mempool, peer banning, gossip
├── wallet/mod.rs     Ed25519 keys, signing, encryption, coin selection
├── rpc/mod.rs        JSON-RPC server, block explorer web UI
├── storage/mod.rs    Sled embedded database
└── main.rs           CLI application
```

## RPC API

The node exposes a JSON-RPC server on port 9334.

```bash
# Chain info
curl -X POST http://127.0.0.1:9334 \
  -d '{"method":"getinfo","params":[],"id":1}'

# Get block by height
curl -X POST http://127.0.0.1:9334 \
  -d '{"method":"getblock","params":["5"],"id":1}'

# Check balance
curl -X POST http://127.0.0.1:9334 \
  -d '{"method":"getbalance","params":["ADDRESS"],"id":1}'
```

**Available methods:** `getinfo` `getblockcount` `getbestblockhash` `getblock` `getbalance` `listunspent` `sendrawtransaction` `getmempool` `getpeerinfo` `getmininginfo`

## Security

- **Wallet Encryption** — AES-256 with HMAC integrity, 100K-iteration key derivation
- **Signature Verification** — All transactions verified against UTXO pubkey hashes
- **Peer Banning** — Strike-based system auto-bans nodes sending invalid data
- **Chain Reorgs** — Cumulative work tracking with automatic reorganization to the best chain

## Run a Seed Node

Help decentralize the network by running a seed node on a VPS:

```bash
# On any Linux VPS with port 9333 open
equiforge node --port 9333
```

Contact us to get your node added to the hardcoded seed list.

## Roadmap

- [x] Custom ASIC-resistant PoW (EquiHash-X)
- [x] P2P network with peer discovery
- [x] Block explorer
- [x] Wallet encryption
- [x] Chain reorg support
- [x] Fee market
- [x] Peer banning
- [ ] Mining pool protocol
- [ ] Wrapped EQF on Solana/Base (DEX trading)
- [ ] Decentralized compute marketplace
- [ ] Smart transaction scripting

## License

MIT — do whatever you want with it.

---

<div align="center">

**[Start Mining](#start-mining)** · **[Block Explorer](http://129.80.239.237:9334)** · **[Discord](https://discord.gg/ZZ8e9NTjdR)** · **[𝕏 Twitter](https://x.com/eqf_crypto)**

</div>
