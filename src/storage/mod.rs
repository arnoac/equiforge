use sled::Db;
use std::path::Path;

use crate::core::types::*;
use crate::core::chain::UtxoEntry;

/// Key prefixes for different data types in sled
const PREFIX_BLOCK: &[u8] = b"blk:";
const PREFIX_HEADER: &[u8] = b"hdr:";
const PREFIX_HEIGHT: &[u8] = b"hgt:";
const PREFIX_UTXO: &[u8] = b"utx:";
const META_TIP: &[u8] = b"meta:tip";
const META_HEIGHT: &[u8] = b"meta:height";
const META_TIMESTAMPS: &[u8] = b"meta:timestamps";
const META_FRACTIONAL_DIFF: &[u8] = b"meta:frac_diff";

/// Persistent storage backend using sled embedded database
pub struct Storage {
    db: Db,
}

/// Serializable UTXO entry for storage
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredUtxoEntry {
    pub amount: u64,
    pub pubkey_hash: Hash256,
    pub height: u64,
    pub is_coinbase: bool,
}

impl From<&UtxoEntry> for StoredUtxoEntry {
    fn from(entry: &UtxoEntry) -> Self {
        StoredUtxoEntry {
            amount: entry.output.amount,
            pubkey_hash: entry.output.pubkey_hash,
            height: entry.height,
            is_coinbase: entry.is_coinbase,
        }
    }
}

impl StoredUtxoEntry {
    fn to_utxo_entry(&self) -> UtxoEntry {
        UtxoEntry {
            output: TxOutput {
                amount: self.amount,
                pubkey_hash: self.pubkey_hash,
            },
            height: self.height,
            is_coinbase: self.is_coinbase,
        }
    }
}

impl Storage {
    /// Open or create a database at the given path
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, StorageError> {
        let db = sled::open(path).map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(Storage { db })
    }

    /// Check if the database has existing chain data
    pub fn has_chain_data(&self) -> bool {
        self.db.contains_key(META_TIP).unwrap_or(false)
    }

    // ─── Block Storage ───────────────────────────────────────────────

    /// Store a complete block
    pub fn put_block(&self, hash: &Hash256, block: &Block) -> Result<(), StorageError> {
        let key = prefixed_key(PREFIX_BLOCK, hash);
        let value = bincode::serialize(block)
            .map_err(|e| StorageError::SerializeError(e.to_string()))?;
        self.db.insert(key, value)
            .map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(())
    }

    /// Retrieve a block by hash
    pub fn get_block(&self, hash: &Hash256) -> Result<Option<Block>, StorageError> {
        let key = prefixed_key(PREFIX_BLOCK, hash);
        match self.db.get(key).map_err(|e| StorageError::DbError(e.to_string()))? {
            Some(bytes) => {
                let block = bincode::deserialize(&bytes)
                    .map_err(|e| StorageError::SerializeError(e.to_string()))?;
                Ok(Some(block))
            }
            None => Ok(None),
        }
    }

    /// Store a block header
    pub fn put_header(&self, hash: &Hash256, header: &BlockHeader) -> Result<(), StorageError> {
        let key = prefixed_key(PREFIX_HEADER, hash);
        let value = bincode::serialize(header)
            .map_err(|e| StorageError::SerializeError(e.to_string()))?;
        self.db.insert(key, value)
            .map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(())
    }

    /// Retrieve a header by hash
    pub fn get_header(&self, hash: &Hash256) -> Result<Option<BlockHeader>, StorageError> {
        let key = prefixed_key(PREFIX_HEADER, hash);
        match self.db.get(key).map_err(|e| StorageError::DbError(e.to_string()))? {
            Some(bytes) => {
                let header = bincode::deserialize(&bytes)
                    .map_err(|e| StorageError::SerializeError(e.to_string()))?;
                Ok(Some(header))
            }
            None => Ok(None),
        }
    }

    /// Map height -> block hash
    pub fn put_height_index(&self, height: u64, hash: &Hash256) -> Result<(), StorageError> {
        let key = prefixed_key(PREFIX_HEIGHT, &height.to_be_bytes());
        self.db.insert(key, hash.as_slice())
            .map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(())
    }

    /// Get block hash at a given height
    pub fn get_hash_at_height(&self, height: u64) -> Result<Option<Hash256>, StorageError> {
        let key = prefixed_key(PREFIX_HEIGHT, &height.to_be_bytes());
        match self.db.get(key).map_err(|e| StorageError::DbError(e.to_string()))? {
            Some(bytes) => {
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&bytes);
                Ok(Some(hash))
            }
            None => Ok(None),
        }
    }

    // ─── UTXO Storage ────────────────────────────────────────────────

    /// Store a UTXO
    pub fn put_utxo(&self, outpoint: &OutPoint, entry: &UtxoEntry) -> Result<(), StorageError> {
        let key = utxo_key(outpoint);
        let stored = StoredUtxoEntry::from(entry);
        let value = bincode::serialize(&stored)
            .map_err(|e| StorageError::SerializeError(e.to_string()))?;
        self.db.insert(key, value)
            .map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(())
    }

    /// Remove a UTXO (when spent)
    pub fn remove_utxo(&self, outpoint: &OutPoint) -> Result<(), StorageError> {
        let key = utxo_key(outpoint);
        self.db.remove(key)
            .map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(())
    }

    /// Get a UTXO
    pub fn get_utxo(&self, outpoint: &OutPoint) -> Result<Option<UtxoEntry>, StorageError> {
        let key = utxo_key(outpoint);
        match self.db.get(key).map_err(|e| StorageError::DbError(e.to_string()))? {
            Some(bytes) => {
                let stored: StoredUtxoEntry = bincode::deserialize(&bytes)
                    .map_err(|e| StorageError::SerializeError(e.to_string()))?;
                Ok(Some(stored.to_utxo_entry()))
            }
            None => Ok(None),
        }
    }

    /// Load all UTXOs into memory (for startup)
    pub fn load_all_utxos(&self) -> Result<Vec<(OutPoint, UtxoEntry)>, StorageError> {
        let mut utxos = Vec::new();
        for item in self.db.scan_prefix(PREFIX_UTXO) {
            let (key, value) = item.map_err(|e| StorageError::DbError(e.to_string()))?;
            let outpoint = outpoint_from_utxo_key(&key)?;
            let stored: StoredUtxoEntry = bincode::deserialize(&value)
                .map_err(|e| StorageError::SerializeError(e.to_string()))?;
            utxos.push((outpoint, stored.to_utxo_entry()));
        }
        Ok(utxos)
    }

    // ─── Chain Metadata ──────────────────────────────────────────────

    /// Store the chain tip hash
    pub fn put_tip(&self, hash: &Hash256) -> Result<(), StorageError> {
        self.db.insert(META_TIP, hash.as_slice())
            .map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(())
    }

    /// Get the chain tip hash
    pub fn get_tip(&self) -> Result<Option<Hash256>, StorageError> {
        match self.db.get(META_TIP).map_err(|e| StorageError::DbError(e.to_string()))? {
            Some(bytes) => {
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&bytes);
                Ok(Some(hash))
            }
            None => Ok(None),
        }
    }

    /// Store the chain height
    pub fn put_height(&self, height: u64) -> Result<(), StorageError> {
        self.db.insert(META_HEIGHT, &height.to_le_bytes())
            .map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(())
    }

    /// Get the chain height
    pub fn get_height(&self) -> Result<Option<u64>, StorageError> {
        match self.db.get(META_HEIGHT).map_err(|e| StorageError::DbError(e.to_string()))? {
            Some(bytes) => {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&bytes);
                Ok(Some(u64::from_le_bytes(buf)))
            }
            None => Ok(None),
        }
    }

    /// Store recent timestamps for LWMA difficulty
    pub fn put_timestamps(&self, timestamps: &[u64]) -> Result<(), StorageError> {
        let value = bincode::serialize(timestamps)
            .map_err(|e| StorageError::SerializeError(e.to_string()))?;
        self.db.insert(META_TIMESTAMPS, value)
            .map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(())
    }

    /// Load recent timestamps
    pub fn get_timestamps(&self) -> Result<Option<Vec<u64>>, StorageError> {
        match self.db.get(META_TIMESTAMPS).map_err(|e| StorageError::DbError(e.to_string()))? {
            Some(bytes) => {
                let timestamps: Vec<u64> = bincode::deserialize(&bytes)
                    .map_err(|e| StorageError::SerializeError(e.to_string()))?;
                Ok(Some(timestamps))
            }
            None => Ok(None),
        }
    }

    /// Store fractional difficulty for smooth LWMA
    pub fn put_fractional_difficulty(&self, frac: f64) -> Result<(), StorageError> {
        self.db.insert(META_FRACTIONAL_DIFF, &frac.to_le_bytes())
            .map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(())
    }

    /// Load fractional difficulty
    pub fn get_fractional_difficulty(&self) -> Result<Option<f64>, StorageError> {
        match self.db.get(META_FRACTIONAL_DIFF).map_err(|e| StorageError::DbError(e.to_string()))? {
            Some(bytes) => {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&bytes);
                Ok(Some(f64::from_le_bytes(buf)))
            }
            None => Ok(None),
        }
    }

    /// Flush all pending writes to disk
    pub fn flush(&self) -> Result<(), StorageError> {
        self.db.flush().map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(())
    }

    /// Clear all data from the database (used during auto-recovery)
    pub fn clear_all(&self) -> Result<(), StorageError> {
        self.db.clear().map_err(|e| StorageError::DbError(e.to_string()))?;
        self.db.flush().map_err(|e| StorageError::DbError(e.to_string()))?;
        Ok(())
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────

fn prefixed_key(prefix: &[u8], data: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + data.len());
    key.extend_from_slice(prefix);
    key.extend_from_slice(data);
    key
}

fn utxo_key(outpoint: &OutPoint) -> Vec<u8> {
    // utx:<txid(32)><vout(4)>
    let mut key = Vec::with_capacity(PREFIX_UTXO.len() + 36);
    key.extend_from_slice(PREFIX_UTXO);
    key.extend_from_slice(&outpoint.txid);
    key.extend_from_slice(&outpoint.vout.to_be_bytes());
    key
}

fn outpoint_from_utxo_key(key: &[u8]) -> Result<OutPoint, StorageError> {
    if key.len() != PREFIX_UTXO.len() + 36 {
        return Err(StorageError::SerializeError("invalid UTXO key length".into()));
    }
    let data = &key[PREFIX_UTXO.len()..];
    let mut txid = [0u8; 32];
    txid.copy_from_slice(&data[0..32]);
    let vout = u32::from_be_bytes(data[32..36].try_into().unwrap());
    Ok(OutPoint { txid, vout })
}

#[derive(Debug)]
pub enum StorageError {
    DbError(String),
    SerializeError(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::DbError(e) => write!(f, "database error: {}", e),
            StorageError::SerializeError(e) => write!(f, "serialization error: {}", e),
        }
    }
}

impl std::error::Error for StorageError {}
