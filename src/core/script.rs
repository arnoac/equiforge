// src/core/script.rs
//! Minimal script validation for EquiForge v1.
//!
//! v1 standard script: P2PKH-like
//! script_pubkey: OP_DUP OP_HASH256 OP_PUSH32 <pubkey_hash32> OP_EQUALVERIFY OP_CHECKSIG
//! script_sig:    OP_PUSHDATA <sig64> OP_PUSHDATA <pubkey32>
use crate::core::types::{Hash256, Transaction, TxInput, TxOutput};
use crate::crypto;

/// Opcodes (minimal subset).
pub const OP_DUP: u8 = 0x76;
pub const OP_HASH256: u8 = 0xAA; // custom "double SHA256" (32-byte) to match your current pubkey_hash size
pub const OP_EQUALVERIFY: u8 = 0x88;
pub const OP_CHECKSIG: u8 = 0xAC;

/// Push helpers
pub const OP_PUSHDATA1: u8 = 0x4c;

#[derive(Debug)]
pub enum ScriptError {
    NonStandard,
    BadEncoding,
    PubkeyHashMismatch,
    BadSignature,
}

/// Build a standard P2PKH script_pubkey from a 32-byte pubkey hash.
///
/// Template:
/// [OP_DUP][OP_HASH256][OP_PUSHDATA1][32][pubkey_hash32][OP_EQUALVERIFY][OP_CHECKSIG]
pub fn script_p2pkh(pubkey_hash: &Hash256) -> Vec<u8> {
    let mut s = Vec::with_capacity(1 + 1 + 1 + 1 + 32 + 1 + 1);
    s.push(OP_DUP);
    s.push(OP_HASH256);
    s.push(OP_PUSHDATA1);
    s.push(32);
    s.extend_from_slice(pubkey_hash);
    s.push(OP_EQUALVERIFY);
    s.push(OP_CHECKSIG);
    s
}

/// Encode script_sig for spending a P2PKH output:
/// [OP_PUSHDATA1][64][sig64][OP_PUSHDATA1][32][pubkey32]
pub fn script_sig_p2pkh(sig64: &[u8; 64], pubkey32: &[u8; 32]) -> Vec<u8> {
    let mut s = Vec::with_capacity(1 + 1 + 64 + 1 + 1 + 32);
    s.push(OP_PUSHDATA1);
    s.push(64);
    s.extend_from_slice(sig64);
    s.push(OP_PUSHDATA1);
    s.push(32);
    s.extend_from_slice(pubkey32);
    s
}

/// Parse script_sig_p2pkh into (sig64, pubkey32).
pub fn parse_script_sig_p2pkh(script_sig: &[u8]) -> Result<([u8; 64], [u8; 32]), ScriptError> {
    // Expect: 0x4c 0x40 <64 bytes> 0x4c 0x20 <32 bytes>
    if script_sig.len() != 1 + 1 + 64 + 1 + 1 + 32 {
        return Err(ScriptError::BadEncoding);
    }
    if script_sig[0] != OP_PUSHDATA1 || script_sig[1] != 64 {
        return Err(ScriptError::BadEncoding);
    }
    if script_sig[66] != OP_PUSHDATA1 || script_sig[67] != 32 {
        return Err(ScriptError::BadEncoding);
    }

    let mut sig = [0u8; 64];
    sig.copy_from_slice(&script_sig[2..66]);

    let mut pk = [0u8; 32];
    pk.copy_from_slice(&script_sig[68..100]);

    Ok((sig, pk))
}

/// Parse script_pubkey P2PKH to extract pubkey_hash32.
pub fn parse_script_pubkey_p2pkh(script_pubkey: &[u8]) -> Result<Hash256, ScriptError> {
    // Expect: OP_DUP OP_HASH256 OP_PUSHDATA1 32 <32 bytes> OP_EQUALVERIFY OP_CHECKSIG
    if script_pubkey.len() != 1 + 1 + 1 + 1 + 32 + 1 + 1 {
        return Err(ScriptError::NonStandard);
    }
    if script_pubkey[0] != OP_DUP
        || script_pubkey[1] != OP_HASH256
        || script_pubkey[2] != OP_PUSHDATA1
        || script_pubkey[3] != 32
        || script_pubkey[36] != OP_EQUALVERIFY
        || script_pubkey[37] != OP_CHECKSIG
    {
        return Err(ScriptError::NonStandard);
    }

    let mut h = [0u8; 32];
    h.copy_from_slice(&script_pubkey[4..36]);
    Ok(h)
}

/// Validate a P2PKH spend.
///
/// - Derive pubkey_hash from pubkey
/// - Must match the pubkey_hash in script_pubkey (and/or output)
/// - Verify Ed25519 signature over tx_signing_hash_v1(...)
pub fn validate_p2pkh_spend(
    tx: &Transaction,
    input_index: usize,
    input: &TxInput,
    prev_output: &TxOutput,
) -> Result<(), ScriptError> {
    // Determine the expected pubkey hash from the locking script
    let lock_hash = parse_script_pubkey_p2pkh(&prev_output.script_pubkey)
        .or_else(|_| {
            // If your TxOutput still stores pubkey_hash directly, you can fallback here.
            // But recommended: use script_pubkey as the source of truth.
            Ok(prev_output.pubkey_hash)
        })?;

    // Unlocking script
    let (sig64, pubkey32) = parse_script_sig_p2pkh(&input.script_sig)?;

    // Verify pubkey hash matches
    let derived = crypto::pubkey_bytes_to_hash(&pubkey32);
    if derived != lock_hash {
        return Err(ScriptError::PubkeyHashMismatch);
    }

    // Verify signature bound to this UTXO + tx outputs
    let sighash = crypto::tx_signing_hash_v1(tx, input_index, prev_output);
    if !crypto::verify_signature(&pubkey32, &sighash, &sig64) {
        return Err(ScriptError::BadSignature);
    }

    Ok(())
}
