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

/// Initial difficulty: number of leading zero bits required in block hash.
pub const INITIAL_DIFFICULTY: u32 = 8;

/// Protocol version — increment when network protocol changes
pub const PROTOCOL_VERSION: u32 = 4;

/// Minimum protocol version we'll accept connections from
/// v4 required: fixed difficulty, fixed compact blocks, fixed sync
pub const MIN_PROTOCOL_VERSION: u32 = 4;

/// PoW algorithm identifier (stored in chain metadata for compatibility checks)
pub const POW_ALGORITHM: &str = "equihash-x-v1";

/// Community fund percentage of block reward (5%)
pub const COMMUNITY_FUND_PERCENT: u64 = 5;

/// Minimum transaction fee in base units
pub const MIN_TX_FEE: u64 = 1000; // 0.00001 EQF

/// Coinbase maturity (blocks before mined coins can be spent)
pub const COINBASE_MATURITY: u64 = 100;

/// How often to request peers from connected nodes (seconds)
pub const PEER_EXCHANGE_INTERVAL: u64 = 120;

/// Maximum number of outbound peer connections to maintain
pub const MAX_OUTBOUND_PEERS: usize = 12;

/// Maximum number of total peer connections (inbound + outbound)
pub const MAX_PEERS: usize = 256;

// ─── Network Configuration (Mainnet vs Testnet) ─────────────────────

use std::sync::OnceLock;

/// Runtime network configuration — set once at startup based on --testnet flag
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub name: &'static str,
    pub magic: [u8; 4],
    pub default_port: u16,
    pub default_rpc_port: u16,
    pub genesis_timestamp: u64,
    pub data_dir: &'static str,
    pub seed_nodes: Vec<String>,
}

static NETWORK: OnceLock<NetworkConfig> = OnceLock::new();

pub fn init_network(testnet: bool) {
    let config = if testnet {
        NetworkConfig {
            name: "testnet",
            magic: [0xEF, 0x01, 0xF0, 0x99],
            default_port: 19333,
            default_rpc_port: 19332,
            genesis_timestamp: 1735689600 + 1, // Different genesis than mainnet
            data_dir: "equiforge_testnet",
            seed_nodes: vec!["129.80.239.237:19333".to_string()],
        }
    } else {
        NetworkConfig {
            name: "mainnet",
            magic: [0xEF, 0x01, 0xF0, 0x42],
            default_port: 9333,
            default_rpc_port: 9332,
            genesis_timestamp: 1735689600,
            data_dir: "equiforge_data",
            seed_nodes: vec!["129.80.239.237:9333".to_string()],
        }
    };
    NETWORK.set(config).expect("Network already initialized");
}

pub fn network() -> &'static NetworkConfig {
    NETWORK.get().expect("Network not initialized — call init_network() first")
}

/// Convenience accessors used throughout the codebase
pub fn magic_bytes() -> [u8; 4] { network().magic }
pub fn genesis_timestamp() -> u64 { network().genesis_timestamp }
pub fn default_port() -> u16 { network().default_port }
pub fn seed_nodes() -> &'static [String] { &network().seed_nodes }
pub fn data_dir() -> &'static str { network().data_dir }
pub fn is_testnet() -> bool { network().name == "testnet" }

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
