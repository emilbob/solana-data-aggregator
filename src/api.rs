use crate::aggregator::{Aggregator, AggregatorError}; // Import AggregatorError
use log::{error, info};
use std::sync::Arc;
use tokio::sync::Mutex;
use warp::http::StatusCode;
use warp::Filter;
use warp::Reply; // Import log

pub fn create_api(
    aggregator: Arc<Mutex<Aggregator>>,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    let aggregator_filter = warp::any().map(move || aggregator.clone());

    // Define the route to fetch recent transactions for a given public key
    warp::path!("transactions" / String)
        .and(aggregator_filter)
        .and_then(handle_get_transactions)
}

async fn handle_get_transactions(
    pub_key: String,
    aggregator: Arc<Mutex<Aggregator>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    info!("Received request for public key: {}", pub_key);

    let locked_aggregator = aggregator.lock().await;

    match locked_aggregator.fetch_recent_transactions(&pub_key).await {
        Ok(transactions) => {
            let limited_transactions = &transactions[..std::cmp::min(5, transactions.len())];
            info!(
                "Returning {} transactions for public key: {}",
                limited_transactions.len(),
                pub_key
            );
            Ok(warp::reply::json(&limited_transactions).into_response())
        }
        Err(AggregatorError::Timeout) => {
            error!(
                "Timeout occurred while fetching transactions for public key: {}",
                pub_key
            );
            let error_message = warp::reply::json(&serde_json::json!({
                "error": "Timeout fetching transactions",
                "details": "The request took too long to complete."
            }));
            Ok(
                warp::reply::with_status(error_message, StatusCode::REQUEST_TIMEOUT)
                    .into_response(),
            )
        }
        Err(e) => {
            error!(
                "Error fetching transactions for public key: {}: {:?}",
                pub_key, e
            );
            let error_message = warp::reply::json(&serde_json::json!({
                "error": "Failed to fetch transactions",
                "details": format!("{:?}", e)
            }));
            Ok(
                warp::reply::with_status(error_message, StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response(),
            )
        }
    }
}
