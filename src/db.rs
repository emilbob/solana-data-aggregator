use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use tokio::sync::Mutex;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TransactionData {
    pub signature: String,
    pub sender: String,
    pub receiver: String,
    pub amount: u64,
    pub timestamp: u64,
}

#[derive(Debug, Default)]
pub struct InMemoryDatabase {
    transactions: Mutex<HashMap<String, Vec<TransactionData>>>,
    file_path: String, // File path for persistence
}

impl InMemoryDatabase {
    pub fn new(file_path: String) -> Self {
        Self {
            transactions: Mutex::new(HashMap::new()),
            file_path,
        }
    }

    // Add a new transaction and save it to a file
    pub async fn add_transaction(&self, pub_key: &str, transaction: TransactionData) {
        let mut transactions = self.transactions.lock().await;
        transactions
            .entry(pub_key.to_string())
            .or_insert_with(Vec::new)
            .push(transaction.clone());

        // Append the transaction to the text file
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)
            .expect("Unable to open file");

        let serialized_transaction =
            serde_json::to_string(&transaction).expect("Failed to serialize transaction");

        writeln!(file, "{}", serialized_transaction).expect("Unable to write to file");
    }

    // Load transactions from the text file
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

    // Get transactions by public key
    pub async fn get_transactions(&self, pub_key: &str) -> Vec<TransactionData> {
        let transactions = self.transactions.lock().await;
        transactions.get(pub_key).cloned().unwrap_or_else(Vec::new)
    }
}
