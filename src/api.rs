use crate::db::InMemoryDatabase;
use chrono::{NaiveDate, TimeZone, Utc};
use log::{error, info};
use serde::Deserialize;
use std::sync::Arc;
use warp::http::StatusCode;
use warp::Filter;
use warp::Reply;

// Define query parameters structure
#[derive(Debug, Deserialize)]
pub struct TransactionQueryParams {
    pub pub_key: String,
    pub day: Option<String>,   // Date in "dd/mm/yyyy" format
    pub limit: Option<usize>,  // Number of transactions to return
    pub offset: Option<usize>, // Pagination offset
}

// Create the API with enhanced querying capabilities
pub fn create_api(
    db: Arc<InMemoryDatabase>,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    let db_filter = warp::any().map(move || db.clone());

    warp::path("transactions")
        .and(warp::query::<TransactionQueryParams>()) // Parse query parameters
        .and(db_filter)
        .and_then(handle_get_transactions)
}

// API handler function
async fn handle_get_transactions(
    params: TransactionQueryParams,
    db: Arc<InMemoryDatabase>,
) -> Result<impl warp::Reply, warp::Rejection> {
    info!("Received request for public key: {}", params.pub_key);

    let transactions = db.get_transactions(&params.pub_key).await;

    // Filter by date if provided
    let filtered_transactions = if let Some(ref day) = params.day {
        if let Ok(date_filter) = parse_date(day) {
            transactions
                .into_iter()
                .filter(|tx| is_same_day(tx.timestamp, date_filter))
                .collect()
        } else {
            error!("Invalid date format: {}", day);
            let error_message = warp::reply::json(&serde_json::json!({
                "error": "Invalid date format",
                "details": "Please use the format dd/mm/yyyy."
            }));
            return Ok(
                warp::reply::with_status(error_message, StatusCode::BAD_REQUEST).into_response(),
            );
        }
    } else {
        transactions
    };

    // Apply pagination if limit and/or offset are provided
    let total = filtered_transactions.len();
    let limit = params.limit.unwrap_or(5);
    let offset = params.offset.unwrap_or(0);
    let limited_transactions = filtered_transactions
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();

    info!(
        "Returning {} transactions (total: {}) for public key: {}",
        limited_transactions.len(),
        total,
        params.pub_key
    );
    Ok(warp::reply::json(&limited_transactions).into_response())
}

// Helper function to parse a date string
fn parse_date(date_str: &str) -> Result<NaiveDate, chrono::format::ParseError> {
    NaiveDate::parse_from_str(date_str, "%d/%m/%Y")
}

// Helper function to check if a transaction's timestamp matches a specific day
fn is_same_day(timestamp: u64, date: NaiveDate) -> bool {
    let datetime = Utc.timestamp_opt(timestamp as i64, 0).single();
    if let Some(transaction_date) = datetime {
        transaction_date.date_naive() == date
    } else {
        false
    }
}
