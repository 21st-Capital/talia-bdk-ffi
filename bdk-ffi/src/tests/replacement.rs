//! TALIA FORK: Speed up and Cancel build a new transaction that replaces a pending one and spends
//! the same coins. bdk_wallet 3.1.0's `add_utxos` takes only coins from `list_unspent`, which
//! leaves out every coin the pending original spends, so every Speed up and Cancel would fail.
//! `TxBuilder::replacing` lets those coins in, but only for the transactions it names.

use crate::bitcoin::{
    Amount, FeeRate, Network, NetworkKind, OutPoint, Psbt, Script, Transaction, Txid,
};
use crate::descriptor::Descriptor;
use crate::error::CreateTxError;
use crate::store::Persister;
use crate::tests::support::{apply, receive, Position};
use crate::tx_builder::{CoinSelectionAlgorithm, TxBuilder};
use crate::types::{KeychainKind, UnconfirmedTx};
use crate::wallet::Wallet;

use bdk_wallet::bitcoin::hashes::hex::FromHex;
use bdk_wallet::bitcoin::hashes::Hash;
use bdk_wallet::bitcoin::{Transaction as BdkTransaction, Txid as BdkTxid};

use std::collections::HashSet;
use std::sync::Arc;

const EXTERNAL: &str = "wpkh(tprv8ZgxMBicQKsPf2qfrEygW6fdYseJDDrVnDv26PH5BHdvSuG6ecCbHqLVof9yZcMoM31z9ur3tTYbSnr1WBqbGX97CbXcmp5H6qeMpyvx35B/84h/1h/1h/0/*)";
const INTERNAL: &str = "wpkh(tprv8ZgxMBicQKsPf2qfrEygW6fdYseJDDrVnDv26PH5BHdvSuG6ecCbHqLVof9yZcMoM31z9ur3tTYbSnr1WBqbGX97CbXcmp5H6qeMpyvx35B/84h/1h/1h/1/*)";

/// A wallet with two confirmed coins: C1 (100,000 sats) and C2 (40,000 sats), tip 200.
struct Fixture {
    wallet: Arc<Wallet>,
    c1: OutPoint,
    c2: OutPoint,
}

fn outpoint(txid: BdkTxid, vout: u32) -> OutPoint {
    OutPoint {
        txid: Arc::new(Txid(txid)),
        vout,
    }
}

fn fixture() -> Fixture {
    let wallet = Arc::new(
        Wallet::new(
            Arc::new(Descriptor::new(EXTERNAL.to_string(), NetworkKind::Test).unwrap()),
            Arc::new(Descriptor::new(INTERNAL.to_string(), NetworkKind::Test).unwrap()),
            Network::Regtest,
            Arc::new(Persister::new_in_memory().unwrap()),
            25,
        )
        .unwrap(),
    );
    let c1 = receive(
        &wallet,
        bdk_wallet::KeychainKind::External,
        0,
        100_000,
        Position::Confirmed(100),
        200,
    );
    let c2 = receive(
        &wallet,
        bdk_wallet::KeychainKind::External,
        1,
        40_000,
        Position::Confirmed(101),
        200,
    );
    Fixture {
        wallet,
        c1: outpoint(c1, 0),
        c2: outpoint(c2, 0),
    }
}

/// A payment to someone outside the wallet.
fn outside() -> Arc<Script> {
    Arc::new(Script::new(
        Vec::from_hex("0014d85c2b71d0060b09c9886aeb815e50991dda124d").unwrap(),
    ))
}

fn feerate(sat_per_vb: u64) -> Arc<FeeRate> {
    Arc::new(FeeRate::from_sat_per_vb(sat_per_vb).unwrap())
}

/// Build a payment of `amount` to the outside spending exactly `coin`, and put it in the mempool
/// (unconfirmed, canonical), as the app does after broadcasting. Returns the transaction.
fn pay_and_broadcast(wallet: &Arc<Wallet>, coin: &OutPoint, amount: u64) -> BdkTransaction {
    let psbt = TxBuilder::new()
        .add_utxo(coin.clone())
        .manually_selected_only()
        .add_recipient(&outside(), Arc::new(Amount::from_sat(amount)))
        .fee_rate(&feerate(2))
        .finish(wallet)
        .unwrap();
    let tx = psbt.extract_tx_unchecked_fee_rate();
    wallet
        .apply_unconfirmed_txs(vec![UnconfirmedTx {
            tx: Arc::clone(&tx),
            last_seen: 1_700_000_500,
        }])
        .unwrap();
    tx.as_ref().into()
}

fn txid_of(tx: &BdkTransaction) -> Arc<Txid> {
    Arc::new(Txid(tx.compute_txid()))
}

fn spent_outpoints(psbt: &Psbt) -> HashSet<(String, u32)> {
    psbt.extract_tx_unchecked_fee_rate()
        .input()
        .iter()
        .map(|txin| {
            (
                txin.previous_output.txid.to_string(),
                txin.previous_output.vout,
            )
        })
        .collect()
}

fn key(outpoint: &OutPoint) -> (String, u32) {
    (outpoint.txid.to_string(), outpoint.vout)
}

/// The change output of `tx` (the output that is not the payment to the outside).
fn change_of(tx: &BdkTransaction) -> OutPoint {
    let vout = tx
        .output
        .iter()
        .position(|txout| txout.script_pubkey != outside().0)
        .unwrap();
    outpoint(tx.compute_txid(), vout as u32)
}

// Accepted: Speed up names the waiting original and spends the coin it spends, with a higher fee.
// Every input carries the coin's full parent transaction and its output, as signers need.
#[test]
fn test_replacement_spends_the_coin_of_the_transaction_it_replaces() {
    let f = fixture();
    let original = pay_and_broadcast(&f.wallet, &f.c1, 10_000);

    let psbt = TxBuilder::new()
        .replacing(vec![txid_of(&original)])
        .add_utxo(f.c1.clone())
        .manually_selected_only()
        .add_recipient(&outside(), Arc::new(Amount::from_sat(10_000)))
        .fee_rate(&feerate(6))
        .finish(&f.wallet)
        .unwrap();

    assert_eq!(spent_outpoints(&psbt), HashSet::from([key(&f.c1)]));
    let input = &psbt.input()[0];
    let parent = input.non_witness_utxo.as_ref().expect("non_witness_utxo");
    assert_eq!(parent.compute_txid().to_string(), f.c1.txid.to_string());
    assert_eq!(input.witness_utxo.as_ref().unwrap().value.to_sat(), 100_000);
    assert_eq!(input.bip32_derivation.len(), 1);
    let original_fee = f
        .wallet
        .calculate_fee(&Transaction::from(original))
        .unwrap()
        .to_sat();
    assert!(psbt.fee().unwrap() > original_fee);
}

// Accepted: when the named coin is not enough, coin selection adds other coins, but never an
// output of the transaction being replaced (here its 90,000-sat change, which LargestFirst would
// otherwise take first).
#[test]
fn test_replacement_never_spends_an_output_of_the_transaction_it_replaces() {
    let f = fixture();
    let original = pay_and_broadcast(&f.wallet, &f.c1, 10_000);
    let change = change_of(&original);
    assert!(f
        .wallet
        .list_unspent()
        .unwrap()
        .iter()
        .any(|utxo| key(&utxo.outpoint) == key(&change)));

    let psbt = TxBuilder::new()
        .replacing(vec![txid_of(&original)])
        .add_utxo(f.c1.clone())
        .add_recipient(&outside(), Arc::new(Amount::from_sat(120_000)))
        .fee_rate(&feerate(6))
        .coin_selection(CoinSelectionAlgorithm::LargestFirst)
        .finish(&f.wallet)
        .unwrap();
    assert_eq!(
        spent_outpoints(&psbt),
        HashSet::from([key(&f.c1), key(&f.c2)])
    );

    // Naming the original's own change is refused outright.
    let result = TxBuilder::new()
        .replacing(vec![txid_of(&original)])
        .add_utxos(vec![f.c1.clone(), change])
        .add_recipient(&outside(), Arc::new(Amount::from_sat(10_000)))
        .fee_rate(&feerate(6))
        .finish(&f.wallet);
    assert!(matches!(result, Err(CreateTxError::UnknownUtxo { .. })));
}

// Refused: the coin of a waiting payment stays unavailable unless that very payment is named.
#[test]
fn test_coins_of_a_transaction_that_is_not_named_stay_unavailable() {
    let f = fixture();
    let original = pay_and_broadcast(&f.wallet, &f.c1, 10_000);
    let other = pay_and_broadcast(&f.wallet, &f.c2, 5_000);
    let build = |builder: Arc<TxBuilder>| {
        builder
            .manually_selected_only()
            .add_recipient(&outside(), Arc::new(Amount::from_sat(10_000)))
            .fee_rate(&feerate(6))
            .finish(&f.wallet)
    };

    // No `replacing` at all: upstream behaviour.
    let result = build(TxBuilder::new().add_utxo(f.c1.clone()));
    assert!(matches!(result, Err(CreateTxError::UnknownUtxo { .. })));

    // Naming another pending payment does not unlock C1, which `original` spends.
    let result = build(
        TxBuilder::new()
            .replacing(vec![txid_of(&other)])
            .add_utxo(f.c1.clone()),
    );
    assert!(matches!(result, Err(CreateTxError::UnknownUtxo { .. })));

    // Naming `original` does not unlock C2, which `other` spends.
    let result = build(
        TxBuilder::new()
            .replacing(vec![txid_of(&original)])
            .add_utxos(vec![f.c1.clone(), f.c2.clone()]),
    );
    assert!(matches!(result, Err(CreateTxError::UnknownUtxo { .. })));

    // Naming both unlocks both.
    let psbt = build(
        TxBuilder::new()
            .replacing(vec![txid_of(&original), txid_of(&other)])
            .add_utxos(vec![f.c1.clone(), f.c2.clone()]),
    )
    .unwrap();
    assert_eq!(
        spent_outpoints(&psbt),
        HashSet::from([key(&f.c1), key(&f.c2)])
    );
}

// Refused: a payment that has confirmed can no longer be replaced.
#[test]
fn test_coins_of_a_confirmed_transaction_stay_unavailable() {
    let f = fixture();
    let original = pay_and_broadcast(&f.wallet, &f.c1, 10_000);
    apply(&f.wallet, original.clone(), Position::Confirmed(201), 201);

    let result = TxBuilder::new()
        .replacing(vec![txid_of(&original)])
        .add_utxo(f.c1.clone())
        .manually_selected_only()
        .add_recipient(&outside(), Arc::new(Amount::from_sat(10_000)))
        .fee_rate(&feerate(6))
        .finish(&f.wallet);
    assert!(matches!(result, Err(CreateTxError::UnknownUtxo { .. })));
}

// Refused: a coin whose parent transaction the wallet knows only as a floating output (no full
// transaction, so no non_witness_utxo for the signers), even when its pending spender is named.
#[test]
fn test_coin_without_its_full_parent_transaction_stays_unavailable() {
    let f = fixture();
    // A coin of the wallet's own (external index 2) known only as a txout.
    let script = f
        .wallet
        .peek_address(KeychainKind::External, 2)
        .unwrap()
        .address
        .script_pubkey();
    let floating = outpoint(BdkTxid::from_byte_array([0x77; 32]), 0);
    f.wallet
        .insert_txout(
            floating.clone(),
            crate::bitcoin::TxOut {
                value: Arc::new(Amount::from_sat(30_000)),
                script_pubkey: script,
            },
        )
        .unwrap();
    // A pending payment spends it.
    let spender = BdkTransaction {
        version: bdk_wallet::bitcoin::transaction::Version::TWO,
        lock_time: bdk_wallet::bitcoin::absolute::LockTime::ZERO,
        input: vec![bdk_wallet::bitcoin::TxIn {
            previous_output: floating.clone().into(),
            sequence: bdk_wallet::bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
            ..Default::default()
        }],
        output: vec![bdk_wallet::bitcoin::TxOut {
            value: bdk_wallet::bitcoin::Amount::from_sat(29_000),
            script_pubkey: outside().0.clone(),
        }],
    };
    apply(
        &f.wallet,
        spender.clone(),
        Position::Unconfirmed(1_700_000_600),
        200,
    );

    let result = TxBuilder::new()
        .replacing(vec![txid_of(&spender)])
        .add_utxo(floating)
        .manually_selected_only()
        .add_recipient(&outside(), Arc::new(Amount::from_sat(10_000)))
        .fee_rate(&feerate(6))
        .finish(&f.wallet);
    assert!(matches!(result, Err(CreateTxError::UnknownUtxo { .. })));
}

// Refused, then accepted: the client pays 10,000 with C1 (A, pending), then pays Carol 5,000 from
// A's change (B, pending). A Speed up of A alone would evict B from the mempool, so it is refused
// whatever the coins; naming the whole chain [A, B] is accepted, and coin selection then still
// leaves out B's change, the largest coin list_unspent offers.
#[test]
fn test_replacing_a_parent_with_an_unnamed_pending_child() {
    let f = fixture();
    let a = pay_and_broadcast(&f.wallet, &f.c1, 10_000);
    let b = pay_and_broadcast(&f.wallet, &change_of(&a), 5_000);
    let speed_up = |replacing: Vec<Arc<Txid>>, amount: u64, manual: bool| {
        let builder = TxBuilder::new()
            .replacing(replacing)
            .add_utxo(f.c1.clone())
            .add_recipient(&outside(), Arc::new(Amount::from_sat(amount)))
            .fee_rate(&feerate(10))
            .coin_selection(CoinSelectionAlgorithm::LargestFirst);
        let builder = if manual {
            builder.manually_selected_only()
        } else {
            builder
        };
        builder.finish(&f.wallet)
    };

    // Refused: only A is named, and B depends on it.
    for (amount, manual) in [(10_000, true), (120_000, false)] {
        let result = speed_up(vec![txid_of(&a)], amount, manual);
        assert!(
            matches!(result, Err(CreateTxError::UnknownUtxo { .. })),
            "{} {}",
            amount,
            manual
        );
    }

    // Accepted: A and B both named.
    let psbt = speed_up(vec![txid_of(&a), txid_of(&b)], 10_000, true).unwrap();
    assert_eq!(spent_outpoints(&psbt), HashSet::from([key(&f.c1)]));
    let largest = f
        .wallet
        .list_unspent()
        .unwrap()
        .into_iter()
        .max_by_key(|utxo| utxo.txout.value.to_sat())
        .unwrap();
    assert_eq!(key(&largest.outpoint), key(&change_of(&b)));
    let psbt = speed_up(vec![txid_of(&a), txid_of(&b)], 120_000, false).unwrap();
    assert_eq!(
        spent_outpoints(&psbt),
        HashSet::from([key(&f.c1), key(&f.c2)])
    );
}

// Refused: naming a descendant's output outright, with or without the descendant itself named.
#[test]
fn test_replacement_never_spends_an_output_of_a_descendant() {
    let f = fixture();
    let a = pay_and_broadcast(&f.wallet, &f.c1, 10_000);
    let b = pay_and_broadcast(&f.wallet, &change_of(&a), 5_000);
    for replacing in [vec![txid_of(&a)], vec![txid_of(&a), txid_of(&b)]] {
        let result = TxBuilder::new()
            .replacing(replacing)
            .add_utxos(vec![f.c1.clone(), change_of(&b)])
            .manually_selected_only()
            .add_recipient(&outside(), Arc::new(Amount::from_sat(10_000)))
            .fee_rate(&feerate(6))
            .finish(&f.wallet);
        assert!(matches!(result, Err(CreateTxError::UnknownUtxo { .. })));
    }
}

// The same rules hold when bdk_wallet's builder is set up in the other order (`add_utxos` before
// `replacing`, which bdk-ffi never does): `create_tx` checks them again.
#[test]
fn test_create_tx_rechecks_replacement_rules_whatever_the_builder_order() {
    use bdk_wallet::bitcoin::{Amount as BdkAmount, FeeRate as BdkFeeRate};
    use bdk_wallet::error::CreateTxError as BdkCreateTxError;

    let f = fixture();
    let a = pay_and_broadcast(&f.wallet, &f.c1, 10_000);
    let a_change = change_of(&a);
    let build = |coin: &OutPoint, replacing: &[BdkTxid]| {
        let mut wallet = f.wallet.get_wallet().unwrap();
        let mut builder = wallet.build_tx();
        // Without `replacing` yet, add_utxos takes any coin list_unspent offers.
        builder
            .add_utxos(&[coin.clone().into()])
            .unwrap()
            .replacing(replacing)
            .manually_selected_only()
            .add_recipient(outside().0.clone(), BdkAmount::from_sat(5_000))
            .fee_rate(BdkFeeRate::from_sat_per_vb(6).unwrap());
        builder.finish()
    };

    // An output of the replaced transaction.
    assert!(matches!(
        build(&a_change, &[a.compute_txid()]),
        Err(BdkCreateTxError::UnknownUtxo)
    ));
    // A replaced transaction with a pending child that is not named, even with an unrelated coin.
    assert!(build(&f.c2, &[a.compute_txid()]).is_ok());
    let b = pay_and_broadcast(&f.wallet, &a_change, 5_000);
    assert!(matches!(
        build(&f.c2, &[a.compute_txid()]),
        Err(BdkCreateTxError::UnknownUtxo)
    ));
    assert!(build(&f.c2, &[a.compute_txid(), b.compute_txid()]).is_ok());
}
