use log::warn;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use tokio::sync::Mutex;

/// Represents a transaction on the Solana blockchain.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct TransactionData {
    pub signature: String, // Signature of the transaction
    pub sender: String,    // Public key of the sender
    pub receiver: String,  // Public key of the receiver
    pub amount: u64,       // Amount transferred in the transaction
    pub timestamp: u64,    // Timestamp of the transaction
}

/// On-disk persistence record. Persisting the index key alongside the
/// transaction keeps the reloaded in-memory keying identical to the keying used
/// at runtime (previously reload re-keyed by `sender`, diverging from the
/// monitored-address key used by `add_transaction`).
#[derive(Debug, Serialize, Deserialize, Clone)]
struct PersistedTx {
    key: String,
    tx: TransactionData,
}

/// An in-memory database that stores transaction data, with persistence capabilities.
#[derive(Debug, Default)]
pub struct InMemoryDatabase {
    transactions: Mutex<HashMap<String, Vec<TransactionData>>>, // Stores transactions by public key
    file_path: String, // File path for persisting transactions
}

impl InMemoryDatabase {
    /// Retrieves a transaction by its signature.
    pub async fn get_transaction_by_signature(&self, signature: &str) -> Option<TransactionData> {
        let transactions = self.transactions.lock().await;
        for txs in transactions.values() {
            if let Some(tx) = txs.iter().find(|t| t.signature == signature) {
                return Some(tx.clone());
            }
        }
        None
    }
    /// Creates a new `InMemoryDatabase` instance with the specified file path for persistence.
    ///
    /// # Arguments
    ///
    /// * `file_path` - A string representing the file path where transactions will be persisted.
    ///
    /// # Returns
    ///
    /// A new instance of `InMemoryDatabase`.
    pub fn new(file_path: String) -> Self {
        Self {
            transactions: Mutex::new(HashMap::new()),
            file_path,
        }
    }

    /// Adds a new transaction to the in-memory database and saves it to a file.
    ///
    /// Idempotent by signature: re-adding a transaction already stored under
    /// `pub_key` is a no-op, so re-fetching the same transactions (every poll
    /// cycle, and across restarts) neither duplicates in memory nor grows the
    /// persistence file without bound. A non-fatal I/O error is logged, not
    /// panicked — a disk hiccup must not take down the fetch loop.
    ///
    /// # Arguments
    ///
    /// * `pub_key` - The public key this transaction is indexed under (the monitored account).
    /// * `transaction` - The transaction data to be added.
    pub async fn add_transaction(&self, pub_key: &str, transaction: TransactionData) {
        let mut transactions = self.transactions.lock().await;
        let entry = transactions.entry(pub_key.to_string()).or_default();
        if entry.iter().any(|t| t.signature == transaction.signature) {
            return; // already stored — skip both the in-memory push and the file append
        }
        entry.push(transaction.clone());
        drop(transactions); // release the lock before touching the filesystem

        if let Err(e) = self.append_to_file(pub_key, &transaction) {
            warn!(
                "Failed to persist transaction {}: {}",
                transaction.signature, e
            );
        }
    }

    /// Appends a single record to the persistence file. Returns any I/O or
    /// serialization error to the caller rather than panicking.
    fn append_to_file(&self, key: &str, tx: &TransactionData) -> std::io::Result<()> {
        let record = PersistedTx {
            key: key.to_string(),
            tx: tx.clone(),
        };
        let serialized = serde_json::to_string(&record)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)?;
        writeln!(file, "{}", serialized)
    }

    /// Loads transactions from the persistence file into the in-memory database.
    ///
    /// Each line is a JSON [`PersistedTx`] record, so transactions are restored
    /// under the same key they were stored with. Duplicates (by signature) are
    /// skipped, so a file that accumulated repeats from an older build still
    /// loads cleanly.
    pub async fn load_from_file(&self) {
        if !Path::new(&self.file_path).exists() {
            return;
        }
        let file = match File::open(&self.file_path) {
            Ok(f) => f,
            Err(e) => {
                warn!("Unable to open persistence file {}: {}", self.file_path, e);
                return;
            }
        };
        let reader = BufReader::new(file);
        let mut transactions = self.transactions.lock().await;

        for line in reader.lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<PersistedTx>(&line) {
                Ok(record) => {
                    let entry = transactions.entry(record.key).or_default();
                    if !entry.iter().any(|t| t.signature == record.tx.signature) {
                        entry.push(record.tx);
                    }
                }
                Err(e) => warn!("Skipping malformed persistence line: {}", e),
            }
        }
    }

    /// Retrieves all transactions associated with a given public key.
    ///
    /// # Arguments
    ///
    /// * `pub_key` - The public key to fetch transactions for.
    ///
    /// # Returns
    ///
    /// A vector of `TransactionData` associated with the public key. Returns an empty
    /// vector if no transactions are found.
    pub async fn get_transactions(&self, pub_key: &str) -> Vec<TransactionData> {
        let transactions = self.transactions.lock().await;
        transactions.get(pub_key).cloned().unwrap_or_else(Vec::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Test to verify that a transaction can be added to the database and retrieved.
    #[tokio::test]
    async fn test_add_and_get_transaction() {
        let db = Arc::new(InMemoryDatabase::new("test_transactions.txt".to_string()));

        let transaction = TransactionData {
            signature: "test_sig".to_string(),
            sender: "sender1".to_string(),
            receiver: "receiver1".to_string(),
            amount: 100,
            timestamp: 1628500000,
        };

        db.add_transaction("sender1", transaction.clone()).await;

        let transactions = db.get_transactions("sender1").await;
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0], transaction);
    }

    /// Round-trips a transaction through the persistence file: a transaction
    /// added under a key must reload under that *same* key (not re-keyed by
    /// sender), preserving runtime keying across restarts.
    #[tokio::test]
    async fn test_persist_and_reload_round_trip() {
        let path = "persistence_test_transactions.txt";
        std::fs::write(path, "").expect("Failed to clear file");

        let transaction = TransactionData {
            signature: "persist_test_sig".to_string(),
            sender: "persist_sender".to_string(),
            receiver: "persist_receiver".to_string(),
            amount: 600,
            timestamp: 1628500000,
        };

        // First instance writes the transaction under the monitored-account key.
        let db1 = Arc::new(InMemoryDatabase::new(path.to_string()));
        db1.add_transaction("monitored_account", transaction.clone())
            .await;

        // A fresh instance loads it back under the same key.
        let db2 = Arc::new(InMemoryDatabase::new(path.to_string()));
        db2.load_from_file().await;

        let transactions = db2.get_transactions("monitored_account").await;
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0], transaction);
        // Not re-keyed by sender:
        assert!(db2.get_transactions("persist_sender").await.is_empty());
    }

    /// Re-adding the same signature (per poll cycle / across restarts) must not
    /// duplicate it in memory or grow the persistence file.
    #[tokio::test]
    async fn test_add_transaction_is_idempotent() {
        let path = "idempotent_test_transactions.txt";
        std::fs::write(path, "").expect("Failed to clear file");

        let db = Arc::new(InMemoryDatabase::new(path.to_string()));
        let transaction = TransactionData {
            signature: "dup_sig".to_string(),
            sender: "s".to_string(),
            receiver: "r".to_string(),
            amount: 1,
            timestamp: 1628500000,
        };

        db.add_transaction("acct", transaction.clone()).await;
        db.add_transaction("acct", transaction.clone()).await;
        db.add_transaction("acct", transaction.clone()).await;

        assert_eq!(db.get_transactions("acct").await.len(), 1);
        let line_count = std::fs::read_to_string(path).unwrap().lines().count();
        assert_eq!(line_count, 1, "duplicate adds must not grow the file");
    }
}
