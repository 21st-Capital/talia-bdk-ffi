//! TALIA FORK: D10, the leaf hazard (decided 2026-10-17; "D10" is this fork's name for the
//! timelocked-leaf finalizer rule), on vendored bdk_wallet 3.1.0. The same rule and the same tests
//! as Talia's earlier bdk 0.28 library.
//!
//! The rule: a spending path with `older(N)` or `after(N)` counts as satisfied only when the
//! transaction itself satisfies it (the input's nSequence and the version for `older`,
//! nLockTime and an nSequence that enables it for `after`). The coin's age, the chain tip and
//! `SignOptions::assume_height` never do.
//!
//! Upstream finalized with the PSBT's own satisfier OR `Older`/`After` satisfiers that only
//! looked at the coin's age and the tip. In a vault where one device holds keys on two paths
//! (Liana's key reuse, same-seed accounts) and signs both, an everyday spend of an old coin was
//! finalized through the timelocked leaf, and the network refuses that transaction. Each
//! everyday test first shows that upstream's satisfiers build an invalid witness for the very
//! same PSBT (`upstream_witness_is_refused`), then that both finalizers (upstream's
//! `finalize_psbt`, which `sign` runs, and Talia's `finalize_psbt_keeping_metadata`, which the
//! bridge runs) leave the input open, and finish it through the everyday path once the second
//! key has signed, with libbitcoinconsensus accepting it.
//!
//! Test keys only (seeds `[s; 32]`).

use std::collections::BTreeMap;
use std::sync::Arc;

use bdk_wallet::bitcoin::bip32::{Xpriv, Xpub};
use bdk_wallet::bitcoin::consensus::encode::serialize;
use bdk_wallet::bitcoin::constants::genesis_block;
use bdk_wallet::bitcoin::hashes::{sha256d, Hash};
use bdk_wallet::bitcoin::secp256k1::Secp256k1;
use bdk_wallet::bitcoin::{
    absolute, relative, transaction, Amount, BlockHash, FeeRate, Network, OutPoint, Psbt,
    ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, WScriptHash, Witness,
};
use bdk_wallet::chain::{BlockId, CheckPoint, ConfirmationBlockTime};
use bdk_wallet::descriptor::policy::{Policy, SatisfiableItem};
use bdk_wallet::miniscript::psbt::PsbtInputSatisfier;
use bdk_wallet::miniscript::{MiniscriptKey, Satisfier, ToPublicKey};
use bdk_wallet::{KeychainKind, SignOptions, Update, Wallet};

const TIP: u32 = 70_000;
/// Liana's unspendable internal key (BIP-341's H point).
const NUMS: &str = "50929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0";

// ---------------------------------------------------------------------------------------------
// Keys, vaults, coins
// ---------------------------------------------------------------------------------------------

/// A test device: the master key of seed `[s; 32]`.
#[derive(Clone, Copy)]
struct Device(u8);

impl Device {
    fn xprv(self) -> Xpriv {
        Xpriv::new_master(Network::Testnet, &[self.0; 32]).unwrap()
    }
    /// The device's key on derivation branch `branch`, as the vault's descriptor writes it: the
    /// tprv in the wallet that signs with it, the tpub everywhere else.
    fn key(self, branch: u32, private: bool) -> String {
        let x = if private {
            self.xprv().to_string()
        } else {
            Xpub::from_priv(&Secp256k1::new(), &self.xprv()).to_string()
        };
        format!("{}/{}/*", x, branch)
    }
}

const A: Device = Device(1);
const B: Device = Device(2);
const C: Device = Device(3);

/// A vault shape, written once for every view of it: `k(device, branch)` gives the key string
/// the view uses (private only for the one key that view signs with).
type Shape = fn(&dyn Fn(Device, u32) -> String) -> String;

/// i-liana-wsh-one-device-two-paths: device A holds a key on the everyday multi AND one on the
/// recovery path (Liana's `<2;3>` key reuse).
fn liana_wsh(k: &dyn Fn(Device, u32) -> String) -> String {
    format!(
        "wsh(or_d(multi(2,{a0},{b0}),and_v(v:thresh(1,pkh({a2}),a:pkh({c0})),older(144))))",
        a0 = k(A, 0),
        b0 = k(B, 0),
        a2 = k(A, 2),
        c0 = k(C, 0)
    )
}

/// i-liana-tr-one-device-two-paths, with Liana's unspendable internal key: an everyday leaf and
/// a recovery leaf, device A on both.
fn liana_tr(k: &dyn Fn(Device, u32) -> String) -> String {
    format!(
        "tr({nums},{{multi_a(2,{a0},{b0}),and_v(v:pk({a2}),older(144))}})",
        nums = NUMS,
        a0 = k(A, 0),
        b0 = k(B, 0),
        a2 = k(A, 2)
    )
}

/// The same hazard with an absolute lock: A's second key behind `after(1000)`.
fn after_wsh(k: &dyn Fn(Device, u32) -> String) -> String {
    format!(
        "wsh(or_d(multi(2,{a0},{b0}),and_v(v:pkh({a2}),after(1000))))",
        a0 = k(A, 0),
        b0 = k(B, 0),
        a2 = k(A, 2)
    )
}

/// The shape with every key public (the app's watch-only view), or with the one key
/// `(device, branch)` private (a signing view).
fn descriptor(shape: Shape, signs_with: Option<(u8, u32)>) -> String {
    shape(&|d: Device, branch: u32| d.key(branch, signs_with == Some((d.0, branch))))
}

/// One coin of the vault at external index `index`, confirmed at `height` (None: unconfirmed).
struct Coin {
    tx: Transaction,
    height: Option<u32>,
}

impl Coin {
    fn outpoint(&self) -> OutPoint {
        OutPoint {
            txid: self.tx.compute_txid(),
            vout: 0,
        }
    }
}

fn new_wallet(desc: String) -> Wallet {
    Wallet::create_single(desc)
        .network(Network::Testnet)
        .create_wallet_no_persist()
        .unwrap()
}

fn coin(shape: Shape, label: &str, index: u32, value: u64, height: Option<u32>) -> Coin {
    let peek = new_wallet(descriptor(shape, None));
    let script = peek
        .peek_address(KeychainKind::External, index)
        .address
        .script_pubkey();
    let prev = Txid::from_raw_hash(sha256d::Hash::hash(
        format!("talia-d10|{}", label).as_bytes(),
    ));
    let tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint { txid: prev, vout: 0 },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey: script,
        }],
    };
    Coin { tx, height }
}

fn block_hash(height: u32) -> BlockHash {
    if height == 0 {
        return genesis_block(Network::Testnet).block_hash();
    }
    let mut bytes = [0xd1u8; 32];
    bytes[..4].copy_from_slice(&height.to_le_bytes());
    BlockHash::from_byte_array(bytes)
}

/// A wallet of `shape` that signs with the key `signs_with` (None: the app's watch-only wallet),
/// holding `coins`, its chain at `tip`: the update a sync would bring.
fn wallet(shape: Shape, signs_with: Option<(u8, u32)>, coins: &[&Coin], tip: u32) -> Wallet {
    let mut w = new_wallet(descriptor(shape, signs_with));
    let mut update = Update::default();
    let mut heights = vec![0, tip];
    for c in coins {
        let txid = c.tx.compute_txid();
        update.tx_update.txs.push(Arc::new(c.tx.clone()));
        match c.height {
            Some(h) => {
                heights.push(h);
                update.tx_update.anchors.insert((
                    ConfirmationBlockTime {
                        block_id: BlockId {
                            height: h,
                            hash: block_hash(h),
                        },
                        confirmation_time: 1_700_000_000 + u64::from(h) * 600,
                    },
                    txid,
                ));
            }
            None => {
                update.tx_update.seen_ats.insert((txid, 1_700_000_000));
            }
        }
    }
    heights.sort_unstable();
    heights.dedup();
    update.chain = Some(
        CheckPoint::from_block_ids(heights.into_iter().map(|height| BlockId {
            height,
            hash: block_hash(height),
        }))
        .unwrap(),
    );
    w.apply_update(update).unwrap();
    w
}

// ---------------------------------------------------------------------------------------------
// Building, signing, checking
// ---------------------------------------------------------------------------------------------

/// Which spending path a build takes.
#[derive(Clone, Copy, PartialEq)]
enum Path {
    Everyday,
    Recovery,
}

fn has_timelock(p: &Policy) -> bool {
    match &p.item {
        SatisfiableItem::RelativeTimelock { .. } | SatisfiableItem::AbsoluteTimelock { .. } => true,
        SatisfiableItem::Thresh { items, .. } => items.iter().any(has_timelock),
        _ => false,
    }
}

/// The root's child for `path`: the first one with a timelock (recovery), or the first without
/// one that is not a lone key (everyday; a taproot root lists the internal key first).
fn child_for(policy: &Policy, path: Path) -> usize {
    let items = match &policy.item {
        SatisfiableItem::Thresh { items, .. } => items,
        _ => panic!("a vault with more than one path"),
    };
    items
        .iter()
        .position(|i| match path {
            Path::Recovery => has_timelock(i),
            Path::Everyday => {
                !has_timelock(i)
                    && !matches!(
                        i.item,
                        SatisfiableItem::EcdsaSignature(_) | SatisfiableItem::SchnorrSignature(_)
                    )
            }
        })
        .unwrap()
}

/// The policy path that picks child `child` of the root, and in that subtree the first
/// `threshold` items of every threshold with a choice (what the app's policyPath.ts does for a
/// path the client picked).
fn path_to(policy: &Policy, child: usize) -> BTreeMap<String, Vec<usize>> {
    fn fill(p: &Policy, out: &mut BTreeMap<String, Vec<usize>>) {
        if let SatisfiableItem::Thresh { items, threshold } = &p.item {
            if *threshold < items.len() {
                out.insert(p.id.clone(), (0..*threshold).collect());
            }
            for i in items {
                fill(i, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    out.insert(policy.id.clone(), vec![child]);
    if let SatisfiableItem::Thresh { items, .. } = &policy.item {
        fill(&items[child], &mut out);
    }
    out
}

fn foreign() -> ScriptBuf {
    ScriptBuf::new_p2wsh(&WScriptHash::hash(b"talia-d10 foreign recipient"))
}

/// The bridge's build: the named coins only, RBF on (bdk_wallet's 0xFFFFFFFD), the chosen
/// path, version 1 unless the path carries an `older()` (as the app's engine asks).
fn build(w: &mut Wallet, coins: &[&Coin], path: Path, locktime: Option<absolute::LockTime>) -> Psbt {
    let policy = w.policies(KeychainKind::External).unwrap().unwrap();
    let child = child_for(&policy, path);
    let csv = path == Path::Recovery && !has_after(&policy, child);
    let outpoints: Vec<OutPoint> = coins.iter().map(|c| c.outpoint()).collect();
    let mut b = w.build_tx();
    b.add_utxos(&outpoints).unwrap();
    b.manually_selected_only()
        .add_recipient(foreign(), Amount::from_sat(25_000))
        .fee_rate(FeeRate::from_sat_per_vb_u32(2))
        .version(if csv { 2 } else { 1 })
        .policy_path(path_to(&policy, child), KeychainKind::External);
    if let Some(lt) = locktime {
        b.nlocktime(lt);
    }
    b.finish().unwrap()
}

fn has_after(policy: &Policy, child: usize) -> bool {
    fn any_after(p: &Policy) -> bool {
        match &p.item {
            SatisfiableItem::AbsoluteTimelock { .. } => true,
            SatisfiableItem::Thresh { items, .. } => items.iter().any(any_after),
            _ => false,
        }
    }
    match &policy.item {
        SatisfiableItem::Thresh { items, .. } => any_after(&items[child]),
        _ => false,
    }
}

/// A device signs every key of its own in the PSBT, on every branch it holds one (a Ledger
/// does: "every leaf per key expression"), without finalizing. bdk_wallet's own signer keeps one
/// signer per fingerprint, so each of the device's keys signs from a wallet of its own.
fn device_signs(shape: Shape, device: Device, coins: &[&Coin], psbt: &mut Psbt) {
    for branch in [0u32, 2] {
        if !descriptor(shape, Some((device.0, branch))).contains("tprv") {
            continue; // the device holds no key on this branch in this shape
        }
        let signer = wallet(shape, Some((device.0, branch)), coins, TIP);
        let done = signer
            .sign(
                psbt,
                SignOptions {
                    try_finalize: false,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!done);
    }
}

/// Which finalizer the app's completePsbt runs.
#[derive(Clone, Copy, Debug)]
enum Finalizer {
    /// Talia's `finalize_psbt_keeping_metadata`, what the bridge calls.
    KeepingMetadata,
    /// Upstream's `sign` (no signers of its own), which runs `finalize_psbt`.
    UpstreamSign,
}

const FINALIZERS: [Finalizer; 2] = [Finalizer::KeepingMetadata, Finalizer::UpstreamSign];

fn app_finalizes(w: &Wallet, psbt: &mut Psbt, f: Finalizer, opts: SignOptions) -> bool {
    match f {
        Finalizer::KeepingMetadata => w.finalize_psbt_keeping_metadata(psbt, opts).unwrap(),
        Finalizer::UpstreamSign => w.sign(psbt, opts).unwrap(),
    }
}

/// libbitcoinconsensus (Bitcoin Core 26.0, every consensus flag including taproot) on every
/// input; "ok" or the error.
fn consensus(tx: &Transaction, spent: &[TxOut]) -> Vec<String> {
    let bytes = serialize(tx);
    let utxos: Vec<bitcoinconsensus::Utxo> = spent
        .iter()
        .map(|o| bitcoinconsensus::Utxo {
            script_pubkey: o.script_pubkey.as_bytes().as_ptr(),
            script_pubkey_len: o.script_pubkey.len() as u32,
            value: o.value.to_sat() as i64,
        })
        .collect();
    (0..tx.input.len())
        .map(|i| {
            match bitcoinconsensus::verify_with_flags(
                spent[i].script_pubkey.as_bytes(),
                spent[i].value.to_sat(),
                &bytes,
                Some(&utxos),
                i,
                bitcoinconsensus::VERIFY_ALL_PRE_TAPROOT | bitcoinconsensus::VERIFY_TAPROOT,
            ) {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("{:?}", e),
            }
        })
        .collect()
}

fn spent_by(tx: &Transaction, coins: &[&Coin]) -> Vec<TxOut> {
    tx.input
        .iter()
        .map(|i| {
            coins
                .iter()
                .find(|c| c.outpoint() == i.previous_output)
                .expect("a coin of the test")
                .tx
                .output[0]
                .clone()
        })
        .collect()
}

/// What upstream passed next to the PSBT's own satisfier: a clock that says every timelock is
/// over (the coin is old, the tip is past the height).
struct UpstreamClock;
impl<Pk: MiniscriptKey + ToPublicKey> Satisfier<Pk> for UpstreamClock {
    fn check_older(&self, _: relative::LockTime) -> bool {
        true
    }
    fn check_after(&self, _: absolute::LockTime) -> bool {
        true
    }
}

/// Upstream's satisfier tuple on this PSBT (every input the vault's external index `index`):
/// it builds a witness, and the network refuses the transaction.
fn upstream_witness_is_refused(w: &Wallet, psbt: &Psbt, coins: &[&Coin], index: u32) {
    let derived = w
        .public_descriptor(KeychainKind::External)
        .at_derivation_index(index)
        .unwrap();
    let mut tx = psbt.unsigned_tx.clone();
    for n in 0..tx.input.len() {
        let mut txin = TxIn::default();
        derived
            .satisfy(&mut txin, (PsbtInputSatisfier::new(psbt, n), UpstreamClock))
            .expect("upstream's satisfiers find a witness");
        tx.input[n].script_sig = txin.script_sig;
        tx.input[n].witness = txin.witness;
    }
    let verdict = consensus(&tx, &spent_by(&tx, coins));
    assert!(
        verdict.iter().any(|v| v != "ok"),
        "upstream's witness would have been valid here, so this is not the D10 case: {:?}",
        verdict
    );
}

fn assert_open(psbt: &Psbt) {
    for (n, i) in psbt.inputs.iter().enumerate() {
        assert!(i.final_script_witness.is_none(), "input {} was finalized", n);
        assert!(i.final_script_sig.is_none(), "input {} was finalized", n);
    }
}

fn assert_valid(psbt: &Psbt, coins: &[&Coin]) -> Transaction {
    let tx = psbt.clone().extract_tx_unchecked_fee_rate();
    let verdict = consensus(&tx, &spent_by(&tx, coins));
    assert!(verdict.iter().all(|v| v == "ok"), "consensus: {:?}", verdict);
    tx
}

// ---------------------------------------------------------------------------------------------
// The everyday path: the coin's age never opens the recovery leaf
// ---------------------------------------------------------------------------------------------

#[test]
fn test_d10_wsh_everyday_spend_is_never_finished_through_the_recovery_leaf() {
    for f in FINALIZERS {
        let old = coin(liana_wsh, "wsh-old", 0, 100_000, Some(1_000));
        let coins = [&old];
        let mut app = wallet(liana_wsh, None, &coins, TIP);
        let mut psbt = build(&mut app, &coins, Path::Everyday, None);
        assert_eq!(psbt.unsigned_tx.version, transaction::Version::ONE);
        assert_eq!(psbt.unsigned_tx.input[0].sequence, Sequence(0xFFFF_FFFD));

        // Device A signs both of its keys: K_A/0/0 (everyday) and K_A/2/0 (recovery).
        device_signs(liana_wsh, A, &coins, &mut psbt);
        assert_eq!(psbt.inputs[0].partial_sigs.len(), 2);
        upstream_witness_is_refused(&app, &psbt, &coins, 0);

        // Not finished; both signatures kept for the next device.
        assert!(!app_finalizes(&app, &mut psbt, f, SignOptions::default()), "{:?}", f);
        assert_open(&psbt);
        assert_eq!(psbt.inputs[0].partial_sigs.len(), 2);

        // Device B signs: finished through the everyday multi, and the network accepts it.
        device_signs(liana_wsh, B, &coins, &mut psbt);
        assert!(app_finalizes(&app, &mut psbt, f, SignOptions::default()), "{:?}", f);
        let tx = assert_valid(&psbt, &coins);
        // or_d's left branch: the multi's dummy, two signatures, the witness script.
        assert_eq!(tx.input[0].witness.len(), 4);
        assert!(tx.input[0].witness.to_vec()[0].is_empty());
    }
}

#[test]
fn test_d10_taproot_everyday_spend_is_never_finished_through_the_recovery_leaf() {
    for f in FINALIZERS {
        let old = coin(liana_tr, "tr-old", 0, 100_000, Some(1_000));
        let coins = [&old];
        let mut app = wallet(liana_tr, None, &coins, TIP);
        let mut psbt = build(&mut app, &coins, Path::Everyday, None);
        assert_eq!(psbt.unsigned_tx.version, transaction::Version::ONE);

        device_signs(liana_tr, A, &coins, &mut psbt);
        assert_eq!(psbt.inputs[0].tap_script_sigs.len(), 2, "A signs on both leaves");
        upstream_witness_is_refused(&app, &psbt, &coins, 0);

        assert!(!app_finalizes(&app, &mut psbt, f, SignOptions::default()), "{:?}", f);
        assert_open(&psbt);

        // With B's signature both leaves have their keys; upstream picked the SMALLER witness,
        // the recovery leaf, even then. Now only the everyday leaf counts.
        device_signs(liana_tr, B, &coins, &mut psbt);
        upstream_witness_is_refused(&app, &psbt, &coins, 0);
        assert!(app_finalizes(&app, &mut psbt, f, SignOptions::default()), "{:?}", f);
        let tx = assert_valid(&psbt, &coins);
        // Script path through multi_a(2, A, B): two signatures, the leaf, the control block.
        assert_eq!(tx.input[0].witness.len(), 4);
    }
}

#[test]
fn test_d10_assume_height_never_satisfies_a_timelock() {
    for f in FINALIZERS {
        // A young coin (10 confirmations): only `assume_height` made upstream's clock say yes.
        let young = coin(liana_wsh, "wsh-young", 1, 100_000, Some(TIP - 9));
        let coins = [&young];
        let mut app = wallet(liana_wsh, None, &coins, TIP);
        let mut psbt = build(&mut app, &coins, Path::Everyday, None);
        device_signs(liana_wsh, A, &coins, &mut psbt);
        upstream_witness_is_refused(&app, &psbt, &coins, 1);
        let opts = SignOptions {
            assume_height: Some(10_000_000),
            ..Default::default()
        };
        assert!(!app_finalizes(&app, &mut psbt, f, opts), "{:?}", f);
        assert_open(&psbt);
    }
}

#[test]
fn test_d10_after_is_satisfied_by_nlocktime_never_by_the_chain_tip() {
    for f in FINALIZERS {
        let old = coin(after_wsh, "after-old", 0, 100_000, Some(1_000));
        let coins = [&old];
        let mut app = wallet(after_wsh, None, &coins, TIP);
        // An everyday spend with nLockTime 0, while the wallet's tip is 70 000, far past
        // after(1000).
        let mut psbt = build(&mut app, &coins, Path::Everyday, Some(absolute::LockTime::ZERO));
        assert_eq!(psbt.unsigned_tx.lock_time, absolute::LockTime::ZERO);
        device_signs(after_wsh, A, &coins, &mut psbt);
        upstream_witness_is_refused(&app, &psbt, &coins, 0);
        assert!(!app_finalizes(&app, &mut psbt, f, SignOptions::default()), "{:?}", f);
        assert_open(&psbt);
        device_signs(after_wsh, B, &coins, &mut psbt);
        assert!(app_finalizes(&app, &mut psbt, f, SignOptions::default()), "{:?}", f);
        assert_valid(&psbt, &coins);
    }
}

// ---------------------------------------------------------------------------------------------
// The timelocked paths: where the transaction carries the lock, nothing changes
// ---------------------------------------------------------------------------------------------

#[test]
fn test_d10_recovery_spend_finalizes_from_the_transactions_own_nsequence() {
    for f in FINALIZERS {
        // An old coin and a young one, on the recovery path: nSequence carries older(144) on
        // both inputs, so both finalize (as 0.28 did: the young coin's wait is the network's
        // BIP-68 check, not a script failure).
        let old = coin(liana_wsh, "rec-old", 0, 100_000, Some(1_000));
        let young = coin(liana_wsh, "rec-young", 1, 50_000, Some(TIP - 9));
        let coins = [&old, &young];
        let mut app = wallet(liana_wsh, None, &coins, TIP);
        let mut psbt = build(&mut app, &coins, Path::Recovery, None);
        assert_eq!(psbt.unsigned_tx.version, transaction::Version::TWO);
        assert!(psbt
            .unsigned_tx
            .input
            .iter()
            .all(|i| i.sequence == Sequence(144)));
        device_signs(liana_wsh, A, &coins, &mut psbt);
        assert!(app_finalizes(&app, &mut psbt, f, SignOptions::default()), "{:?}", f);
        assert_valid(&psbt, &coins);
    }
}

#[test]
fn test_d10_taproot_recovery_leaf_finalizes_from_the_transactions_own_nsequence() {
    for f in FINALIZERS {
        let old = coin(liana_tr, "tr-rec-old", 0, 100_000, Some(1_000));
        let coins = [&old];
        let mut app = wallet(liana_tr, None, &coins, TIP);
        let mut psbt = build(&mut app, &coins, Path::Recovery, None);
        assert_eq!(psbt.unsigned_tx.input[0].sequence, Sequence(144));
        device_signs(liana_tr, A, &coins, &mut psbt);
        assert!(app_finalizes(&app, &mut psbt, f, SignOptions::default()), "{:?}", f);
        let tx = assert_valid(&psbt, &coins);
        // The recovery leaf: one signature, the leaf, the control block.
        assert_eq!(tx.input[0].witness.len(), 3);
    }
}

#[test]
fn test_d10_after_path_finalizes_from_the_transactions_own_nlocktime() {
    for f in FINALIZERS {
        let old = coin(after_wsh, "after-path-old", 0, 100_000, Some(1_000));
        let coins = [&old];
        let mut app = wallet(after_wsh, None, &coins, TIP);
        let mut psbt = build(&mut app, &coins, Path::Recovery, None);
        assert!(psbt.unsigned_tx.lock_time.to_consensus_u32() >= 1_000);
        device_signs(after_wsh, A, &coins, &mut psbt);
        assert!(app_finalizes(&app, &mut psbt, f, SignOptions::default()), "{:?}", f);
        assert_valid(&psbt, &coins);
    }
}

#[test]
fn test_d10_unconfirmed_coin_on_the_recovery_path_still_finalizes_without_panicking() {
    for f in FINALIZERS {
        // The upstream panic (#557) that `check_older.rs` covers cannot come back through the
        // finalizer either: it no longer reads coin heights at all.
        let mem = coin(liana_wsh, "rec-mem", 2, 100_000, None);
        let coins = [&mem];
        let mut app = wallet(liana_wsh, None, &coins, TIP);
        let mut psbt = build(&mut app, &coins, Path::Recovery, None);
        device_signs(liana_wsh, A, &coins, &mut psbt);
        assert!(app_finalizes(&app, &mut psbt, f, SignOptions::default()), "{:?}", f);
        assert_valid(&psbt, &coins);
    }
}
