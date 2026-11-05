//! TALIA FORK: offline chain data for tests.
//!
//! Upstream's multisig tests synced a wallet from the live Mutinynet Esplora (and were ignored
//! by default). This fork has no Esplora client, so tests hand the wallet the same kind of update a
//! sync would: a transaction paying one of the wallet's addresses, either confirmed at a height or
//! seen in the mempool, plus a chain of fake block hashes up to a tip.

use crate::types::Update;
use crate::wallet::Wallet;

use bdk_wallet::bitcoin::constants::genesis_block;
use bdk_wallet::bitcoin::hashes::Hash;
use bdk_wallet::bitcoin::{
    absolute, transaction, Amount, BlockHash, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxOut, Txid, Witness,
};
use bdk_wallet::chain::{BlockId, CheckPoint, ConfirmationBlockTime};
use bdk_wallet::KeychainKind;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// Where a test transaction sits.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Position {
    /// Confirmed in the block at this height.
    Confirmed(u32),
    /// In the mempool, last seen at this unix time.
    Unconfirmed(u64),
}

/// The fake block hash every test uses for `height` (the real genesis hash at 0), so updates made
/// at different times always agree on the chain.
pub(crate) fn block_hash(wallet: &Wallet, height: u32) -> BlockHash {
    if height == 0 {
        return genesis_block(wallet.network().unwrap()).block_hash();
    }
    let mut bytes = [0xb1u8; 32];
    bytes[..4].copy_from_slice(&height.to_le_bytes());
    BlockHash::from_byte_array(bytes)
}

/// A chain update from genesis to `tip` that keeps every block the wallet already has.
fn chain_to(wallet: &Wallet, heights: &[u32], tip: u32) -> CheckPoint {
    let mut blocks: BTreeMap<u32, BlockHash> = wallet
        .checkpoints()
        .unwrap()
        .into_iter()
        .map(|block| (block.height, block.hash.0))
        .collect();
    for &height in heights.iter().chain([0, tip].iter()) {
        blocks.insert(height, block_hash(wallet, height));
    }
    CheckPoint::from_block_ids(
        blocks
            .into_iter()
            .map(|(height, hash)| BlockId { height, hash }),
    )
    .expect("heights are sorted")
}

/// Apply `tx` to the wallet at `position`, with the chain tip at `tip`.
pub(crate) fn apply(wallet: &Wallet, tx: Transaction, position: Position, tip: u32) -> Txid {
    let txid = tx.compute_txid();
    let mut update = bdk_wallet::Update::default();
    update.tx_update.txs.push(Arc::new(tx));
    let mut heights = vec![];
    match position {
        Position::Confirmed(height) => {
            heights.push(height);
            update.tx_update.anchors.insert((
                ConfirmationBlockTime {
                    block_id: BlockId {
                        height,
                        hash: block_hash(wallet, height),
                    },
                    confirmation_time: 1_700_000_000 + u64::from(height) * 600,
                },
                txid,
            ));
        }
        Position::Unconfirmed(seen_at) => {
            update.tx_update.seen_ats.insert((txid, seen_at));
        }
    }
    update.chain = Some(chain_to(wallet, &heights, tip));
    wallet.apply_update(Arc::new(Update(update))).unwrap();
    txid
}

/// A transaction from outside the wallet paying `value` sats to the wallet's address at
/// `keychain`/`index`. Each call spends a different made-up outpoint, so every funding
/// transaction has its own txid.
pub(crate) fn funding_tx(
    wallet: &Wallet,
    keychain: KeychainKind,
    index: u32,
    value: u64,
) -> Transaction {
    static NEXT: AtomicU8 = AtomicU8::new(1);
    let script_pubkey: ScriptBuf = wallet
        .peek_address(keychain, index)
        .unwrap()
        .address
        .script_pubkey()
        .0
        .clone();
    wallet.reveal_addresses_to(keychain, index).unwrap();
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array([NEXT.fetch_add(1, Ordering::Relaxed); 32]),
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey,
        }],
    }
}

/// Pay `value` sats to the wallet's address at `keychain`/`index`, at `position`, with the chain
/// tip at `tip`. Returns the funding transaction's txid (its output 0 is the coin).
pub(crate) fn receive(
    wallet: &Wallet,
    keychain: KeychainKind,
    index: u32,
    value: u64,
    position: Position,
    tip: u32,
) -> Txid {
    let tx = funding_tx(wallet, keychain, index, value);
    apply(wallet, tx, position, tip)
}
