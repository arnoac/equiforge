/// EquiForge Chain Parameters
/// All consensus-critical constants are defined here.

/// Maximum total supply of EquiForge coins (in base units / satoshi-equivalent)
/// 42,000,000 coins * 100,000,000 units per coin
pub const MAX_SUPPLY: u64 = 42_000_000 * COIN;

/// Base unit denomination (like satoshis for Bitcoin)
pub const COIN: u64 = 100_000_000;

/// Initial block reward: 50 coins
pub const INITIAL_BLOCK_REWARD: u64 = 50 * COIN;

/// Halving interval: every 6 years
/// At ~90 second block time: 6 * 365.25 * 24 * 60 * 60 / 90 ≈ 2,103,840 blocks
pub const HALVING_INTERVAL: u64 = 2_103_840;

/// Target block time in seconds (90 seconds = 1.5 minutes)
pub const TARGET_BLOCK_TIME: u64 = 90;

/// Maximum block size in bytes (4 MB)
pub const MAX_BLOCK_SIZE: usize = 4 * 1024 * 1024;

/// Maximum transactions per block
pub const MAX_TXS_PER_BLOCK: usize = 10_000;

/// Genesis block timestamp (2025-01-01 00:00:00 UTC)
pub const GENESIS_TIMESTAMP: u64 = 1735689600;

/// Initial difficulty: number of leading zero bits required in block hash.
///
/// With EquiHash-X (memory-hard, ~100-200 H/s per core on modern CPU):
///   4  bits = ~16 hashes           → <1s
///   6  bits = ~64 hashes           → <1s
///   8  bits = ~256 hashes          → ~1-2s
///   10 bits = ~1024 hashes         → ~5-10s
///   12 bits = ~4096 hashes         → ~20-40s
///   14 bits = ~16K hashes          → ~1.5-3 min
///
/// Starting at 8 gives ~1-2 seconds per block on a single core.
/// Multi-core (12 threads) will find blocks faster, and the LWMA
/// will converge to 90s target by adjusting upward.
pub const INITIAL_DIFFICULTY: u32 = 8;

/// Protocol version — increment when chain format changes
pub const PROTOCOL_VERSION: u32 = 2;

/// PoW algorithm identifier (stored in chain metadata for compatibility checks)
pub const POW_ALGORITHM: &str = "equihash-x-v1";

/// Network magic bytes for testnet
pub const TESTNET_MAGIC: [u8; 4] = [0xEF, 0x01, 0xF0, 0x42];

/// Community fund percentage of block reward (5%)
pub const COMMUNITY_FUND_PERCENT: u64 = 5;

/// Minimum transaction fee in base units
pub const MIN_TX_FEE: u64 = 1000; // 0.00001 EQF

/// Coinbase maturity (blocks before mined coins can be spent)
pub const COINBASE_MATURITY: u64 = 100;

/// Default seed nodes for initial peer discovery.
/// These are the first nodes a new client connects to.
/// Once connected, peers are discovered via gossip (GetPeers/Peers).
/// To run your own seed node: `equiforge node --port 9333` on a VPS with port 9333 open.
pub const SEED_NODES: &[&str] = &[
    "129.80.239.237:9333",
];

/// How often to request peers from connected nodes (seconds)
pub const PEER_EXCHANGE_INTERVAL: u64 = 120;

/// Maximum number of outbound peer connections to maintain
pub const MAX_OUTBOUND_PEERS: usize = 8;

/// Maximum number of total peer connections
pub const MAX_PEERS: usize = 32;

/// Calculate block reward at a given height
pub fn block_reward(height: u64) -> u64 {
    let halvings = height / HALVING_INTERVAL;
    if halvings >= 64 {
        return 0;
    }
    INITIAL_BLOCK_REWARD >> halvings
}

/// Calculate the community fund amount for a given block reward
pub fn community_fund_amount(reward: u64) -> u64 {
    reward * COMMUNITY_FUND_PERCENT / 100
}

/// Calculate miner reward (block reward minus community fund)
pub fn miner_reward(height: u64) -> u64 {
    let reward = block_reward(height);
    reward - community_fund_amount(reward)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_reward() {
        assert_eq!(block_reward(0), 50 * COIN);
    }

    #[test]
    fn test_first_halving() {
        assert_eq!(block_reward(HALVING_INTERVAL), 25 * COIN);
    }

    #[test]
    fn test_second_halving() {
        assert_eq!(block_reward(HALVING_INTERVAL * 2), 12 * COIN + COIN / 2);
    }

    #[test]
    fn test_eventual_zero_reward() {
        assert_eq!(block_reward(HALVING_INTERVAL * 64), 0);
    }

    #[test]
    fn test_total_supply_approximation() {
        // Verify total mined supply approaches 42M
        let mut total: u64 = 0;
        let mut height: u64 = 0;
        loop {
            let reward = block_reward(height);
            if reward == 0 {
                break;
            }
            // Add reward for all blocks in this halving epoch
            let epoch_end = ((height / HALVING_INTERVAL) + 1) * HALVING_INTERVAL;
            let blocks_remaining = epoch_end - height;
            total = total.saturating_add(reward.saturating_mul(blocks_remaining));
            height = epoch_end;
        }
        let total_coins = total / COIN;
        // Should be close to 42M (within rounding)
        assert!(total_coins <= 42_000_000);
        assert!(total_coins > 41_900_000);
        println!("Total supply: {} EQF", total_coins);
    }

    #[test]
    fn test_community_fund() {
        let reward = block_reward(0);
        let fund = community_fund_amount(reward);
        let miner = miner_reward(0);
        assert_eq!(fund, 2 * COIN + COIN / 2); // 2.5 EQF
        assert_eq!(miner, 47 * COIN + COIN / 2); // 47.5 EQF
        assert_eq!(fund + miner, reward);
    }
}
