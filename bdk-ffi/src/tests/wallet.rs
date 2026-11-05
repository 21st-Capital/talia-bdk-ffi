use crate::bitcoin::{Amount, BlockHash, FeeRate, Network, NetworkKind};
use crate::descriptor::Descriptor;
use crate::error::{CreateTxError, LoadWithPersistError};
use crate::signer::SignersContainer;
use crate::store::Persister;
use crate::tx_builder::{BumpFeeTxBuilder, TxBuilder};
use crate::types::{UnconfirmedTx, Update};
use crate::wallet::{CreateParams, LoadParams, Wallet};

use bdk_wallet::bitcoin::Amount as BdkAmount;
use bdk_wallet::bitcoin::Transaction as BdkTransaction;
use bdk_wallet::bitcoin::{absolute, transaction, TxOut as BdkTxOut};
use bdk_wallet::KeychainKind;

use std::sync::Arc;

const EXTERNAL_DESCRIPTOR: &str = "wpkh(tprv8ZgxMBicQKsPf2qfrEygW6fdYseJDDrVnDv26PH5BHdvSuG6ecCbHqLVof9yZcMoM31z9ur3tTYbSnr1WBqbGX97CbXcmp5H6qeMpyvx35B/84h/1h/1h/0/*)";
const INTERNAL_DESCRIPTOR: &str = "wpkh(tprv8ZgxMBicQKsPf2qfrEygW6fdYseJDDrVnDv26PH5BHdvSuG6ecCbHqLVof9yZcMoM31z9ur3tTYbSnr1WBqbGX97CbXcmp5H6qeMpyvx35B/84h/1h/1h/1/*)";
const TWO_PATH_DESCRIPTOR: &str = "wpkh([9a6a2580/84'/1'/0']tpubDDnGNapGEY6AZAdQbfRJgMg9fvz8pUBrLwvyvUqEgcUfgzM6zc2eVK4vY9x9L5FJWdX8WumXuLEDV5zDZnTfbn87vLe9XceCFwTu9so9Kks/<0;1>/*)";
const EXPECTED_FIRST_ADDRESS: &str = "tb1qhjys9wxlfykmte7ftryptx975uqgd6kcm6a7z4";

fn external_descriptor() -> Arc<Descriptor> {
    Arc::new(Descriptor::new(EXTERNAL_DESCRIPTOR.to_string(), NetworkKind::Test).unwrap())
}

fn internal_descriptor() -> Arc<Descriptor> {
    Arc::new(Descriptor::new(INTERNAL_DESCRIPTOR.to_string(), NetworkKind::Test).unwrap())
}

fn two_path_descriptor() -> Arc<Descriptor> {
    Arc::new(Descriptor::new(TWO_PATH_DESCRIPTOR.to_string(), NetworkKind::Test).unwrap())
}

fn custom_genesis_hash() -> Arc<BlockHash> {
    Arc::new(
        BlockHash::from_string(
            "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        )
        .unwrap(),
    )
}

fn build_wallet() -> Wallet {
    Wallet::new(
        external_descriptor(),
        internal_descriptor(),
        Network::Signet,
        Arc::new(Persister::new_in_memory().unwrap()),
        25,
    )
    .unwrap()
}

fn funded_wallet() -> Wallet {
    let wallet = Wallet::new(
        external_descriptor(),
        internal_descriptor(),
        Network::Regtest,
        Arc::new(Persister::new_in_memory().unwrap()),
        25,
    )
    .unwrap();

    let address = wallet
        .reveal_next_address(KeychainKind::External)
        .unwrap()
        .address;
    let funding_tx = BdkTransaction {
        version: transaction::Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![],
        output: vec![BdkTxOut {
            value: BdkAmount::from_sat(76_000),
            script_pubkey: address.script_pubkey().0.clone(),
        }],
    };
    let txid = funding_tx.compute_txid();
    let mut update = bdk_wallet::Update::default();
    update.last_active_indices.insert(KeychainKind::External, 0);
    update.tx_update.txs.push(Arc::new(funding_tx));
    update.tx_update.seen_ats.insert((txid, 1));

    wallet.apply_update(Arc::new(Update(update))).unwrap();

    wallet
}

#[test]
fn test_tx_builder_invalid_current_height_returns_error() {
    let wallet = Arc::new(funded_wallet());
    let recipient_script = wallet
        .next_unused_address(KeychainKind::External)
        .unwrap()
        .address
        .script_pubkey();

    let result = TxBuilder::new()
        .add_recipient(&recipient_script, Arc::new(Amount::from_sat(10_000)))
        .current_height(500_000_000)
        .finish(&wallet);

    assert!(matches!(
        result,
        Err(CreateTxError::LockTimeConversionError)
    ));
}

#[test]
fn test_bump_fee_tx_builder_invalid_current_height_returns_error() {
    let wallet = Arc::new(funded_wallet());
    let recipient_script = wallet
        .next_unused_address(KeychainKind::External)
        .unwrap()
        .address
        .script_pubkey();
    let original_tx = TxBuilder::new()
        .add_recipient(&recipient_script, Arc::new(Amount::from_sat(10_000)))
        .finish(&wallet)
        .unwrap()
        .extract_tx()
        .unwrap();

    assert!(original_tx.is_explicitly_rbf());

    let txid = original_tx.compute_txid();
    wallet
        .apply_unconfirmed_txs(vec![UnconfirmedTx {
            tx: original_tx,
            last_seen: 2,
        }])
        .unwrap();

    let result = BumpFeeTxBuilder::new(txid, Arc::new(FeeRate::from_sat_per_vb(2).unwrap()))
        .current_height(500_000_000)
        .finish(&wallet);

    assert!(matches!(
        result,
        Err(CreateTxError::LockTimeConversionError)
    ));
}

#[test]
fn test_create_wallet_with_params_sets_custom_genesis_hash() {
    let genesis_hash = custom_genesis_hash();
    let params = CreateParams {
        genesis_hash: Some(Arc::clone(&genesis_hash)),
        lookahead: 25,
        use_spk_cache: true,
    };

    let wallet = Wallet::create_with_params(
        external_descriptor(),
        internal_descriptor(),
        Network::Signet,
        Arc::new(Persister::new_in_memory().unwrap()),
        params,
    )
    .unwrap();

    assert_eq!(wallet.network().unwrap(), Network::Signet);
    assert_eq!(wallet.latest_checkpoint().unwrap().hash, genesis_hash);
}

#[test]
fn test_load_wallet_with_params_checks_network_and_genesis_hash() {
    let persister = Arc::new(Persister::new_in_memory().unwrap());
    let genesis_hash = custom_genesis_hash();
    let create_params = CreateParams {
        genesis_hash: Some(Arc::clone(&genesis_hash)),
        lookahead: 25,
        use_spk_cache: true,
    };

    Wallet::create_with_params(
        external_descriptor(),
        internal_descriptor(),
        Network::Signet,
        Arc::clone(&persister),
        create_params,
    )
    .unwrap();

    let load_params = LoadParams {
        check_network: Some(Network::Signet),
        check_genesis_hash: Some(Arc::clone(&genesis_hash)),
        lookahead: 25,
        use_spk_cache: true,
    };
    let wallet = Wallet::load_with_params(
        external_descriptor(),
        internal_descriptor(),
        Arc::clone(&persister),
        load_params,
    )
    .unwrap();

    assert_eq!(wallet.network().unwrap(), Network::Signet);
    assert_eq!(wallet.latest_checkpoint().unwrap().hash, genesis_hash);

    let mismatched_params = LoadParams {
        check_network: Some(Network::Bitcoin),
        check_genesis_hash: Some(custom_genesis_hash()),
        lookahead: 25,
        use_spk_cache: true,
    };
    let error = match Wallet::load_with_params(
        external_descriptor(),
        internal_descriptor(),
        persister,
        mismatched_params,
    ) {
        Ok(_) => panic!("loading with mismatched network should fail"),
        Err(error) => error,
    };

    match error {
        LoadWithPersistError::InvalidChangeSet { error_message } => {
            assert!(error_message.contains("Network mismatch"));
        }
        error => panic!("expected InvalidChangeSet error, got {:?}", error),
    }
}

#[test]
fn test_create_wallet() {
    let wallet = build_wallet();

    assert_eq!(wallet.network().unwrap(), Network::Signet);
    assert_eq!(wallet.balance().unwrap().total.to_sat(), 0u64);
    assert!(wallet.list_unspent().unwrap().is_empty());
    assert_eq!(
        wallet.derivation_index(KeychainKind::External).unwrap(),
        None
    );
    assert_eq!(
        wallet
            .next_derivation_index(KeychainKind::External)
            .unwrap(),
        0
    );
}

#[test]
fn test_keychains() {
    let wallet = build_wallet();

    let keychains = wallet.keychains().unwrap();

    assert_eq!(keychains.len(), 2);

    let external = keychains
        .iter()
        .find(|keychain| keychain.keychain == KeychainKind::External)
        .unwrap();
    let internal = keychains
        .iter()
        .find(|keychain| keychain.keychain == KeychainKind::Internal)
        .unwrap();
    let external_public_descriptor = external.public_descriptor.to_string();
    let internal_public_descriptor = internal.public_descriptor.to_string();

    assert_eq!(
        external_public_descriptor,
        wallet.public_descriptor(KeychainKind::External).unwrap()
    );
    assert_eq!(
        internal_public_descriptor,
        wallet.public_descriptor(KeychainKind::Internal).unwrap()
    );
    assert!(external.public_descriptor.has_wildcard());
    assert!(internal.public_descriptor.has_wildcard());
    assert!(!external_public_descriptor.contains("tprv"));
    assert!(!internal_public_descriptor.contains("tprv"));
}

#[test]
fn test_reveal_next_address() {
    let wallet = build_wallet();

    let address_info = wallet.reveal_next_address(KeychainKind::External).unwrap();

    assert_eq!(address_info.index, 0);
    assert_eq!(address_info.keychain, KeychainKind::External);
    assert_eq!(address_info.address.to_string(), EXPECTED_FIRST_ADDRESS);
}

#[test]
fn test_signers_container_from_descriptor() {
    let secret_descriptor = external_descriptor();
    let public_descriptor = secret_descriptor.as_public();
    let secret_signers = SignersContainer::from_descriptor(Arc::clone(&secret_descriptor));
    let public_signers = SignersContainer::from_descriptor(Arc::clone(&public_descriptor));

    assert_eq!(public_descriptor.to_string(), secret_descriptor.to_string());
    assert_eq!(
        public_descriptor.to_string_with_secret(),
        secret_descriptor.to_string()
    );
    assert!(!secret_signers.is_empty());
    assert_eq!(secret_signers.len(), 1);
    assert!(public_signers.is_empty());
    assert_eq!(public_signers.len(), 0);
}

#[test]
fn test_sign_with_signers() {
    let wallet = Arc::new(funded_wallet());
    let recipient_script = wallet
        .next_unused_address(KeychainKind::External)
        .unwrap()
        .address
        .script_pubkey();
    let psbt = TxBuilder::new()
        .add_recipient(&recipient_script, Arc::new(Amount::from_sat(10_000)))
        .finish(&wallet)
        .unwrap();
    let signers = vec![Arc::new(SignersContainer::from_descriptor(
        external_descriptor(),
    ))];

    let finalized = wallet
        .sign_with_signers(Arc::clone(&psbt), signers, None)
        .unwrap();
    let signed_tx = psbt.extract_tx().unwrap();

    assert!(finalized);
    assert!(!signed_tx.input()[0].witness.is_empty());
}

#[test]
fn test_sign_with_signers_for_public_wallet() {
    let external_signer_descriptor = external_descriptor();
    let external_public_descriptor = external_signer_descriptor.as_public();
    let internal_public_descriptor = internal_descriptor().as_public();
    let wallet = Arc::new(
        Wallet::new(
            Arc::clone(&external_public_descriptor),
            internal_public_descriptor,
            Network::Regtest,
            Arc::new(Persister::new_in_memory().unwrap()),
            25,
        )
        .unwrap(),
    );

    let address = wallet
        .reveal_next_address(KeychainKind::External)
        .unwrap()
        .address;
    let funding_tx = BdkTransaction {
        version: transaction::Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![],
        output: vec![BdkTxOut {
            value: BdkAmount::from_sat(76_000),
            script_pubkey: address.script_pubkey().0.clone(),
        }],
    };
    let txid = funding_tx.compute_txid();
    let mut update = bdk_wallet::Update::default();
    update.last_active_indices.insert(KeychainKind::External, 0);
    update.tx_update.txs.push(Arc::new(funding_tx));
    update.tx_update.seen_ats.insert((txid, 1));
    wallet.apply_update(Arc::new(Update(update))).unwrap();

    let recipient_script = wallet
        .next_unused_address(KeychainKind::External)
        .unwrap()
        .address
        .script_pubkey();
    let psbt = TxBuilder::new()
        .add_recipient(&recipient_script, Arc::new(Amount::from_sat(10_000)))
        .finish(&wallet)
        .unwrap();
    let signers = vec![Arc::new(SignersContainer::from_descriptor_with_context(
        external_signer_descriptor,
        external_public_descriptor,
    ))];

    let finalized = wallet
        .sign_with_signers(Arc::clone(&psbt), signers, None)
        .unwrap();
    let signed_tx = psbt.extract_tx().unwrap();

    assert!(finalized);
    assert!(!signed_tx.input()[0].witness.is_empty());
}

#[test]
fn test_create_single_wallet() {
    let wallet = Wallet::create_single(
        external_descriptor(),
        Network::Signet,
        Arc::new(Persister::new_in_memory().unwrap()),
        25,
    )
    .unwrap();

    assert_eq!(
        wallet.derivation_index(KeychainKind::External).unwrap(),
        None
    );

    let keychains = wallet.keychains().unwrap();

    assert_eq!(keychains.len(), 1);
    assert_eq!(keychains[0].keychain, KeychainKind::External);
    let public_descriptor = keychains[0].public_descriptor.to_string();
    assert_eq!(
        public_descriptor,
        wallet.public_descriptor(KeychainKind::External).unwrap()
    );
    assert!(keychains[0].public_descriptor.has_wildcard());
    assert!(!public_descriptor.contains("tprv"));

    let address_info = wallet.reveal_next_address(KeychainKind::External).unwrap();

    assert_eq!(address_info.index, 0);
    assert_eq!(address_info.keychain, KeychainKind::External);
    assert_eq!(address_info.address.to_string(), EXPECTED_FIRST_ADDRESS);
    assert_eq!(
        wallet.derivation_index(KeychainKind::External).unwrap(),
        Some(0)
    );
    assert_eq!(
        wallet
            .next_derivation_index(KeychainKind::External)
            .unwrap(),
        1
    );
}

#[test]
fn test_create_two_path_wallet() {
    let wallet = Wallet::create_from_two_path_descriptor(
        two_path_descriptor(),
        Network::Signet,
        Arc::new(Persister::new_in_memory().unwrap()),
        25,
    )
    .unwrap();

    assert_eq!(
        wallet.derivation_index(KeychainKind::External).unwrap(),
        None
    );
    assert_eq!(
        wallet.derivation_index(KeychainKind::Internal).unwrap(),
        None
    );

    let external_address = wallet.reveal_next_address(KeychainKind::External).unwrap();
    let internal_address = wallet.reveal_next_address(KeychainKind::Internal).unwrap();

    assert_eq!(external_address.index, 0);
    assert_eq!(external_address.keychain, KeychainKind::External);
    assert_eq!(internal_address.index, 0);
    assert_eq!(internal_address.keychain, KeychainKind::Internal);
    assert_ne!(
        external_address.address.to_string(),
        internal_address.address.to_string()
    );
    assert_eq!(
        wallet.derivation_index(KeychainKind::External).unwrap(),
        Some(0)
    );
    assert_eq!(
        wallet.derivation_index(KeychainKind::Internal).unwrap(),
        Some(0)
    );
}

#[test]
fn test_load_from_two_path_descriptor() {
    let persister = Arc::new(Persister::new_in_memory().unwrap());
    let wallet = Wallet::create_from_two_path_descriptor(
        two_path_descriptor(),
        Network::Signet,
        Arc::clone(&persister),
        25,
    )
    .unwrap();

    wallet.reveal_next_address(KeychainKind::External).unwrap();
    wallet.reveal_next_address(KeychainKind::Internal).unwrap();
    assert!(wallet.persist(Arc::clone(&persister)).unwrap());

    let loaded_wallet =
        Wallet::load_from_two_path_descriptor(two_path_descriptor(), Arc::clone(&persister), 25)
            .unwrap();

    assert_eq!(loaded_wallet.network().unwrap(), Network::Signet);
    assert_eq!(
        loaded_wallet
            .derivation_index(KeychainKind::External)
            .unwrap(),
        Some(0)
    );
    assert_eq!(
        loaded_wallet
            .derivation_index(KeychainKind::Internal)
            .unwrap(),
        Some(0)
    );
    assert_eq!(
        loaded_wallet
            .next_derivation_index(KeychainKind::External)
            .unwrap(),
        1
    );
    assert_eq!(
        loaded_wallet
            .next_derivation_index(KeychainKind::Internal)
            .unwrap(),
        1
    );
}

#[test]
fn test_load_from_two_path_descriptor_with_params() {
    let persister = Arc::new(Persister::new_in_memory().unwrap());
    Wallet::create_from_two_path_descriptor(
        two_path_descriptor(),
        Network::Signet,
        Arc::clone(&persister),
        25,
    )
    .unwrap();

    let params = LoadParams {
        check_network: Some(Network::Bitcoin),
        check_genesis_hash: None,
        lookahead: 25,
        use_spk_cache: false,
    };
    let error = match Wallet::load_from_two_path_descriptor_with_params(
        two_path_descriptor(),
        persister,
        params,
    ) {
        Ok(_) => panic!("loading with mismatched network should fail"),
        Err(error) => error,
    };

    match error {
        LoadWithPersistError::InvalidChangeSet { error_message } => {
            assert!(error_message.contains("Network mismatch"));
        }
        error => panic!("expected InvalidChangeSet error, got {:?}", error),
    }
}

// TALIA FORK: a panic inside a call that holds the wallet must leave the app running. uniffi
// catches the panic (panic = "unwind") and the poisoned wallet lock then turns every later call
// into an error, never a second panic or an abort.
#[test]
fn test_panic_inside_finish_then_balance_returns_an_error_not_an_abort() {
    use crate::error::{
        CalculateFeeError, CannotConnectError, DescriptorError, PersistenceError, SignerError,
        WalletError,
    };

    let wallet = Arc::new(funded_wallet());
    let recipient_script = wallet
        .peek_address(KeychainKind::External, 5)
        .unwrap()
        .address
        .script_pubkey();
    let builder =
        TxBuilder::new().add_recipient(&recipient_script, Arc::new(Amount::from_sat(10_000)));
    let unsigned = builder.finish(&wallet).unwrap();
    assert_eq!(wallet.balance().unwrap().total.to_sat(), 76_000);

    // The panic goes through uniffi's own wrapper, the one every exported call runs in on the
    // phone: it must come back as an "unexpected error" status, with the process still alive.
    crate::tx_builder::tests::panic_inside_next_finish();
    let mut status = uniffi::RustCallStatus::default();
    uniffi::rust_call(
        &mut status,
        std::panic::AssertUnwindSafe(|| {
            let _ = builder.finish(&wallet);
            Ok(())
        }),
    );
    assert_eq!(status.code, uniffi::RustCallStatusCode::UnexpectedError);

    // Every call the bridge makes now returns an error instead of panicking again.
    assert!(matches!(wallet.balance(), Err(WalletError::WalletPoisoned)));
    assert!(matches!(
        wallet.is_mine(Arc::clone(&recipient_script)),
        Err(WalletError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.list_unspent(),
        Err(WalletError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.transactions(),
        Err(WalletError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.peek_address(KeychainKind::External, 0),
        Err(WalletError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.reveal_next_address(KeychainKind::External),
        Err(WalletError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.reveal_addresses_to(KeychainKind::Internal, 3),
        Err(WalletError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.derivation_index(KeychainKind::External),
        Err(WalletError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.latest_checkpoint(),
        Err(WalletError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.start_full_scan(),
        Err(WalletError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.apply_update(Arc::new(Update(bdk_wallet::Update::default()))),
        Err(CannotConnectError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.policies(KeychainKind::External),
        Err(DescriptorError::WalletPoisoned)
    ));
    assert!(matches!(
        builder.finish(&wallet),
        Err(CreateTxError::WalletPoisoned)
    ));
    let unsigned_tx = unsigned.extract_tx_unchecked_fee_rate();
    assert!(matches!(
        wallet.calculate_fee(&unsigned_tx),
        Err(CalculateFeeError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.finalize_psbt(Arc::clone(&unsigned), None),
        Err(SignerError::WalletPoisoned)
    ));
    assert!(matches!(
        wallet.persist(Arc::new(Persister::new_in_memory().unwrap())),
        Err(PersistenceError::WalletPoisoned)
    ));
}

// TALIA FORK: the PSBT the app holds survives a panic inside a call that works on it. Wallet
// methods that change a PSBT work on a copy and store it back only when bdk_wallet returns, so a
// panic in between poisons the wallet but leaves the app's PSBT unlocked and exactly as it was
// (upstream held the PSBT's own lock while bdk_wallet ran: a panic poisoned it too, and every
// later read of it panicked again).
#[test]
fn test_panic_inside_finalize_leaves_the_apps_psbt_untouched_and_usable() {
    use crate::error::SignerError;
    use crate::types::SignOptions;

    let wallet = Arc::new(funded_wallet());
    let recipient_script = wallet
        .peek_address(KeychainKind::External, 5)
        .unwrap()
        .address
        .script_pubkey();
    let psbt = TxBuilder::new()
        .add_recipient(&recipient_script, Arc::new(Amount::from_sat(10_000)))
        .finish(&wallet)
        .unwrap();
    let sign_only = SignOptions {
        trust_witness_utxo: false,
        assume_height: None,
        allow_all_sighashes: false,
        try_finalize: false,
        sign_with_tap_internal_key: true,
        allow_grinding: true,
    };
    assert!(!wallet.sign(Arc::clone(&psbt), Some(sign_only)).unwrap());
    let signed = psbt.working_copy();
    let signed_serialized = psbt.serialize();
    assert!(signed
        .inputs
        .iter()
        .all(|input| !input.partial_sigs.is_empty()));

    // bdk_wallet finalizes the copy, then the panic comes before it is stored back.
    crate::wallet::tests::panic_inside_next_finalize();
    let mut status = uniffi::RustCallStatus::default();
    uniffi::rust_call(
        &mut status,
        std::panic::AssertUnwindSafe(|| {
            let _ = wallet.finalize_psbt_keeping_metadata(Arc::clone(&psbt), None);
            Ok(())
        }),
    );
    assert_eq!(status.code, uniffi::RustCallStatusCode::UnexpectedError);
    assert!(matches!(
        wallet.finalize_psbt_keeping_metadata(Arc::clone(&psbt), None),
        Err(SignerError::WalletPoisoned)
    ));

    // The PSBT is not poisoned and not half finalized: every read works and gives what it gave
    // before the call.
    assert_eq!(psbt.serialize(), signed_serialized);
    assert_eq!(psbt.input().len(), 1);
    assert!(psbt.input()[0].final_script_witness.is_none());
    assert_eq!(psbt.working_copy(), signed);
    // A new wallet for the same keys, as the bridge makes after a panic, finalizes it.
    let fresh = Arc::new(funded_wallet());
    assert!(fresh
        .finalize_psbt_keeping_metadata(Arc::clone(&psbt), None)
        .unwrap());
    assert!(psbt.extract_tx().is_ok());
}

#[test]
fn test_peek_address_above_bip32_max_is_an_error_not_a_panic() {
    use crate::error::WalletError;

    let wallet = build_wallet();
    assert!(matches!(
        wallet.peek_address(KeychainKind::External, 0x8000_0000),
        Err(WalletError::IndexOutOfRange { index: 0x8000_0000 })
    ));
    // Upstream panicked here while holding the wallet; this wallet is still usable.
    assert_eq!(
        wallet
            .peek_address(KeychainKind::External, 0x7FFF_FFFF)
            .unwrap()
            .index,
        0x7FFF_FFFF
    );
    assert_eq!(
        wallet
            .peek_address(KeychainKind::External, 0)
            .unwrap()
            .address
            .to_string(),
        EXPECTED_FIRST_ADDRESS
    );
}

// TALIA FORK: Persister::new_in_memory keeps the aggregate changeset in memory (no SQLite). A
// wallet loaded back from it must see the same coins, transactions and revealed addresses.
#[test]
fn test_in_memory_persister_round_trips_the_wallet() {
    use crate::tests::support::{receive, Position};

    let persister = Arc::new(Persister::new_in_memory().unwrap());
    let wallet = Wallet::new(
        external_descriptor(),
        internal_descriptor(),
        Network::Regtest,
        Arc::clone(&persister),
        25,
    )
    .unwrap();
    receive(
        &wallet,
        KeychainKind::External,
        0,
        50_000,
        Position::Confirmed(10),
        20,
    );
    receive(
        &wallet,
        KeychainKind::External,
        3,
        7_000,
        Position::Unconfirmed(1_700_000_123),
        20,
    );
    assert!(wallet.persist(Arc::clone(&persister)).unwrap());
    // Nothing new to persist the second time.
    assert!(!wallet.persist(Arc::clone(&persister)).unwrap());

    let loaded = Wallet::load(external_descriptor(), internal_descriptor(), persister, 25).unwrap();
    let balance = loaded.balance().unwrap();
    assert_eq!(balance.confirmed.to_sat(), 50_000);
    assert_eq!(balance.untrusted_pending.to_sat(), 7_000);
    assert_eq!(loaded.transactions().unwrap().len(), 2);
    assert_eq!(loaded.list_unspent().unwrap().len(), 2);
    assert_eq!(
        loaded.derivation_index(KeychainKind::External).unwrap(),
        Some(3)
    );
    assert_eq!(loaded.latest_checkpoint().unwrap().height, 20);
}

// TALIA FORK: not enough coins is CreateTxError::InsufficientFunds with both amounts in sats,
// not a CoinSelection error with the amounts only in its text.
#[test]
fn test_insufficient_funds_carries_the_amounts_in_sats() {
    let wallet = Arc::new(funded_wallet());
    let recipient_script = wallet
        .peek_address(KeychainKind::External, 5)
        .unwrap()
        .address
        .script_pubkey();

    let result = TxBuilder::new()
        .add_recipient(&recipient_script, Arc::new(Amount::from_sat(100_000)))
        .fee_rate(&FeeRate::from_sat_per_vb(2).unwrap())
        .finish(&wallet);

    match result {
        Err(CreateTxError::InsufficientFunds { needed, available }) => {
            assert_eq!(available, 76_000);
            // The payment plus the fee for at least its output and the transaction's overhead.
            assert!(needed > 100_000 && needed < 101_000, "needed {}", needed);
            assert_eq!(
                CreateTxError::InsufficientFunds { needed, available }.to_string(),
                format!("insufficient funds: needed {needed} sat, available 76000 sat")
            );
        }
        Err(other) => panic!("expected InsufficientFunds, got {:?}", other),
        Ok(_) => panic!("expected InsufficientFunds, got a transaction"),
    }
}
