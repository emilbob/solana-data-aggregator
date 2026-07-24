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

/// Collects the monitored accounts from `SOLANA_PUBLIC_KEYS` (comma-separated)
/// and/or the single `SOLANA_PUBLIC_KEY`, de-duplicated and order-preserving.
fn collect_accounts(single: Option<String>, multi: Option<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim();
        if !s.is_empty() && !out.iter().any(|e| e == s) {
            out.push(s.to_string());
        }
    };
    if let Some(m) = multi {
        for k in m.split(',') {
            push(k);
        }
    }
    if let Some(s) = single {
        push(&s);
    }
    out
}

/// Derives a WebSocket URL from an RPC URL (`https`→`wss`, `http`→`ws`).
fn derive_ws_url(rpc_url: &str) -> String {
    if let Some(rest) = rpc_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = rpc_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        rpc_url.to_string()
    }
}

#[tokio::main]
async fn main() {
    // Initialize the logger from environment variables, defaulting to "info" level
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    // Load environment variables from a .env file, if present
    dotenv().ok();

    // Process start, for the uptime metric.
    let started = std::time::Instant::now();

    // Retrieve the RPC URL from the environment.
    let rpc_url = require_env("SOLANA_RPC_URL");

    // Monitored accounts: SOLANA_PUBLIC_KEYS (comma-separated) and/or the single
    // SOLANA_PUBLIC_KEY. At least one is required.
    let pub_keys = collect_accounts(
        env::var("SOLANA_PUBLIC_KEY").ok(),
        env::var("SOLANA_PUBLIC_KEYS").ok(),
    );
    if pub_keys.is_empty() {
        eprintln!("error: set SOLANA_PUBLIC_KEY or SOLANA_PUBLIC_KEYS (see README / .env)");
        std::process::exit(1);
    }
    info!(
        "Monitoring {} account(s): {}",
        pub_keys.len(),
        pub_keys.join(", ")
    );

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

    // Optional real-time ingestion: subscribe to each account's transaction logs
    // over WebSocket, in addition to polling (which stays on as a backfill safety
    // net). Enabled by WEBSOCKET=true or by setting SOLANA_WS_URL. The WS URL is
    // derived from SOLANA_RPC_URL unless SOLANA_WS_URL is given.
    let ws_enabled = env::var("SOLANA_WS_URL").is_ok()
        || matches!(
            env::var("WEBSOCKET").ok().as_deref(),
            Some("true") | Some("1")
        );
    if ws_enabled {
        let ws_url = env::var("SOLANA_WS_URL").unwrap_or_else(|_| derive_ws_url(&rpc_url));
        info!("Real-time WebSocket ingestion enabled: {ws_url}");
        for account in &pub_keys {
            let agg = aggregator.clone();
            let url = ws_url.clone();
            let acct = account.clone();
            tokio::spawn(async move { agg.subscribe_account(url, acct).await });
        }
    }

    // Refresh callback for /refresh endpoint — refreshes every monitored account.
    let aggregator_clone = aggregator.clone();
    let refresh_keys = pub_keys.clone();
    let refresh_callback: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let aggregator = aggregator_clone.clone();
        let keys = refresh_keys.clone();
        tokio::spawn(async move {
            for key in &keys {
                let _ = aggregator.fetch_recent_transactions(key).await;
            }
        });
    });

    info!("Starting Solana Data Aggregator...");

    // Set up a one-shot channel for shutdown signaling
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Create the API and bind it to the configured address (SERVER_ADDR,
    // default 127.0.0.1:3030).
    let api = create_api(
        db.clone(),
        rpc_url.clone(),
        pub_keys.clone(),
        started,
        refresh_callback,
    );
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

    // Task to periodically fetch recent transactions for every monitored account.
    let poll_keys = pub_keys.clone();
    let fetch_task = tokio::spawn(async move {
        loop {
            for key in &poll_keys {
                match aggregator.fetch_recent_transactions(key).await {
                    Ok(transactions) => {
                        info!(
                            "Fetched {} new transactions for {}",
                            transactions.len(),
                            key
                        )
                    }
                    Err(err) => error!("Error fetching transactions for {}: {:?}", key, err),
                }
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

#[cfg(test)]
mod tests {
    use super::{collect_accounts, derive_ws_url};

    #[test]
    fn derive_ws_url_maps_schemes() {
        assert_eq!(
            derive_ws_url("https://api.testnet.solana.com"),
            "wss://api.testnet.solana.com"
        );
        assert_eq!(
            derive_ws_url("http://localhost:8899"),
            "ws://localhost:8899"
        );
        // Unknown scheme is passed through unchanged.
        assert_eq!(derive_ws_url("wss://x.example"), "wss://x.example");
    }

    #[test]
    fn collect_accounts_merges_dedups_and_trims() {
        // Multi (comma) first, then single appended; trimmed, blanks dropped,
        // duplicates removed, order preserved.
        assert_eq!(
            collect_accounts(Some(" A ".to_string()), Some("B, C ,B,".to_string())),
            vec!["B", "C", "A"]
        );
        assert_eq!(collect_accounts(Some("X".to_string()), None), vec!["X"]);
        assert_eq!(
            collect_accounts(None, Some("X,Y".to_string())),
            vec!["X", "Y"]
        );
        assert!(collect_accounts(None, None).is_empty());
        // De-dup across single + multi.
        assert_eq!(
            collect_accounts(Some("A".to_string()), Some("A".to_string())),
            vec!["A"]
        );
    }
}
