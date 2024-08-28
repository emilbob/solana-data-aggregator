use crate::db::{InMemoryDatabase, TransactionData};
use log::{error, info};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::{
    EncodedTransaction, UiMessage, UiTransaction, UiTransactionEncoding,
};
use std::sync::Arc;
use thiserror::Error;
use tokio::time::{error::Elapsed, timeout, Duration};

/// Custom error type for the `Aggregator` struct, encapsulating various errors
/// that can occur while interacting with the Solana blockchain.
#[derive(Debug, Error)]
pub enum AggregatorError {
    /// Indicates an invalid public key format.
    #[error("Invalid public key format")]
    InvalidPublicKey,

    /// Error that occurs when fetching signatures from the Solana blockchain.
    #[error("Failed to fetch signatures: {0}")]
    FetchSignaturesError(#[source] solana_client::client_error::ClientError),

    /// Error that occurs when fetching transaction details from the Solana blockchain.
    #[error("Failed to fetch transaction details: {0}")]
    FetchTransactionError(#[source] solana_client::client_error::ClientError),

    /// Error that occurs when parsing a transaction signature.
    #[error("Failed to parse signature: {0}")]
    ParseSignatureError(String),

    /// Indicates that an operation has timed out.
    #[error("Operation timed out")]
    Elapsed(#[from] Elapsed),
}

/// Struct that handles fetching transactions from the Solana blockchain and storing
/// them in an in-memory database.
pub struct Aggregator {
    client: RpcClient,         // Solana RPC client used to interact with the blockchain
    db: Arc<InMemoryDatabase>, // In-memory database for storing transactions
}

impl Aggregator {
    /// Creates a new `Aggregator` instance with the specified Solana RPC URL and
    /// in-memory database.
    ///
    /// # Arguments
    ///
    /// * `url` - A string slice representing the URL of the Solana RPC endpoint.
    /// * `db` - A thread-safe reference to an `InMemoryDatabase` instance.
    ///
    /// # Returns
    ///
    /// A new instance of `Aggregator`.
    pub fn new(url: &str, db: Arc<InMemoryDatabase>) -> Self {
        let client = RpcClient::new(url.to_string());
        Self { client, db }
    }

    /// Fetches the start time (Unix timestamp) of the current Solana epoch.
    ///
    /// # Returns
    ///
    /// A result containing the epoch start time in seconds since Unix epoch, or an
    /// `AggregatorError` if an error occurs.
    async fn get_epoch_start_time(&self) -> Result<i64, AggregatorError> {
        let epoch_info = self
            .client
            .get_epoch_info()
            .map_err(AggregatorError::FetchTransactionError)?;

        // Approximate time per Solana slot (in seconds)
        let block_production_time_per_slot = 0.4;

        // Calculate the start slot and its corresponding timestamp
        let slots_since_epoch_start = epoch_info.slot_index;
        let seconds_since_epoch_start =
            (slots_since_epoch_start as f64 * block_production_time_per_slot) as i64;
        let current_time = self
            .client
            .get_block_time(epoch_info.absolute_slot)
            .map_err(AggregatorError::FetchTransactionError)?;

        Ok(current_time - seconds_since_epoch_start)
    }

    /// Fetches recent transactions for the specified Solana address and stores
    /// them in the in-memory database.
    ///
    /// # Arguments
    ///
    /// * `address` - A string slice representing the Solana public key of the account.
    ///
    /// # Returns
    ///
    /// A result containing a vector of `TransactionData` if successful, or an `AggregatorError` if an error occurs.
    pub async fn fetch_recent_transactions(
        &self,
        address: &str,
    ) -> Result<Vec<TransactionData>, AggregatorError> {
        let timeout_duration = Duration::from_secs(10); // Set a timeout duration of 10 seconds

        info!("Starting transaction fetch for address: {}", address);

        // Fetch the start time of the current epoch
        let epoch_start_time = self.get_epoch_start_time().await?;

        let transactions = timeout(timeout_duration, async {
            let pubkey: Pubkey = address
                .parse()
                .map_err(|_| AggregatorError::InvalidPublicKey)?;

            info!("Fetching signatures for address: {}", pubkey);

            // Fetch the signatures of recent transactions for the specified address
            let signatures = self
                .client
                .get_signatures_for_address(&pubkey)
                .map_err(AggregatorError::FetchSignaturesError)?;

            info!(
                "Fetched {} signatures for address: {}",
                signatures.len(),
                pubkey
            );

            let mut transactions = Vec::new();

            // Iterate through each signature and fetch transaction details
            for signature_info in signatures {
                info!("Processing signature: {}", signature_info.signature);

                let signature: Signature = signature_info.signature.parse().map_err(|_| {
                    AggregatorError::ParseSignatureError(signature_info.signature.clone())
                })?;

                if let Ok(transaction_with_meta) = self
                    .client
                    .get_transaction(&signature, UiTransactionEncoding::JsonParsed)
                    .map_err(AggregatorError::FetchTransactionError)
                {
                    if let Some(block_time) = transaction_with_meta.block_time {
                        // Process only transactions from the current epoch
                        if block_time >= epoch_start_time {
                            let timestamp = block_time;
                            if let Some(meta) = &transaction_with_meta.transaction.meta {
                                match &transaction_with_meta.transaction.transaction {
                                    EncodedTransaction::Json(transaction) => {
                                        let UiTransaction { message, .. } = transaction;
                                        let (sender, receiver) = match message {
                                            UiMessage::Parsed(parsed_message) => {
                                                let sender = parsed_message
                                                    .account_keys
                                                    .get(0)
                                                    .map_or("unknown".to_string(), |acc| {
                                                        acc.pubkey.clone()
                                                    });
                                                let receiver = parsed_message
                                                    .account_keys
                                                    .get(1)
                                                    .map_or("unknown".to_string(), |acc| {
                                                        acc.pubkey.clone()
                                                    });
                                                (sender, receiver)
                                            }
                                            UiMessage::Raw(raw_message) => {
                                                let sender = raw_message
                                                    .account_keys
                                                    .get(0)
                                                    .map_or("unknown".to_string(), |key| {
                                                        key.clone()
                                                    });
                                                let receiver = raw_message
                                                    .account_keys
                                                    .get(1)
                                                    .map_or("unknown".to_string(), |key| {
                                                        key.clone()
                                                    });
                                                (sender, receiver)
                                            }
                                        };
                                        let amount = meta.post_balances[1] - meta.pre_balances[1];

                                        let transaction_data = TransactionData {
                                            signature: signature_info.signature.clone(),
                                            sender,
                                            receiver,
                                            amount: amount as u64,
                                            timestamp: timestamp as u64,
                                        };

                                        transactions.push(transaction_data.clone());

                                        // Save each transaction to the in-memory database
                                        self.db.add_transaction(address, transaction_data).await;
                                    }
                                    _ => {}
                                }
                            }
                        } else {
                            info!(
                                "Skipping transaction from previous epoch: {}",
                                signature_info.signature
                            );
                        }
                    }
                }
            }

            Ok::<Vec<TransactionData>, AggregatorError>(transactions)
        })
        .await??;

        info!(
            "Transaction fetch completed successfully for address: {}",
            address
        );

        Ok(transactions)
    }
}

#[cfg(test)]
mod tests {

    use crate::db::{InMemoryDatabase, TransactionData};
    use std::sync::Arc;

    /// Test to verify that the `Aggregator` can add a transaction to the in-memory
    /// database and retrieve it correctly.
    #[tokio::test]
    async fn test_aggregator_add_and_fetch_transaction() {
        // Initialize the in-memory database
        let db = Arc::new(InMemoryDatabase::new("test_transactions.txt".to_string()));

        // Create a mock transaction
        let transaction = TransactionData {
            signature: "test_signature".to_string(),
            sender: "sender1".to_string(),
            receiver: "receiver1".to_string(),
            amount: 100,
            timestamp: 1628500000,
        };

        // Add the transaction to the database
        db.add_transaction("sender1", transaction.clone()).await;

        // Fetch the transactions for the sender
        let transactions = db.get_transactions("sender1").await;

        // Verify that the transaction is correctly stored and retrieved
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0], transaction);
    }
}
