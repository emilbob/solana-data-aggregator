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

/// An in-memory database that stores transaction data, with persistence capabilities.
#[derive(Debug, Default)]
pub struct InMemoryDatabase {
    transactions: Mutex<HashMap<String, Vec<TransactionData>>>, // Stores transactions by public key
    file_path: String, // File path for persisting transactions
}

impl InMemoryDatabase {
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
    /// # Arguments
    ///
    /// * `pub_key` - The public key of the sender or receiver to associate with this transaction.
    /// * `transaction` - The transaction data to be added.
    pub async fn add_transaction(&self, pub_key: &str, transaction: TransactionData) {
        let mut transactions = self.transactions.lock().await;
        transactions
            .entry(pub_key.to_string())
            .or_insert_with(Vec::new)
            .push(transaction.clone());

        // Append the transaction to the text file for persistence
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)
            .expect("Unable to open file");

        let serialized_transaction =
            serde_json::to_string(&transaction).expect("Failed to serialize transaction");

        writeln!(file, "{}", serialized_transaction).expect("Unable to write to file");
    }

    /// Loads transactions from a text file into the in-memory database.
    ///
    /// This method reads each line from the specified file and attempts to deserialize
    /// it into a `TransactionData` struct. If successful, the transaction is added to
    /// the in-memory database under the corresponding sender's public key.
    pub async fn load_from_file(&self) {
        if Path::new(&self.file_path).exists() {
            let file = File::open(&self.file_path).expect("Unable to open file");
            let reader = BufReader::new(file);
            let mut transactions = self.transactions.lock().await;

            for line in reader.lines() {
                if let Ok(transaction_str) = line {
                    if let Ok(transaction) =
                        serde_json::from_str::<TransactionData>(&transaction_str)
                    {
                        transactions
                            .entry(transaction.sender.clone())
                            .or_insert_with(Vec::new)
                            .push(transaction);
                    }
                }
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

    /// Test to verify that transactions can be loaded from a file into the in-memory database.
    #[tokio::test]
    async fn test_load_from_file() {
        // Ensure the file is clean before the test
        std::fs::write("persistence_test_transactions.txt", "").expect("Failed to clear file");

        let db = Arc::new(InMemoryDatabase::new(
            "persistence_test_transactions.txt".to_string(),
        ));

        let transaction = TransactionData {
            signature: "persist_test_sig".to_string(),
            sender: "persist_sender".to_string(),
            receiver: "persist_receiver".to_string(),
            amount: 600,
            timestamp: 1628500000,
        };

        // Write transaction directly to file
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("persistence_test_transactions.txt")
            .expect("Unable to open file");
        writeln!(file, "{}", serde_json::to_string(&transaction).unwrap())
            .expect("Unable to write to file");

        // Load from file
        db.load_from_file().await;

        // Check in-memory data
        let transactions = db.get_transactions("persist_sender").await;
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0], transaction);
    }
}
