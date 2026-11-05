// SPDX-License-Identifier: MIT OR Apache-2.0

//! # JWT Dynamic Authentication
//!
//! ## Advanced: Token Refresh with Keycloak
//!
//! This example demonstrates how to use dynamic JWT authentication with the
//! electrum-client library.
//!
//! ## Overview
//!
//! The electrum-client supports embedding authorization tokens (such as JWT
//! Bearer tokens) directly in JSON-RPC requests. This is achieved through an
//! [`AuthProvider`](electrum_client::config::AuthProvider) callback that is
//! invoked before each request.
//!
//! In order to have an automatic token refresh (e.g it expires every 5 minutes),
//! you should use a shared token holder (e.g KeycloakTokenManager)
//! behind an `Arc<RwLock<...>>` and spawn a background task to refresh it.
//!
//! ## JSON-RPC Request Format
//!
//! With the auth provider configured, each JSON-RPC request will include the
//! authorization field:
//!
//! ```json
//! {
//!   "jsonrpc": "2.0",
//!   "method": "blockchain.headers.subscribe",
//!   "params": [],
//!   "id": 1,
//!   "authorization": "Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9..."
//! }
//! ```
//!
//! If the provider returns `None`, the authorization field is omitted from the
//! request.
//!
//! ## Thread Safety
//!
//! The `AuthProvider` type is defined as:
//!
//! ```rust,ignore
//! pub type AuthProvider = Arc<dyn Fn() -> Option<String> + Send + Sync>;
//! ```
//!
//! This ensures thread-safe access to tokens across all RPC calls.

use electrum_client::{Client, ConfigBuilder, ElectrumApi};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::sleep;

/// Manages JWT tokens from Keycloak with automatic refresh
struct KeycloakTokenManager {
    token: Arc<RwLock<Option<String>>>,
    keycloak_url: String,
    grant_type: String,
    client_id: String,
    client_secret: String,
}

impl KeycloakTokenManager {
    fn new(
        keycloak_url: String,
        grant_type: String,
        client_id: String,
        client_secret: String,
    ) -> Self {
        Self {
            token: Arc::new(RwLock::new(None)),
            keycloak_url,
            client_id,
            client_secret,
            grant_type,
        }
    }

    /// Get the current token (for the auth provider)
    fn get_token(&self) -> Option<String> {
        self.token.read().unwrap().clone()
    }

    /// Fetch a fresh token from Keycloak
    async fn fetch_token(&self) -> Result<String, Box<dyn std::error::Error>> {
        let url = format!("{}/protocol/openid-connect/token", self.keycloak_url);

        // if you're using other HTTP client (i.e `reqwest`), you can probably use `.form` methods.
        // it's currently not implemented in `bitreq`, needs to be built manually.
        let body = format!(
            "grant_type={}&client_id={}&client_secret={}",
            self.grant_type, self.client_id, self.client_secret
        );

        let response = bitreq::post(url)
            .with_header("Content-Type", "application/x-www-form-urlencoded")
            .with_body(body)
            .send_async()
            .await?;

        let json: serde_json::Value = response.json()?;
        let access_token = json["access_token"]
            .as_str()
            .ok_or("Missing access_token")?
            .to_string();

        Ok(format!("Bearer {}", access_token))
    }

    /// Background task that refreshes the token every 4 minutes
    async fn refresh_loop(self: Arc<Self>) {
        loop {
            // Refresh every 4 minutes (tokens expire at 5 minutes)
            sleep(Duration::from_secs(240)).await;

            match self.fetch_token().await {
                Ok(new_token) => {
                    println!("Token refreshed successfully");
                    // In a background thread/task, periodically update the token
                    *self.token.write().unwrap() = Some(new_token);
                }
                Err(e) => {
                    eprintln!("Failed to refresh token: {}", e);
                    // Keep using old token until we can refresh
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The Electrum Server URL (i.e `ELECTRUM_URL` environment variable)
    let electrum_url = std::env::var("ELECTRUM_URL")
        .expect("SHOULD have the `ELECTRUM_URL` environment variable!");

    // The JWT_TOKEN manager setup (i.e Keycloak server URL, client ID and secret)
    let keycloak_url = std::env::var("KEYCLOAK_URL")
        .expect("SHOULD have the `KEYCLOAK_URL` environment variable!");

    let grant_type = std::env::var("GRANT_TYPE").unwrap_or("client_credentials".to_string());
    let client_id =
        std::env::var("CLIENT_ID").expect("SHOULD have the `CLIENT_ID` environment variable!");
    let client_secret = std::env::var("CLIENT_SECRET")
        .expect("SHOULD have the `CLIENT_SECRET` environment variable!");

    // Setup `KeycloakTokenManager`
    let token_manager = Arc::new(KeycloakTokenManager::new(
        keycloak_url,
        grant_type,
        client_id,
        client_secret,
    ));

    // Fetch initial token
    let jwt_token = token_manager.fetch_token().await?;

    println!("JWT_TOKEN='{}'", &jwt_token[..jwt_token.len().min(40)]);

    *token_manager.token.write().unwrap() = Some(jwt_token);

    // Start background refresh task
    let tm_clone = token_manager.clone();
    tokio::spawn(async move {
        tm_clone.refresh_loop().await;
    });

    // Create Electrum client with dynamic auth provider
    let tm_for_provider = token_manager.clone();
    let config = ConfigBuilder::new()
        .authorization_provider(Some(Arc::new(move || tm_for_provider.get_token())))
        .build();

    let client = Client::from_config(&electrum_url, config)?;

    // All RPC calls will automatically include fresh JWT tokens
    loop {
        match client.server_features() {
            Ok(features) => println!("Connected: {:?}", features),
            Err(e) => eprintln!("Error: {}", e),
        }

        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
