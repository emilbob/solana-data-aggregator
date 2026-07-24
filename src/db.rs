use log::warn;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// A decoded native-SOL transfer extracted from a transaction's parsed System
/// Program instruction. `None` on records whose transaction isn't a simple SOL
/// transfer (votes, token ops, program calls, …) — so `lamports` is the real
/// transferred amount, not a balance-delta guess.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Transfer {
    pub source: String,
    pub destination: String,
    pub lamports: u64,
}

/// A decoded Solana transaction, enriched with the fields the RPC response
/// actually carries rather than a transfer-biased guess.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct TransactionData {
    pub signature: String,          // Transaction signature
    pub slot: u64,                  // Slot the transaction landed in
    pub timestamp: u64,             // Block time (unix seconds)
    pub fee: u64,                   // Fee paid, in lamports
    pub fee_payer: String,          // The account that paid the fee (first signer)
    pub success: bool,              // Whether the transaction succeeded (meta.err is None)
    pub tx_type: String, // Classified kind: "transfer" | "vote" | "token" | program | "unknown"
    pub programs: Vec<String>, // Programs the transaction invoked
    pub transfer: Option<Transfer>, // Present only when this is a native SOL transfer
}

impl TransactionData {
    /// A compact, human-readable one-line summary for logging — so the terminal
    /// narrates *what* was ingested, not just how many.
    pub fn summary(&self) -> String {
        let status = if self.success { "ok" } else { "FAIL" };
        let mut s = format!(
            "{:<8} {:<4} fee={:<6} payer={} slot={} sig={}",
            self.tx_type,
            status,
            self.fee,
            short(&self.fee_payer),
            self.slot,
            short(&self.signature),
        );
        if let Some(t) = &self.transfer {
            s.push_str(&format!(
                " {:.9} SOL {}→{}",
                t.lamports as f64 / 1e9,
                short(&t.source),
                short(&t.destination),
            ));
        }
        s
    }
}

/// Truncates a base58 key/signature (ASCII) to a compact prefix for logging.
fn short(s: &str) -> String {
    let n = s.len().min(8);
    if s.len() > n {
        format!("{}…", &s[..n])
    } else {
        s.to_string()
    }
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

        if let Err(e) = self.append_to_file(pub_key, &transaction).await {
            warn!(
                "Failed to persist transaction {}: {}",
                transaction.signature, e
            );
        }
    }

    /// Appends a single record to the persistence file without blocking the
    /// async runtime. Returns any I/O or serialization error to the caller
    /// rather than panicking.
    async fn append_to_file(&self, key: &str, tx: &TransactionData) -> std::io::Result<()> {
        let record = PersistedTx {
            key: key.to_string(),
            tx: tx.clone(),
        };
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        // `tokio::fs::File` buffers internally and only flushes on drop
        // (asynchronously, not awaited), so without this the write can be
        // invisible to a subsequent read — i.e. `add_transaction` could return
        // before the transaction is actually persisted.
        file.flush().await
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

    /// Minimal sample record for tests that only care about storage/persistence.
    pub(crate) fn sample_tx(signature: &str) -> TransactionData {
        TransactionData {
            signature: signature.to_string(),
            slot: 100,
            timestamp: 1628500000,
            fee: 5000,
            fee_payer: "payer".to_string(),
            success: true,
            tx_type: "transfer".to_string(),
            programs: vec!["system".to_string()],
            transfer: Some(Transfer {
                source: "payer".to_string(),
                destination: "dest".to_string(),
                lamports: 100,
            }),
        }
    }

    #[test]
    fn test_summary_transfer_and_non_transfer() {
        // Transfer record includes the SOL amount and source→destination.
        let tx = sample_tx("abcdefghXXXXXXXX");
        let s = tx.summary();
        assert!(s.contains("transfer"), "{s}");
        assert!(s.contains("SOL") && s.contains('→'), "{s}");
        assert!(s.contains("sig=abcdefgh…"), "{s}");

        // Non-transfer (e.g. vote) omits the SOL clause.
        let mut vote = sample_tx("votesig0000");
        vote.tx_type = "vote".to_string();
        vote.transfer = None;
        let vs = vote.summary();
        assert!(vs.contains("vote"), "{vs}");
        assert!(!vs.contains("SOL"), "{vs}");
    }

    /// Test to verify that a transaction can be added to the database and retrieved.
    #[tokio::test]
    async fn test_add_and_get_transaction() {
        let db = Arc::new(InMemoryDatabase::new("test_transactions.txt".to_string()));

        let transaction = sample_tx("test_sig");

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

        let transaction = sample_tx("persist_test_sig");

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
        // Reloaded under the monitored-account key, not re-keyed by an inner
        // field such as the fee payer:
        assert!(db2.get_transactions("payer").await.is_empty());
    }

    /// Re-adding the same signature (per poll cycle / across restarts) must not
    /// duplicate it in memory or grow the persistence file.
    #[tokio::test]
    async fn test_add_transaction_is_idempotent() {
        let path = "idempotent_test_transactions.txt";
        std::fs::write(path, "").expect("Failed to clear file");

        let db = Arc::new(InMemoryDatabase::new(path.to_string()));
        let transaction = sample_tx("dup_sig");

        db.add_transaction("acct", transaction.clone()).await;
        db.add_transaction("acct", transaction.clone()).await;
        db.add_transaction("acct", transaction.clone()).await;

        assert_eq!(db.get_transactions("acct").await.len(), 1);
        let content = std::fs::read_to_string(path).unwrap();
        let line_count = content.lines().count();
        assert_eq!(
            line_count, 1,
            "duplicate adds must not grow the file; got {line_count} lines: {content:?}"
        );
    }
}
