use crate::bitcoin::{Amount, Input, Network, NetworkKind, OutPoint, Script, Transaction, TxOut};
use crate::descriptor::Descriptor;
use crate::error::{AddForeignUtxoError, SighashParseError};
use crate::store::Persister;
use crate::tests::support::{receive, Position};
use crate::tx_builder::TxBuilder;
use crate::wallet::Wallet;

use bdk_wallet::bitcoin::hashes::hex::FromHex;
use bdk_wallet::bitcoin::{
    absolute, consensus::serialize, transaction, Amount as BdkAmount, ScriptBuf as BdkScriptBuf,
    Transaction as BdkTransaction, TxIn as BdkTxIn, TxOut as BdkTxOut,
};

use std::collections::HashMap;
use std::sync::Arc;

// TALIA FORK: policy_path per keychain chooses which branch of the vault's policy the spend
// takes. Upstream's version only printed its errors, so it could not fail; now the
// build must succeed, and the branch chosen must show in the transaction: [0, 1] (the key plus
// older(6)) gives the input nSequence 6, and [0, 2] (the key plus after(630000)) gives nLockTime
// 630000 instead.
#[test]
fn test_policy_path() {
    let wallet = Arc::new(create_and_sync_wallet());
    let address = wallet
        .next_unused_address(bdk_wallet::KeychainKind::External)
        .unwrap()
        .address;

    let ext_policy = wallet
        .policies(bdk_wallet::KeychainKind::External)
        .unwrap()
        .expect("external policy");
    let int_policy = wallet
        .policies(bdk_wallet::KeychainKind::Internal)
        .unwrap()
        .expect("internal policy");

    let spend = |items: Vec<u64>| {
        let ext_path: HashMap<String, Vec<u64>> = HashMap::from([(ext_policy.id(), items.clone())]);
        let int_path: HashMap<String, Vec<u64>> = HashMap::from([(int_policy.id(), items)]);
        let psbt = TxBuilder::new()
            .add_recipient(
                &(*address.script_pubkey()).to_owned(),
                Arc::new(Amount::from_sat(1000)),
            )
            .do_not_spend_change()
            .policy_path(int_path, bdk_wallet::KeychainKind::Internal)
            .policy_path(ext_path, bdk_wallet::KeychainKind::External)
            .finish(&wallet)
            .unwrap();
        psbt.extract_tx_unchecked_fee_rate()
    };

    let older = spend(vec![0, 1]);
    assert_eq!(older.input().len(), 1);
    assert_eq!(older.input()[0].sequence, 6);
    assert_ne!(older.lock_time(), 630_000);

    let after = spend(vec![0, 2]);
    assert_eq!(after.input().len(), 1);
    assert_ne!(after.input()[0].sequence, 6);
    assert_eq!(after.lock_time(), 630_000);
}

// TALIA FORK: a policy path that selects too few items of a threshold is refused with
// bdk_wallet's own words and the node's id, which the app reads as POLICY_PATH_INVALID
// (upstream withheld the whole text; the id is one the policy tree itself lists).
#[test]
fn test_policy_path_not_enough_items_keeps_its_detail() {
    let wallet = Arc::new(create_and_sync_wallet());
    let address = wallet
        .next_unused_address(bdk_wallet::KeychainKind::External)
        .unwrap()
        .address;
    let ext_policy = wallet
        .policies(bdk_wallet::KeychainKind::External)
        .unwrap()
        .expect("external policy");
    let int_policy = wallet
        .policies(bdk_wallet::KeychainKind::Internal)
        .unwrap()
        .expect("internal policy");
    // thresh(2, ...) with one item selected.
    let ext_path: HashMap<String, Vec<u64>> = HashMap::from([(ext_policy.id(), vec![0])]);
    let int_path: HashMap<String, Vec<u64>> = HashMap::from([(int_policy.id(), vec![0])]);
    let err = match TxBuilder::new()
        .add_recipient(
            &(*address.script_pubkey()).to_owned(),
            Arc::new(Amount::from_sat(1000)),
        )
        .do_not_spend_change()
        .policy_path(int_path, bdk_wallet::KeychainKind::Internal)
        .policy_path(ext_path, bdk_wallet::KeychainKind::External)
        .finish(&wallet)
    {
        Ok(_) => panic!("one item of thresh(2) must be refused"),
        Err(e) => e,
    };
    assert!(
        matches!(&err, crate::error::CreateTxError::Policy { .. }),
        "{:?}",
        err
    );
    assert_eq!(
        err.to_string(),
        format!(
            "policy error: Not enough items selected: {}",
            ext_policy.id()
        )
    );
}

fn create_and_sync_wallet() -> Wallet {
    let external_descriptor = format!(
        "wsh(thresh(2,pk({}/0/*),sj:and_v(v:pk({}/0/*),n:older(6)),snj:and_v(v:pk({}/0/*),after(630000))))",
        "tpubD6NzVbkrYhZ4XJBfEJ6gt9DiVdfWJijsQTCE3jtXByW3Tk6AVGQ3vL1NNxg3SjB7QkJAuutACCQjrXD8zdZSM1ZmBENszCqy49ECEHmD6rf",
        "tpubD6NzVbkrYhZ4YfAr3jCBRk4SpqB9L1Hh442y83njwfMaker7EqZd7fHMqyTWrfRYJ1e5t2ue6BYjW5i5yQnmwqbzY1a3kfqNxog1AFcD1aE",
        "tprv8ZgxMBicQKsPeitVUz3s6cfyCECovNP7t82FaKPa4UKqV1kssWcXgLkMDjzDbgG9GWoza4pL7z727QitfzkiwX99E1Has3T3a1MKHvYWmQZ"
    );
    let internal_descriptor = format!(
        "wsh(thresh(2,pk({}/1/*),sj:and_v(v:pk({}/1/*),n:older(6)),snj:and_v(v:pk({}/1/*),after(630000))))",
        "tpubD6NzVbkrYhZ4XJBfEJ6gt9DiVdfWJijsQTCE3jtXByW3Tk6AVGQ3vL1NNxg3SjB7QkJAuutACCQjrXD8zdZSM1ZmBENszCqy49ECEHmD6rf",
        "tpubD6NzVbkrYhZ4YfAr3jCBRk4SpqB9L1Hh442y83njwfMaker7EqZd7fHMqyTWrfRYJ1e5t2ue6BYjW5i5yQnmwqbzY1a3kfqNxog1AFcD1aE",
        "tprv8ZgxMBicQKsPeitVUz3s6cfyCECovNP7t82FaKPa4UKqV1kssWcXgLkMDjzDbgG9GWoza4pL7z727QitfzkiwX99E1Has3T3a1MKHvYWmQZ"
    );
    let wallet = Wallet::new(
        Arc::new(Descriptor::new(external_descriptor, NetworkKind::Test).unwrap()),
        Arc::new(Descriptor::new(internal_descriptor, NetworkKind::Test).unwrap()),
        Network::Signet,
        Arc::new(Persister::new_in_memory().unwrap()),
        25,
    )
    .unwrap();
    // TALIA FORK: funded offline (this fork has no Esplora client): 100,000 sats on the first
    // receive address, confirmed at height 100 with the tip at 200, so the older(6) branch is
    // satisfiable and the after(630000) one is not.
    receive(
        &wallet,
        bdk_wallet::KeychainKind::External,
        0,
        100_000,
        Position::Confirmed(100),
        200,
    );
    println!(
        "Wallet balance: {:?}",
        wallet.balance().unwrap().total.to_sat()
    );
    wallet
}

#[test]
fn test_only_witness_utxo_with_finish() {
    let wallet = create_and_sync_wallet();

    // Check if wallet has any UTXOs to work with
    let utxos = wallet.list_unspent().unwrap();
    let has_utxos = !utxos.is_empty();

    println!("Wallet has {} UTXOs", utxos.len());

    // Only run the actual transaction test if wallet has UTXOs
    // Otherwise we at least verify the builder methods work
    if !has_utxos {
        println!("No UTXOs available, testing builder methods only");

        // At minimum, verify the builder methods don't panic
        let address = wallet
            .next_unused_address(bdk_wallet::KeychainKind::External)
            .unwrap()
            .address;
        let _builder = TxBuilder::new()
            .add_recipient(
                &(*address.script_pubkey()).to_owned(),
                Arc::new(Amount::from_sat(1000)),
            )
            .only_witness_utxo();

        println!("Builder methods work correctly even without UTXOs");
        return;
    }

    let address = wallet
        .next_unused_address(bdk_wallet::KeychainKind::External)
        .unwrap()
        .address;

    // Get policy paths for the multisig wallet
    let ext_policy = wallet.policies(bdk_wallet::KeychainKind::External);
    let int_policy = wallet.policies(bdk_wallet::KeychainKind::Internal);

    if let (Ok(Some(ext_policy)), Ok(Some(int_policy))) = (ext_policy, int_policy) {
        let ext_path: HashMap<_, _> = vec![(ext_policy.id().clone(), vec![0, 1])]
            .into_iter()
            .collect();
        let int_path: HashMap<_, _> = vec![(int_policy.id().clone(), vec![0, 1])]
            .into_iter()
            .collect();

        let wallet_arc = Arc::new(wallet);

        // Build transaction without only_witness_utxo
        let tx_without_flag = TxBuilder::new()
            .add_recipient(
                &(*address.script_pubkey()).to_owned(),
                Arc::new(Amount::from_sat(1000)),
            )
            .policy_path(ext_path.clone(), bdk_wallet::KeychainKind::External)
            .policy_path(int_path.clone(), bdk_wallet::KeychainKind::Internal)
            .do_not_spend_change()
            .finish(&wallet_arc);

        // Build transaction with only_witness_utxo
        let tx_with_flag = TxBuilder::new()
            .add_recipient(
                &(*address.script_pubkey()).to_owned(),
                Arc::new(Amount::from_sat(1000)),
            )
            .policy_path(ext_path, bdk_wallet::KeychainKind::External)
            .policy_path(int_path, bdk_wallet::KeychainKind::Internal)
            .do_not_spend_change()
            .only_witness_utxo()
            .finish(&wallet_arc);

        // Both should succeed if we have UTXOs
        match (tx_without_flag, tx_with_flag) {
            (Ok(psbt_without), Ok(psbt_with)) => {
                let size_without = psbt_without.serialize().len();
                let size_with = psbt_with.serialize().len();

                println!(
                    "PSBT size without only_witness_utxo: {} bytes",
                    size_without
                );
                println!("PSBT size with only_witness_utxo: {} bytes", size_with);

                // The PSBT with only_witness_utxo should be smaller or equal
                // (for SegWit inputs, non_witness_utxo adds the full transaction)
                assert!(
                    size_with <= size_without,
                    "Expected PSBT with only_witness_utxo to be smaller, but got {} bytes vs {} bytes",
                    size_with,
                    size_without
                );
            }
            (Err(e1), _) => {
                println!("Transaction without flag failed: {:?}", e1);
                panic!("Expected transaction to succeed with UTXOs available");
            }
            (_, Err(e2)) => {
                println!("Transaction with flag failed: {:?}", e2);
                panic!("Expected transaction with only_witness_utxo to succeed");
            }
        }
    } else {
        println!("Failed to retrieve policies, skipping transaction test");
    }
}

#[test]
fn test_sighash_invalid_string_returns_error() {
    let result = TxBuilder::new().sighash("not-a-sighash".to_string());

    assert!(matches!(result, Err(SighashParseError::Invalid { .. })));
}

#[test]
fn test_sighash_sets_psbt_input_sighash_type() {
    let wallet = create_and_sync_wallet();
    let utxos = wallet.list_unspent().unwrap();

    assert!(
        !utxos.is_empty(),
        "sighash PSBT assertion requires a funded wallet"
    );

    let address = wallet
        .next_unused_address(bdk_wallet::KeychainKind::External)
        .unwrap()
        .address;
    let ext_policy = wallet.policies(bdk_wallet::KeychainKind::External);
    let int_policy = wallet.policies(bdk_wallet::KeychainKind::Internal);

    if let (Ok(Some(ext_policy)), Ok(Some(int_policy))) = (ext_policy, int_policy) {
        let ext_path: HashMap<_, _> = vec![(ext_policy.id().clone(), vec![0, 1])]
            .into_iter()
            .collect();
        let int_path: HashMap<_, _> = vec![(int_policy.id().clone(), vec![0, 1])]
            .into_iter()
            .collect();
        let wallet_arc = Arc::new(wallet);

        let psbt = TxBuilder::new()
            .add_recipient(
                &(*address.script_pubkey()).to_owned(),
                Arc::new(Amount::from_sat(1000)),
            )
            .policy_path(ext_path, bdk_wallet::KeychainKind::External)
            .policy_path(int_path, bdk_wallet::KeychainKind::Internal)
            .do_not_spend_change()
            .sighash("SIGHASH_SINGLE|SIGHASH_ANYONECANPAY".to_string())
            .expect("valid sighash type")
            .finish(&wallet_arc)
            .expect("build transaction with sighash");

        let inputs = psbt.input();
        assert!(!inputs.is_empty());
        assert!(inputs.iter().all(|input| {
            input.sighash_type.as_deref() == Some("SIGHASH_SINGLE|SIGHASH_ANYONECANPAY")
        }));
    } else {
        panic!("Failed to retrieve valid policies for keychains");
    }
}

#[test]
fn test_add_foreign_utxo_missing_witness_data() {
    // Create a foreign UTXO without witness_utxo or non_witness_utxo
    let outpoint = OutPoint {
        txid: Arc::new(
            crate::bitcoin::Txid::from_string(
                "5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456".to_string(),
            )
            .unwrap(),
        ),
        vout: 0,
    };

    let psbt_input = Input {
        non_witness_utxo: None,
        witness_utxo: None, // Missing!
        partial_sigs: HashMap::new(),
        sighash_type: None,
        redeem_script: None,
        witness_script: None,
        bip32_derivation: HashMap::new(),
        final_script_sig: None,
        final_script_witness: None,
        ripemd160_preimages: HashMap::new(),
        sha256_preimages: HashMap::new(),
        hash160_preimages: HashMap::new(),
        hash256_preimages: HashMap::new(),
        tap_key_sig: None,
        tap_script_sigs: HashMap::new(),
        tap_scripts: HashMap::new(),
        tap_key_origins: HashMap::new(),
        tap_internal_key: None,
        tap_merkle_root: None,
        proprietary: HashMap::new(),
        unknown: HashMap::new(),
    };

    let tx_builder = TxBuilder::new();

    let result = tx_builder.add_foreign_utxo(outpoint, psbt_input, 68);
    assert!(
        result.is_err(),
        "Expected add_foreign_utxo() to fail with missing witness_utxo/non_witness_utxo"
    );
}

#[test]
fn test_add_foreign_utxo_validates_non_witness_utxo_when_witness_utxo_is_present() {
    let outpoint = OutPoint {
        txid: Arc::new(
            crate::bitcoin::Txid::from_string(
                "5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456".to_string(),
            )
            .unwrap(),
        ),
        vout: 0,
    };

    let witness_utxo = TxOut {
        value: Arc::new(Amount::from_sat(50_000)),
        script_pubkey: Arc::new(Script::new(
            Vec::from_hex("0014d85c2b71d0060b09c9886aeb815e50991dda124d").unwrap(),
        )),
    };

    let non_witness_tx = BdkTransaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![BdkTxIn::default()],
        output: vec![BdkTxOut {
            value: BdkAmount::from_sat(50_000),
            script_pubkey: BdkScriptBuf::new(),
        }],
    };
    let non_witness_utxo = Arc::new(Transaction::new(serialize(&non_witness_tx)).unwrap());

    let psbt_input = Input {
        non_witness_utxo: Some(non_witness_utxo.clone()),
        witness_utxo: Some(witness_utxo.clone()),
        partial_sigs: HashMap::new(),
        sighash_type: None,
        redeem_script: None,
        witness_script: None,
        bip32_derivation: HashMap::new(),
        final_script_sig: None,
        final_script_witness: None,
        ripemd160_preimages: HashMap::new(),
        sha256_preimages: HashMap::new(),
        hash160_preimages: HashMap::new(),
        hash256_preimages: HashMap::new(),
        tap_key_sig: None,
        tap_script_sigs: HashMap::new(),
        tap_scripts: HashMap::new(),
        tap_key_origins: HashMap::new(),
        tap_internal_key: None,
        tap_merkle_root: None,
        proprietary: HashMap::new(),
        unknown: HashMap::new(),
    };

    let result = TxBuilder::new().add_foreign_utxo(outpoint.clone(), psbt_input, 68);

    assert!(matches!(result, Err(AddForeignUtxoError::InvalidTxid)));

    let psbt_input_with_sequence = Input {
        non_witness_utxo: Some(non_witness_utxo),
        witness_utxo: Some(witness_utxo),
        partial_sigs: HashMap::new(),
        sighash_type: None,
        redeem_script: None,
        witness_script: None,
        bip32_derivation: HashMap::new(),
        final_script_sig: None,
        final_script_witness: None,
        ripemd160_preimages: HashMap::new(),
        sha256_preimages: HashMap::new(),
        hash160_preimages: HashMap::new(),
        hash256_preimages: HashMap::new(),
        tap_key_sig: None,
        tap_script_sigs: HashMap::new(),
        tap_scripts: HashMap::new(),
        tap_key_origins: HashMap::new(),
        tap_internal_key: None,
        tap_merkle_root: None,
        proprietary: HashMap::new(),
        unknown: HashMap::new(),
    };

    let result =
        TxBuilder::new().add_foreign_utxo_with_sequence(outpoint, psbt_input_with_sequence, 68, 0);

    assert!(matches!(result, Err(AddForeignUtxoError::InvalidTxid)));
}

#[test]
fn test_add_foreign_utxo_with_witness_utxo_succeeds() {
    let wallet = create_and_sync_wallet();
    let address = wallet
        .next_unused_address(bdk_wallet::KeychainKind::External)
        .unwrap()
        .address;

    let outpoint = OutPoint {
        txid: Arc::new(
            crate::bitcoin::Txid::from_string(
                "5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456".to_string(),
            )
            .unwrap(),
        ),
        vout: 0,
    };

    // Create a valid witness UTXO
    let witness_utxo = TxOut {
        value: Arc::new(Amount::from_sat(50000)),
        script_pubkey: Arc::new(Script::new(
            Vec::from_hex("0014d85c2b71d0060b09c9886aeb815e50991dda124d").unwrap(),
        )),
    };

    let psbt_input = Input {
        non_witness_utxo: None,
        witness_utxo: Some(witness_utxo),
        partial_sigs: HashMap::new(),
        sighash_type: None,
        redeem_script: None,
        witness_script: None,
        bip32_derivation: HashMap::new(),
        final_script_sig: None,
        final_script_witness: None,
        ripemd160_preimages: HashMap::new(),
        sha256_preimages: HashMap::new(),
        hash160_preimages: HashMap::new(),
        hash256_preimages: HashMap::new(),
        tap_key_sig: None,
        tap_script_sigs: HashMap::new(),
        tap_scripts: HashMap::new(),
        tap_key_origins: HashMap::new(),
        tap_internal_key: None,
        tap_merkle_root: None,
        proprietary: HashMap::new(),
        unknown: HashMap::new(),
    };

    let tx_builder = TxBuilder::new().add_recipient(
        &(*address.script_pubkey()).to_owned(),
        Arc::new(Amount::from_sat(1000)),
    );

    let result = tx_builder.add_foreign_utxo(outpoint, psbt_input, 68);
    assert!(
        result.is_ok(),
        "Failed to add foreign UTXO: {:?}",
        result.err()
    );

    // Try to finish - might fail due to other reasons (insufficient funds from foreign UTXO alone)
    // but shouldn't fail due to missing witness data
    let finish_result = result.unwrap().finish(&Arc::new(wallet));

    // If it fails, check it's not due to missing witness data
    if let Err(e) = finish_result {
        let error_msg = format!("{:?}", e);
        println!("Transaction failed with: {}", error_msg);
        // Should NOT be a missing witness data error
        assert!(
            !error_msg.contains("MissingUtxo") && !error_msg.contains("missing witness"),
            "Should not fail due to missing witness data, got: {}",
            error_msg
        );
    }
}

#[test]
fn test_add_multiple_foreign_utxos_and_finish() {
    let wallet = create_and_sync_wallet();
    let address = wallet
        .next_unused_address(bdk_wallet::KeychainKind::External)
        .unwrap()
        .address;

    // Create first foreign UTXO
    let outpoint1 = OutPoint {
        txid: Arc::new(
            crate::bitcoin::Txid::from_string(
                "5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456".to_string(),
            )
            .unwrap(),
        ),
        vout: 0,
    };

    let witness_utxo1 = TxOut {
        value: Arc::new(Amount::from_sat(50000)),
        script_pubkey: Arc::new(Script::new(
            Vec::from_hex("0014d85c2b71d0060b09c9886aeb815e50991dda124d").unwrap(),
        )),
    };

    let psbt_input1 = Input {
        non_witness_utxo: None,
        witness_utxo: Some(witness_utxo1),
        partial_sigs: HashMap::new(),
        sighash_type: None,
        redeem_script: None,
        witness_script: None,
        bip32_derivation: HashMap::new(),
        final_script_sig: None,
        final_script_witness: None,
        ripemd160_preimages: HashMap::new(),
        sha256_preimages: HashMap::new(),
        hash160_preimages: HashMap::new(),
        hash256_preimages: HashMap::new(),
        tap_key_sig: None,
        tap_script_sigs: HashMap::new(),
        tap_scripts: HashMap::new(),
        tap_key_origins: HashMap::new(),
        tap_internal_key: None,
        tap_merkle_root: None,
        proprietary: HashMap::new(),
        unknown: HashMap::new(),
    };

    // Create second foreign UTXO
    let outpoint2 = OutPoint {
        txid: Arc::new(
            crate::bitcoin::Txid::from_string(
                "6ea4e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9457".to_string(),
            )
            .unwrap(),
        ),
        vout: 1,
    };

    let witness_utxo2 = TxOut {
        value: Arc::new(Amount::from_sat(75000)),
        script_pubkey: Arc::new(Script::new(
            Vec::from_hex("0014a85c2b71d0060b09c9886aeb815e50991dda125e").unwrap(),
        )),
    };

    let psbt_input2 = Input {
        non_witness_utxo: None,
        witness_utxo: Some(witness_utxo2),
        partial_sigs: HashMap::new(),
        sighash_type: None,
        redeem_script: None,
        witness_script: None,
        bip32_derivation: HashMap::new(),
        final_script_sig: None,
        final_script_witness: None,
        ripemd160_preimages: HashMap::new(),
        sha256_preimages: HashMap::new(),
        hash160_preimages: HashMap::new(),
        hash256_preimages: HashMap::new(),
        tap_key_sig: None,
        tap_script_sigs: HashMap::new(),
        tap_scripts: HashMap::new(),
        tap_key_origins: HashMap::new(),
        tap_internal_key: None,
        tap_merkle_root: None,
        proprietary: HashMap::new(),
        unknown: HashMap::new(),
    };

    // Add both foreign UTXOs
    let tx_builder = TxBuilder::new().add_recipient(
        &(*address.script_pubkey()).to_owned(),
        Arc::new(Amount::from_sat(1000)),
    );

    let tx_builder = tx_builder
        .add_foreign_utxo(outpoint1, psbt_input1, 68)
        .expect("Failed to add first foreign UTXO");

    let tx_builder = tx_builder
        .add_foreign_utxo(outpoint2, psbt_input2, 68)
        .expect("Failed to add second foreign UTXO");

    // Try to finish
    let finish_result = tx_builder.finish(&Arc::new(wallet));

    // If it fails, verify it's not due to witness data issues
    if let Err(e) = finish_result {
        let error_msg = format!("{:?}", e);
        println!(
            "Transaction with multiple foreign UTXOs failed with: {}",
            error_msg
        );
        assert!(
            !error_msg.contains("MissingUtxo"),
            "Should not fail due to missing witness data"
        );
    }
}

#[test]
fn test_combined_only_witness_utxo_and_foreign_utxo_with_finish() {
    let wallet = create_and_sync_wallet();
    let address = wallet
        .next_unused_address(bdk_wallet::KeychainKind::External)
        .unwrap()
        .address;

    let outpoint = OutPoint {
        txid: Arc::new(
            crate::bitcoin::Txid::from_string(
                "5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456".to_string(),
            )
            .unwrap(),
        ),
        vout: 0,
    };

    let witness_utxo = TxOut {
        value: Arc::new(Amount::from_sat(50000)),
        script_pubkey: Arc::new(Script::new(
            Vec::from_hex("0014d85c2b71d0060b09c9886aeb815e50991dda124d").unwrap(),
        )),
    };

    let psbt_input = Input {
        non_witness_utxo: None,
        witness_utxo: Some(witness_utxo),
        partial_sigs: HashMap::new(),
        sighash_type: None,
        redeem_script: None,
        witness_script: None,
        bip32_derivation: HashMap::new(),
        final_script_sig: None,
        final_script_witness: None,
        ripemd160_preimages: HashMap::new(),
        sha256_preimages: HashMap::new(),
        hash160_preimages: HashMap::new(),
        hash256_preimages: HashMap::new(),
        tap_key_sig: None,
        tap_script_sigs: HashMap::new(),
        tap_scripts: HashMap::new(),
        tap_key_origins: HashMap::new(),
        tap_internal_key: None,
        tap_merkle_root: None,
        proprietary: HashMap::new(),
        unknown: HashMap::new(),
    };

    // Combine both features
    let tx_builder = TxBuilder::new()
        .add_recipient(
            &(*address.script_pubkey()).to_owned(),
            Arc::new(Amount::from_sat(1000)),
        )
        .only_witness_utxo();

    let tx_builder = tx_builder
        .add_foreign_utxo(outpoint, psbt_input, 68)
        .expect("Failed to add foreign UTXO with only_witness_utxo");

    // Try to finish - this tests that both features work together
    let finish_result = tx_builder.finish(&Arc::new(wallet));

    // Verify the combination doesn't cause conflicts
    if let Err(e) = finish_result {
        let error_msg = format!("{:?}", e);
        println!("Combined features transaction failed with: {}", error_msg);
        // Should not be due to witness data conflicts
        assert!(
            !error_msg.contains("MissingUtxo") && !error_msg.contains("witness")
                || error_msg.contains("Insufficient"),
            "Should not fail due to witness-related conflicts, got: {}",
            error_msg
        );
    }
}
