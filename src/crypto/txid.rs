use sha2::{Digest, Sha256};
use crate::core::types::{Hash256, Transaction};

fn dsha256(data: &[u8]) -> Hash256 {
    let a = Sha256::digest(data);
    let b = Sha256::digest(&a);
    let mut out = [0u8; 32];
    out.copy_from_slice(&b);
    out
}

/// Canonical encoding for TXID that EXCLUDES script_sig (unlocking data).
/// This makes txid stable even if unlocking data changes in future upgrades.
///
/// v1 encoding:
/// TAG || version || inputs(outpoint+sequence only) || outputs(amount+pubkey_hash+script_pubkey) || lock_time
pub fn txid_v1(tx: &Transaction) -> Hash256 {
    const TAG: &[u8] = b"EQF_TXID_V1";
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(TAG);

    buf.extend_from_slice(&tx.version.to_le_bytes());

    buf.extend_from_slice(&(tx.inputs.len() as u32).to_le_bytes());
    for i in &tx.inputs {
        buf.extend_from_slice(&i.previous_output.txid);
        buf.extend_from_slice(&i.previous_output.vout.to_le_bytes());
        buf.extend_from_slice(&i.sequence.to_le_bytes());
        // EXCLUDE script_sig
    }

    buf.extend_from_slice(&(tx.outputs.len() as u32).to_le_bytes());
    for o in &tx.outputs {
        buf.extend_from_slice(&o.amount.to_le_bytes());
        buf.extend_from_slice(&o.pubkey_hash);
        buf.extend_from_slice(&(o.script_pubkey.len() as u32).to_le_bytes());
        buf.extend_from_slice(&o.script_pubkey);
    }

    buf.extend_from_slice(&tx.lock_time.to_le_bytes());
    dsha256(&buf)
}

/// WTXID includes script_sig (unlocking data).
/// Useful for p2p relay uniqueness / compact blocks later.
pub fn wtxid_v1(tx: &Transaction) -> Hash256 {
    const TAG: &[u8] = b"EQF_WTXID_V1";
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(TAG);

    buf.extend_from_slice(&tx.version.to_le_bytes());

    buf.extend_from_slice(&(tx.inputs.len() as u32).to_le_bytes());
    for i in &tx.inputs {
        buf.extend_from_slice(&i.previous_output.txid);
        buf.extend_from_slice(&i.previous_output.vout.to_le_bytes());
        buf.extend_from_slice(&i.sequence.to_le_bytes());
        buf.extend_from_slice(&(i.script_sig.len() as u32).to_le_bytes());
        buf.extend_from_slice(&i.script_sig);
    }

    buf.extend_from_slice(&(tx.outputs.len() as u32).to_le_bytes());
    for o in &tx.outputs {
        buf.extend_from_slice(&o.amount.to_le_bytes());
        buf.extend_from_slice(&o.pubkey_hash);
        buf.extend_from_slice(&(o.script_pubkey.len() as u32).to_le_bytes());
        buf.extend_from_slice(&o.script_pubkey);
    }

    buf.extend_from_slice(&tx.lock_time.to_le_bytes());
    dsha256(&buf)
}
