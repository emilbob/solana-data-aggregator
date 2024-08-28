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

#[derive(Debug, Error)]
pub enum AggregatorError {
    #[error("Invalid public key format")]
    InvalidPublicKey,
    #[error("Failed to fetch signatures: {0}")]
    FetchSignaturesError(#[source] solana_client::client_error::ClientError),
    #[error("Failed to fetch transaction details: {0}")]
    FetchTransactionError(#[source] solana_client::client_error::ClientError),
    #[error("Failed to parse signature: {0}")]
    ParseSignatureError(String),

    #[error("Operation timed out")]
    Elapsed(#[from] Elapsed),
}

pub struct Aggregator {
    client: RpcClient,
    db: Arc<InMemoryDatabase>,
}

impl Aggregator {
    pub fn new(url: &str, db: Arc<InMemoryDatabase>) -> Self {
        let client = RpcClient::new(url.to_string());
        Self { client, db }
    }

    // Fetch the start time (Unix timestamp) of the current epoch
    async fn get_epoch_start_time(&self) -> Result<i64, AggregatorError> {
        let epoch_info = self
            .client
            .get_epoch_info()
            .map_err(AggregatorError::FetchTransactionError)?;

        // Get the block production rate to estimate time per slot
        let block_production_time_per_slot = 0.4; // Approx. 0.4 seconds per slot on Solana

        // Calculate the start slot and its timestamp
        let slots_since_epoch_start = epoch_info.slot_index;
        let seconds_since_epoch_start =
            (slots_since_epoch_start as f64 * block_production_time_per_slot) as i64;
        let current_time = self
            .client
            .get_block_time(epoch_info.absolute_slot)
            .map_err(AggregatorError::FetchTransactionError)?;

        Ok(current_time - seconds_since_epoch_start)
    }

    pub async fn fetch_recent_transactions(
        &self,
        address: &str,
    ) -> Result<Vec<TransactionData>, AggregatorError> {
        let timeout_duration = Duration::from_secs(10); // Timeout after 10 seconds

        info!("Starting transaction fetch for address: {}", address);

        // Fetch the start time of the current epoch
        let epoch_start_time = self.get_epoch_start_time().await?;

        let transactions = timeout(timeout_duration, async {
            let pubkey: Pubkey = address
                .parse()
                .map_err(|_| AggregatorError::InvalidPublicKey)?;

            info!("Fetching signatures for address: {}", pubkey);

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
