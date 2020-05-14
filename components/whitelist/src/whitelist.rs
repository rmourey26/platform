#![deny(warnings)]
use curve25519_dalek::scalar::Scalar;
use ledger::data_model::errors::PlatformError;
use ledger::data_model::AssetTypeCode;
use ledger::error_location;
use serde::{Deserialize, Serialize};
use txn_cli::txn_lib::query_utxo_and_get_type_commitment;
use zei::crypto::whitelist::{prove_array_membership, verify_array_membership};
use zei::xfr::structs::asset_type_to_scalar;

const PROTOCOL: &str = "http";
const HOST: &str = "localhost";

/// Code of whitelisted assets
pub type WhiteListedCode = Scalar;

/// Asset whitelist
#[derive(Default, Deserialize, Serialize)]
pub struct Whitelist {
  /// List of whitelisted asset codes
  // TODO (Keyao): Make this a merkle tree instead?
  pub members: Vec<WhiteListedCode>,
}

impl Whitelist {
  // Adds an asset code to the whitelist
  pub fn add_member(&mut self, code: AssetTypeCode) {
    self.members.push(asset_type_to_scalar(&code.val));
  }

  /// Proves and verifies the whitelist membership of a confidential asset transferred in a transaction.
  /// # Arguments
  /// * `index`: index in the whitelist.
  /// * `utxo`: UTXO SID of the transaction.
  /// * `blind`: blinding factor for the asset type commitment.
  pub fn prove_and_verify_membership(&mut self,
                                     index: u64,
                                     utxo: u64,
                                     blind: Scalar)
                                     -> Result<(), PlatformError> {
    let commitment = query_utxo_and_get_type_commitment(utxo, PROTOCOL, HOST)?;
    let proof = prove_array_membership(&self.members, index as usize, &commitment, &blind).or_else(|e| Err(PlatformError::ZeiError(error_location!(), e)))?;
    verify_array_membership(&self.members, &commitment, &proof).or_else(|e| Err(PlatformError::ZeiError(error_location!(), e)))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use ledger::data_model::AssetRules;
  use ledger_standalone::LedgerStandalone;
  use rand_chacha::ChaChaRng;
  use rand_core::SeedableRng;
  use txn_cli::txn_lib::define_issue_transfer_and_get_utxo_and_blinds;
  use zei::xfr::asset_record::AssetRecordType;
  use zei::xfr::sig::XfrKeyPair;

  // Ignoring tests below as they fail due to the below validation in ledger/src/store/effects.rs:
  //
  // if let XfrAssetType::Confidential(_) = out.asset_type {
  //   return Err(PlatformError::InputsError(error_location!()));
  // }
  //
  // To test the functionalities of whitelist proof:
  // * Comment out the validation in ledger/src/store/effects.rs
  // * Run the tests with -- --ignored
  // * Verify the test results
  // * Restore the validation in ledger/src/store/effects.rs
  //
  // (Issue #320)

  #[should_panic]
  #[test]
  #[ignore]
  // (Issue #320)
  fn test_prove_membership_incorrect_index() {
    // Start the standalone ledger
    let ledger_standalone = &LedgerStandalone::new();
    ledger_standalone.poll_until_ready().unwrap();

    // Generate asset codes and add codes to the whitelist
    let codes = vec![AssetTypeCode::gen_random(), AssetTypeCode::gen_random()];
    let whitelist = &mut Whitelist::default();
    for code in &codes {
      whitelist.add_member(*code);
    }

    // Transfer the third asset, and get the UTXO SID and asset type blind
    let prng = &mut ChaChaRng::from_entropy();
    let (utxo_2, _, blind_2) = define_issue_transfer_and_get_utxo_and_blinds(&XfrKeyPair::generate(&mut ChaChaRng::from_entropy()),
                                                                      &XfrKeyPair::generate(&mut ChaChaRng::from_entropy()),
                                                                      10,
                                                                      codes[2],
                                                                      AssetRules::default(),
                                                                      AssetRecordType::NonConfidentialAmount_ConfidentialAssetType,
                                                                      ledger_standalone,
                                                                      prng).unwrap();

    // Prove the whitelist memberships of the second asset with the incorrect index
    // Should panic
    whitelist.prove_and_verify_membership(1, utxo_2, blind_2)
             .unwrap();
  }

  #[should_panic]
  #[test]
  #[ignore]
  // (Issue #320)
  fn test_prove_membership_incorrect_utxo() {
    // Start the standalone ledger
    let ledger_standalone = &LedgerStandalone::new();
    ledger_standalone.poll_until_ready().unwrap();

    // Generate asset codes and add codes to the whitelist
    let codes = vec![AssetTypeCode::gen_random(), AssetTypeCode::gen_random()];
    let whitelist = &mut Whitelist::default();
    for code in &codes {
      whitelist.add_member(*code);
    }

    // Transfer the second and third asset, and get the UTXO SIDs and asset type blinds
    let prng = &mut ChaChaRng::from_entropy();
    let (utxo_1, _, _) = define_issue_transfer_and_get_utxo_and_blinds(&XfrKeyPair::generate(&mut ChaChaRng::from_entropy()),
    &XfrKeyPair::generate(&mut ChaChaRng::from_entropy()),
    10,
    codes[1],
    AssetRules::default(),
    AssetRecordType::NonConfidentialAmount_ConfidentialAssetType,
    ledger_standalone,
    prng).unwrap();
    let (_, _, blind_2) = define_issue_transfer_and_get_utxo_and_blinds(&XfrKeyPair::generate(&mut ChaChaRng::from_entropy()),
                                                                      &XfrKeyPair::generate(&mut ChaChaRng::from_entropy()),
                                                                      10,
                                                                      codes[2],
                                                                      AssetRules::default(),
                                                                      AssetRecordType::NonConfidentialAmount_ConfidentialAssetType,
                                                                      ledger_standalone,
                                                                      prng).unwrap();

    // Prove the whitelist memberships of the second asset with the incorrect UTXO SID
    // Should panic
    whitelist.prove_and_verify_membership(2, utxo_1, blind_2)
             .unwrap();
  }

  #[should_panic]
  #[test]
  #[ignore]
  // (Issue #320)
  fn test_prove_membership_incorrect_blind() {
    // Start the standalone ledger
    let ledger_standalone = &LedgerStandalone::new();
    ledger_standalone.poll_until_ready().unwrap();

    // Generate asset codes and add codes to the whitelist
    let codes = vec![AssetTypeCode::gen_random(), AssetTypeCode::gen_random()];
    let whitelist = &mut Whitelist::default();
    for code in &codes {
      whitelist.add_member(*code);
    }

    // Transfer the second and third assets, and get the UTXO SIDs and asset type blinds
    let prng = &mut ChaChaRng::from_entropy();
    let (_, _, blind_1) = define_issue_transfer_and_get_utxo_and_blinds(&XfrKeyPair::generate(&mut ChaChaRng::from_entropy()),
    &XfrKeyPair::generate(&mut ChaChaRng::from_entropy()),
    10,
    codes[1],
    AssetRules::default(),
    AssetRecordType::NonConfidentialAmount_ConfidentialAssetType,
    ledger_standalone,
    prng).unwrap();
    let (utxo_2, _, _) = define_issue_transfer_and_get_utxo_and_blinds(&XfrKeyPair::generate(&mut ChaChaRng::from_entropy()),
                                                                      &XfrKeyPair::generate(&mut ChaChaRng::from_entropy()),
                                                                      10,
                                                                      codes[2],
                                                                      AssetRules::default(),
                                                                      AssetRecordType::NonConfidentialAmount_ConfidentialAssetType,
                                                                      ledger_standalone,
                                                                      prng).unwrap();

    // Prove the whitelist memberships of the second asset with the incorrect UTXO SID
    // Should panic
    whitelist.prove_and_verify_membership(2, utxo_2, blind_1)
             .unwrap();
  }

  #[test]
  #[ignore]
  // (Issue #320)
  fn test_prove_and_verify_membership() {
    // Start the standalone ledger
    let ledger_standalone = &LedgerStandalone::new();
    ledger_standalone.poll_until_ready().unwrap();

    // Generate key pair and asset codes
    let issuer_key_pair = XfrKeyPair::generate(&mut ChaChaRng::from_entropy());
    let recipient_key_pair = XfrKeyPair::generate(&mut ChaChaRng::from_entropy());
    let codes = vec![AssetTypeCode::gen_random(),
                     AssetTypeCode::gen_random(),
                     AssetTypeCode::gen_random(),
                     AssetTypeCode::gen_random(),
                     AssetTypeCode::gen_random()];

    // Add codes to the whitelist
    let whitelist = &mut Whitelist::default();
    for code in &codes {
      whitelist.add_member(*code);
    }
    assert_eq!(whitelist.members.len(), 5);

    // Transfer the second and third assets, and get the UTXO SIDs and asset type blinds
    let prng = &mut ChaChaRng::from_entropy();
    let (utxo_1, _, blind_1) = define_issue_transfer_and_get_utxo_and_blinds(&issuer_key_pair,
                                                                     &recipient_key_pair,
                                                                     10,
                                                                     codes[1],
                                                                     AssetRules::default(),
                                                                     AssetRecordType::NonConfidentialAmount_ConfidentialAssetType,
                                                                     ledger_standalone,
                                                                     prng).unwrap();

    let (utxo_2, _, blind_2) = define_issue_transfer_and_get_utxo_and_blinds(&issuer_key_pair,
                                                                      &recipient_key_pair,
                                                                      10,
                                                                      codes[2],
                                                                      AssetRules::default(),
                                                                      AssetRecordType::NonConfidentialAmount_ConfidentialAssetType,
                                                                      ledger_standalone,
                                                                      prng).unwrap();

    // Prove and verify the whitelist memberships of the second and third assets
    assert!(whitelist.prove_and_verify_membership(1, utxo_1, blind_1)
                     .is_ok());
    assert!(whitelist.prove_and_verify_membership(2, utxo_2, blind_2)
                     .is_ok());
  }
}