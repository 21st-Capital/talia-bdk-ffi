//! TALIA FORK: bdk_wallet 3.1.0's `Older::check_older` did
//! `create_height.checked_add(n).expect("Overflowing addition")`, and `finalize_psbt` (also run at
//! the end of `sign`) gives an input spending an UNCONFIRMED coin `create_height = u32::MAX`. So
//! finalizing any PSBT that spends an unconfirmed coin of a vault with an `older()` branch
//! panicked: upstream issue bitcoindevkit/bdk_wallet#557, the crash a production build hit in July
//! 2026 on 0.28 (fixed in Talia's 0.28 library). The vendored copy saturates instead: a coin whose
//! clock has not started simply does not satisfy `older()` yet.

use crate::bitcoin::{Amount, FeeRate, Network, NetworkKind, Psbt};
use crate::descriptor::Descriptor;
use crate::store::Persister;
use crate::tests::support::{apply, funding_tx, Position};
use crate::tx_builder::TxBuilder;
use crate::types::KeychainKind;
use crate::wallet::Wallet;

use std::collections::HashMap;
use std::sync::Arc;

const KEY_A: &str = "tprv8ZgxMBicQKsPf2qfrEygW6fdYseJDDrVnDv26PH5BHdvSuG6ecCbHqLVof9yZcMoM31z9ur3tTYbSnr1WBqbGX97CbXcmp5H6qeMpyvx35B";
const KEY_B: &str = "tpubD6NzVbkrYhZ4XJBfEJ6gt9DiVdfWJijsQTCE3jtXByW3Tk6AVGQ3vL1NNxg3SjB7QkJAuutACCQjrXD8zdZSM1ZmBENszCqy49ECEHmD6rf";

fn wallet(external: String, internal: String) -> Arc<Wallet> {
    Arc::new(
        Wallet::new(
            Arc::new(Descriptor::new(external, NetworkKind::Test).unwrap()),
            Arc::new(Descriptor::new(internal, NetworkKind::Test).unwrap()),
            Network::Regtest,
            Arc::new(Persister::new_in_memory().unwrap()),
            25,
        )
        .unwrap(),
    )
}

/// A one-coin spend to an outside-looking address of the wallet's own (index 9), choosing the
/// first branch of each keychain's policy when it needs a choice.
fn spend(wallet: &Arc<Wallet>, choose_first_branch: bool) -> Arc<Psbt> {
    let to = wallet
        .peek_address(KeychainKind::External, 9)
        .unwrap()
        .address
        .script_pubkey();
    let mut builder = TxBuilder::new()
        .add_recipient(&to, Arc::new(Amount::from_sat(20_000)))
        .fee_rate(&FeeRate::from_sat_per_vb(2).unwrap());
    if choose_first_branch {
        for keychain in [KeychainKind::External, KeychainKind::Internal] {
            let id = wallet.policies(keychain).unwrap().unwrap().id();
            builder = builder.policy_path(HashMap::from([(id, vec![0])]), keychain);
        }
    }
    builder.finish(wallet).unwrap()
}

// Two branches: key A alone, or key B after 6 blocks. The coin is unconfirmed, the build takes
// key A's branch. Finalizing still asks the older(6) branch whether it is satisfied, which is
// where 3.1.0 panicked.
#[test]
fn test_sign_unconfirmed_coin_of_a_timelocked_vault_does_not_panic() {
    let wallet = wallet(
        format!("wsh(or_d(pk({KEY_A}/0/*),and_v(v:pk({KEY_B}/0/*),older(6))))"),
        format!("wsh(or_d(pk({KEY_A}/1/*),and_v(v:pk({KEY_B}/1/*),older(6))))"),
    );
    let coin = funding_tx(&wallet, KeychainKind::External, 0, 100_000);
    apply(&wallet, coin, Position::Unconfirmed(1_700_000_000), 500);

    let psbt = spend(&wallet, true);
    // 3.1.0 as published: panicked "Overflowing addition" here.
    let finalized = wallet.sign(Arc::clone(&psbt), None).unwrap();

    assert!(finalized, "key A's branch needs no timelock");
    assert!(psbt.extract_tx().is_ok());
}

// The same vault, a PSBT nobody has signed yet: finalizing it must say "not finalized" (key A's
// signature is missing, and the older(6) branch is not met by an unconfirmed coin), where 3.1.0
// panicked. Once the coin has 6 blocks on top, the older(6) branch counts as met but still lacks
// key B's signature, so the answer stays "not finalized", and the PSBT is untouched.
#[test]
fn test_finalize_unsigned_spend_of_an_unconfirmed_coin_is_false_not_a_panic() {
    let wallet = wallet(
        format!("wsh(or_d(pk({KEY_A}/0/*),and_v(v:pk({KEY_B}/0/*),older(6))))"),
        format!("wsh(or_d(pk({KEY_A}/1/*),and_v(v:pk({KEY_B}/1/*),older(6))))"),
    );
    let coin = funding_tx(&wallet, KeychainKind::External, 0, 100_000);
    apply(
        &wallet,
        coin.clone(),
        Position::Unconfirmed(1_700_000_000),
        500,
    );

    let psbt = spend(&wallet, true);
    let before = psbt.serialize();
    // 3.1.0 as published: panicked "Overflowing addition" here.
    assert!(!wallet.finalize_psbt(Arc::clone(&psbt), None).unwrap());
    assert_eq!(psbt.serialize(), before);

    apply(&wallet, coin, Position::Confirmed(500), 506);
    assert!(!wallet.finalize_psbt(Arc::clone(&psbt), None).unwrap());
    assert_eq!(psbt.serialize(), before);
}
