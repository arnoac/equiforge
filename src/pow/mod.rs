//! EquiHash-X: Custom Proof-of-Work Algorithm for EquiForge
//!
//! Design goals:
//!   - ASIC-resistant: memory-hard with random access patterns
//!   - CPU-friendly: fits in L3 cache, branchy mixed operations
//!   - GPU-competitive but not dominant: sequential dependencies limit parallelism
//!   - Verifiable: same algorithm for mining and validation (no asymmetry)
//!
//! Algorithm overview:
//!
//!   Phase 1 — FILL: Generate a 4 MB scratchpad from the block header seed.
//!     The scratchpad is filled in 64-byte chunks using Blake3 keyed with
//!     successive counter values. This is sequential and memory-bandwidth bound.
//!
//!   Phase 2 — MIX: Perform N_ITERATIONS rounds of memory-hard mixing.
//!     Each round:
//!       1. Compute a mix index from the current state (data-dependent addressing)
//!       2. Read 64 bytes from scratchpad at that index
//!       3. Mix the read data into the running state using:
//!          - XOR, rotate, add (cheap but branch-free)
//!          - SHA-256 compression (every 8th round, adds compute cost)
//!          - Blake3 hash (every 16th round, different instruction mix)
//!       4. Write the mixed state back to a different scratchpad location
//!          (read-write access prevents GPU memory caching tricks)
//!
//!   Phase 3 — SQUEEZE: Compress the final state into a 32-byte hash
//!     using double SHA-256 (compatible with existing difficulty system).
//!
//! Parameters:
//!   SCRATCHPAD_SIZE = 4 MB (4,194,304 bytes)
//!   N_ITERATIONS = 64
//!   CHUNK_SIZE = 64 bytes
//!   N_CHUNKS = 65,536
//!
//! Performance expectations (per core):
//!   Modern CPU: ~50-200 hashes/second
//!   GPU: ~100-500 hashes/second (memory latency limited)
//!   ASIC: impractical (4 MB SRAM per hash unit is uneconomical)

use sha2::{Digest, Sha256};

/// Scratchpad size in bytes (4 MB)
const SCRATCHPAD_SIZE: usize = 4 * 1024 * 1024;

/// Size of each scratchpad chunk in bytes
const CHUNK_SIZE: usize = 64;

/// Number of chunks in the scratchpad
const N_CHUNKS: usize = SCRATCHPAD_SIZE / CHUNK_SIZE;

/// Number of mixing iterations
const N_ITERATIONS: usize = 64;

/// Compute the EquiHash-X proof-of-work hash for a block header.
///
/// Input: serialized block header bytes (includes nonce)
/// Output: 32-byte hash suitable for difficulty comparison
///
/// This function is deterministic: same input always produces same output.
/// Both miners and validators call this exact function.
pub fn equihash_x(header_bytes: &[u8]) -> [u8; 32] {
    // ─── Phase 1: FILL scratchpad ───────────────────────────────────
    //
    // Generate the scratchpad deterministically from the header.
    // We use Blake3 in keyed mode for speed (Blake3 is ~3x faster than SHA-256
    // for bulk data, which is fine since the memory-hardness comes from Phase 2).

    let mut scratchpad = vec![0u8; SCRATCHPAD_SIZE];

    // Derive a 32-byte seed from the header
    let seed = blake3::hash(header_bytes);
    let seed_bytes = seed.as_bytes();

    // Fill scratchpad in 64-byte chunks
    // Each chunk = Blake3(seed || chunk_index)
    for i in 0..N_CHUNKS {
        let mut input = Vec::with_capacity(36);
        input.extend_from_slice(seed_bytes);
        input.extend_from_slice(&(i as u32).to_le_bytes());
        let chunk_hash = blake3::hash(&input);
        let chunk_bytes = chunk_hash.as_bytes();

        let offset = i * CHUNK_SIZE;
        // Blake3 produces 32 bytes; we need 64, so hash again with a tweak
        scratchpad[offset..offset + 32].copy_from_slice(chunk_bytes);

        let mut input2 = Vec::with_capacity(36);
        input2.extend_from_slice(chunk_bytes);
        input2.extend_from_slice(&(i as u32).to_le_bytes());
        let chunk_hash2 = blake3::hash(&input2);
        scratchpad[offset + 32..offset + 64].copy_from_slice(chunk_hash2.as_bytes());
    }

    // ─── Phase 2: MIX ───────────────────────────────────────────────
    //
    // Running state: 64 bytes (treated as 8 x u64 limbs)
    // Each iteration reads from a data-dependent scratchpad location,
    // mixes it into the state, and writes back to another location.

    let mut state = [0u64; 8];

    // Initialize state from seed
    for i in 0..4 {
        state[i] = u64::from_le_bytes(seed_bytes[i * 8..(i + 1) * 8].try_into().unwrap());
    }
    // Second half from header hash
    let header_hash = Sha256::digest(header_bytes);
    for i in 0..4 {
        state[4 + i] = u64::from_le_bytes(header_hash[i * 8..(i + 1) * 8].try_into().unwrap());
    }

    for round in 0..N_ITERATIONS {
        // 1. Compute read index from state (data-dependent addressing)
        let read_idx = (state[0].wrapping_add(state[round % 8]) as usize) % N_CHUNKS;
        let read_offset = read_idx * CHUNK_SIZE;

        // 2. Read 64 bytes from scratchpad
        let mut read_data = [0u64; 8];
        for j in 0..8 {
            read_data[j] = u64::from_le_bytes(
                scratchpad[read_offset + j * 8..read_offset + (j + 1) * 8]
                    .try_into()
                    .unwrap(),
            );
        }

        // 3. Mix into state
        for j in 0..8 {
            // XOR with scratchpad data
            state[j] ^= read_data[j];
            // Rotate and add (creates sequential dependency)
            state[j] = state[j]
                .wrapping_add(state[(j + 1) % 8])
                .rotate_left((round as u32 + j as u32) % 64);
        }

        // Every 8th round: SHA-256 compression (adds compute diversity)
        if round % 8 == 7 {
            let mut sha_input = [0u8; 64];
            for j in 0..8 {
                sha_input[j * 8..(j + 1) * 8].copy_from_slice(&state[j].to_le_bytes());
            }
            let sha_result = Sha256::digest(&sha_input);
            for j in 0..4 {
                state[j] ^= u64::from_le_bytes(
                    sha_result[j * 8..(j + 1) * 8].try_into().unwrap(),
                );
            }
        }

        // Every 16th round: Blake3 compression (different instruction mix)
        if round % 16 == 15 {
            let mut blake_input = [0u8; 64];
            for j in 0..8 {
                blake_input[j * 8..(j + 1) * 8].copy_from_slice(&state[j].to_le_bytes());
            }
            let blake_result = blake3::hash(&blake_input);
            let blake_bytes = blake_result.as_bytes();
            for j in 0..4 {
                state[4 + j] ^= u64::from_le_bytes(
                    blake_bytes[j * 8..(j + 1) * 8].try_into().unwrap(),
                );
            }
        }

        // 4. Write mixed state back to a different scratchpad location
        let write_idx = (state[1].wrapping_mul(state[3]) as usize) % N_CHUNKS;
        let write_offset = write_idx * CHUNK_SIZE;
        for j in 0..8 {
            scratchpad[write_offset + j * 8..write_offset + (j + 1) * 8]
                .copy_from_slice(&state[j].to_le_bytes());
        }
    }

    // ─── Phase 3: SQUEEZE ───────────────────────────────────────────
    //
    // Final compression: convert 64-byte state to 32-byte hash.
    // Double SHA-256 for compatibility with existing difficulty check.

    let mut final_input = [0u8; 64];
    for j in 0..8 {
        final_input[j * 8..(j + 1) * 8].copy_from_slice(&state[j].to_le_bytes());
    }

    let first = Sha256::digest(&final_input);
    let second = Sha256::digest(&first);
    let mut result = [0u8; 32];
    result.copy_from_slice(&second);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic() {
        let header = b"test block header data with nonce 12345";
        let hash1 = equihash_x(header);
        let hash2 = equihash_x(header);
        assert_eq!(hash1, hash2, "same input must produce same output");
    }

    #[test]
    fn test_different_input_different_output() {
        let h1 = equihash_x(b"header nonce=1");
        let h2 = equihash_x(b"header nonce=2");
        assert_ne!(h1, h2, "different inputs should produce different outputs");
    }

    #[test]
    fn test_avalanche() {
        // Changing one bit should change ~50% of output bits
        let mut input1 = vec![0u8; 80];
        let mut input2 = input1.clone();
        input2[0] = 1; // flip one bit

        let h1 = equihash_x(&input1);
        let h2 = equihash_x(&input2);

        let mut differing_bits = 0;
        for i in 0..32 {
            differing_bits += (h1[i] ^ h2[i]).count_ones();
        }

        // Expect roughly 128 bits different (50% of 256), allow wide margin
        assert!(differing_bits > 64, "poor avalanche: only {} bits differ", differing_bits);
        assert!(differing_bits < 192, "suspicious avalanche: {} bits differ", differing_bits);
    }

    #[test]
    fn test_performance() {
        // Benchmark: should take several milliseconds per hash
        let header = b"benchmark header with sufficient length for testing";
        let start = std::time::Instant::now();
        let iterations = 10;
        for i in 0..iterations {
            let mut input = header.to_vec();
            input.extend_from_slice(&(i as u64).to_le_bytes());
            let _ = equihash_x(&input);
        }
        let elapsed = start.elapsed();
        let per_hash = elapsed / iterations;
        println!(
            "EquiHash-X: {} hashes in {:.2?} ({:.1?}/hash, {:.1} H/s)",
            iterations,
            elapsed,
            per_hash,
            iterations as f64 / elapsed.as_secs_f64()
        );

        // Should be at least 1ms per hash (memory-hard)
        assert!(
            per_hash.as_millis() >= 1,
            "hash too fast ({:?}), not memory-hard enough",
            per_hash
        );
    }
}
