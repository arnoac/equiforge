use serde::{Deserialize, Serialize};
use std::fmt;

/// A 32-byte hash used throughout the system
pub type Hash256 = [u8; 32];

/// Null hash (all zeros) used for genesis block's prev_hash
pub const NULL_HASH: Hash256 = [0u8; 32];

// ─── Transaction Types ───────────────────────────────────────────────

/// Represents a reference to a previous transaction output
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct OutPoint {
    pub txid: Hash256,
    pub vout: u32,
}

/// Transaction input - spends a previous output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxInput {
    pub previous_output: OutPoint,
    pub signature: Vec<u8>,
    pub pubkey: Vec<u8>,
    pub sequence: u32,
}

/// Transaction output - creates a new spendable output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxOutput {
    pub amount: u64,
    pub pubkey_hash: Hash256,
}

/// A complete transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub version: u32,
    pub inputs: Vec<TxInput>,
    pub outputs: Vec<TxOutput>,
    pub lock_time: u64,
}

impl Transaction {
    /// Create a coinbase transaction (mining reward)
    pub fn new_coinbase(
        height: u64,
        reward: u64,
        miner_pubkey_hash: Hash256,
        community_fund_hash: Hash256,
    ) -> Self {
        let community_amount = super::params::community_fund_amount(reward);
        let miner_amount = reward - community_amount;

        let mut outputs = vec![TxOutput {
            amount: miner_amount,
            pubkey_hash: miner_pubkey_hash,
        }];

        if community_amount > 0 {
            outputs.push(TxOutput {
                amount: community_amount,
                pubkey_hash: community_fund_hash,
            });
        }

        Transaction {
            version: 1,
            inputs: vec![TxInput {
                previous_output: OutPoint {
                    txid: NULL_HASH,
                    vout: 0xFFFFFFFF,
                },
                signature: height.to_le_bytes().to_vec(),
                pubkey: vec![],
                sequence: 0xFFFFFFFF,
            }],
            outputs,
            lock_time: 0,
        }
    }

    pub fn is_coinbase(&self) -> bool {
        self.inputs.len() == 1
            && self.inputs[0].previous_output.txid == NULL_HASH
            && self.inputs[0].previous_output.vout == 0xFFFFFFFF
    }

    pub fn total_output(&self) -> u64 {
        self.outputs.iter().map(|o| o.amount).sum()
    }

    /// Compute the transaction hash (double SHA-256)
    pub fn hash(&self) -> Hash256 {
        use sha2::{Digest, Sha256};
        let serialized = bincode::serialize(self).expect("tx serialization failed");
        let first = Sha256::digest(&serialized);
        let second = Sha256::digest(&first);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&second);
        hash
    }

    pub fn size(&self) -> usize {
        bincode::serialized_size(self).unwrap_or(0) as usize
    }
}

// ─── Block Types ─────────────────────────────────────────────────────

/// Block header
///
/// `difficulty_target` is the number of leading zero BITS required in the block hash.
///   - 8  = hash must start with 0x00 (1 zero byte)          ~256 hashes
///   - 16 = hash must start with 0x0000 (2 zero bytes)       ~65K hashes
///   - 20 = 5 leading hex zeros                               ~1M hashes
///   - 24 = hash must start with 0x000000 (3 zero bytes)     ~16M hashes
///   - 32 = 4 zero bytes                                      ~4B hashes
///   - 40 = 5 zero bytes                                      ~1T hashes
///
/// For reference, Bitcoin's current difficulty requires ~75+ leading zero bits.
/// A single modern CPU doing SHA-256 can do roughly 5-20 MH/s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockHeader {
    pub version: u32,
    pub prev_hash: Hash256,
    pub merkle_root: Hash256,
    pub timestamp: u64,
    /// Number of leading zero bits required in the block hash
    pub difficulty_target: u32,
    pub nonce: u64,
    pub height: u64,
}

impl BlockHeader {
    /// Compute the block hash using EquiHash-X (memory-hard, ASIC-resistant).
    ///
    /// This replaces double-SHA256 with a custom algorithm that requires
    /// 4 MB of memory and 64 mixing iterations per hash, making dedicated
    /// hardware impractical while keeping CPUs and GPUs competitive.
    pub fn hash(&self) -> Hash256 {
        let serialized = bincode::serialize(self).expect("header serialization failed");
        crate::pow::equihash_x(&serialized)
    }

    /// Fast hash for non-PoW purposes (block ID in storage, merkle trees, etc.)
    /// Uses double SHA-256 since it doesn't need to be memory-hard.
    pub fn id_hash(&self) -> Hash256 {
        use sha2::{Digest, Sha256};
        let serialized = bincode::serialize(self).expect("header serialization failed");
        let first = Sha256::digest(&serialized);
        let second = Sha256::digest(&first);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&second);
        hash
    }

    /// Check if the block hash meets the difficulty target
    pub fn meets_difficulty(&self) -> bool {
        let hash = self.hash();
        leading_zero_bits(&hash) >= self.difficulty_target
    }
}

/// Count leading zero bits in a hash
pub fn leading_zero_bits(hash: &Hash256) -> u32 {
    let mut count = 0u32;
    for byte in hash {
        if *byte == 0 {
            count += 8;
        } else {
            count += byte.leading_zeros();
            break;
        }
    }
    count
}

/// Estimate the average number of hashes needed for a given difficulty (leading zero bits).
///
/// With EquiHash-X (~100-200 H/s per core), time estimates:
///   8  bits = ~256 hashes        → ~1-2s
///   10 bits = ~1024 hashes       → ~5-10s
///   12 bits = ~4096 hashes       → ~20-40s
///   14 bits = ~16384 hashes      → ~80-160s (~1.5-3 min)
///   16 bits = ~65536 hashes      → ~5-10 min
pub fn estimated_hashes_for_difficulty(difficulty_bits: u32) -> f64 {
    2.0_f64.powi(difficulty_bits as i32)
}

/// A complete block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub header: BlockHeader,
    pub transactions: Vec<Transaction>,
}

impl Block {
    /// Compute the merkle root from the block's transactions
    pub fn compute_merkle_root(&self) -> Hash256 {
        if self.transactions.is_empty() {
            return NULL_HASH;
        }

        let mut hashes: Vec<Hash256> = self.transactions.iter().map(|tx| tx.hash()).collect();

        while hashes.len() > 1 {
            if hashes.len() % 2 != 0 {
                let last = *hashes.last().unwrap();
                hashes.push(last);
            }

            let mut next_level = Vec::new();
            for chunk in hashes.chunks(2) {
                use sha2::{Digest, Sha256};
                let mut combined = Vec::new();
                combined.extend_from_slice(&chunk[0]);
                combined.extend_from_slice(&chunk[1]);
                let first = Sha256::digest(&combined);
                let second = Sha256::digest(&first);
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&second);
                next_level.push(hash);
            }
            hashes = next_level;
        }

        hashes[0]
    }

    pub fn validate_merkle_root(&self) -> bool {
        self.header.merkle_root == self.compute_merkle_root()
    }

    pub fn size(&self) -> usize {
        bincode::serialized_size(self).unwrap_or(0) as usize
    }
}

impl fmt::Display for BlockHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Block #{} [{}] diff={} ts={}",
            self.height,
            hex::encode(self.hash()),
            self.difficulty_target,
            self.timestamp,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coinbase_transaction() {
        let miner_hash = [1u8; 32];
        let fund_hash = [2u8; 32];
        let tx = Transaction::new_coinbase(0, 50 * super::super::params::COIN, miner_hash, fund_hash);
        assert!(tx.is_coinbase());
        assert_eq!(tx.outputs.len(), 2);
        assert_eq!(tx.total_output(), 50 * super::super::params::COIN);
    }

    #[test]
    fn test_tx_hash_deterministic() {
        let miner_hash = [1u8; 32];
        let fund_hash = [2u8; 32];
        let tx = Transaction::new_coinbase(0, 5_000_000_000, miner_hash, fund_hash);
        assert_eq!(tx.hash(), tx.hash());
        assert_ne!(tx.hash(), NULL_HASH);
    }

    #[test]
    fn test_leading_zero_bits() {
        assert_eq!(leading_zero_bits(&[0x00, 0x00, 0xFF, 0; 29]), 16);
        assert_eq!(leading_zero_bits(&[0x00, 0x0F, 0; 30]), 12);
        assert_eq!(leading_zero_bits(&[0x00, 0x00, 0x00, 0x01, 0; 28]), 31);
        assert_eq!(leading_zero_bits(&[0xFF; 32]), 0);
        assert_eq!(leading_zero_bits(&[0; 32]), 256);
    }

    #[test]
    fn test_estimated_hashes() {
        // 8 bits = ~256 hashes on average
        assert!((estimated_hashes_for_difficulty(8) - 256.0).abs() < 1.0);
        // 16 bits = ~65536
        assert!((estimated_hashes_for_difficulty(16) - 65536.0).abs() < 1.0);
        // 24 bits = ~16.7M
        assert!((estimated_hashes_for_difficulty(24) - 16777216.0).abs() < 1.0);
    }

    #[test]
    fn test_merkle_root_single_tx() {
        let tx = Transaction::new_coinbase(0, 5_000_000_000, [1u8; 32], [2u8; 32]);
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_hash: NULL_HASH,
                merkle_root: NULL_HASH,
                timestamp: 0,
                difficulty_target: 8,
                nonce: 0,
                height: 0,
            },
            transactions: vec![tx.clone()],
        };
        assert_eq!(block.compute_merkle_root(), tx.hash());
    }
}
