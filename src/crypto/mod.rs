//! Cryptographic primitives for EquiForge.
//!
//! Per the EquiForge whitepaper (v1.0, Feb 2026), EquiForge uses **Ed25519**
//! signatures for transaction authorization.

use ed25519_dalek::{Signature, SigningKey, Signer, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

use crate::core::types::{Hash256, Transaction, TxOutput};

pub mod txid;


/// Holds an Ed25519 signing key and its verifying key.
#[derive(Clone)]
pub struct Keypair {
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
}

impl Keypair {
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        Self { signing_key, verifying_key }
    }

    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self {
        let signing_key = SigningKey::from_bytes(bytes);
        let verifying_key = signing_key.verifying_key();
        Self { signing_key, verifying_key }
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.verifying_key.to_bytes()
    }

    /// Sign an arbitrary byte string.
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing_key.sign(msg).to_bytes()
    }

    /// Sign a 32-byte hash.
    pub fn sign_hash(&self, hash: &Hash256) -> [u8; 64] {
        self.sign(hash)
    }
}

/// Verify an Ed25519 signature. Expects a 32-byte pubkey and a 64-byte signature.
pub fn verify_signature(pubkey: &[u8], msg: &[u8], signature: &[u8]) -> bool {
    if pubkey.len() != 32 || signature.len() != 64 {
        return false;
    }

    let Ok(vk) = VerifyingKey::from_bytes(pubkey.try_into().unwrap()) else {
        return false;
    };

    let sig = Signature::from_bytes(signature.try_into().unwrap());
    vk.verify(msg, &sig).is_ok()
}

/// Deterministic "pubkey hash" used by EquiForge v1.
///
/// Note: this is **not** Bitcoin's HASH160; it is double-SHA256(pubkey).
/// That's what the current chain code stores in `TxOutput.pubkey_hash`.
pub fn pubkey_bytes_to_hash(pubkey: &[u8]) -> Hash256 {
    let first = Sha256::digest(pubkey);
    let second = Sha256::digest(&first);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&second);
    hash
}

fn double_sha256(data: &[u8]) -> Hash256 {
    let first = Sha256::digest(data);
    let second = Sha256::digest(&first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}

/// Canonical signing hash for tx inputs (v1).
///
/// Safer than the prior bincode-based hash:
/// - explicit, stable encoding (no serde/bincode dependency)
/// - binds the signature to the *specific UTXO being spent*
/// - domain separation
pub fn tx_signing_hash_v1(tx: &Transaction, input_index: usize, prev_output: &TxOutput) -> Hash256 {
    const TAG: &[u8] = b"EQF_TXSIG_V1";

    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(TAG);

    // Version
    buf.extend_from_slice(&tx.version.to_le_bytes());

    // Inputs
    buf.extend_from_slice(&(tx.inputs.len() as u32).to_le_bytes());
    for (i, input) in tx.inputs.iter().enumerate() {
        buf.extend_from_slice(&input.previous_output.txid);
        buf.extend_from_slice(&input.previous_output.vout.to_le_bytes());
        buf.extend_from_slice(&input.sequence.to_le_bytes());

        // For the input we're signing, bind the UTXO being spent
        if i == input_index {
            buf.extend_from_slice(&prev_output.amount.to_le_bytes());
            buf.extend_from_slice(&prev_output.pubkey_hash);
        }
    }

    // Outputs
    buf.extend_from_slice(&(tx.outputs.len() as u32).to_le_bytes());
    for o in &tx.outputs {
        buf.extend_from_slice(&o.amount.to_le_bytes());
        buf.extend_from_slice(&o.pubkey_hash);
    }

    // Locktime
    buf.extend_from_slice(&tx.lock_time.to_le_bytes());

    double_sha256(&buf)
}
