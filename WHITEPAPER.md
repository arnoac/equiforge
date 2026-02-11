# EquiForge: A Fair, Accessible Blockchain for Decentralized Compute

**Whitepaper v1.0 — February 2026**

---

## Abstract

EquiForge is a layer-1 blockchain designed around a single principle: computational fairness. By combining a novel memory-hard proof-of-work algorithm (EquiHash-X) with a long-term vision for decentralized compute, EquiForge creates a network where mining hardware doubles as productive infrastructure. The chain launches with no premine, no ICO, and no insider allocation — every coin is earned through work. The native token (EQF) serves first as a medium of exchange secured by CPU-friendly mining, and evolves into the payment layer for a decentralized AI training and compute marketplace.

This paper describes the technical architecture of the EquiForge blockchain, the design rationale behind EquiHash-X, the economic model governing token supply, and the roadmap toward a compute marketplace that gives EQF intrinsic utility beyond speculation.

---

## 1. Introduction

### 1.1 The Problem

The original promise of cryptocurrency was democratized finance — anyone with a computer could participate in securing and benefiting from a decentralized network. That promise has been broken.

Bitcoin mining is dominated by ASIC manufacturers and industrial-scale operations. Ethereum moved to proof-of-stake, requiring 32 ETH ($100,000+) to validate. Most new tokens launch with venture capital backing, insider allocations, and pre-mined supplies that concentrate wealth before the public ever participates.

Meanwhile, the world faces a growing compute bottleneck. Training AI models requires enormous computational resources controlled by a handful of cloud providers. Researchers, startups, and individuals are priced out of the AI revolution by the cost of GPU time on AWS, Google Cloud, and Azure.

### 1.2 The EquiForge Thesis

EquiForge addresses both problems simultaneously:

1. **Fair mining** — A memory-hard PoW algorithm that resists ASIC optimization, keeping mining accessible to anyone with a modern CPU.
2. **Useful mining** — A roadmap to transform the mining network into a decentralized compute marketplace, where miners earn EQF not only from block rewards but from executing real workloads: AI model training, inference, data processing, and scientific computation.

The key insight is that miners already operate hardware 24/7. If that hardware can serve dual purpose — securing the chain and performing useful computation — the network creates real economic value rather than burning electricity on meaningless hashes.

---

## 2. EquiHash-X: Memory-Hard Proof of Work

### 2.1 Design Goals

EquiHash-X was designed with three objectives:

1. **ASIC resistance** — The algorithm must be economically impractical to implement in custom silicon, ensuring CPUs remain competitive.
2. **GPU parity** — GPUs should offer minimal advantage over CPUs, preventing the GPU arms race that plagued earlier memory-hard algorithms.
3. **Verifiability** — Hash verification must be fast (milliseconds) even though hash computation is slow (5-20ms), enabling lightweight validation by all nodes.

### 2.2 Algorithm

EquiHash-X operates in three phases:

**Phase 1: FILL (Scratchpad Generation)**

A 4 MB scratchpad is generated from the block header. The header is hashed with Blake3 to produce a 32-byte seed. The scratchpad is then filled in 64-byte chunks (65,536 total), where each chunk is derived from the seed and the chunk index:

```
chunk[i] = Blake3(seed || i) || Blake3(Blake3(seed || i) || i)
```

This phase is sequential and memory-bandwidth bound, establishing the minimum memory requirement.

**Phase 2: MIX (Memory-Hard Mixing)**

A 64-byte running state (8 × u64 limbs) undergoes 64 rounds of memory-hard mixing:

1. Compute a read index from the current state: `idx = (state[0] + state[round % 8]) % N_CHUNKS`
2. Read 64 bytes from the scratchpad at the computed index
3. XOR the read data into the state with rotate-add sequential dependencies
4. Every 8th round: apply SHA-256 compression for instruction diversity
5. Every 16th round: apply Blake3 compression for additional instruction mix
6. Write the mixed state back to a different location: `(state[1] * state[3]) % N_CHUNKS`

The data-dependent addressing pattern (step 1) is the critical ASIC-resistance mechanism. Because the next memory address depends on the current state, the pipeline cannot prefetch memory and must stall on every read. This is fundamentally different from algorithms like Ethash where access patterns are predictable.

The read-write access pattern (step 6) prevents GPU texture caching optimizations that exploit read-only memory access.

**Phase 3: SQUEEZE (Final Compression)**

The final 64-byte state is compressed via double SHA-256 to produce a 32-byte hash, which is compared against the difficulty target using leading-zero-bit counting.

### 2.3 ASIC Resistance Analysis

The memory requirement of 4 MB per hash unit is the primary defense. For an ASIC to achieve meaningful parallelism:

| ASIC Cores | Required On-Die SRAM | Estimated Cost |
|---|---|---|
| 10 | 40 MB | Feasible but marginal advantage |
| 100 | 400 MB | ~$50-100 per chip |
| 1,000 | 4 GB | Economically impractical |

For comparison, a high-end consumer CPU (AMD Ryzen 9) achieves 100-200 H/s per core using its existing cache hierarchy. An ASIC with 100 cores and 400 MB SRAM might achieve 5,000-10,000 H/s at a cost of $50-100 per chip — a cost/performance ratio comparable to consumer CPUs, eliminating the economic incentive for ASIC development.

The mixed instruction set (SHA-256 + Blake3 + bitwise operations) further complicates ASIC design by requiring multiple functional units rather than a single optimized pipeline.

### 2.4 Performance Characteristics

| Hardware | Hashrate | Notes |
|---|---|---|
| Modern CPU (per core) | 50-200 H/s | Memory-bandwidth limited |
| 12-core CPU (total) | 600-2,400 H/s | Near-linear scaling |
| High-end GPU | 100-500 H/s | Memory latency limited, minimal advantage |
| Theoretical 100-core ASIC | 5,000-10,000 H/s | Comparable cost/perf to CPUs |

---

## 3. Blockchain Architecture

### 3.1 Consensus Model

EquiForge uses a UTXO-based transaction model with Nakamoto consensus (longest chain wins, measured by cumulative proof-of-work). Key design choices:

- **UTXO model** over account model — simpler to verify, naturally supports parallel validation, and provides better privacy through address rotation.
- **Ed25519 signatures** — smaller and faster than ECDSA (Bitcoin's scheme), with equivalent security.
- **Base58Check addresses** — human-readable, checksummed, compatible with existing wallet infrastructure patterns.

### 3.2 Difficulty Adjustment

EquiForge uses a Linear Weighted Moving Average (LWMA) algorithm that adjusts difficulty on every block, targeting 90-second block intervals. Unlike Bitcoin's 2-week adjustment window, LWMA responds to hashrate changes within minutes.

The algorithm:
1. Look back up to 60 blocks
2. Weight recent solve times more heavily than older ones
3. Compute a fractional difficulty adjustment (max ±0.5 bits per block)
4. Apply a warmup factor for networks with fewer than 60 blocks of history

This prevents the "difficulty bomb" problem where a sudden hashrate drop makes blocks impossibly slow, and the "oscillation" problem where difficulty overshoots in response to mining pool hopping.

### 3.3 Chain Reorganization

The chain tracks cumulative work (sum of 2^difficulty for all blocks) across all known branches. When a side chain accumulates more total work than the current main chain, the node automatically reorganizes:

1. Identify the fork point (common ancestor)
2. Rebuild the UTXO set from genesis along the new best chain
3. Update difficulty and timestamp tracking
4. Switch the active tip

This ensures the network converges on a single canonical chain even during temporary forks caused by network latency or competing miners.

### 3.4 Network Protocol

Peer-to-peer communication uses a custom binary protocol over TCP:

- **Handshake** — Version exchange with chain height, enabling immediate sync detection
- **Block propagation** — New blocks are broadcast to all connected peers
- **Transaction relay** — Validated transactions are forwarded through the mempool
- **Peer discovery** — Gossip-based discovery via `GetPeers`/`Peers` messages
- **Sync** — Batch block requests with automatic continuation for catching up

Seed nodes provide initial peer discovery. Once connected, nodes discover additional peers through gossip, making the network resilient to seed node failure.

### 3.5 Peer Reputation

A strike-based system tracks misbehavior per IP address:

| Offense | Strikes | Description |
|---|---|---|
| Invalid block (bad PoW, bad signature) | 2 | Truly invalid data |
| Invalid transaction | 1 | Failed signature or UTXO validation |
| Malformed message | 3 | Protocol violation |

After 20 strikes, the IP is banned for 30 minutes. Harmless rejections (duplicate blocks, stale blocks during sync) do not incur strikes. Bans expire automatically, and strike counters are reset when bans clear.

---

## 4. Token Economics

### 4.1 Supply Schedule

| Parameter | Value |
|---|---|
| Maximum supply | 42,000,000 EQF |
| Initial block reward | 50 EQF |
| Halving interval | 2,103,840 blocks (~6 years) |
| Block time target | 90 seconds |
| Smallest unit | 0.00000001 EQF (1 base unit) |

The supply curve follows a geometric decay:

| Period | Block Reward | Cumulative Supply | % of Max |
|---|---|---|---|
| Years 0-6 | 50 EQF | ~21,000,000 | 50% |
| Years 6-12 | 25 EQF | ~31,500,000 | 75% |
| Years 12-18 | 12.5 EQF | ~36,750,000 | 87.5% |
| Years 18-24 | 6.25 EQF | ~39,375,000 | 93.75% |
| Years 24-30 | 3.125 EQF | ~40,687,500 | 96.875% |

Half of all EQF that will ever exist is mined in the first six years, creating strong incentives for early participation.

### 4.2 Community Fund

5% of each block reward is allocated to a community fund address. This fund is intended for:

- Development grants
- Security audits
- Exchange listing fees
- Ecosystem development
- Bug bounties

Governance of the community fund will transition to on-chain voting as the network matures.

### 4.3 Fee Market

Transactions require a minimum fee of 0.00001 EQF. The mempool sorts transactions by fee-per-byte (fee rate), and miners select the highest-paying transactions first. This creates a natural fee market that scales with demand:

- Low demand: minimum fees (~free)
- High demand: competitive fees determined by market dynamics
- No artificial fee burning or complex EIP-1559-style mechanisms

Transaction fees become increasingly important as block rewards diminish through halvings, providing long-term security budget for the network.

### 4.4 Coinbase Maturity

Newly mined coins cannot be spent for 100 blocks (~2.5 hours). This prevents miners from spending coins on a chain that may be reorganized, protecting merchants and exchanges from double-spend attacks during temporary forks.

---

## 5. Decentralized Compute Marketplace (Roadmap)

### 5.1 Vision

The long-term vision for EquiForge is to transform the mining network into a decentralized compute marketplace. Miners already operate hardware 24/7 — the compute marketplace allows them to earn additional EQF by performing useful work submitted by users.

This gives EQF intrinsic utility: anyone who wants to run AI training, inference, rendering, or data processing must purchase EQF to pay miners. Demand for the token becomes tied to demand for computation, creating sustainable value independent of speculation.

### 5.2 Architecture

The compute marketplace will be implemented as a new transaction type (v2 transactions) that existing nodes can process without a hard fork:

**Job Submission:**
```
User creates a ComputeJob transaction:
  - WASM binary (the program to execute)
  - Input data hash (data available via IPFS/Arweave)
  - Payment: X EQF locked in escrow
  - Requirements: memory, CPU time, deadline
```

**Job Execution:**
```
Miners claim jobs:
  - Download WASM binary and input data
  - Execute in a sandboxed WASM runtime
  - Post result hash on-chain
  - Receive payment from escrow
```

**Verification:**
```
Results are verified via redundant execution:
  - 3 miners execute the same job
  - Majority result is accepted
  - Dissenting miners are penalized (slashed)
  - Agreement releases payment to all correct executors
```

### 5.3 AI Training Marketplace

The highest-value application of the compute marketplace is decentralized AI training:

1. **Fine-tuning as a service** — Users submit base model weights + training data + EQF payment. Miners with GPUs/CPUs run the fine-tuning and return the trained model.
2. **Inference marketplace** — Users submit prompts + model references + micropayments. Miners run inference and return results in real-time.
3. **Dataset processing** — Large-scale data cleaning, transformation, and labeling distributed across the mining network.

This positions EquiForge as a decentralized alternative to cloud AI services, where:
- No AWS account, credit card, or KYC required
- Pay-per-computation with EQF
- Censorship-resistant (no provider can refuse your workload)
- Competitive pricing through market dynamics

### 5.4 Proof of Useful Work (Future)

The ultimate evolution is replacing the EquiHash-X proof-of-work with proof of useful work, where the mining computation itself is a useful task. Rather than solving arbitrary puzzles, miners would perform verifiable AI training steps, scientific simulations, or cryptographic computations that have real-world value.

This requires solving the verification problem: how to confirm a miner did real work without re-doing the entire computation. Approaches under research include:

- **Optimistic verification** — Assume results are correct; slash miners if a fraud proof is submitted
- **Zero-knowledge proofs** — Generate ZK proofs of correct computation (expensive but trustless)
- **Spot-checking** — Verifiers re-run random portions of the computation to detect cheating
- **Trusted execution environments** — Use hardware enclaves (Intel SGX, ARM TrustZone) for attestation

### 5.5 Implementation Timeline

| Phase | Timeline | Deliverable |
|---|---|---|
| Phase 1: Foundation | Complete | Core blockchain, PoW, P2P, wallet, explorer |
| Phase 2: Liquidity | Q2 2026 | Wrapped EQF on Solana/Base, DEX trading |
| Phase 3: Compute MVP | Q3 2026 | WASM job submission and execution |
| Phase 4: AI Marketplace | Q4 2026 | Fine-tuning and inference marketplace |
| Phase 5: Useful PoW | 2027+ | Transition to proof of useful work |

---

## 6. Security Considerations

### 6.1 51% Attack Resistance

Like all PoW chains, EquiForge is vulnerable to 51% attacks where a miner controlling the majority of hashrate can double-spend. The memory-hard PoW makes this more expensive than SHA-256-based chains because attackers cannot rent ASIC hashrate — they must provision sufficient memory bandwidth, which limits the effective parallelism of any single attacker.

As the network grows and hashrate increases, the cost of a 51% attack rises proportionally.

### 6.2 Wallet Security

Private keys are stored locally with optional AES-256 encryption using a password-derived key (100,000-iteration SHA-256 KDF). The encryption provides authenticated integrity checking via HMAC, preventing both key theft and silent corruption.

Future versions will support hardware wallet integration and multi-signature transactions.

### 6.3 Network Security

- **Peer banning** prevents sustained spam attacks from malicious nodes
- **Transaction validation** occurs at both the mempool and block levels, with full Ed25519 signature verification
- **Chain reorganization** follows cumulative work, preventing low-difficulty side chain attacks
- **Coinbase maturity** prevents miners from spending rewards on chains that may be orphaned

---

## 7. Comparison

| Feature | EquiForge | Bitcoin | Ethereum | Monero |
|---|---|---|---|---|
| Consensus | PoW (EquiHash-X) | PoW (SHA-256d) | PoS | PoW (RandomX) |
| ASIC Resistant | Yes (4 MB memory) | No | N/A | Yes (2 MB memory) |
| Block Time | 90 seconds | 10 minutes | 12 seconds | 2 minutes |
| Max Supply | 42M | 21M | Unlimited | ~18.4M + tail emission |
| Difficulty Adj. | Every block (LWMA) | Every 2016 blocks | Every block | Every block |
| Premine | None | None | Yes (ICO) | None |
| Compute Utility | Planned | None | Smart contracts | None |
| Wallet Encryption | Built-in | External | External | Built-in |

EquiForge's closest technical comparison is Monero (both use memory-hard PoW with per-block difficulty adjustment), but with a distinct economic model (hard cap vs tail emission) and a unique roadmap toward compute utility.

---

## 8. Conclusion

EquiForge is a blockchain built on the conviction that decentralized networks should serve people, not the other way around. By keeping mining accessible through ASIC-resistant proof-of-work and building toward a compute marketplace that gives the token real utility, EquiForge aims to be both a fair currency and a productive network.

The code is open source. The launch is fair. Every coin is mined. The network is live.

Start mining at [github.com/arnoac/equiforge](https://github.com/arnoac/equiforge).

---

*This document describes the current state and intended direction of the EquiForge project. The compute marketplace described in Section 5 is planned functionality, not yet implemented. All technical specifications are subject to change as the project evolves.*
