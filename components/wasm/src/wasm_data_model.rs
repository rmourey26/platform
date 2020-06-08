#![deny(warnings)]
use crate::util::error_to_jsvalue;
use credentials::{
  CredCommitment, CredIssuerPublicKey, CredIssuerSecretKey, CredPoK, CredRevealSig, CredSignature,
  CredUserPublicKey, CredUserSecretKey, Credential as PlatformCredential,
};
use ledger::data_model::{
  b64dec, AssetRules as PlatformAssetRules, KVBlind as PlatformKVBlind, KVHash as PlatformKVHash,
  SignatureRules as PlatformSignatureRules, TransferType as PlatformTransferType, TxOutput,
  TxoRef as PlatformTxoRef, TxoSID,
};
use rand_chacha::ChaChaRng;
use rand_core::{RngCore, SeedableRng};
use serde::{Deserialize, Serialize};
use utils::HashOf;
use wasm_bindgen::prelude::*;
use zei::xfr::asset_tracer::gen_asset_tracer_keypair;
use zei::xfr::sig::XfrPublicKey;
use zei::xfr::structs::{
  AssetTracerDecKeys, AssetTracerEncKeys, AssetTracerKeyPair as ZeiAssetTracerKeyPair,
  AssetTracingPolicy, BlindAssetRecord, OwnerMemo as ZeiOwnerMemo,
};

#[wasm_bindgen]
pub struct TxoRef {
  pub(crate) txo_ref: PlatformTxoRef,
}

#[wasm_bindgen]
impl TxoRef {
  /// Creates a relative txo reference as a JSON string. Relative txo references are offset
  /// backwards from the operation they appear in -- 0 is the most recent, (n-1) is the first output
  /// of the transaction.
  ///
  /// Use relative txo indexing when referring to outputs of intermediate operations (e.g. a
  /// transaction containing both an issuance and a transfer).
  ///
  /// # Arguments
  /// @param {BigInt} idx -  Relative Txo (transaction output) SID.
  pub fn relative(idx: u64) -> Self {
    TxoRef { txo_ref: PlatformTxoRef::Relative(idx) }
  }

  /// Creates an absolute transaction reference as a JSON string.
  ///
  /// Use absolute txo indexing when referring to an output that has been assigned a utxo index (i.e.
  /// when the utxo has been committed to the ledger in an earlier transaction).
  ///
  /// # Arguments
  /// @param {BigInt} idx -  Txo (transaction output) SID.
  pub fn absolute(idx: u64) -> Self {
    TxoRef { txo_ref: PlatformTxoRef::Absolute(TxoSID(idx)) }
  }
}

impl TxoRef {
  pub fn get_txo(&self) -> &PlatformTxoRef {
    &self.txo_ref
  }
}

#[wasm_bindgen]
pub struct TransferType {
  transfer_type: PlatformTransferType,
}

#[wasm_bindgen]
impl TransferType {
  /// Standard TransferType variant for txn builder.
  /// Returns a token as a string signifying that the Standard policy should be used when evaluating the transaction.
  pub fn standard_transfer_type() -> Self {
    TransferType { transfer_type: PlatformTransferType::Standard }
  }

  /// Debt swap TransferType variant for txn builder.
  /// Returns a token as a string signifying that the DebtSwap policy should be used when evaluating the transaction.
  pub fn debt_transfer_type() -> Self {
    TransferType { transfer_type: PlatformTransferType::DebtSwap }
  }
}

impl TransferType {
  pub fn get_type(&self) -> &PlatformTransferType {
    &self.transfer_type
  }
}

#[wasm_bindgen]
pub struct ClientAssetRecord {
  pub(crate) output: TxOutput,
}

#[wasm_bindgen]
#[derive(Serialize, Deserialize)]
pub struct AssetTracerKeyPair {
  pub(crate) keypair: ZeiAssetTracerKeyPair,
}

#[wasm_bindgen]
impl AssetTracerKeyPair {
  pub fn new() -> Self {
    let mut small_rng = ChaChaRng::from_entropy();
    AssetTracerKeyPair { keypair: gen_asset_tracer_keypair(&mut small_rng) }
  }
}
impl Default for AssetTracerKeyPair {
  fn default() -> Self {
    Self::new()
  }
}

impl AssetTracerKeyPair {
  pub fn get_enc_key(&self) -> &AssetTracerEncKeys {
    &self.keypair.enc_key
  }

  pub fn get_dec_key(&self) -> &AssetTracerDecKeys {
    &self.keypair.dec_key
  }

  pub fn get_keys(&self) -> &ZeiAssetTracerKeyPair {
    &self.keypair
  }
}

#[wasm_bindgen]
pub struct OwnerMemo {
  pub(crate) memo: ZeiOwnerMemo,
}

impl ClientAssetRecord {
  pub fn get_bar_ref(&self) -> &BlindAssetRecord {
    &self.output.0
  }
}

#[wasm_bindgen]
impl ClientAssetRecord {
  /// Builds a client record from an asset record fetched from the ledger server.
  /// @param {record} - JSON asset record fetched from server.
  pub fn from_json_record(record: &JsValue) -> Self {
    ClientAssetRecord { output: TxOutput(record.into_serde().unwrap()) }
  }
}

impl OwnerMemo {
  pub fn get_memo_ref(&self) -> &ZeiOwnerMemo {
    &self.memo
  }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct AttributeDefinition {
  pub name: String,
  pub size: usize,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct AttributeAssignment {
  pub name: String,
  pub val: String,
}

#[wasm_bindgen]
#[derive(Serialize, Deserialize)]
pub struct CredentialUserKeyPair {
  pub(crate) pk: CredUserPublicKey,
  pub(crate) sk: CredUserSecretKey,
}

#[wasm_bindgen]
#[derive(Serialize, Deserialize)]
pub struct CredentialIssuerKeyPair {
  pub(crate) pk: CredIssuerPublicKey,
  pub(crate) sk: CredIssuerSecretKey,
}

#[wasm_bindgen]
#[derive(Serialize, Deserialize)]
pub struct CredentialSignature {
  pub(crate) sig: CredSignature,
}

#[wasm_bindgen]
#[derive(Serialize, Deserialize)]
pub struct CredentialRevealSig {
  pub(crate) sig: CredRevealSig,
}

#[wasm_bindgen]
#[derive(Serialize, Deserialize)]
pub struct CredentialCommitment {
  pub(crate) commitment: CredCommitment,
  pub(crate) pok: CredPoK,
}

#[wasm_bindgen]
#[derive(Serialize, Deserialize)]
pub struct Credential {
  pub(crate) credential: PlatformCredential,
}

impl CredentialCommitment {
  pub fn get_commitment_ref(&self) -> &CredCommitment {
    &self.commitment
  }
  pub fn get_pok_ref(&self) -> &CredPoK {
    &self.pok
  }
}

impl CredentialSignature {
  pub fn get_sig_ref(&self) -> &CredSignature {
    &self.sig
  }
}

impl Credential {
  pub fn get_cred_ref(&self) -> &PlatformCredential {
    &self.credential
  }
}

impl CredentialRevealSig {
  pub fn get_sig_ref(&self) -> &CredRevealSig {
    &self.sig
  }
}

#[wasm_bindgen]
impl CredentialIssuerKeyPair {
  pub fn get_pk(&self) -> CredIssuerPublicKey {
    self.pk.clone()
  }
  pub fn get_sk(&self) -> CredIssuerSecretKey {
    self.sk.clone()
  }
  pub fn to_jsvalue(&self) -> JsValue {
    JsValue::from_serde(&self).unwrap()
  }
  pub fn from_jsvalue(val: &JsValue) -> Self {
    val.into_serde().unwrap()
  }
}

#[wasm_bindgen]
impl CredentialUserKeyPair {
  pub fn get_pk(&self) -> CredUserPublicKey {
    self.pk.clone()
  }
  pub fn get_sk(&self) -> CredUserSecretKey {
    self.sk.clone()
  }
  pub fn serialize(&self) -> String {
    serde_json::to_string(&self).unwrap()
  }
  pub fn to_jsvalue(&self) -> JsValue {
    JsValue::from_serde(&self).unwrap()
  }
  pub fn from_jsvalue(val: &JsValue) -> Self {
    val.into_serde().unwrap()
  }
}

#[wasm_bindgen]
pub struct SignatureRules {
  pub(crate) sig_rules: PlatformSignatureRules,
}

#[wasm_bindgen]
/// Creates a new set of co-signature rules.
///
/// @param {BigInt} threshold - Minimum sum of signature weights that is required for an asset
/// transfer.
/// @param {JsValue} weights - Array of public key weights of the form `[["kAb...", BigInt(5)]]', where the
/// first element of each tuple is a base64 encoded public key and the second is the key's
/// associated weight.
impl SignatureRules {
  pub fn new(threshold: u64, weights: JsValue) -> Result<SignatureRules, JsValue> {
    let weights: Vec<(String, u64)> = weights.into_serde().map_err(error_to_jsvalue)?;
    let weights: Vec<(XfrPublicKey, u64)> =
      weights.iter()
             .map(|(b64_key, weight)| {
               let parsed = crate::util::public_key_from_base64(b64_key.clone());
               match parsed {
                 Err(err) => Err(err),
                 Ok(pk) => Ok((pk, *weight)),
               }
             })
             .collect::<Result<Vec<(XfrPublicKey, u64)>, JsValue>>()?;
    let sig_rules = PlatformSignatureRules { threshold, weights };
    Ok(SignatureRules { sig_rules })
  }
}

#[wasm_bindgen]
#[derive(Default)]
pub struct TracingPolicy {
  policy: AssetTracingPolicy,
}

#[wasm_bindgen]
impl TracingPolicy {
  pub fn new_with_tracking(tracing_key: &AssetTracerEncKey) -> Self {
    let policy = AssetTracingPolicy { enc_keys: tracing_key.get_enc_key().clone(),
                                      asset_tracking: true,
                                      identity_tracking: None };
    TracingPolicy { policy }
  }

  pub fn new_with_identity_tracking(tracing_key: &AssetTracerEncKey,
                                    cred_issuer_key: &CredIssuerPublicKey,
                                    reveal_map: JsValue,
                                    tracking: bool)
                                    -> Result<Self, JsValue> {
    let reveal_map: Vec<bool> = reveal_map.into_serde().map_err(error_to_jsvalue)?;
    let policy = AssetTracingPolicy {enc_keys: tracking_key.get_
  }
}

#[wasm_bindgen]
#[derive(Default)]
pub struct AssetRules {
  pub(crate) rules: PlatformAssetRules,
}

#[wasm_bindgen]
impl AssetRules {
  /// Create a default set of asset rules.
  pub fn new() -> AssetRules {
    AssetRules::default()
  }

  /// Toggles asset traceability.
  /// @param {TracingPolicy} policy - Tracing policy for the new asset.
  pub fn set_tracing(mut self, policy: &TracingPolicy) -> AssetRules {
    self.rules.traceable = Some(policy.get_policy_ref().clone());
    self
  }

  /// Set a cap on the number of units of this asset that can be issued.
  /// @param {BigInt} max_units - Maximum number of units that can be issued.
  pub fn set_max_units(mut self, max_units: u64) -> AssetRules {
    self.rules.max_units = Some(max_units);
    self
  }

  /// Transferability toggle. Assets that are not transferable can only be transferred by the asset
  /// issuer.
  /// @param {bool} transferable - Boolean indicating whether asset can be transferred.
  pub fn set_transferable(mut self, transferable: bool) -> AssetRules {
    self.rules.transferable = transferable;
    self
  }

  /// The updatable flag determines whether the asset memo can be updated after issuance.
  /// @param {bool} updatable - Boolean indicating whether asset memo can be updated.
  pub fn set_updatable(mut self, updatable: bool) -> AssetRules {
    self.rules.updatable = updatable;
    self
  }

  /// Co-signature rules. Assets with co-signatue rules require additional weighted signatures to
  /// be transferred.
  /// @param {SignatureRules} multisig_rules - Co-signature restrictions.
  pub fn set_transfer_multisig_rules(mut self, multisig_rules: SignatureRules) -> AssetRules {
    self.rules.transfer_multisig_rules = Some(multisig_rules.sig_rules);
    self
  }
}

#[wasm_bindgen]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct KVBlind {
  pub(crate) blind: PlatformKVBlind,
}

#[wasm_bindgen]
impl KVBlind {
  pub fn gen_random() -> Self {
    let mut small_rng = ChaChaRng::from_entropy();
    let mut buf: [u8; 16] = [0u8; 16];
    small_rng.fill_bytes(&mut buf);
    KVBlind { blind: PlatformKVBlind(buf) }
  }
}

impl KVBlind {
  pub fn get_blind_ref(&self) -> &PlatformKVBlind {
    &self.blind
  }
}

#[wasm_bindgen]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct KVHash {
  pub(crate) hash: PlatformKVHash,
}

#[wasm_bindgen]
impl KVHash {
  pub fn new_no_blind(data: &str) -> Self {
    KVHash { hash: PlatformKVHash(HashOf::new(&(b64dec(data).as_ref().unwrap().to_vec(), None))) }
  }

  pub fn new_with_blind(data: &str, kv_blind: &KVBlind) -> Self {
    KVHash { hash: PlatformKVHash(HashOf::new(&(b64dec(data).as_ref().unwrap().to_vec(),
                                                Some(kv_blind.get_blind_ref().clone())))) }
  }
}

impl KVHash {
  pub fn get_hash(self) -> PlatformKVHash {
    self.hash
  }
}
