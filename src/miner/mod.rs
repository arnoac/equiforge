use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::chain::Chain;
use crate::core::params::*;
use crate::core::types::*;

/// Mining configuration
pub struct MinerConfig {
    /// Public key hash to receive mining rewards
    pub miner_pubkey_hash: Hash256,
    /// Community fund address
    pub community_fund_hash: Hash256,
    /// Number of mining threads
    pub threads: usize,
}

impl Default for MinerConfig {
    fn default() -> Self {
        Self {
            miner_pubkey_hash: [0u8; 32],
            community_fund_hash: [0xCF; 32],
            threads: 1,
        }
    }
}

/// Create a block template ready for mining
pub fn create_block_template(
    chain: &Chain,
    pending_txs: &[Transaction],
    config: &MinerConfig,
) -> Block {
    let height = chain.height + 1;
    let reward = block_reward(height);
    let prev_hash = chain.tip;
    let difficulty = chain.next_difficulty();

    // Use real wall clock time, but ensure strictly greater than prev block.
    // If we mine faster than 1 second, bump by 1. This is correct behavior —
    // the difficulty adjustment will see the fast timestamps and increase difficulty
    // until blocks naturally take ~90s each.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let prev_timestamp = chain.tip_header().timestamp;
    let timestamp = if now > prev_timestamp { now } else { prev_timestamp + 1 };

    // Calculate total fees from pending transactions
    // We need to estimate fees since we don't fully validate here
    // (the chain validates on add_block). For coinbase amount, we include
    // the declared fee based on tx input/output difference from chain's UTXO set.
    let mut total_fees: u64 = 0;
    let mut valid_txs: Vec<Transaction> = Vec::new();
    let mut block_size: usize = 0;

    for tx in pending_txs {
        if tx.is_coinbase() { continue; }
        let tx_size = tx.size();
        if block_size + tx_size > MAX_BLOCK_SIZE { break; }
        if valid_txs.len() + 1 >= MAX_TXS_PER_BLOCK { break; }

        // Try to calculate fee from UTXO set
        let mut input_sum: u64 = 0;
        let mut valid = true;
        for input in &tx.inputs {
            match chain.utxo_set.get(&input.previous_output) {
                Some(utxo) => input_sum += utxo.output.amount,
                None => { valid = false; break; }
            }
        }
        if !valid { continue; }

        let output_sum = tx.total_output();
        if output_sum > input_sum { continue; }

        let fee = input_sum - output_sum;
        total_fees += fee;
        valid_txs.push(tx.clone());
        block_size += tx_size;
    }

    // Create coinbase with reward + fees
    let coinbase = Transaction::new_coinbase(
        height,
        reward + total_fees,
        config.miner_pubkey_hash,
        config.community_fund_hash,
    );

    let mut txs = vec![coinbase];
    txs.extend(valid_txs);

    // Build block with placeholder nonce
    let mut block = Block {
        header: BlockHeader {
            version: PROTOCOL_VERSION,
            prev_hash,
            merkle_root: NULL_HASH,
            timestamp,
            difficulty_target: difficulty,
            nonce: 0,
            height,
        },
        transactions: txs,
    };

    block.header.merkle_root = block.compute_merkle_root();

    block
}

/// Result of a mining attempt
pub enum MineResult {
    Found(Block),
    Cancelled,
}

/// Mine a block (single-threaded)
pub fn mine_block(mut block: Block, stop: Arc<AtomicBool>) -> MineResult {
    let mut nonce: u64 = 0;
    let mut hashes: u64 = 0;
    let start = std::time::Instant::now();
    let difficulty = block.header.difficulty_target;

    tracing::info!(
        "⛏️  Mining block #{} (difficulty: {} bits, ~{:.0} expected hashes)...",
        block.header.height,
        difficulty,
        estimated_hashes_for_difficulty(difficulty),
    );

    loop {
        if stop.load(Ordering::Relaxed) {
            return MineResult::Cancelled;
        }

        block.header.nonce = nonce;

        if block.header.meets_difficulty() {
            let elapsed = start.elapsed().as_secs_f64();
            let hashrate = if elapsed > 0.0 {
                hashes as f64 / elapsed
            } else {
                0.0
            };
            tracing::info!(
                "⛏️  Block #{} mined! nonce={} hash={} time={:.2}s hashrate={:.1} H/s",
                block.header.height,
                nonce,
                hex::encode(block.header.hash()),
                elapsed,
                hashrate,
            );
            return MineResult::Found(block);
        }

        nonce = nonce.wrapping_add(1);
        hashes += 1;

        // Update timestamp periodically so it stays current
        // With EquiHash-X (~100-200 H/s), log every 100 hashes (~0.5-1s)
        if hashes % 100 == 0 && hashes > 0 {
            let elapsed = start.elapsed().as_secs_f64();
            let hashrate = hashes as f64 / elapsed;
            tracing::debug!(
                "  Mining block #{}: {} hashes, {:.1} H/s, {:.0}s elapsed",
                block.header.height,
                hashes,
                hashrate,
                elapsed,
            );

            // Refresh timestamp to stay within the 2-hour future window
            block.header.timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            // Recompute merkle root if timestamp is in the header hash
            // (it is, since we hash the whole header)
        }
    }
}

/// Multi-threaded mining (splits nonce space across threads)
pub fn mine_block_parallel(block: Block, threads: usize, stop: Arc<AtomicBool>) -> MineResult {
    if threads <= 1 {
        return mine_block(block, stop);
    }

    let difficulty = block.header.difficulty_target;
    tracing::info!(
        "⛏️  Mining block #{} (difficulty: {} bits, ~{:.0} expected hashes, {} threads)...",
        block.header.height,
        difficulty,
        estimated_hashes_for_difficulty(difficulty),
        threads,
    );

    let nonce_range_size = u64::MAX / threads as u64;
    let (tx, rx) = std::sync::mpsc::channel();
    let start = std::time::Instant::now();

    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let mut thread_block = block.clone();
            let stop = stop.clone();
            let tx = tx.clone();
            let start_nonce = i as u64 * nonce_range_size;

            std::thread::spawn(move || {
                let mut nonce = start_nonce;
                let end_nonce = start_nonce + nonce_range_size;

                while nonce < end_nonce {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }

                    thread_block.header.nonce = nonce;
                    if thread_block.header.meets_difficulty() {
                        let _ = tx.send(thread_block);
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }

                    nonce += 1;
                }
            })
        })
        .collect();

    drop(tx);

    match rx.recv() {
        Ok(mined_block) => {
            stop.store(true, Ordering::Relaxed);
            for handle in handles {
                let _ = handle.join();
            }

            let elapsed = start.elapsed().as_secs_f64();
            tracing::info!(
                "⛏️  Block #{} mined! hash={} time={:.2}s",
                mined_block.header.height,
                hex::encode(mined_block.header.hash()),
                elapsed,
            );

            MineResult::Found(mined_block)
        }
        Err(_) => MineResult::Cancelled,
    }
}

/// Continuously mine blocks (main mining loop for standalone mode)
pub fn mining_loop(chain: &mut Chain, config: &MinerConfig, stop: Arc<AtomicBool>) {
    tracing::info!("⛏️  Starting mining loop...");
    tracing::info!("  Miner address: {}", hex::encode(config.miner_pubkey_hash));
    tracing::info!("  Threads: {}", config.threads);

    loop {
        if stop.load(Ordering::Relaxed) {
            tracing::info!("Mining stopped.");
            break;
        }

        let pending_txs = vec![]; // TODO: get from mempool
        let template = create_block_template(chain, &pending_txs, config);

        let mine_stop = Arc::new(AtomicBool::new(false));
        let result = mine_block_parallel(template, config.threads, mine_stop);

        match result {
            MineResult::Found(block) => {
                let block_hash = block.header.hash();
                match chain.add_block(block) {
                    Ok(_) => {
                        tracing::info!(
                            "✅ Block #{} added. Hash: {} Difficulty: {} bits",
                            chain.height,
                            hex::encode(block_hash),
                            chain.tip_header().difficulty_target,
                        );
                    }
                    Err(e) => {
                        tracing::error!("❌ Block rejected: {}", e);
                    }
                }
            }
            MineResult::Cancelled => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_block_template() {
        let chain = Chain::new();
        let config = MinerConfig::default();
        let template = create_block_template(&chain, &[], &config);

        assert_eq!(template.header.height, 1);
        assert_eq!(template.header.prev_hash, chain.tip);
        assert_eq!(template.transactions.len(), 1);
        assert!(template.transactions[0].is_coinbase());
        assert_eq!(template.header.difficulty_target, INITIAL_DIFFICULTY);
    }

    #[test]
    fn test_mine_single_block() {
        let chain = Chain::new();
        let config = MinerConfig::default();
        let template = create_block_template(&chain, &[], &config);
        let stop = Arc::new(AtomicBool::new(false));

        let result = mine_block(template, stop);
        match result {
            MineResult::Found(block) => {
                assert!(block.header.meets_difficulty());
                assert_eq!(block.header.height, 1);
            }
            MineResult::Cancelled => panic!("should not be cancelled"),
        }
    }
}
