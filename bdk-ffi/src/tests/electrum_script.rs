//! TALIA FORK: `ElectrumClient.script_get_history`, `ElectrumClient.script_list_unspent` and
//! `ElectrumClient.batch_transaction_get` (decided 2026-11-05), end to end: the fake Electrum server
//! of `electrum_batch.rs` on 127.0.0.1, reached over TCP through the crate's own `ElectrumClient`,
//! answers `blockchain.scripthash.get_history` and `blockchain.scripthash.listunspent` for one
//! script, and a batch of `blockchain.transaction.get` for its history.
//!
//! The script is paid, its coin spent with change back to the same script, paid again and kept;
//! in the mempool the change coin is spent, and two more payments wait, one of them from a coin
//! that is itself unconfirmed (height -1 in the history).

use super::electrum_batch::{spk, ChainBuilder, Faults, Server, EXT, FOREIGN};
use crate::bitcoin::Script;
use crate::electrum::ElectrumClient;

use bdk_wallet::bitcoin::{OutPoint, Txid};

fn client(server: &Server) -> ElectrumClient {
    ElectrumClient::new(server.url.clone(), None, Some(5), Some(0), true).unwrap()
}

#[test]
fn a_script_s_history_and_coins_cross_as_the_server_reports_them() {
    let mut b = ChainBuilder::new();
    b.empty_blocks(2);
    let watched = spk(EXT, 7);

    // Paid, then the coin spent with change back to the same script.
    let parent = b.foreign_coin(80_000);
    let paid = b.tx(
        &[OutPoint::new(parent.compute_txid(), 0)],
        &[(spk(FOREIGN, 0x0300), 30_000), (watched.clone(), 40_000)],
    );
    let paid_at = b.confirm(vec![parent, paid.clone()]);
    let spend = b.tx(
        &[OutPoint::new(paid.compute_txid(), 1)],
        &[(spk(FOREIGN, 0x0301), 10_000), (watched.clone(), 29_000)],
    );
    let spend_at = b.confirm(vec![spend.clone()]);
    // Paid again and kept.
    let parent = b.foreign_coin(60_000);
    let kept = b.tx(
        &[OutPoint::new(parent.compute_txid(), 0)],
        &[(watched.clone(), 50_000)],
    );
    let kept_at = b.confirm(vec![parent, kept.clone()]);
    // A confirmed outside coin, for a waiting payment whose input is confirmed.
    let outside = b.foreign_coin(90_000);
    b.confirm(vec![outside.clone()]);
    b.empty_blocks(3);

    // In the mempool: the change coin spent, a payment from a coin that is waiting too, and a
    // payment from the confirmed outside coin.
    let respend = b.tx(
        &[OutPoint::new(spend.compute_txid(), 1)],
        &[(spk(FOREIGN, 0x0302), 28_000)],
    );
    let waiting_parent = b.foreign_coin(70_000);
    let waiting = b.tx(
        &[OutPoint::new(waiting_parent.compute_txid(), 0)],
        &[(watched.clone(), 65_000)],
    );
    let pending = b.tx(
        &[OutPoint::new(outside.compute_txid(), 0)],
        &[(spk(FOREIGN, 0x0303), 1_000), (watched.clone(), 85_000)],
    );
    b.chain.mempool.extend([
        respend.clone(),
        waiting_parent,
        waiting.clone(),
        pending.clone(),
    ]);

    let server = Server::start(b.chain, Faults::default());
    let client = client(&server);
    let script = Script::new(watched.to_bytes());

    let history: Vec<(Txid, i32)> = client
        .script_get_history(&script)
        .unwrap()
        .iter()
        .map(|entry| (entry.txid.0, entry.height))
        .collect();
    assert_eq!(
        history,
        [
            (paid.compute_txid(), paid_at as i32),
            (spend.compute_txid(), spend_at as i32),
            (kept.compute_txid(), kept_at as i32),
            (respend.compute_txid(), 0),
            (waiting.compute_txid(), -1),
            (pending.compute_txid(), 0),
        ]
    );

    // electrum-client orders the coins by height, then output index: the unconfirmed ones (height
    // 0) first. Both spent coins are gone, the one spent only in the mempool too.
    let unspent: Vec<(Txid, u32, u64, u32)> = client
        .script_list_unspent(&script)
        .unwrap()
        .iter()
        .map(|coin| (coin.txid.0, coin.vout, coin.value.to_sat(), coin.height))
        .collect();
    assert_eq!(
        unspent,
        [
            (waiting.compute_txid(), 0, 65_000, 0),
            (pending.compute_txid(), 1, 85_000, 0),
            (kept.compute_txid(), 0, 50_000, kept_at),
        ]
    );
}

#[test]
fn a_script_the_server_never_saw_has_no_history_and_no_coins() {
    let mut b = ChainBuilder::new();
    b.empty_blocks(2);
    let parent = b.foreign_coin(80_000);
    let paid = b.tx(
        &[OutPoint::new(parent.compute_txid(), 0)],
        &[(spk(EXT, 0), 40_000)],
    );
    b.confirm(vec![parent, paid]);

    let server = Server::start(b.chain, Faults::default());
    let client = client(&server);
    let script = Script::new(spk(EXT, 1).to_bytes());

    assert!(client.script_get_history(&script).unwrap().is_empty());
    assert!(client.script_list_unspent(&script).unwrap().is_empty());
}

#[test]
fn a_history_s_transactions_come_in_one_batch_in_the_order_asked() {
    let mut b = ChainBuilder::new();
    b.empty_blocks(2);
    let watched = spk(EXT, 3);
    let parent = b.foreign_coin(80_000);
    let paid = b.tx(
        &[OutPoint::new(parent.compute_txid(), 0)],
        &[(spk(FOREIGN, 0x0400), 30_000), (watched.clone(), 40_000)],
    );
    b.confirm(vec![parent, paid.clone()]);
    let spend = b.tx(
        &[OutPoint::new(paid.compute_txid(), 1)],
        &[(spk(FOREIGN, 0x0401), 39_000)],
    );
    b.confirm(vec![spend.clone()]);
    let parent = b.foreign_coin(60_000);
    let waiting = b.tx(
        &[OutPoint::new(parent.compute_txid(), 0)],
        &[(watched.clone(), 59_000)],
    );
    b.chain.mempool.extend([parent, waiting.clone()]);

    // The server writes the answers of a batch in reverse order: they still come back in the
    // order asked.
    let mut faults = Faults::default();
    faults.reverse = true;
    let server = Server::start(b.chain, faults);
    let client = client(&server);
    let script = Script::new(watched.to_bytes());
    let txids: Vec<_> = client
        .script_get_history(&script)
        .unwrap()
        .iter()
        .map(|entry| entry.txid.clone())
        .collect();
    let asked: Vec<Txid> = txids.iter().map(|txid| txid.0).collect();
    assert_eq!(
        asked,
        [
            paid.compute_txid(),
            spend.compute_txid(),
            waiting.compute_txid()
        ]
    );

    let got: Vec<Txid> = client
        .batch_transaction_get(txids)
        .unwrap()
        .iter()
        .map(|tx| tx.compute_txid().0)
        .collect();
    assert_eq!(got, asked);

    // An empty list sends nothing and answers nothing.
    assert!(client.batch_transaction_get(vec![]).unwrap().is_empty());
}
