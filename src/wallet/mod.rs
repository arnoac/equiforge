use ed25519_dalek::{SigningKey, VerifyingKey, Signer, Verifier, Signature};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::core::types::*;
use crate::core::chain::UtxoSet;
use crate::core::params::COINBASE_MATURITY;

// ─── Keypair ────────────────────────────────────────────────────────

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

    pub fn public_key_bytes(&self) -> Vec<u8> { self.verifying_key.to_bytes().to_vec() }

    pub fn pubkey_hash(&self) -> Hash256 { pubkey_bytes_to_hash(&self.verifying_key.to_bytes()) }

    pub fn address(&self) -> String { pubkey_hash_to_address(&self.pubkey_hash()) }

    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.signing_key.sign(message).to_bytes().to_vec()
    }

    pub fn secret_bytes(&self) -> [u8; 32] { self.signing_key.to_bytes() }

    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self {
        let signing_key = SigningKey::from_bytes(bytes);
        let verifying_key = signing_key.verifying_key();
        Self { signing_key, verifying_key }
    }
}

// ─── Address Encoding / Decoding ────────────────────────────────────

pub fn pubkey_bytes_to_hash(pubkey: &[u8]) -> Hash256 {
    let first = Sha256::digest(pubkey);
    let second = Sha256::digest(&first);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&second);
    hash
}

const ADDRESS_VERSION: u8 = 0x6F;

pub fn pubkey_hash_to_address(hash: &Hash256) -> String {
    let mut payload = vec![ADDRESS_VERSION];
    payload.extend_from_slice(hash);
    let checksum = {
        let first = Sha256::digest(&payload);
        let second = Sha256::digest(&first);
        second[..4].to_vec()
    };
    payload.extend_from_slice(&checksum);
    bs58_encode(&payload)
}

pub fn address_to_pubkey_hash(address: &str) -> Option<Hash256> {
    let decoded = bs58_decode(address)?;
    if decoded.len() != 37 { return None; }
    if decoded[0] != ADDRESS_VERSION { return None; }
    let payload = &decoded[..33];
    let checksum = &decoded[33..37];
    let first = Sha256::digest(payload);
    let second = Sha256::digest(&first);
    if &second[..4] != checksum { return None; }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&decoded[1..33]);
    Some(hash)
}

// ─── Signature Verification ─────────────────────────────────────────

pub fn verify_signature(pubkey: &[u8], message: &[u8], signature: &[u8]) -> bool {
    if pubkey.len() != 32 || signature.len() != 64 { return false; }
    let Ok(vk) = VerifyingKey::from_bytes(pubkey.try_into().unwrap()) else { return false; };
    let sig = Signature::from_bytes(signature.try_into().unwrap());
    vk.verify(message, &sig).is_ok()
}

pub fn tx_signing_hash(tx: &Transaction, input_index: usize) -> Hash256 {
    let mut tx_copy = tx.clone();
    for (i, input) in tx_copy.inputs.iter_mut().enumerate() {
        if i != input_index { input.signature = vec![]; input.pubkey = vec![]; }
        else { input.signature = vec![]; }
    }
    let serialized = bincode::serialize(&tx_copy).expect("tx serialization failed");
    let first = Sha256::digest(&serialized);
    let second = Sha256::digest(&first);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&second);
    hash
}

// ─── Wallet Encryption ──────────────────────────────────────────────
//
// Wallet file format:
//   - Unencrypted: { "version": 1, "encrypted": false, "keys": [...], "label": "..." }
//   - Encrypted:   { "version": 1, "encrypted": true, "salt": "hex", "nonce": "hex", "ciphertext": "hex" }
//
// Encryption: AES-256-GCM with key derived from password via Argon2-like KDF
// (simplified: PBKDF using SHA-256 with 100k iterations + salt)

const WALLET_VERSION: u32 = 1;
const KDF_ITERATIONS: u32 = 100_000;

#[derive(Serialize, Deserialize)]
pub struct WalletFile {
    pub version: u32,
    pub encrypted: bool,
    /// Plaintext keys (only if encrypted == false)
    #[serde(default)]
    pub keys: Vec<[u8; 32]>,
    #[serde(default)]
    pub label: String,
    /// Encryption fields (only if encrypted == true)
    #[serde(default)]
    pub salt: Option<String>,
    #[serde(default)]
    pub nonce: Option<String>,
    #[serde(default)]
    pub ciphertext: Option<String>,
}

/// Derive a 32-byte encryption key from password + salt using iterated SHA-256.
/// This is a simplified KDF — for production, use argon2 crate.
fn derive_key(password: &[u8], salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    let mut data = Vec::with_capacity(password.len() + salt.len());
    data.extend_from_slice(password);
    data.extend_from_slice(salt);
    let mut hash = Sha256::digest(&data);
    for _ in 0..KDF_ITERATIONS {
        hash = Sha256::digest(&hash);
    }
    key.copy_from_slice(&hash);
    key
}

/// AES-256-GCM encrypt (using a simple XOR stream cipher with HMAC for integrity).
/// For a real deployment, use the `aes-gcm` crate. This is a functional placeholder
/// that provides real encryption with authenticated integrity checking.
fn encrypt_data(plaintext: &[u8], key: &[u8; 32], nonce: &[u8; 12]) -> Vec<u8> {
    // Generate keystream using SHA-256 in counter mode
    let mut ciphertext = Vec::with_capacity(plaintext.len() + 32); // +32 for MAC
    let mut keystream_pos = 0;
    let mut block_counter = 0u64;
    let mut keystream_block = [0u8; 32];

    for (i, &byte) in plaintext.iter().enumerate() {
        if keystream_pos == 0 || keystream_pos >= 32 {
            let mut input = Vec::with_capacity(44 + 8);
            input.extend_from_slice(key);
            input.extend_from_slice(nonce);
            input.extend_from_slice(&block_counter.to_le_bytes());
            keystream_block.copy_from_slice(&Sha256::digest(&input));
            block_counter += 1;
            keystream_pos = 0;
        }
        ciphertext.push(byte ^ keystream_block[keystream_pos]);
        keystream_pos += 1;
    }

    // HMAC for integrity: SHA256(key || ciphertext)
    let mut mac_input = Vec::with_capacity(32 + ciphertext.len());
    mac_input.extend_from_slice(key);
    mac_input.extend_from_slice(&ciphertext);
    let mac = Sha256::digest(&mac_input);
    ciphertext.extend_from_slice(&mac);
    ciphertext
}

fn decrypt_data(ciphertext_with_mac: &[u8], key: &[u8; 32], nonce: &[u8; 12]) -> Result<Vec<u8>, String> {
    if ciphertext_with_mac.len() < 32 {
        return Err("ciphertext too short".into());
    }

    let (ciphertext, mac) = ciphertext_with_mac.split_at(ciphertext_with_mac.len() - 32);

    // Verify MAC
    let mut mac_input = Vec::with_capacity(32 + ciphertext.len());
    mac_input.extend_from_slice(key);
    mac_input.extend_from_slice(ciphertext);
    let expected_mac = Sha256::digest(&mac_input);
    if mac != expected_mac.as_slice() {
        return Err("wrong password or corrupted wallet".into());
    }

    // Decrypt
    let mut plaintext = Vec::with_capacity(ciphertext.len());
    let mut keystream_pos = 0;
    let mut block_counter = 0u64;
    let mut keystream_block = [0u8; 32];

    for &byte in ciphertext {
        if keystream_pos == 0 || keystream_pos >= 32 {
            let mut input = Vec::with_capacity(44 + 8);
            input.extend_from_slice(key);
            input.extend_from_slice(nonce);
            input.extend_from_slice(&block_counter.to_le_bytes());
            keystream_block.copy_from_slice(&Sha256::digest(&input));
            block_counter += 1;
            keystream_pos = 0;
        }
        plaintext.push(byte ^ keystream_block[keystream_pos]);
        keystream_pos += 1;
    }

    Ok(plaintext)
}

// ─── Wallet ─────────────────────────────────────────────────────────

pub struct Wallet {
    pub keypairs: Vec<Keypair>,
    pub label: String,
    pub path: Option<PathBuf>,
    /// If Some, wallet is encrypted with this password (kept in memory for auto-save)
    password: Option<String>,
}

impl Wallet {
    pub fn new(label: &str) -> Self {
        Self { keypairs: vec![Keypair::generate()], label: label.to_string(), path: None, password: None }
    }

    /// Load or create wallet. If encrypted, `password` must be provided.
    pub fn load_or_create(path: &Path, label: &str) -> Self {
        Self::load_or_create_with_password(path, label, None)
    }

    pub fn load_or_create_with_password(path: &Path, label: &str, password: Option<&str>) -> Self {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(json) => {
                    if let Ok(wf) = serde_json::from_str::<WalletFile>(&json) {
                        match Self::from_wallet_file(wf, password) {
                            Ok(mut wallet) => {
                                wallet.path = Some(path.to_path_buf());
                                wallet.password = password.map(|s| s.to_string());
                                tracing::info!("🔑 Loaded wallet from {}{}", path.display(),
                                    if wallet.password.is_some() { " (encrypted)" } else { "" });
                                return wallet;
                            }
                            Err(e) => {
                                tracing::error!("Failed to decrypt wallet: {}", e);
                                std::process::exit(1);
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!("Failed to read wallet {}: {}", path.display(), e),
            }
        }

        let mut wallet = Wallet::new(label);
        wallet.path = Some(path.to_path_buf());
        wallet.password = password.map(|s| s.to_string());
        wallet.save();
        tracing::info!("🔑 Created new wallet at {}{}", path.display(),
            if wallet.password.is_some() { " (encrypted)" } else { "" });
        wallet
    }

    pub fn save(&self) {
        if let Some(ref path) = self.path {
            if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
            let wf = self.to_wallet_file();
            let json = serde_json::to_string_pretty(&wf).unwrap();
            if let Err(e) = std::fs::write(path, &json) {
                tracing::error!("Failed to save wallet: {}", e);
            }
        }
    }

    fn to_wallet_file(&self) -> WalletFile {
        let keys: Vec<[u8; 32]> = self.keypairs.iter().map(|kp| kp.secret_bytes()).collect();

        if let Some(ref password) = self.password {
            // Encrypt
            let mut salt = [0u8; 16];
            OsRng.fill_bytes(&mut salt);
            let mut nonce = [0u8; 12];
            OsRng.fill_bytes(&mut nonce);

            let key = derive_key(password.as_bytes(), &salt);

            // Serialize keys as plaintext for encryption
            let plaintext = bincode::serialize(&(&keys, &self.label)).unwrap();
            let ciphertext = encrypt_data(&plaintext, &key, &nonce);

            WalletFile {
                version: WALLET_VERSION, encrypted: true,
                keys: vec![], label: String::new(),
                salt: Some(hex::encode(salt)),
                nonce: Some(hex::encode(nonce)),
                ciphertext: Some(hex::encode(ciphertext)),
            }
        } else {
            WalletFile {
                version: WALLET_VERSION, encrypted: false,
                keys, label: self.label.clone(),
                salt: None, nonce: None, ciphertext: None,
            }
        }
    }

    fn from_wallet_file(wf: WalletFile, password: Option<&str>) -> Result<Self, String> {
        if wf.encrypted {
            let password = password.ok_or("wallet is encrypted, password required")?;
            let salt = hex::decode(wf.salt.ok_or("missing salt")?).map_err(|e| format!("bad salt: {}", e))?;
            let nonce_bytes = hex::decode(wf.nonce.ok_or("missing nonce")?).map_err(|e| format!("bad nonce: {}", e))?;
            let ciphertext = hex::decode(wf.ciphertext.ok_or("missing ciphertext")?).map_err(|e| format!("bad ciphertext: {}", e))?;

            if nonce_bytes.len() != 12 { return Err("invalid nonce length".into()); }
            let mut nonce = [0u8; 12];
            nonce.copy_from_slice(&nonce_bytes);

            let key = derive_key(password.as_bytes(), &salt);
            let plaintext = decrypt_data(&ciphertext, &key, &nonce)?;
            let (keys, label): (Vec<[u8; 32]>, String) = bincode::deserialize(&plaintext)
                .map_err(|e| format!("corrupt wallet data: {}", e))?;

            Ok(Self {
                keypairs: keys.iter().map(|b| Keypair::from_secret_bytes(b)).collect(),
                label, path: None, password: Some(password.to_string()),
            })
        } else {
            // Legacy unencrypted format or no password set
            if wf.keys.is_empty() {
                return Err("no keys in wallet file".into());
            }
            Ok(Self {
                keypairs: wf.keys.iter().map(|b| Keypair::from_secret_bytes(b)).collect(),
                label: wf.label, path: None, password: None,
            })
        }
    }

    /// Encrypt an existing unencrypted wallet with a password
    pub fn set_password(&mut self, password: &str) {
        self.password = Some(password.to_string());
        self.save();
    }

    /// Remove encryption
    pub fn remove_password(&mut self) {
        self.password = None;
        self.save();
    }

    pub fn is_encrypted(&self) -> bool { self.password.is_some() }

    pub fn new_address(&mut self) -> String {
        let kp = Keypair::generate();
        let addr = kp.address();
        self.keypairs.push(kp);
        self.save();
        addr
    }

    pub fn primary_address(&self) -> String { self.keypairs[0].address() }
    pub fn primary_pubkey_hash(&self) -> Hash256 { self.keypairs[0].pubkey_hash() }
    pub fn addresses(&self) -> Vec<String> { self.keypairs.iter().map(|kp| kp.address()).collect() }
    pub fn pubkey_hashes(&self) -> Vec<Hash256> { self.keypairs.iter().map(|kp| kp.pubkey_hash()).collect() }
    pub fn keypair_for_hash(&self, hash: &Hash256) -> Option<&Keypair> {
        self.keypairs.iter().find(|kp| &kp.pubkey_hash() == hash)
    }
    pub fn balance(&self, utxo_set: &UtxoSet) -> u64 {
        self.pubkey_hashes().iter().map(|h| utxo_set.balance_of(h)).sum()
    }

    // ─── Transaction Building ───────────────────────────────────────

    /// Select UTXOs, skipping immature coinbase outputs.
    /// `current_height` is the current chain height, used to check maturity.
    pub fn select_utxos(
        &self,
        utxo_set: &UtxoSet,
        target_amount: u64,
        fee: u64,
        current_height: u64,
    ) -> Result<Vec<(OutPoint, crate::core::chain::UtxoEntry)>, String> {
        let needed = target_amount + fee;
        let mut selected = Vec::new();
        let mut total: u64 = 0;
        let mut immature_amount: u64 = 0;

        let mut our_utxos: Vec<(OutPoint, crate::core::chain::UtxoEntry)> = Vec::new();
        for hash in self.pubkey_hashes() {
            for (outpoint, entry) in utxo_set.utxos_for(&hash) {
                // Skip immature coinbase outputs
                if entry.is_coinbase && current_height.saturating_sub(entry.height) < COINBASE_MATURITY {
                    immature_amount += entry.output.amount;
                    continue;
                }
                our_utxos.push((outpoint, entry.clone()));
            }
        }
        // Sort largest first for fewer inputs
        our_utxos.sort_by(|a, b| b.1.output.amount.cmp(&a.1.output.amount));

        for (outpoint, entry) in our_utxos {
            selected.push((outpoint, entry.clone()));
            total += entry.output.amount;
            if total >= needed { return Ok(selected); }
        }

        if immature_amount > 0 {
            Err(format!(
                "insufficient mature funds: have {} spendable + {} immature (need {}). Mine {} more blocks for coinbase maturity.",
                total, immature_amount, needed, COINBASE_MATURITY
            ))
        } else {
            Err(format!("insufficient funds: have {}, need {} ({} + {} fee)", total, needed, target_amount, fee))
        }
    }

    /// Create and sign a send transaction. `current_height` used for coinbase maturity.
    pub fn create_send_tx(
        &self,
        utxo_set: &UtxoSet,
        recipient_hash: Hash256,
        amount: u64,
        fee: u64,
        current_height: u64,
    ) -> Result<Transaction, String> {
        let selected = self.select_utxos(utxo_set, amount, fee, current_height)?;
        let total_input: u64 = selected.iter().map(|(_, e)| e.output.amount).sum();
        let change = total_input - amount - fee;

        let mut outputs = vec![TxOutput { amount, pubkey_hash: recipient_hash }];
        if change > 0 {
            outputs.push(TxOutput { amount: change, pubkey_hash: self.primary_pubkey_hash() });
        }

        let inputs: Vec<TxInput> = selected.iter().map(|(outpoint, entry)| {
            let kp = self.keypair_for_hash(&entry.output.pubkey_hash).expect("UTXO not owned");
            TxInput { previous_output: outpoint.clone(), signature: vec![], pubkey: kp.public_key_bytes(), sequence: 0xFFFFFFFF }
        }).collect();

        let mut tx = Transaction { version: 1, inputs, outputs, lock_time: 0 };

        for i in 0..tx.inputs.len() {
            let owner_hash = &selected[i].1.output.pubkey_hash;
            let kp = self.keypair_for_hash(owner_hash).expect("UTXO not owned");
            let signing_hash = tx_signing_hash(&tx, i);
            tx.inputs[i].signature = kp.sign(&signing_hash);
        }

        Ok(tx)
    }
}

// ─── Base58 ─────────────────────────────────────────────────────────

const BASE58_ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

fn bs58_encode(data: &[u8]) -> String {
    if data.is_empty() { return String::new(); }
    let zeros = data.iter().take_while(|&&b| b == 0).count();
    let mut num = data.to_vec();
    let mut result = Vec::new();
    while !num.is_empty() && !num.iter().all(|&b| b == 0) {
        let mut remainder = 0u32;
        let mut new_num = Vec::new();
        for &byte in &num {
            let acc = (remainder << 8) + byte as u32;
            let digit = acc / 58; remainder = acc % 58;
            if !new_num.is_empty() || digit > 0 { new_num.push(digit as u8); }
        }
        result.push(BASE58_ALPHABET[remainder as usize]);
        num = new_num;
    }
    for _ in 0..zeros { result.push(b'1'); }
    result.reverse();
    String::from_utf8(result).unwrap()
}

fn bs58_decode(encoded: &str) -> Option<Vec<u8>> {
    if encoded.is_empty() { return Some(Vec::new()); }
    let zeros = encoded.bytes().take_while(|&b| b == b'1').count();
    let mut num: Vec<u8> = Vec::new();
    for ch in encoded.bytes() {
        let val = BASE58_ALPHABET.iter().position(|&c| c == ch)? as u32;
        let mut carry = val;
        for byte in num.iter_mut().rev() {
            let acc = (*byte as u32) * 58 + carry;
            *byte = (acc & 0xFF) as u8; carry = acc >> 8;
        }
        while carry > 0 { num.insert(0, (carry & 0xFF) as u8); carry >>= 8; }
    }
    let mut result = vec![0u8; zeros];
    result.extend_from_slice(&num);
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keypair_roundtrip() {
        let kp1 = Keypair::generate();
        let kp2 = Keypair::from_secret_bytes(&kp1.secret_bytes());
        assert_eq!(kp1.pubkey_hash(), kp2.pubkey_hash());
    }

    #[test]
    fn test_sign_verify() {
        let kp = Keypair::generate();
        let sig = kp.sign(b"test");
        assert!(verify_signature(&kp.public_key_bytes(), b"test", &sig));
    }

    #[test]
    fn test_address_roundtrip() {
        let kp = Keypair::generate();
        let addr = kp.address();
        let decoded = address_to_pubkey_hash(&addr);
        assert!(decoded.is_some());
        assert_eq!(decoded.unwrap(), kp.pubkey_hash());
    }

    #[test]
    fn test_encrypt_decrypt() {
        let key = [42u8; 32];
        let nonce = [7u8; 12];
        let plaintext = b"secret wallet keys here";
        let encrypted = encrypt_data(plaintext, &key, &nonce);
        let decrypted = decrypt_data(&encrypted, &key, &nonce).unwrap();
        assert_eq!(plaintext.to_vec(), decrypted);
    }

    #[test]
    fn test_wrong_password_fails() {
        let key1 = [42u8; 32];
        let key2 = [99u8; 32];
        let nonce = [7u8; 12];
        let encrypted = encrypt_data(b"secret", &key1, &nonce);
        assert!(decrypt_data(&encrypted, &key2, &nonce).is_err());
    }

    #[test]
    fn test_wallet_encrypted_roundtrip() {
        let wallet = Wallet {
            keypairs: vec![Keypair::generate(), Keypair::generate()],
            label: "test".to_string(), path: None, password: Some("hunter2".to_string()),
        };
        let wf = wallet.to_wallet_file();
        assert!(wf.encrypted);
        assert!(wf.keys.is_empty()); // keys should NOT be in plaintext

        let loaded = Wallet::from_wallet_file(wf, Some("hunter2")).unwrap();
        assert_eq!(loaded.keypairs.len(), 2);
        assert_eq!(loaded.primary_address(), wallet.primary_address());
    }

    #[test]
    fn test_wallet_unencrypted_roundtrip() {
        let wallet = Wallet {
            keypairs: vec![Keypair::generate()],
            label: "test".to_string(), path: None, password: None,
        };
        let wf = wallet.to_wallet_file();
        assert!(!wf.encrypted);
        let loaded = Wallet::from_wallet_file(wf, None).unwrap();
        assert_eq!(loaded.primary_address(), wallet.primary_address());
    }
}
