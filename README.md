# Solana Data Aggregator

The Solana Data Aggregator is a Rust-based tool designed to collect, process, and store transaction data from the Solana blockchain. It supports real-time data retrieval, in-memory storage with persistence capabilities, and provides a RESTful API for querying transaction data.

## Table of Contents

- Overview
- Features
- Installation
  - Prerequisites
  - Clone the Repository
  - Set Up Environment Variables
  - Build the Project
  - Run the Application
- Usage
  - API Endpoints
  - GET /transactions
- Project Structure
- Persistence
- Testing
- Design Decisions
- Future Enhancements
- Contributing
- License
- Acknowledgments

## Overview

The Solana Data Aggregator is a tool that helps developers, researchers, and blockchain enthusiasts monitor and analyze transaction activity on the Solana network. By aggregating transaction data in real-time and providing an easy-to-use API, it simplifies the process of blockchain data analysis.

## Features

- Real-Time Data Retrieval: Continuously fetches the most recent transactions from the Solana blockchain.
  In-Memory Database with Persistence: Stores transactions in memory for fast access, with periodic saving to a text file for persistence across restarts.

- RESTful API: Exposes a simple API for querying transaction data by public key, date, and with pagination.
  Graceful Shutdown: Handles shutdown signals cleanly, ensuring no data is lost.
  Modular and Extensible Design: The codebase is modular, making it easy to extend and customize for different use cases.

## Installation

### Prerequisites

Before you begin, make sure you have the following installed:

Install Rust:

```
rustup
```

Solana RPC URL: Obtain an RPC URL from a provider like Helius or use the official Solana testnet.

### Clone the Repository

First, clone the repository to your local machine:

```
git clone https://github.com/emilbob/solana-data-aggregator
cd solana-data-aggregator
```

### Set Up Environment Variables

Create a .env file in the root of the project directory:

```
SOLANA_RPC_URL=https://api.testnet.solana.com
SOLANA_PUBLIC_KEY=YourPublicKeyHere
# Optional — defaults to 127.0.0.1:3030
SERVER_ADDR=127.0.0.1:3030
# Optional — max signatures pulled per fetch cycle (default 20)
FETCH_LIMIT=20
```

Replace YourPublicKeyHere with the public key you want to monitor.

- `SERVER_ADDR` (optional) — the address the HTTP API binds to.
- `FETCH_LIMIT` (optional, default `20`) — how many recent signatures to pull
  each cycle. Each cycle fetches only transactions newer than the last one it
  ingested and decodes them concurrently, so a small limit keeps a busy account
  from exceeding the per-cycle timeout. Very active accounts on a slow/public
  RPC still benefit from a dedicated RPC provider (e.g. Helius).

### Build the Project

Use Cargo to build the project:

```
cargo build
```

### Run the Application

Run the application using the following command:

```
cargo run
```

The application will start a server at `http://127.0.0.1:3030` and begin fetching transactions from the Solana blockchain.

## Usage

Once the application is running, you can interact with it using the provided API. The server will listen on `http://127.0.0.1:3030` by default.

You can query the transactions stored in the database using the API. Refer to the API Endpoints section below for detailed information on how to make these queries.

### API Endpoints

| Method | Path | Description |
|---|---|---|
| GET | `/health` | Liveness probe; returns `{"status":"ok"}`. |
| GET | `/transactions` | Stored transactions for a public key (see query params below). |
| GET | `/transactions/{signature}` | A single stored transaction by its signature. |
| GET | `/accounts/{pub_key}/balance` | Current lamport balance for an account, fetched live from the RPC. |
| POST | `/refresh` | Triggers an out-of-band fetch of recent transactions for the monitored key. |

#### `GET /transactions`

Retrieves stored transactions filtered by public key and optional date.

**Query parameters:**

- `pub_key`: The public key to fetch transactions for.
- `day` (optional): Filter transactions by a specific day in `dd/mm/yyyy` format.
- `limit` (optional): Limit the number of transactions returned (default is 5).
- `offset` (optional): Offset for pagination.

Example:

```
curl "http://127.0.0.1:3030/transactions?pub_key=YourPublicKeyHere"
```

**Response shape** — each transaction is decoded from the RPC response into an
enriched record (not a transfer-biased guess):

```json
{
  "signature": "5dWj3A…",
  "slot": 331457812,
  "timestamp": 1721812345,
  "fee": 5000,
  "fee_payer": "9xQe…",
  "success": true,
  "tx_type": "transfer",
  "programs": ["system"],
  "transfer": { "source": "9xQe…", "destination": "3Fh2…", "lamports": 1000000 }
}
```

- `tx_type` classifies the transaction: `transfer` (native SOL), `vote`,
  `token` (SPL), the invoked program name, or `unknown`.
- `transfer` is present **only** for native SOL transfers, and its `lamports`
  is the *actual* amount parsed from the System Program instruction — non-transfer
  transactions (votes, token ops, program calls) carry `null` here instead of a
  meaningless number.

## Project Structure

The project is organized into the following modules:

- aggregator.rs: Handles the logic for fetching transactions from the Solana blockchain.
- api.rs: Defines and implements the RESTful API for querying transactions.
- db.rs: Implements an in-memory database with the ability to persist transactions to a text file.
- main.rs: The entry point of the application. It initializes components, starts the server, and handles graceful shutdown.

## Persistence

The in-memory database stores transaction data during the application's runtime. To ensure data is not lost when the application restarts, the database is periodically saved to a text file (transactions.txt). This file is loaded into the database on startup, ensuring data continuity.

## Testing

The project includes a comprehensive set of unit tests to ensure the correctness of its core components:

- Database Tests: Verify that transactions are correctly added, retrieved, and persisted.
- API Tests: Check the functionality and correctness of the API endpoints.
- Aggregator Tests: Test the integration between transaction fetching and storage.

To run the tests:

```
cargo test
```

## Design Decisions

In-Memory Database with File Persistence: This design was chosen for its balance between performance and simplicity. The in-memory database allows for fast querying, while file persistence ensures data is not lost between sessions.
Timeouts for Data Fetching: To prevent the application from hanging if the Solana network is slow or unresponsive, timeouts are used when fetching transactions.

## Future Enhancements

Support for Multiple Public Keys: Enable the aggregator to monitor multiple public keys simultaneously.
Dockerization: Provide a Dockerfile to containerize the application for easier deployment.
Enhanced Error Handling: Improve the robustness of error handling across the application.

## Contributing

Contributions are welcome! If you have ideas for improvements or want to fix bugs, please open an issue or submit a pull request. Before contributing, please read our contribution guidelines.

## License

This project is licensed under the MIT License. For more details, see the LICENSE.md file.

## Acknowledgments

A big thank you to the Solana community for providing the tools and documentation that made this project possible.
Thanks to all the contributors who helped improve this project.
