mod aggregator;
mod api;
mod db;
mod store;

use aggregator::Aggregator;
use api::create_api;
use dotenv::dotenv;
use env_logger::Env;
use log::{error, info};
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use store::Store;
use tokio::signal;
use tokio::time::Duration;

/// Reads a required env var, exiting with a clear message instead of panicking.
fn require_env(key: &str) -> String {
    match env::var(key) {
        Ok(val) => val,
        Err(_) => {
            eprintln!("error: {key} must be set (see README / .env)");
            std::process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() {
    // Initialize the logger from environment variables, defaulting to "info" level
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    // Load environment variables from a .env file, if present
    dotenv().ok();

    // Retrieve the RPC URL and public key from environment variables
    let rpc_url = require_env("SOLANA_RPC_URL");
    let pub_key = require_env("SOLANA_PUBLIC_KEY");

    // Max signatures to pull per fetch cycle (optional; default 20). Keeping this
    // bounded is what stops a busy account from blowing the per-cycle timeout.
    let fetch_limit: usize = env::var("FETCH_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    // Per-cycle fetch budget in seconds (optional; default 10). Raise it to grind
    // through a slow or rate-limited RPC.
    let poll_timeout_secs: u64 = env::var("POLL_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    // Select the storage backend: Postgres when DATABASE_URL is set (durable,
    // queryable, multi-run history), otherwise the zero-setup in-memory + file
    // store. The rest of the app is backend-agnostic.
    let store = match env::var("DATABASE_URL") {
        Ok(url) => match Store::connect_postgres(&url).await {
            Ok(s) => {
                info!("Using Postgres store");
                s
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Err(_) => {
            info!("Using in-memory store (set DATABASE_URL to use Postgres)");
            Store::memory("transactions.txt")
        }
    };
    let db = Arc::new(store);

    // Load any persisted data (no-op for Postgres — already durable).
    db.load().await;

    // Initialize the aggregator with the RPC URL and the database reference. It
    // uses interior mutability for its cursor, so no outer Mutex is needed — the
    // poll loop and an on-demand /refresh can run concurrently.
    let aggregator = Arc::new(Aggregator::new(
        &rpc_url,
        db.clone(),
        fetch_limit,
        poll_timeout_secs,
    ));

    // Refresh callback for /refresh endpoint
    let aggregator_clone = aggregator.clone();
    let pub_key_clone = pub_key.clone();
    let refresh_callback: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let aggregator = aggregator_clone.clone();
        let pub_key = pub_key_clone.clone();
        tokio::spawn(async move {
            let _ = aggregator.fetch_recent_transactions(&pub_key).await;
        });
    });

    info!("Starting Solana Data Aggregator...");

    // Set up a one-shot channel for shutdown signaling
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Create the API and bind it to the configured address (SERVER_ADDR,
    // default 127.0.0.1:3030).
    let api = create_api(db.clone(), rpc_url.clone(), refresh_callback);
    let addr: SocketAddr = env::var("SERVER_ADDR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| ([127, 0, 0, 1], 3030).into());

    // Start the Warp server with graceful shutdown wired to the oneshot: when
    // `shutdown_tx` fires, warp stops accepting connections and drains in-flight
    // requests instead of being killed mid-response.
    let mut warp_server_task = tokio::spawn(async move {
        warp::serve(api)
            .bind(addr)
            .await
            .graceful(async move {
                shutdown_rx.await.ok();
            })
            .run()
            .await;
    });
    info!("API listening on http://{}", addr);

    // Task to periodically fetch recent transactions from the Solana blockchain
    let fetch_task = tokio::spawn(async move {
        loop {
            match aggregator.fetch_recent_transactions(&pub_key).await {
                Ok(transactions) => info!("Fetched {} new transactions", transactions.len()),
                Err(err) => error!("Error fetching transactions: {:?}", err),
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });

    let mut fetch_task = Some(fetch_task);

    // Gracefully handle shutdown signals
    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down gracefully...");

            // Abort the fetch task if it is running
            if let Some(task) = fetch_task.take() {
                task.abort();
                info!("Fetch task aborted");
            }

            // Signal the Warp server to drain and stop.
            let _ = shutdown_tx.send(());
            info!("Sent shutdown signal to Warp server");

            // Give in-flight requests up to 5s to drain, then force the issue.
            match tokio::time::timeout(Duration::from_secs(5), &mut warp_server_task).await {
                Ok(_) => info!("Warp server shut down cleanly."),
                Err(_) => {
                    warp_server_task.abort();
                    info!("Warp shutdown timed out after 5s; aborted.");
                }
            }
        },
        _ = &mut warp_server_task => {
            info!("Warp server task completed.");
            if let Some(task) = fetch_task.take() {
                task.abort();
            }
        },
    }

    info!("Shutdown process finished.");
}
