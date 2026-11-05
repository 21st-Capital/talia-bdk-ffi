mod bitcoin;
mod descriptor;
mod electrum;
mod error;
mod keys;
mod macros;
mod signer;
mod store;
mod tx_builder;
mod types;
mod wallet;

#[cfg(test)]
mod tests;

/// TALIA FORK (feature "talia-golden" only, never in a build the app links): what 21st Capital's
/// equivalence test harness, kept outside this repository, drives. Re-exports of the
/// exported API the app's bridge calls, so the harness runs the shipped Rust layer, plus one
/// constructor the exported API does not have: an `Update` made from bdk_wallet's, so the harness
/// can hand a wallet the documented chain state a sync would bring (it also runs real syncs).
#[cfg(feature = "talia-golden")]
#[doc(hidden)]
pub mod talia_golden_api {
    pub use crate::bitcoin::{Address, Amount, FeeRate, OutPoint, Psbt, Script, Transaction, Txid};
    pub use crate::descriptor::Descriptor;
    pub use crate::electrum::ElectrumClient;
    pub use crate::store::Persister;
    pub use crate::tx_builder::{TxBuilder, TxOrdering};
    pub use crate::types::{ChainPosition, Policy, Update};
    pub use crate::wallet::Wallet;

    /// An `Update` carrying bdk_wallet's update as it is.
    pub fn update_from(update: bdk_wallet::Update) -> std::sync::Arc<Update> {
        std::sync::Arc::new(Update(update))
    }
}

use crate::bitcoin::OutPoint;

uniffi::setup_scaffolding!("bdk");
