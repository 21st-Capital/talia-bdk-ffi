// SPDX-License-Identifier: MIT OR Apache-2.0

//! # JWT Static Authentication with Electrum Client
//!
//! This example demonstrates how to use a static JWT_TOKEN authentication with the
//! electrum-client library.

use bitcoin::Txid;
use electrum_client::{Client, ConfigBuilder, ElectrumApi};
use std::{str::FromStr, sync::Arc};

const ELECTRUM_URL: &str = "ssl://electrum.blockstream.info:50002";

const GENESIS_HEIGHT: usize = 0;
const GENESIS_TXID: &str = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";

fn main() {
    // A static JWT_TOKEN (i.e JWT_TOKEN="Bearer jwt_token...")
    let auth_provider = Arc::new(move || {
        let jwt_token = std::env::var("JWT_TOKEN").expect("JWT_TOKEN env variable not set");
        Some(jwt_token)
    });

    // The Electrum Server URL (i.e `ELECTRUM_URL` environment variable, or defaults to `ELECTRUM_URL` const above)
    let electrum_url = std::env::var("ELECTRUM_URL").unwrap_or(ELECTRUM_URL.to_owned());

    // Builds the electrum-client `Config`.
    let config = ConfigBuilder::new()
        .validate_domain(false)
        .authorization_provider(Some(auth_provider))
        .build();

    // Builds & Connect electrum-client `Client`.
    match Client::from_config(&electrum_url, config) {
        Ok(client) => {
            println!(
                "Successfully connected to Electrum Server: {:#?}; with JWT authentication!",
                electrum_url
            );

            // try to call the `server.features` method, it can fail on some servers.
            match client.server_features() {
                Ok(features) => println!(
                    "Successfully fetched the `server.features`!\n{:#?}",
                    features
                ),
                Err(e) => eprintln!("Failed to fetch the `server.features`!\nError: {:#?}", e),
            }

            // try to call the `blockchain.block.header` method, it should NOT fail.
            let genesis_height = GENESIS_HEIGHT;
            match client.block_header(genesis_height) {
                Ok(header) => {
                    println!(
                        "Successfully fetched the `Header` for given `height`={}!\n{:#?}",
                        genesis_height, header
                    );
                }
                Err(err) => eprintln!(
                    "Failed to fetch the `Header` for given `height`!\nError: {:#?}",
                    err
                ),
            }

            // try to call the `blockchain.transaction.get` method, it should NOT fail.
            let genesis_txid =
                Txid::from_str(GENESIS_TXID).expect("SHOULD have a valid genesis `txid`");
            match client.transaction_get(&genesis_txid) {
                Ok(tx) => {
                    println!(
                        "Successfully fetched the `Transaction` for given `txid`={}!\n{:#?}",
                        genesis_txid, tx
                    );
                }
                Err(err) => eprintln!(
                    "Failed to fetch the `Transaction` for given `txid`!\nError: {:#?}",
                    err
                ),
            }
        }
        Err(err) => {
            eprintln!(
                "Failed to build and connect `Client` to {:#?}!\nError: {:#?}\n",
                electrum_url, err
            );
            eprintln!("NOTE: This example requires an Electrum Server that handles/accept JWT authentication!");
            eprintln!("Try to update the `ELECTRUM_URL` and `JWT_TOKEN to match your setup.");
        }
    }
}
