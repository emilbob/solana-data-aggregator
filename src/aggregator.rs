use log::{error, info};
use serde::Serialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::{
    EncodedTransaction, UiMessage, UiTransaction, UiTransactionEncoding,
};
use thiserror::Error;
use tokio::time::{timeout, Duration};

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
    #[error("Timeout fetching transactions")]
    Timeout,
}

#[derive(Debug, Serialize)]
pub struct TransactionData {
    pub signature: String,
    pub sender: String,
    pub receiver: String,
    pub amount: u64,
    pub timestamp: u64,
}

pub struct Aggregator {
    client: RpcClient,
}

impl Aggregator {
    pub fn new(url: &str) -> Self {
        let client = RpcClient::new(url.to_string());
        Self { client }
    }

    pub async fn fetch_recent_transactions(
        &self,
        address: &str,
    ) -> Result<Vec<TransactionData>, AggregatorError> {
        let timeout_duration = Duration::from_secs(10); // Timeout after 10 seconds

        info!("Starting transaction fetch for address: {}", address);

        // Wrap fetching logic with a timeout
        let result = timeout(timeout_duration, async {
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
                    if let Some(meta) = &transaction_with_meta.transaction.meta {
                        info!(
                            "Fetched transaction metadata for signature: {}",
                            signature_info.signature
                        );

                        let timestamp = transaction_with_meta.block_time.unwrap_or(0);
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
                                            .map_or("unknown".to_string(), |key| key.clone());
                                        let receiver = raw_message
                                            .account_keys
                                            .get(1)
                                            .map_or("unknown".to_string(), |key| key.clone());
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

                                transactions.push(transaction_data);
                            }
                            _ => {}
                        }
                    }
                }
            }

            Ok(transactions)
        })
        .await;

        // Handle timeout
        match result {
            Ok(res) => {
                info!(
                    "Transaction fetch completed successfully for address: {}",
                    address
                );
                res
            }
            Err(_) => {
                error!("Transaction fetch timed out for address: {}", address);
                Err(AggregatorError::Timeout)
            }
        }
    }
}
