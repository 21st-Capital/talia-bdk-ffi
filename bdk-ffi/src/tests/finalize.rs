//! TALIA FORK: `Wallet::finalize_psbt_keeping_metadata` finalizes the way bdk 0.28's watch-only
//! `sign()` did, and `Wallet::update_psbt_with_descriptor` (vendored bdk_wallet's
//! `repair_psbt_key_origins`) repairs a combined PSBT.
//!
//! Each test signs with a separate signer (as a device would) and finalizes with a watch-only
//! wallet, then compares the whole PSBT field by field.

use crate::bitcoin::{Amount, FeeRate, Network, NetworkKind, OutPoint, Psbt, Script};
use crate::descriptor::Descriptor;
use crate::error::SignerError;
use crate::signer::SignersContainer;
use crate::store::Persister;
use crate::tests::support::{receive, Position};
use crate::tx_builder::TxBuilder;
use crate::types::SignOptions;
use crate::wallet::Wallet;

use bdk_wallet::bitcoin::bip32::Fingerprint;
use bdk_wallet::bitcoin::hashes::hex::FromHex;
use bdk_wallet::bitcoin::psbt::{Input as BdkInput, PsbtSighashType};
use bdk_wallet::bitcoin::{EcdsaSighashType, Psbt as BdkPsbt, ScriptBuf};
use bdk_wallet::KeychainKind;

use std::sync::Arc;

const TPRV: &str = "tprv8ZgxMBicQKsPf2qfrEygW6fdYseJDDrVnDv26PH5BHdvSuG6ecCbHqLVof9yZcMoM31z9ur3tTYbSnr1WBqbGX97CbXcmp5H6qeMpyvx35B";

/// A watch-only wallet (`kind` = "wpkh" or "tr", key origins included) holding two confirmed
/// coins of 60,000 and 50,000 sats, and the signers for its keys, kept apart as on the phone.
struct Vault {
    watch: Arc<Wallet>,
    signers: Vec<Arc<SignersContainer>>,
}

fn vault(kind: &str) -> Vault {
    let secret = |branch: u32| {
        Arc::new(
            Descriptor::new(
                format!("{kind}({TPRV}/86h/1h/0h/{branch}/*)"),
                NetworkKind::Test,
            )
            .unwrap(),
        )
    };
    let public = |descriptor: &Arc<Descriptor>| {
        Arc::new(Descriptor::new(descriptor.to_string(), NetworkKind::Test).unwrap())
    };
    let (external, internal) = (secret(0), secret(1));
    assert!(public(&external)
        .to_string()
        .starts_with(&format!("{kind}([")));
    let watch = Arc::new(
        Wallet::new(
            public(&external),
            public(&internal),
            Network::Regtest,
            Arc::new(Persister::new_in_memory().unwrap()),
            25,
        )
        .unwrap(),
    );
    for (index, value) in [(0, 60_000), (1, 50_000)] {
        receive(
            &watch,
            KeychainKind::External,
            index,
            value,
            Position::Confirmed(100 + index),
            200,
        );
    }
    Vault {
        watch,
        signers: vec![
            Arc::new(SignersContainer::from_descriptor(external)),
            Arc::new(SignersContainer::from_descriptor(internal)),
        ],
    }
}

/// Both coins to someone outside the wallet, with change, unsigned.
fn spend_both(vault: &Vault) -> Arc<Psbt> {
    let coins: Vec<OutPoint> = vault
        .watch
        .list_unspent()
        .unwrap()
        .into_iter()
        .map(|utxo| utxo.outpoint)
        .collect();
    assert_eq!(coins.len(), 2);
    let outside = Arc::new(Script::new(
        Vec::from_hex("0014d85c2b71d0060b09c9886aeb815e50991dda124d").unwrap(),
    ));
    TxBuilder::new()
        .add_utxos(coins)
        .manually_selected_only()
        .add_recipient(&outside, Arc::new(Amount::from_sat(70_000)))
        .fee_rate(&FeeRate::from_sat_per_vb(2).unwrap())
        .finish(&vault.watch)
        .unwrap()
}

/// What the devices hand back: signatures only, nothing finalized.
fn device_signs(vault: &Vault, psbt: &Arc<Psbt>) {
    let options = SignOptions {
        trust_witness_utxo: false,
        assume_height: None,
        allow_all_sighashes: false,
        try_finalize: false,
        sign_with_tap_internal_key: true,
        allow_grinding: true,
    };
    let finalized = vault
        .watch
        .sign_with_signers(Arc::clone(psbt), vault.signers.clone(), Some(options))
        .unwrap();
    assert!(!finalized);
}

fn copy(psbt: &Arc<Psbt>) -> BdkPsbt {
    psbt.working_copy()
}

fn psbt_of(psbt: BdkPsbt) -> Arc<Psbt> {
    Arc::new(psbt.into())
}

/// `before` as 0.28's finalize left a finalized input: final fields written (scriptSig empty for
/// segwit, as 0.28 wrote it), partial_sigs gone, everything else unchanged.
fn finalized_like_028(before: &BdkInput, after: &BdkInput) -> BdkInput {
    let mut expected = before.clone();
    expected.partial_sigs.clear();
    expected.final_script_sig = Some(ScriptBuf::new());
    expected.final_script_witness = after.final_script_witness.clone();
    expected
}

#[test]
fn test_every_input_final_keeps_metadata_and_outputs() {
    let vault = vault("wpkh");
    let psbt = spend_both(&vault);
    device_signs(&vault, &psbt);
    let before = copy(&psbt);
    assert!(before
        .outputs
        .iter()
        .any(|o| !o.bip32_derivation.is_empty()));

    assert!(vault
        .watch
        .finalize_psbt_keeping_metadata(Arc::clone(&psbt), None)
        .unwrap());

    let after = copy(&psbt);
    assert_eq!(after.unsigned_tx, before.unsigned_tx);
    assert_eq!(after.outputs, before.outputs, "outputs untouched");
    for (b, a) in before.inputs.iter().zip(&after.inputs) {
        assert!(!a.final_script_witness.as_ref().unwrap().is_empty());
        assert_eq!(a, &finalized_like_028(b, a));
        assert!(!a.bip32_derivation.is_empty() && a.non_witness_utxo.is_some());
    }

    // Upstream finalize_psbt on the same signed copy: the same transaction, but the inputs keep
    // only their UTXOs and final fields and the outputs lose their derivations.
    let stripped = psbt_of(before);
    assert!(vault
        .watch
        .finalize_psbt(Arc::clone(&stripped), None)
        .unwrap());
    let stripped_copy = copy(&stripped);
    assert!(stripped_copy
        .inputs
        .iter()
        .all(|i| i.bip32_derivation.is_empty()));
    assert!(stripped_copy
        .outputs
        .iter()
        .all(|o| o.bip32_derivation.is_empty()));
    assert_eq!(
        stripped.extract_tx().unwrap().serialize(),
        psbt.extract_tx().unwrap().serialize()
    );
}

// One input signed, the other not (a device that has signed only some inputs): the signed one is
// finalized, the other and every output stay byte for byte as they were.
#[test]
fn test_partial_finalization_leaves_the_unfinished_input_untouched() {
    let vault = vault("wpkh");
    let psbt = spend_both(&vault);
    device_signs(&vault, &psbt);
    let mut half_signed = copy(&psbt);
    half_signed.inputs[1].partial_sigs.clear();
    let before = half_signed.clone();
    let psbt = psbt_of(half_signed);

    assert!(!vault
        .watch
        .finalize_psbt_keeping_metadata(Arc::clone(&psbt), None)
        .unwrap());

    let after = copy(&psbt);
    assert_eq!(
        after.inputs[0],
        finalized_like_028(&before.inputs[0], &after.inputs[0])
    );
    assert!(after.inputs[0].final_script_witness.is_some());
    assert_eq!(
        after.inputs[1], before.inputs[1],
        "unfinished input untouched"
    );
    assert_eq!(after.outputs, before.outputs);

    // Upstream finalize_psbt strips the finalized input even though the PSBT is not done: the
    // copy the next device gets has an input without its key origins.
    let upstream = psbt_of(before);
    assert!(!vault
        .watch
        .finalize_psbt(Arc::clone(&upstream), None)
        .unwrap());
    assert!(copy(&upstream).inputs[0].bip32_derivation.is_empty());
}

// Taproot key path: the key signature, key origins and internal key stay on the finalized input.
#[test]
fn test_taproot_input_keeps_its_key_signature_and_origins() {
    let vault = vault("tr");
    let psbt = spend_both(&vault);
    device_signs(&vault, &psbt);
    let before = copy(&psbt);
    assert!(before.inputs.iter().all(|i| i.tap_key_sig.is_some()));

    assert!(vault
        .watch
        .finalize_psbt_keeping_metadata(Arc::clone(&psbt), None)
        .unwrap());

    let after = copy(&psbt);
    for (b, a) in before.inputs.iter().zip(&after.inputs) {
        assert_eq!(a.tap_key_sig, b.tap_key_sig);
        assert_eq!(a.tap_key_origins, b.tap_key_origins);
        assert!(a.tap_internal_key.is_some());
        assert_eq!(a, &finalized_like_028(b, a));
    }
    assert_eq!(after.outputs, before.outputs);
}

// A slim copy (UTXOs and signatures only, as a single-input rescue copy can be) gets the
// wallet's metadata back before it is finalized.
#[test]
fn test_slim_copy_gets_its_metadata_back() {
    let vault = vault("wpkh");
    let psbt = spend_both(&vault);
    device_signs(&vault, &psbt);
    let full = copy(&psbt);
    let mut slim = full.clone();
    for input in &mut slim.inputs {
        input.bip32_derivation.clear();
    }
    for output in &mut slim.outputs {
        output.bip32_derivation.clear();
    }
    let psbt = psbt_of(slim);

    assert!(vault
        .watch
        .finalize_psbt_keeping_metadata(Arc::clone(&psbt), None)
        .unwrap());

    let after = copy(&psbt);
    for (f, a) in full.inputs.iter().zip(&after.inputs) {
        assert_eq!(a.bip32_derivation, f.bip32_derivation);
    }
    assert_eq!(after.outputs, full.outputs);
}

// The gates of 0.28's sign(), with the options Talia passes (the defaults).
#[test]
fn test_sign_gates_missing_non_witness_utxo_and_non_standard_sighash() {
    let vault = vault("wpkh");
    let psbt = spend_both(&vault);
    device_signs(&vault, &psbt);

    let mut no_parent = copy(&psbt);
    no_parent.inputs[1].non_witness_utxo = None;
    let before = no_parent.clone();
    let no_parent = psbt_of(no_parent);
    assert!(matches!(
        vault
            .watch
            .finalize_psbt_keeping_metadata(Arc::clone(&no_parent), None),
        Err(SignerError::MissingNonWitnessUtxo)
    ));
    assert_eq!(copy(&no_parent), before, "refused before touching anything");
    let trusting = SignOptions {
        trust_witness_utxo: true,
        assume_height: None,
        allow_all_sighashes: false,
        try_finalize: true,
        sign_with_tap_internal_key: true,
        allow_grinding: true,
    };
    assert!(vault
        .watch
        .finalize_psbt_keeping_metadata(no_parent, Some(trusting))
        .unwrap());

    let mut single = copy(&psbt);
    single.inputs[0].sighash_type = Some(PsbtSighashType::from(EcdsaSighashType::Single));
    assert!(matches!(
        vault
            .watch
            .finalize_psbt_keeping_metadata(psbt_of(single), None),
        Err(SignerError::NonStandardSighash)
    ));
}

// Decided 2026-10-17: a finalize that returns an error leaves the caller's copy exactly as
// it came. The metadata update runs before sign()'s gates, so a SLIM copy (the rescue copy: no
// derivations on its inputs or outputs) that a gate then refuses used to come back with the
// vault's derivations written into it. 0.28's sign() left it untouched.
#[test]
fn test_refused_finalize_leaves_a_slim_copy_untouched() {
    let vault = vault("wpkh");
    let psbt = spend_both(&vault);
    device_signs(&vault, &psbt);

    let mut slim = copy(&psbt);
    for input in slim.inputs.iter_mut() {
        input.bip32_derivation.clear();
    }
    for output in slim.outputs.iter_mut() {
        output.bip32_derivation.clear();
    }
    let mut no_parent = slim.clone();
    no_parent.inputs[1].non_witness_utxo = None;
    let before = no_parent.clone();
    let refused = psbt_of(no_parent);
    assert!(matches!(
        vault
            .watch
            .finalize_psbt_keeping_metadata(Arc::clone(&refused), None),
        Err(SignerError::MissingNonWitnessUtxo)
    ));
    assert_eq!(copy(&refused), before, "a refused finalize writes nothing back");

    let mut single = slim.clone();
    single.inputs[0].sighash_type = Some(PsbtSighashType::from(EcdsaSighashType::Single));
    let before = single.clone();
    let refused = psbt_of(single);
    assert!(matches!(
        vault
            .watch
            .finalize_psbt_keeping_metadata(Arc::clone(&refused), None),
        Err(SignerError::NonStandardSighash)
    ));
    assert_eq!(copy(&refused), before, "a refused finalize writes nothing back");

    // The same slim copy, accepted: the metadata goes back in and every input finalizes.
    let accepted = psbt_of(slim);
    assert!(vault
        .watch
        .finalize_psbt_keeping_metadata(Arc::clone(&accepted), None)
        .unwrap());
    let after = copy(&accepted);
    assert!(after.inputs.iter().all(|i| !i.bip32_derivation.is_empty()));
}

// Combining a device's copy lets the device's key origins win; update_psbt_with_descriptor puts
// the vault's own back (wpkh: bip32_derivation).
#[test]
fn test_update_psbt_with_descriptor_repairs_key_origins_after_combine() {
    let vault = vault("wpkh");
    let original = spend_both(&vault);
    let ours = copy(&original).inputs[0].bip32_derivation.clone();
    assert_eq!(ours.len(), 1);

    let device = Arc::new(Psbt::new(original.serialize()).unwrap());
    device_signs(&vault, &device);
    let mut device_copy = copy(&device);
    for (_, (fingerprint, _)) in device_copy.inputs[0].bip32_derivation.iter_mut() {
        *fingerprint = Fingerprint::from([0xde, 0xad, 0xbe, 0xef]);
    }
    let device = psbt_of(device_copy);

    let combined = original.combine(device).unwrap();
    assert_ne!(copy(&combined).inputs[0].bip32_derivation, ours);

    vault
        .watch
        .update_psbt_with_descriptor(Arc::clone(&combined))
        .unwrap();
    assert_eq!(copy(&combined).inputs[0].bip32_derivation, ours);
    assert!(vault
        .watch
        .finalize_psbt_keeping_metadata(combined, None)
        .unwrap());
}

/// Three more test keys, each with its own master fingerprint, so each one is its own signer (a
/// signers container keeps one signer per fingerprint).
const LEAF_TPRVS: [&str; 3] = [
    "tprv8ZgxMBicQKsPd3EupYiPRhaMooHKUHJxNsTfYuScep13go8QFfHdtkG9nRkFGb7busX4isf6X9dURGCoKgitaApQ6MupRhZMcELAxTBRJgS",
    "tprv8ZgxMBicQKsPdpkqS7Eair4YxjcuuvDPNYmKX3sCniCf16tHEVrjjiSXEkFRnUH77yXc6ZcwHHcLNfjdi5qUvw3VDfgYiH5mNsj5izuiu2N",
    "tprv8ZgxMBicQKsPcx5nBGsR63Pe8KnRUqmbJNENAfGftF3yuXoMMoVJJcYeUw5eVkm9WBPjWYt6HMWYJNesB5HaNVBaFc1M6dRjWSYnmewUMYy",
];

/// BIP341's NUMS point: an internal key nobody can sign for, so every spend takes the script path.
const NUMS: &str = "50929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0";

/// A watch-only taproot multisig vault of the shape every new Talia multisig has,
/// `tr(<internal>, multi_a(threshold, A, B, C))`, holding the same two coins, and one signer per
/// leaf key (A, B, C). The internal key is the NUMS point, or a key of the wallet's own (with its
/// origin) when `nums` is false.
fn taproot_multisig_vault(nums: bool, threshold: u32) -> (Arc<Wallet>, Vec<Arc<SignersContainer>>) {
    let path = |branch: u32| format!("86h/1h/0h/{branch}/*");
    let public = |branch: u32| {
        let internal = if nums {
            NUMS.to_string()
        } else {
            format!("{TPRV}/{}", path(branch))
        };
        let [a, b, c] = LEAF_TPRVS.map(|tprv| format!("{tprv}/{}", path(branch)));
        let secret = Descriptor::new(
            format!("tr({internal},multi_a({threshold},{a},{b},{c}))"),
            NetworkKind::Test,
        )
        .unwrap();
        Arc::new(Descriptor::new(secret.to_string(), NetworkKind::Test).unwrap())
    };
    let watch = Arc::new(
        Wallet::new(
            public(0),
            public(1),
            Network::Regtest,
            Arc::new(Persister::new_in_memory().unwrap()),
            25,
        )
        .unwrap(),
    );
    for (index, value) in [(0, 60_000), (1, 50_000)] {
        receive(
            &watch,
            KeychainKind::External,
            index,
            value,
            Position::Confirmed(100 + index),
            200,
        );
    }
    let signers = LEAF_TPRVS
        .iter()
        .map(|tprv| {
            Arc::new(SignersContainer::from_descriptor(Arc::new(
                Descriptor::new(format!("tr({tprv}/{})", path(0)), NetworkKind::Test).unwrap(),
            )))
        })
        .collect();
    (watch, signers)
}

/// A device with these leaf keys signs on the script path only, finalizing nothing.
fn leaf_keys_sign(watch: &Arc<Wallet>, psbt: &Arc<Psbt>, signers: &[Arc<SignersContainer>]) {
    let options = SignOptions {
        trust_witness_utxo: false,
        assume_height: None,
        allow_all_sighashes: false,
        try_finalize: false,
        sign_with_tap_internal_key: false,
        allow_grinding: true,
    };
    assert!(!watch
        .sign_with_signers(Arc::clone(psbt), signers.to_vec(), Some(options))
        .unwrap());
}

fn spend_both_of(watch: &Arc<Wallet>) -> Arc<Psbt> {
    spend_both(&Vault {
        watch: Arc::clone(watch),
        signers: vec![],
    })
}

// Taproot multisig (an internal key of the vault's own plus multi_a in a leaf): a device copy
// whose origins for the internal key and the leaf keys carry another fingerprint, another path
// and no leaf hashes, on an input and on the change output, wins the combine. The repair puts
// back exactly the vault's own origins, leaf hashes included, keeps a key the vault does not
// have and every signature, and the copy then finalizes. Upstream's update (what `sign` runs)
// would only have fixed the internal key: miniscript keeps a leaf key's origin once one is there.
#[test]
fn test_update_psbt_with_descriptor_repairs_taproot_leaf_key_origins_after_combine() {
    use bdk_wallet::bitcoin::bip32::DerivationPath;
    use bdk_wallet::bitcoin::secp256k1::XOnlyPublicKey;
    use std::str::FromStr;

    let (watch, signers) = taproot_multisig_vault(false, 2);
    let original = spend_both_of(&watch);
    let ours = copy(&original);
    let change = ours
        .outputs
        .iter()
        .position(|o| !o.tap_key_origins.is_empty())
        .expect("change output");
    for origins in [
        &ours.inputs[0].tap_key_origins,
        &ours.outputs[change].tap_key_origins,
    ] {
        assert_eq!(origins.len(), 4, "internal key and three leaf keys");
        assert_eq!(
            origins
                .values()
                .filter(|(leaves, _)| leaves.len() == 1)
                .count(),
            3
        );
    }

    let wrong = (
        Fingerprint::from([0xde, 0xad, 0xbe, 0xef]),
        DerivationPath::from_str("m/9/9/9").unwrap(),
    );
    let foreign = XOnlyPublicKey::from_str(NUMS).unwrap();
    let device = Arc::new(Psbt::new(original.serialize()).unwrap());
    leaf_keys_sign(&watch, &device, &signers[..2]);
    let mut device_copy = copy(&device);
    for origins in [
        &mut device_copy.inputs[0].tap_key_origins,
        &mut device_copy.outputs[change].tap_key_origins,
    ] {
        for (leaves, source) in origins.values_mut() {
            leaves.clear();
            *source = wrong.clone();
        }
    }
    device_copy.inputs[0]
        .tap_key_origins
        .insert(foreign, (vec![], wrong.clone()));
    let device = psbt_of(device_copy);

    let combined = original.combine(device).unwrap();
    let broken = copy(&combined);
    for origins in [
        &broken.inputs[0].tap_key_origins,
        &broken.outputs[change].tap_key_origins,
    ] {
        assert!(origins
            .values()
            .all(|entry| entry == &(vec![], wrong.clone())));
    }

    watch
        .update_psbt_with_descriptor(Arc::clone(&combined))
        .unwrap();
    let repaired = copy(&combined);
    let mut expected = ours.inputs[0].tap_key_origins.clone();
    expected.insert(foreign, (vec![], wrong.clone()));
    assert_eq!(repaired.inputs[0].tap_key_origins, expected);
    assert_eq!(
        repaired.inputs[1].tap_key_origins,
        ours.inputs[1].tap_key_origins
    );
    assert_eq!(repaired.outputs, ours.outputs);
    for (r, b) in repaired.inputs.iter().zip(&broken.inputs) {
        assert_eq!(r.tap_script_sigs.len(), 2);
        assert_eq!(r.tap_script_sigs, b.tap_script_sigs, "signatures kept");
    }
    assert!(watch
        .finalize_psbt_keeping_metadata(combined, None)
        .unwrap());
}

// Taproot script path, partial: a 2-of-3 multi_a vault with a NUMS internal key and
// two coins; input 0 carries two leaf signatures, input 1 only one. Input 0 is finalized and keeps
// its tap_script_sigs, tap_scripts and tap_key_origins (it loses only partial_sigs, which taproot
// does not use); input 1 and every output stay byte for byte as they were, so the next device
// still finds its key on both inputs.
#[test]
fn test_taproot_script_path_partial_finalization_keeps_leaf_signatures_and_scripts() {
    let (watch, signers) = taproot_multisig_vault(true, 2);
    let psbt = spend_both_of(&watch);
    let two = Arc::new(Psbt::new(psbt.serialize()).unwrap());
    leaf_keys_sign(&watch, &two, &signers[..2]);
    let one = Arc::new(Psbt::new(psbt.serialize()).unwrap());
    leaf_keys_sign(&watch, &one, &signers[..1]);
    let mut mixed = copy(&two);
    mixed.inputs[1] = copy(&one).inputs[1].clone();
    let before = mixed.clone();
    assert_eq!(before.inputs[0].tap_script_sigs.len(), 2);
    assert_eq!(before.inputs[1].tap_script_sigs.len(), 1);
    for input in &before.inputs {
        assert!(input.tap_key_sig.is_none());
        assert_eq!(input.tap_scripts.len(), 1);
        // The three leaf keys, and the NUMS internal key with an empty path.
        assert_eq!(input.tap_key_origins.len(), 4);
    }
    let psbt = psbt_of(mixed);

    assert!(!watch
        .finalize_psbt_keeping_metadata(Arc::clone(&psbt), None)
        .unwrap());

    let after = copy(&psbt);
    assert_eq!(after.unsigned_tx, before.unsigned_tx);
    assert_eq!(
        after.inputs[0],
        finalized_like_028(&before.inputs[0], &after.inputs[0])
    );
    // Script path: two signatures, one empty slot, the leaf script and its control block.
    assert_eq!(
        after.inputs[0].final_script_witness.as_ref().unwrap().len(),
        5
    );
    assert_eq!(
        after.inputs[1], before.inputs[1],
        "unfinished input untouched"
    );
    assert_eq!(after.outputs, before.outputs, "outputs untouched");

    // Upstream finalize_psbt strips input 0 down to its UTXO and final witness.
    let upstream = psbt_of(before);
    assert!(!watch.finalize_psbt(Arc::clone(&upstream), None).unwrap());
    let stripped = &copy(&upstream).inputs[0];
    assert!(stripped.tap_script_sigs.is_empty() && stripped.tap_key_origins.is_empty());
}
