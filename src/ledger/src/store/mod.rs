use crate::{
    data_model::*,
    staking::{Staking, FRA_TOTAL_AMOUNT},
};
use bitmap::{BitMap, SparseMap};
use bnc::{
    helper::Value, mapx::Mapx, new_mapx, new_vecx, vecx::ValueMut as VecxValueMut,
    vecx::Vecx,
};
use cryptohash::sha256::Digest as BitDigest;
use log::info;
use merkle_tree::append_only_merkle::AppendOnlyMerkle;
use rand_chacha::ChaChaRng;
use rand_core::{CryptoRng, RngCore, SeedableRng};
use ruc::*;
use serde::{Deserialize, Serialize};
use sliding_set::SlidingSet;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter},
    mem,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};
use utils::{HashOf, ProofOf, SignatureOf};
use zei::{
    setup::PublicParams,
    xfr::{
        asset_record::{build_blind_asset_record, AssetRecordType},
        lib::XfrNotePolicies,
        sig::{XfrKeyPair, XfrPublicKey},
        structs::{
            AssetRecordTemplate, OwnerMemo, TracingPolicies, TracingPolicy, XfrAssetType,
        },
    },
};

const TRANSACTION_WINDOW_WIDTH: u64 = 128;

pub struct SnapshotId {
    pub id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LoggedBlock {
    pub block: Vec<Transaction>,
    pub state: StateCommitmentData,
}

const MAX_VERSION: usize = 100;

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct LedgerStatus {
    // Paths to archival logs for the merkle tree and transaction history
    block_merkle_path: String,
    txn_merkle_path: String,
    utxo_map_path: String,

    // All currently-unspent TXOs
    utxos: Mapx<TxoSID, Utxo>,

    owned_utxos: Mapx<XfrPublicKey, HashSet<TxoSID>>,

    // All spent TXOs
    // pub spent_utxos: HashMap<TxoSID, Utxo>,
    pub spent_utxos: Mapx<TxoSID, Utxo>,

    // Map a TXO to its output position in a transaction
    txo_to_txn_location: Mapx<TxoSID, (TxnSID, OutputPosition)>,

    // Digests of the UTXO bitmap to (I think -joe) track recent states of
    // the UTXO map
    // TODO(joe): should this be an ordered map of some sort?
    utxo_map_versions: VecDeque<(TxnSID, BitDigest)>,

    // State commitment history. The BitDigest at index i is the state commitment of the ledger at block height  i + 1.
    state_commitment_versions: Vecx<HashOf<Option<StateCommitmentData>>>,

    // Registered asset types, and one-more-than the most recently issued
    // sequence number. Issuance numbers must be increasing over time to
    // prevent replays, but (as far as I know -joe) need not be strictly
    // sequential.
    asset_types: Mapx<AssetTypeCode, AssetType>,
    // Tracing policy for each asset type
    tracing_policies: Mapx<AssetTypeCode, TracingPolicy>,
    issuance_num: Mapx<AssetTypeCode, u64>,
    // Issuance amounts for assets with limits
    issuance_amounts: Mapx<AssetTypeCode, u64>,

    // Should be equal to the count of transactions
    next_txn: Vecx<TxnSID>,
    // Should be equal to the count of TXOs
    next_txo: Vecx<TxoSID>,

    // Hash and sequence number of the most recent "full checkpoint" of the
    // ledger -- committing to the whole ledger history up to the most recent
    // such checkpoint.
    state_commitment_data: Option<StateCommitmentData>,
    block_commit_count: u64,

    // cumulative consensus specific counter, up to the current block.
    // Updated when applying next block. Always 0 if consensus does not need it,
    // for tendermint with no empty blocks flag, it will go up by exactly 1
    // each time there is a lull in transactions. For tendermint without the flag,
    // it will go up by 1 once a second (by default) unless there is a transaction.

    // Hash of the transactions in the most recent block
    txns_in_block_hash: Option<HashOf<Vec<Transaction>>>,

    // Sliding window of operations for replay attack prevention
    sliding_set: SlidingSet<[u8; 8]>,

    staking: Vecx<Staking>,

    //staking of effective trading blocks
    backup_staking: Vecx<Staking>,
    // tendermint commit height
    td_commit_height: Vecx<u64>,
    // flags that can be flush when a block has a tx
    is_effective_block: bool,
}

pub struct LedgerState {
    status: LedgerStatus,

    // PRNG used for transaction validation
    prng: ChaChaRng,

    // Key pair used for signing the state commitment
    // TODO(joe): update this to the generic zei signing API when it exists
    signing_key: XfrKeyPair,

    // Merkle tree tracing the sequence of transaction hashes in the block
    // Each appended hash is the hash of transactions in the same block
    block_merkle: AppendOnlyMerkle,
    // Merkle tree tracing the sequence of all transaction hashes
    // Each appended hash is the hash of a transaction
    txn_merkle: AppendOnlyMerkle,

    // The `FinalizedTransaction`s consist of a Transaction and an index into
    // `merkle` representing its hash.
    // TODO(joe): should this be in-memory?
    pub blocks: Vecx<FinalizedBlock>,

    // <tx id> => [<block id>, <tx idx in block>]
    pub tx_to_block_location: Mapx<TxnSID, [usize; 2]>,

    // Bitmap tracing all the live TXOs
    utxo_map: BitMap,

    block_ctx: Option<BlockEffect>,
}

impl LedgerStatus {
    fn fast_invariant_check(&self) -> Result<()> {
        if self.block_commit_count != self.state_commitment_versions.len() as u64 {
            println!(
                "{}: {}",
                self.block_commit_count,
                self.state_commitment_versions.len()
            );
            return Err(eg!());
        }
        if self
            .state_commitment_data
            .as_ref()
            .map(|x| x.compute_commitment())
            != self
                .state_commitment_versions
                .last()
                .map(|v| v.deref().clone())
        {
            return Err(eg!());
        }
        Ok(())
    }
}

impl LedgerState {
    fn fast_invariant_check(&self) -> Result<()> {
        self.status.fast_invariant_check().c(d!())
    }
}

impl LedgerStatus {
    pub fn new(
        block_merkle_path: &str,
        txn_merkle_path: &str,
        utxo_map_path: &str,
        states_data_path: &str,
    ) -> Result<LedgerStatus> {
        let utxos_path = states_data_path.to_string() + "/utxo";
        let spent_utxos_path = states_data_path.to_string() + "/spent_utxos";
        let txo_to_txn_location_path =
            states_data_path.to_string() + "/txo_to_txn_location";
        let issuance_amounts_path = states_data_path.to_string() + "/issuance_amounts";
        let state_commitment_versions_path =
            states_data_path.to_string() + "/state_commitment_versions";
        let asset_types_path = states_data_path.to_string() + "/asset_types";
        let tracing_policies_path = states_data_path.to_string() + "/tracing_policies";
        let issuance_num_path = states_data_path.to_string() + "/issuance_num";
        let next_txn_path = states_data_path.to_string() + "/next_txn";
        let next_txo_path = states_data_path.to_string() + "/next_txo";
        let staking_path = states_data_path.to_string() + "/staking";
        let backup_staking_path = states_data_path.to_string() + "/backup_staking";
        let tendermint_commit_path = states_data_path.to_string() + "/tendermint_commit";
        let owned_utxos_path = states_data_path.to_string() + "/owned_utxos";

        let mut ledger = LedgerStatus {
            block_merkle_path: block_merkle_path.to_owned(),
            txn_merkle_path: txn_merkle_path.to_owned(),
            sliding_set: SlidingSet::<[u8; 8]>::new(TRANSACTION_WINDOW_WIDTH as usize),
            utxo_map_path: utxo_map_path.to_owned(),
            utxos: new_mapx!(utxos_path.as_str()),
            owned_utxos: new_mapx!(owned_utxos_path.as_str()),
            spent_utxos: new_mapx!(spent_utxos_path.as_str()),
            txo_to_txn_location: new_mapx!(txo_to_txn_location_path.as_str()),
            issuance_amounts: new_mapx!(issuance_amounts_path.as_str()),
            utxo_map_versions: VecDeque::new(),
            state_commitment_versions: new_vecx!(state_commitment_versions_path.as_str()),
            asset_types: new_mapx!(asset_types_path.as_str()),
            tracing_policies: new_mapx!(tracing_policies_path.as_str()),
            issuance_num: new_mapx!(issuance_num_path.as_str()),
            next_txn: new_vecx!(next_txn_path.as_str(), 1),
            next_txo: new_vecx!(next_txo_path.as_str(), 1),
            txns_in_block_hash: None,
            state_commitment_data: None,
            block_commit_count: 0,
            staking: new_vecx!(staking_path.as_str(), 1),
            backup_staking: new_vecx!(backup_staking_path.as_str(), 1),
            td_commit_height: new_vecx!(tendermint_commit_path.as_str(), 1),
            is_effective_block: false,
        };

        if ledger.staking.is_empty() {
            ledger.staking.set_value(0, Staking::new());
        }

        if ledger.backup_staking.is_empty() {
            ledger.backup_staking.set_value(0, Staking::new());
        }

        if ledger.next_txn.is_empty() {
            ledger.next_txn.set_value(0, TxnSID(0));
        }

        if ledger.next_txo.is_empty() {
            ledger.next_txo.set_value(0, TxoSID(0));
        }

        if ledger.td_commit_height.is_empty() {
            ledger.td_commit_height.set_value(0, 0);
        }

        Ok(ledger)
    }

    pub fn incr_block_commit_count(&mut self) {
        self.block_commit_count += 1;
        self.sliding_set.incr_current();
    }

    // Check that `txn` can be safely applied to the current ledger.
    //
    // Returns the same TxnEffect (unchanged) if it is safe. Consumes `txn`
    // if it would be invalid to prevent a simple case of forgetting error
    // handling, such as:
    //
    //  ledger.check_txn_effects(txn_effect);
    //  block.add_txn_effect(txn_effect);
    //
    #[allow(clippy::clone_double_ref)]
    #[allow(clippy::cognitive_complexity)]
    fn check_txn_effects(&self, txn_effect: &TxnEffect) -> Result<()> {
        // The current transactions seq_id must be within the sliding window over seq_ids
        let (rand, seq_id) = (
            txn_effect.txn.body.no_replay_token.get_rand(),
            txn_effect.txn.body.no_replay_token.get_seq_id(),
        );
        if seq_id > self.block_commit_count {
            return Err(eg!(("Transaction seq_id ahead of block_count")));
        } else if seq_id + (TRANSACTION_WINDOW_WIDTH as u64) < self.block_commit_count {
            return Err(eg!(("Transaction seq_id too far behind block_count")));
        } else {
            // Check to see that this nrpt has not been seen before
            if self.sliding_set.has_key_at(seq_id as usize, rand) {
                return Err(eg!(format!(
                    "No replay token ({:?}, {})seen before at  possible replay",
                    rand, seq_id
                )));
            }
        }

        // 1. Each input must be unspent and correspond to the claimed record
        // 2. Inputs with transfer restrictions can only be owned by the asset issuer
        for (inp_sid, inp_record) in txn_effect.input_txos.iter() {
            // (1)
            let inp_utxo = self.utxos.get(inp_sid).c(d!("Input must be unspent"))?;
            let record = &(inp_utxo.0);
            if record != inp_record {
                return Err(eg!((format!(
                    "Input must correspond to claimed record: {} != {}",
                    serde_json::to_string(&record).c(d!())?,
                    serde_json::to_string(inp_record).unwrap()
                ))));
            }
            // (2)
            if let Some(code) = record
                .record
                .asset_type
                .get_asset_type()
                .map(|v| AssetTypeCode { val: v })
            {
                let asset_type = self
                    .asset_types
                    .get(&code)
                    .or_else(|| txn_effect.new_asset_codes.get(&code).map(Value::from))
                    .c(d!())?;
                if !asset_type.properties.asset_rules.transferable
                    && asset_type.properties.issuer.key != record.record.public_key
                {
                    return Err(eg!(
                        ("Non-transferable asset type must be owned by asset issuer")
                    ));
                }
            }
        }

        // Internally spend inputs with transfer restrictions can only be owned by the asset issuer
        for record in txn_effect.internally_spent_txos.iter() {
            if let Some(code) = record
                .record
                .asset_type
                .get_asset_type()
                .map(|v| AssetTypeCode { val: v })
            {
                // dbg!(&self.asset_types);
                let asset_type = self
                    .asset_types
                    .get(&code)
                    .or_else(|| txn_effect.new_asset_codes.get(&code).map(Value::from))
                    .c(d!())?;
                if !asset_type.properties.asset_rules.transferable
                    && asset_type.properties.issuer.key != record.record.public_key
                {
                    return Err(eg!(
                        ("Non-transferable asset type must be owned by asset issuer")
                    ));
                }
            }
        }

        // dbg!("records work");

        // New asset types must not already exist
        for (code, _asset_type) in txn_effect.new_asset_codes.iter() {
            if self.asset_types.contains_key(&code) {
                return Err(eg!(format!("Asset type {:?} already defined", &code)));
            }
            if self.issuance_num.contains_key(&code) {
                return Err(eg!(format!(
                    "Asset type {:?} is being defined after issue",
                    &code
                )));
            }

            // Asset issuance should match the currently registered key
        }

        // dbg!("new types work");

        // New issuance numbers
        // (1) Must refer to a created asset type
        //  - NOTE: if the asset type is created in this transaction, this
        //    function is assuming that the ordering within the transaction is
        //    already valid.
        // (2) Must not be below the current asset cap
        //  - NOTE: this relies on the sequence numbers appearing in sorted
        //    order
        for (code, seq_nums) in txn_effect.new_issuance_nums.iter() {
            let iss_key = txn_effect.issuance_keys.get(&code).c(d!())?;
            let asset_type = self
                .asset_types
                .get(&code)
                .or_else(|| txn_effect.new_asset_codes.get(&code).map(Value::from))
                .c(d!())?;
            let proper_key = asset_type.properties.issuer;
            if *iss_key != proper_key {
                return Err(eg!(
                    ("Issuance key is not the same as key of properties issuer")
                ));
            }

            if seq_nums.is_empty() {
                if !txn_effect.new_asset_codes.contains_key(&code) {
                    return Err(eg!(("Code is not contained in new asset codes")));
                }
            // We could re-check that self.issuance_num doesn't contain `code`,
            // but currently it's redundant with the new-asset-type checks
            } else {
                let curr_seq_num_limit = self
                    .issuance_num
                    .get(&code)
                    .unwrap_or_else(|| Value::from(0));
                let min_seq_num = seq_nums.first().c(d!())?;
                if min_seq_num < curr_seq_num_limit.deref() {
                    return Err(eg!(("Minimum seq num is less than limit")));
                }
            }
        }

        // Asset Caps
        // (1) New issuance amounts cannot exceed asset cap
        // (2) No confidential issuances allowed for assets with issuance restrictions
        for (code, amount) in txn_effect.issuance_amounts.iter() {
            let asset_type = self
                .asset_types
                .get(&code)
                .or_else(|| txn_effect.new_asset_codes.get(&code).map(Value::from))
                .c(d!())?;
            // (1)
            if let Some(cap) = asset_type.properties.asset_rules.max_units {
                let current_amount = self
                    .issuance_amounts
                    .get(code)
                    .unwrap_or_else(|| Value::from(0));
                if current_amount.checked_add(*amount).c(d!())? > cap {
                    return Err(eg!(("Amount exceeds asset cap")));
                }
            }
        }

        // (2)
        for code in txn_effect.confidential_issuance_types.iter() {
            let asset_type = self
                .asset_types
                .get(&code)
                .or_else(|| txn_effect.new_asset_codes.get(&code).map(Value::from))
                .c(d!())?;
            if asset_type.has_issuance_restrictions() {
                return Err(eg!(("This asset type has issuance restrictions")));
            }
        }

        // Assets with cosignature requirements must have enough signatures
        for ((op_idx, input_idx), key_set) in txn_effect.cosig_keys.iter() {
            let op = &txn_effect.txn.body.operations[*op_idx];

            macro_rules! extract_asset_type {
                ($asset: expr) => {
                    match $asset.body.transfer.inputs.get(*input_idx) {
                        Some(record) => match record.asset_type {
                            XfrAssetType::NonConfidential(val) => {
                                Some(AssetTypeCode { val })
                            }
                            _ => None,
                        },
                        _ => None,
                    }
                };
            }

            let sig_type = match op {
                Operation::TransferAsset(xfr) => {
                    extract_asset_type!(xfr)
                }
                _ => {
                    return Err(eg!());
                }
            };

            let signature_rules = if let Some(code) = sig_type {
                self.asset_types
                    .get(&code)
                    .or_else(|| txn_effect.new_asset_codes.get(&code).map(Value::from))
                    .c(d!())?
                    .properties
                    .asset_rules
                    .transfer_multisig_rules
                    .clone()
            } else {
                None
            };

            if let Some(rules) = signature_rules {
                rules.check_signature_set(key_set).c(d!())?;
            }
        }

        // Check that asset types were validated under the correct tracing policies
        for (code, tracing_policies) in txn_effect.tracing_policies.iter() {
            let definition_policies = &self
                .asset_types
                .get(&code)
                .or_else(|| txn_effect.new_asset_codes.get(&code).map(Value::from))
                .c(d!())?
                .properties
                .asset_rules
                .tracing_policies;

            if definition_policies != tracing_policies {
                return Err(eg!(
                    ("Definition policies are not equal to tracing policies")
                ));
            }
        }

        // Memo updates
        // Multiple memo updates for the same asset are allowed, but only the last one will be applied.
        for memo_update in txn_effect.memo_updates.iter() {
            let asset = self.asset_types.get(&memo_update.0).c(d!())?;
            // Asset must be updatable and key must be correct
            if !asset.properties.asset_rules.updatable
                || asset.properties.issuer != (IssuerPublicKey { key: memo_update.1 })
            {
                return Err(eg!(("Non updatable asset or issuer mismatch")));
            }
        }

        // Until we can distinguish assets that have policies that invoke transfer restrictions
        // from those that don't, prevent any non-confidential assets with transfer restrictions
        // from becoming confidential
        for code in txn_effect.confidential_transfer_inputs.iter() {
            let asset_type = self
                .asset_types
                .get(&code)
                .or_else(|| txn_effect.new_asset_codes.get(&code).map(Value::from))
                .c(d!())?;
            if asset_type.has_transfer_restrictions() {
                return Err(eg!(
                    ("non-confidential assets with transfer restrictions can't become confidential")
                ));
            }
        }

        Ok(())
    }

    // This function assumes that `block` is COMPLETELY CONSISTENT with the
    // ledger state. Calling `check_txn_effects` for each TxnEffect getting
    // mixed into the BlockEffect *should* be enough to guarantee that (if
    // that is ever false, it's a bug).
    //
    // This drains every field of `block` except `txns` and `temp_sids`.
    #[allow(clippy::cognitive_complexity)]
    fn apply_block_effects(
        &mut self,
        block: &mut BlockEffect,
    ) -> HashMap<TxnTempSID, (TxnSID, Vec<TxoSID>)> {
        for no_replay_token in block.no_replay_tokens.iter() {
            let (rand, seq_id) = (
                no_replay_token.get_rand(),
                no_replay_token.get_seq_id() as usize,
            );
            match self.sliding_set.insert(rand, seq_id) {
                Ok(_) => (),
                Err(s) => println!("Error inserting into window: {}", s),
            }
        }
        block.no_replay_tokens.clear();

        // Remove consumed UTXOs
        for (inp_sid, utxo) in block.input_txos.drain() {
            if let Some(mut v) = self.owned_utxos.get_mut(&utxo.record.public_key) {
                v.deref_mut().remove(&inp_sid);
            }
            if let Some(v) = self.utxos.remove(&inp_sid) {
                self.spent_utxos.insert(inp_sid, v);
            }
        }

        // Apply memo updates
        for (code, memo) in block.memo_updates.drain() {
            let mut asset = self.asset_types.get_mut(&code).unwrap();
            (*asset).properties.memo = memo;
        }

        for (code, amount) in block.issuance_amounts.drain() {
            let mut amt = self.issuance_amounts.entry(code).or_insert(0);
            *amt.deref_mut() += amount;
        }

        // Add new UTXOs
        // Each transaction gets a TxnSID, and each of its unspent TXOs gets
        // a TxoSID. TxoSID assignments are based on the order TXOs appear in
        // the transaction.
        let mut new_utxo_sids: HashMap<TxnTempSID, (TxnSID, Vec<TxoSID>)> = map! {};
        {
            let mut next_txn = pnk!(self.next_txn.get(0).c(d!())).0;
            let mut next_txo = pnk!(self.next_txo.get(0).c(d!())).0;

            for (ix, txos) in block.temp_sids.iter().zip(block.txos.drain(..)) {
                let txn_sid = next_txn;
                next_txn += 1;

                let mut txn_utxo_sids: Vec<TxoSID> = vec![];

                for txo in txos {
                    let txo_sid = next_txo;
                    next_txo += 1;
                    if let Some(tx_output) = txo {
                        self.owned_utxos
                            .entry(tx_output.record.public_key)
                            .or_insert_with(HashSet::new)
                            .insert(TxoSID(txo_sid));
                        self.utxos.insert(TxoSID(txo_sid), Utxo(tx_output));
                        txn_utxo_sids.push(TxoSID(txo_sid));
                    }
                }

                new_utxo_sids.insert(*ix, (TxnSID(txn_sid), txn_utxo_sids));
            }
            self.next_txn.set_value(0, TxnSID(next_txn));
            self.next_txo.set_value(0, TxoSID(next_txo));
        }

        // Update issuance sequence number limits
        for (code, seq_nums) in block.new_issuance_nums.drain() {
            // One more than the greatest sequence number, or 0
            let new_max_seq_num = seq_nums.last().map(|x| x + 1).unwrap_or(0);
            self.issuance_num.insert(code, new_max_seq_num);
        }

        // Register new asset types
        for (code, asset_type) in block.new_asset_codes.drain() {
            self.asset_types.insert(code, asset_type.clone());
        }

        // issuance_keys should already have been checked
        block.issuance_keys.clear();

        new_utxo_sids
    }

    /// Check if an txo_sid is unspent.
    pub fn is_unspent_txo(&self, addr: TxoSID) -> bool {
        self.utxos.contains_key(&addr)
    }
}

impl LedgerState {
    pub fn get_prng(&mut self) -> &mut ChaChaRng {
        &mut self.prng
    }

    pub fn start_block(&mut self) -> Result<BlockEffect> {
        if let Some(mut block) = self.block_ctx.take() {
            *block.get_staking_simulator_mut() = self.get_staking().deref().clone();
            Ok(block)
        } else {
            Err(eg!())
        }
    }

    pub fn apply_transaction(
        &self,
        block: &mut BlockEffect,
        txe: TxnEffect,
        is_loading: bool,
    ) -> Result<TxnTempSID> {
        let tx = txe.txn.clone();
        self.status
            .check_txn_effects(&txe)
            .c(d!())
            .and_then(|_| block.add_txn_effect(txe, is_loading).c(d!()))
            .and_then(|tmpid| {
                // NOTE: set at the last position
                if is_loading {
                    Ok(tmpid)
                } else {
                    block
                        .staking_simulator
                        .coinbase_check_and_pay(&tx)
                        .c(d!())
                        .map(|_| tmpid)
                }
            })
    }

    pub fn abort_block(
        &mut self,
        block: BlockEffect,
    ) -> HashMap<TxnTempSID, Transaction> {
        let mut block = block;
        let txns = block.txns.drain(..);
        let ret: HashMap<TxnTempSID, Transaction> =
            block.temp_sids.drain(..).zip(txns).collect();

        block.txos.clear();
        block.input_txos.clear();
        block.new_asset_codes.clear();
        block.new_issuance_nums.clear();
        block.issuance_keys.clear();

        ret
    }

    #[allow(clippy::cognitive_complexity)]
    pub fn finish_block(
        &mut self,
        mut block: BlockEffect,
    ) -> Result<HashMap<TxnTempSID, (TxnSID, Vec<TxoSID>)>> {
        let base_sid = self.get_next_txo().0;
        let txn_temp_sids = block.temp_sids.clone();

        for (inp_sid, _) in block.input_txos.iter() {
            // Remove from bitmap
            self.utxo_map.clear(inp_sid.0 as usize).c(d!())?;
        }

        let temp_sid_map = self.status.apply_block_effects(&mut block);

        let max_sid = self.get_next_txo().0; // mutated by apply_txn_effects

        {
            // Update the UTXO bitmap
            // This is, unfortunately, some horrible index-walking messiness.
            // The core idea is that we walk over every new TXO SID (ix), tracing:
            //  - by `temp_sid_ix`, which transaction we're in
            //  - by `txo_sid_ix`, which UTXO within that transaction is next.
            let mut temp_sid_ix = 0;
            let mut txo_sid_ix = 0;

            // Find the first index that matters
            while temp_sid_ix < txn_temp_sids.len()
                && (temp_sid_map[&txn_temp_sids[temp_sid_ix]].1).is_empty()
            {
                temp_sid_ix += 1;
            }

            for ix in base_sid..max_sid {
                let temp_sid = txn_temp_sids[temp_sid_ix];
                let utxo_sids = &temp_sid_map[&temp_sid].1;

                // Only .set() extends the bitmap, so to append a 0 we currently
                // nead to .set() then .clear().
                //
                // TODO(joe): are these unwraps okay?
                self.utxo_map.set(ix as usize).c(d!())?;
                if let Some(TxoSID(utxo_sid)) = utxo_sids.get(txo_sid_ix) {
                    if *utxo_sid != ix {
                        self.utxo_map.clear(ix as usize).c(d!())?;
                    } else {
                        txo_sid_ix += 1;

                        // We've reached the end of this UTXO list, search for the next
                        // relevant one
                        if txo_sid_ix == utxo_sids.len() {
                            txo_sid_ix = 0;

                            temp_sid_ix += 1;
                            while temp_sid_ix < txn_temp_sids.len()
                                && (temp_sid_map[&txn_temp_sids[temp_sid_ix]].1)
                                    .is_empty()
                            {
                                temp_sid_ix += 1;
                            }
                        }
                    }
                }
            }
        }

        {
            let mut tx_block = Vec::new();

            // TODO(joe/keyao): reorder these so that we can drain things

            // Update the transaction Merkle tree and transaction log
            // Store the location of each utxo so we can create authenticated utxo proofs
            for (tmp_sid, txn) in block.temp_sids.iter().zip(block.txns.iter()) {
                let txn = txn.clone();
                let txo_sid_map = temp_sid_map.get(&tmp_sid).c(d!())?;
                let txn_sid = txo_sid_map.0;
                let txo_sids = &txo_sid_map.1;

                // TODO(joe/jonathan): Since errors in the merkle tree are things like
                // corruption and I/O failure, we don't have a good recovery story. Is
                // panicking acceptable?
                let merkle_id = self
                    .txn_merkle
                    .append_hash(&txn.hash(txn_sid).0.hash.into())
                    .c(d!())?;

                tx_block.push(FinalizedTransaction {
                    txn: txn.clone(),
                    tx_id: txn_sid,
                    txo_ids: txo_sids.clone(),
                    merkle_id,
                });

                // TODO(joe/noah): is this check important?
                // let outputs = txn.get_outputs_ref(false);

                for (position, sid) in txo_sids.iter().enumerate() {
                    self.status
                        .txo_to_txn_location
                        .insert(*sid, (txn_sid, OutputPosition(position)));
                }
            }

            // Checkpoint
            let block_merkle_id = self.checkpoint(&block).c(d!())?;
            block.temp_sids.clear();
            block.txns.clear();

            let block_idx = self.blocks.len();
            tx_block.iter().enumerate().for_each(|(tx_idx, tx)| {
                self.tx_to_block_location
                    .insert(tx.tx_id, [block_idx, tx_idx]);
            });

            self.blocks.push(FinalizedBlock {
                txns: tx_block,
                merkle_id: block_merkle_id,
                state: self.status.state_commitment_data.clone().c(d!())?,
            });
        }
        self.status
            .backup_staking
            .set_value(0, block.staking_simulator.clone());

        mem::swap(
            &mut block.staking_simulator,
            self.get_staking_mut().deref_mut(),
        );

        self.block_ctx = Some(block);
        self.status.is_effective_block = true;
        Ok(temp_sid_map)
    }

    pub fn get_staking_mut(&mut self) -> VecxValueMut<Staking> {
        pnk!(self.status.staking.get_mut(0).c(d!()))
    }

    pub fn flush_data(&mut self) {
        if self.status.is_effective_block {
            self.status.is_effective_block = false;
            self.blocks.flush_data();
            self.status.txo_to_txn_location.flush_data();
            self.status.state_commitment_versions.flush_data();
            self.status.utxos.flush_data();
            self.status.asset_types.flush_data();
            self.status.issuance_amounts.flush_data();
            self.status.issuance_num.flush_data();
            self.status.spent_utxos.flush_data();
            self.status.tracing_policies.flush_data();
            self.status.staking.flush_data();
            self.status.next_txn.flush_data();
            self.status.next_txo.flush_data();
            self.status.backup_staking.flush_data();
            self.status.owned_utxos.flush_data();
        }
    }

    pub fn flush_staking(&self) {
        self.status.staking.flush_data();
    }

    pub fn set_tendermint_commit(&mut self, tendermint_commit: u64) {
        self.status.td_commit_height.set_value(0, tendermint_commit);
        self.status.td_commit_height.flush_data();
    }
}

impl LedgerState {
    pub fn get_next_txn(&self) -> Value<TxnSID> {
        pnk!(self.status.next_txn.get(0).c(d!()))
    }

    pub fn get_next_txo(&self) -> Value<TxoSID> {
        pnk!(self.status.next_txo.get(0).c(d!()))
    }

    pub fn get_status(&self) -> &LedgerStatus {
        &self.status
    }

    // Create a ledger for use by a unit test.
    pub fn test_ledger() -> LedgerState {
        let tmp_dir = utils::fresh_tmp_dir();

        let ret =
            LedgerState::new(tmp_dir.clone(), None, None, Some(String::from("test")))
                .unwrap();

        let key_buf = tmp_dir.join("test_sig_key");
        let key_path = key_buf.to_str().unwrap();
        {
            let file = File::create(key_path).unwrap();
            {
                let mut writer = BufWriter::new(file);

                serde_json::to_writer::<&mut BufWriter<File>, XfrKeyPair>(
                    &mut writer,
                    &ret.signing_key,
                )
                .unwrap();
            }
        }

        ret
    }

    // TODO(joe): Make this an iterator of some sort so that we don't have to load the whole log
    // into memory
    fn load_transaction_log(path: &str) -> Result<Vec<LoggedBlock>> {
        let file = File::open(path).c(d!())?;
        let reader = BufReader::new(file);
        let mut v = Vec::new();
        for l in reader.lines() {
            let l = l.c(d!())?;
            match serde_json::from_str::<LoggedBlock>(&l) {
                Ok(next_block) => {
                    v.push(next_block);
                }
                Err(e) => {
                    if !l.is_empty() {
                        return Err(eg!(format!("{:?} (deserializing '{:?}')", e, &l)));
                    }
                }
            }
        }
        Ok(v)
    }

    fn save_utxo_map_version(&mut self) {
        if self.status.utxo_map_versions.len() >= MAX_VERSION {
            self.status.utxo_map_versions.pop_front();
        }

        self.status.utxo_map_versions.push_back((
            *pnk!(self.status.next_txn.get(0).c(d!())),
            self.utxo_map.compute_checksum(),
        ));
    }

    // In this functionn:
    //  1. Compute the hash of transactions in the block and update txns_in_block_hash
    //  2. Append txns_in_block_hash to block_merkle
    fn compute_and_append_txns_hash(&mut self, block: &BlockEffect) -> u64 {
        // 1. Compute the hash of transactions in the block and update txns_in_block_hash
        let txns_in_block_hash = block.compute_txns_in_block_hash();
        self.status.txns_in_block_hash = Some(txns_in_block_hash.clone());

        // 2. Append txns_in_block_hash to block_merkle
        //  2.1 Update the block Merkle tree
        let ret = self
            .block_merkle
            .append_hash(&txns_in_block_hash.0.hash.into())
            .unwrap();

        ret
    }

    fn compute_and_save_state_commitment_data(&mut self, pulse_count: u64) {
        let bitmap = self.utxo_map.compute_checksum();
        let prev_commitment = HashOf::new(&self.status.state_commitment_data);
        let block_merkle = self.block_merkle.get_root_hash();
        let transaction_merkle_commitment = self.txn_merkle.get_root_hash();
        let txns_in_block_hash =
            self.status.txns_in_block_hash.as_ref().cloned().unwrap();
        let previous_state_commitment = prev_commitment;
        let txo_count = self.get_next_txo().0;

        let state_commitment_data = StateCommitmentData {
            bitmap,
            block_merkle,
            transaction_merkle_commitment,
            txns_in_block_hash,
            previous_state_commitment,
            air_commitment: BitDigest::from_slice(&[0; 32][..]).unwrap(),
            txo_count,
            pulse_count,
        };
        let state_commitment_data_hash = state_commitment_data.compute_commitment();

        self.status.state_commitment_data = Some(state_commitment_data);

        self.status
            .state_commitment_versions
            .push(state_commitment_data_hash);

        self.status.incr_block_commit_count();
    }

    // Initialize a logged Merkle tree for the ledger. We might
    // be creating a new tree or opening an existing one. We
    // always start a new log file.
    fn init_merkle_log(path: &str, create: bool) -> Result<AppendOnlyMerkle> {
        // Create a merkle tree or open an existing one.
        let tree = if create {
            ruc::omit!(fs::remove_file(path));
            AppendOnlyMerkle::create(path).c(d!())?
        } else {
            AppendOnlyMerkle::open(path).c(d!())?
        };

        info!("Using path {} for the Merkle tree.", path);

        // Create a log for the tree. The tree size ("state") is appended to
        // the end of the path.
        // TODO: START https://github.com/findoraorg/platform/issues/307
        // let next_id = tree.total_size();
        // let writer = LedgerState::create_merkle_log(path.to_owned(), next_id)?;
        // TODO: END This is being disabled as we decide what to do about about logging, archival, etc
        Ok(tree)
        // Ok(LoggedMerkle::new(tree, writer))
    }

    // Initialize a bitmap to track the unspent utxos.
    fn init_utxo_map(path: &str, create: bool) -> Result<BitMap> {
        let mut file = OpenOptions::new();
        let f = file.read(true).write(true);

        if create {
            f.create(true)
                .truncate(true)
                .open(path)
                .c(d!())
                .and_then(|f| BitMap::create(f).c(d!()))
        } else {
            f.open(path).c(d!()).and_then(|f| BitMap::open(f).c(d!()))
        }
    }

    // Initialize a new Ledger structure.
    pub fn new(
        base_dir: PathBuf,
        keypair: Option<XfrKeyPair>,
        prng_seed: Option<[u8; 32]>,
        prefix: Option<String>,
    ) -> Result<LedgerState> {
        let (var1, var2, var3, var4, var5, var6) = match prefix {
            None => (
                "block_merkle".to_string(),
                "txn_merkle".to_string(),
                "utxo_map".to_string(),
                "states_data".to_string(),
                "blocks".to_string(),
                "tx_to_block_location".to_string(),
            ),
            Some(s) => {
                let block_merkle = s.clone() + "_block_merkle";
                let txn_merkle = s.clone() + "_txn_merkle";
                let utxo_map = s.clone() + "_utxo_map";
                let states_data = s.clone() + "_states_data";
                let blocks = s.clone() + "_blocks";
                let tx_to_block_location = s + "_tx_to_block_location";

                (
                    block_merkle,
                    txn_merkle,
                    utxo_map,
                    states_data,
                    blocks,
                    tx_to_block_location,
                )
            }
        };

        let block_merkle_buf = base_dir.join(var1);
        let block_merkle_path = block_merkle_buf.to_str().unwrap();

        let txn_merkle_buf = base_dir.join(var2);
        let txn_merkle_path = txn_merkle_buf.to_str().unwrap();

        let utxo_map_buf = base_dir.join(var3);
        let utxo_map_path = utxo_map_buf.to_str().unwrap();

        let states_data_buf = base_dir.join(var4);
        let states_data_path = states_data_buf.to_str().unwrap();

        let blocks_buf = base_dir.join(var5);
        let blocks_path = blocks_buf.to_str().unwrap();

        let tx_to_block_location_buf = base_dir.join(var6);
        let tx_to_block_location_path =
            tx_to_block_location_buf.to_str().map(String::from).unwrap();

        let mut prng = prng_seed
            .map(rand_chacha::ChaChaRng::from_seed)
            .unwrap_or_else(ChaChaRng::from_entropy);
        let signing_key = keypair.unwrap_or_else(|| XfrKeyPair::generate(&mut prng));
        let ledger = LedgerState {
            status: LedgerStatus::new(
                block_merkle_path,
                txn_merkle_path,
                utxo_map_path,
                states_data_path,
            )
            .c(d!())?,
            prng,
            signing_key,
            block_merkle: LedgerState::init_merkle_log(block_merkle_path, true)
                .c(d!())?,
            txn_merkle: LedgerState::init_merkle_log(txn_merkle_path, true).c(d!())?,
            blocks: pnk!(Vecx::new(blocks_path, None, false)),
            tx_to_block_location: new_mapx!(tx_to_block_location_path.as_str()),
            utxo_map: LedgerState::init_utxo_map(utxo_map_path, true).c(d!())?,
            block_ctx: Some(BlockEffect::new()),
        };

        Ok(ledger)
    }

    pub fn load_from_log(
        base_dir: &Path,
        prng_seed: Option<[u8; 32]>,
    ) -> Result<LedgerState> {
        let sig_key_file_buf = base_dir.join("sig_key");
        let signing_key_path = sig_key_file_buf.to_str().c(d!())?;

        let block_buf = base_dir.join("block_merkle");
        let block_merkle_path = block_buf.to_str().c(d!())?;

        let txn_merkle_buf = base_dir.join("txn_merkle");
        let txn_merkle_path = txn_merkle_buf.to_str().c(d!())?;

        let utxo_map_buf = base_dir.join("utxo_map");
        let utxo_map_path = utxo_map_buf.to_str().c(d!())?;

        let states_data_buf = base_dir.join("states_data");
        let states_data_path = states_data_buf.to_str().c(d!())?;

        let blocks_buf = base_dir.join("blocks");
        let blocks_path = blocks_buf.to_str().c(d!())?;

        let txn_log_buf = base_dir.join("txn_log");
        let txn_log_path = txn_log_buf.to_str().c(d!())?;

        let tx_to_block_location_buf = base_dir.join("tx_to_block_location");
        let tx_to_block_location_path =
            tx_to_block_location_buf.to_str().map(String::from).unwrap();

        let mut prng = prng_seed
            .map(rand_chacha::ChaChaRng::from_seed)
            .unwrap_or_else(ChaChaRng::from_entropy);
        let signing_key = {
            let ret = File::open(signing_key_path).c(d!()).and_then(|file| {
                let mut reader = BufReader::new(file);
                serde_json::from_reader::<&mut BufReader<File>, XfrKeyPair>(&mut reader)
                    .c(d!())
            });
            ret.or_else(|_| {
                let key = XfrKeyPair::generate(&mut prng);
                File::create(signing_key_path)
                    .c(d!())
                    .and_then(|file| {
                        let mut writer = BufWriter::new(file);

                        serde_json::to_writer::<&mut BufWriter<File>, XfrKeyPair>(
                            &mut writer,
                            &key,
                        )
                        .c(d!())
                        .map(|_| key)
                    })
                    .c(d!())
            })
            .c(d!())?
        };

        let mut ledger = LedgerState {
            status: LedgerStatus::new(
                block_merkle_path,
                txn_merkle_path,
                utxo_map_path,
                states_data_path,
            )
            .c(d!())?,
            prng,
            signing_key,
            block_merkle: LedgerState::init_merkle_log(
                block_merkle_path,
                !Path::new(block_merkle_path).is_file(),
            )
            .c(d!())?,
            txn_merkle: LedgerState::init_merkle_log(
                txn_merkle_path,
                !Path::new(txn_merkle_path).is_file(),
            )
            .c(d!())?,
            blocks: pnk!(Vecx::new(blocks_path, None, false)),
            tx_to_block_location: new_mapx!(tx_to_block_location_path.as_str()),
            utxo_map: LedgerState::init_utxo_map(
                utxo_map_path,
                !Path::new(utxo_map_path).is_file(),
            )
            .c(d!())?,
            block_ctx: Some(BlockEffect::new()),
        };

        let blocks_len = ledger.blocks.len();

        if let Ok(old_blocks) = LedgerState::load_transaction_log(txn_log_path).c(d!()) {
            let old_blocks_len = old_blocks.len();
            if old_blocks_len > blocks_len {
                let mut old_td_height = blocks_len as u64;

                for (commit_count, logged_block) in old_blocks.into_iter().enumerate() {
                    let block = logged_block.block;
                    let mut block_builder = ledger.start_block().c(d!())?;
                    for txn in block {
                        let eff = TxnEffect::compute_effect(txn).c(d!())?;
                        ledger
                            .apply_transaction(&mut block_builder, eff, true)
                            .c(d!())?;
                    }

                    let pulse_count = logged_block.state.pulse_count;
                    block_builder
                        .staking_simulator
                        .set_custom_block_height(pulse_count + commit_count as u64 + 1);

                    ledger.finish_block(block_builder).c(d!())?;

                    if commit_count == old_blocks_len - 1 {
                        old_td_height = pulse_count + (commit_count as u64) + 1;
                    }
                }
                ledger.flush_data();

                ledger.status.td_commit_height.set_value(0, old_td_height);
                ledger
                    .get_staking_mut()
                    .set_custom_block_height(old_td_height);

                ledger.fast_invariant_check().c(d!())?;

                return Ok(ledger);
            }
        }

        let td_commit_height = pnk!(ledger.status.td_commit_height.get(0));
        let staking_tendermint_height = ledger.get_staking().cur_height();

        if td_commit_height != staking_tendermint_height {
            let backup_staking = pnk!(ledger.status.backup_staking.get(0));

            if backup_staking.cur_height() == 0 && staking_tendermint_height > 0 {
                panic!("backup staking is wrong data");
            }

            ledger
                .status
                .staking
                .set_value(0, backup_staking.deref().clone());
        }

        omit!(ledger.utxo_map.compute_checksum());

        // this var is used in query_server to determine how many blocks to cross when restarting
        std::env::set_var("LOAD_BLOCKS_LEN", ledger.blocks.len().to_string());

        if let Some(v) = ledger.blocks.iter().last() {
            let block_builder = &mut pnk!(ledger.start_block().c(d!()));
            for finalized_tx in v.txns.iter() {
                let f_tx = finalized_tx.clone();
                let eff = pnk!(TxnEffect::compute_effect(f_tx.txn).c(d!()));
                pnk!(block_builder.add_txn_effect(eff, true).c(d!()));
            }

            let txns_in_block_hash = block_builder.compute_txns_in_block_hash();
            ledger.status.txns_in_block_hash = Some(txns_in_block_hash);

            let mut block_builder = BlockEffect::new();

            block_builder
                .staking_simulator
                .set_custom_block_height(ledger.get_staking().cur_height());

            ledger.block_ctx = Some(block_builder);
            ledger.status.state_commitment_data = Some(v.state);
        }

        (0..blocks_len).for_each(|_| {
            ledger.status.incr_block_commit_count();
        });
        pnk!(ledger.fast_invariant_check().c(d!()));

        Ok(ledger)
    }

    pub fn load_or_init(base_dir: &Path) -> Result<LedgerState> {
        LedgerState::load_from_log(base_dir, None).or_else(|e| {
            e.print();
            let ret =
                LedgerState::new(base_dir.to_path_buf(), None, None, None).c(d!())?;

            {
                let sig_key_file_buf = base_dir.join("sig_key");
                let signing_key_path = sig_key_file_buf.to_str().c(d!())?;

                let file = File::create(signing_key_path).c(d!())?;
                let mut writer = BufWriter::new(file);

                serde_json::to_writer::<&mut BufWriter<File>, XfrKeyPair>(
                    &mut writer,
                    &ret.signing_key,
                )
                .c(d!())?;
            }

            Ok(ret)
        })
    }

    pub fn checkpoint(&mut self, block: &BlockEffect) -> Result<u64> {
        self.save_utxo_map_version();
        let merkle_id = self.compute_and_append_txns_hash(&block);
        let pulse_count = block
            .staking_simulator
            .cur_height()
            .saturating_sub(self.get_block_commit_count() + 1);
        self.compute_and_save_state_commitment_data(pulse_count);
        self.utxo_map.write().c(d!())?;
        self.txn_merkle.write().c(d!())?;
        self.block_merkle.write().c(d!())?;

        Ok(merkle_id)
    }
}

impl LedgerStatus {
    #[allow(missing_docs)]
    pub fn get_owned_utxos(&self, addr: &XfrPublicKey) -> Vec<TxoSID> {
        self.owned_utxos
            .get(addr)
            .map(|v| v.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn get_utxo(&self, id: TxoSID) -> Option<Value<Utxo>> {
        self.utxos.get(&id)
    }

    #[inline(always)]
    fn get_spent_utxo(&self, addr: TxoSID) -> Option<Value<Utxo>> {
        self.spent_utxos.get(&addr)
    }

    fn get_issuance_num(&self, code: &AssetTypeCode) -> Option<u64> {
        self.issuance_num.get(code).map(|v| *v.deref())
    }

    fn get_asset_type(&self, code: &AssetTypeCode) -> Option<Value<AssetType>> {
        self.asset_types.get(code)
    }
}

impl LedgerState {
    pub fn get_utxo(&self, id: TxoSID) -> Option<AuthenticatedUtxo> {
        let utxo = self.status.get_utxo(id);
        if let Some(utxo) = utxo {
            let txn_location = *self.status.txo_to_txn_location.get(&id).unwrap();
            let authenticated_txn = self.get_transaction(txn_location.0).unwrap();
            let authenticated_spent_status = self.get_utxo_status(id);
            let state_commitment_data =
                self.status.state_commitment_data.as_ref().unwrap().clone();
            let utxo_location = txn_location.1;
            Some(AuthenticatedUtxo {
                utxo: utxo.deref().clone(),
                authenticated_txn,
                authenticated_spent_status,
                utxo_location,
                state_commitment_data,
            })
        } else {
            None
        }
    }

    pub fn get_utxo_light(&self, id: TxoSID) -> Option<UnAuthenticatedUtxo> {
        let utxo = self.status.get_utxo(id);
        if let Some(utxo) = utxo {
            let txn_location = *self.status.txo_to_txn_location.get(&id).unwrap();
            let txn = self.get_transaction_light(txn_location.0).unwrap();
            let utxo_location = txn_location.1;
            Some(UnAuthenticatedUtxo {
                utxo: utxo.deref().clone(),
                txn,
                utxo_location,
            })
        } else {
            None
        }
    }

    pub fn get_spent_utxo(&self, addr: TxoSID) -> Option<AuthenticatedUtxo> {
        let utxo = self.status.get_spent_utxo(addr);
        if let Some(utxo) = utxo {
            let txn_location = *self.status.txo_to_txn_location.get(&addr).unwrap();
            let authenticated_txn = self.get_transaction(txn_location.0).unwrap();
            let authenticated_spent_status = self.get_utxo_status(addr);
            let state_commitment_data =
                self.status.state_commitment_data.as_ref().unwrap().clone();
            let utxo_location = txn_location.1;
            Some(AuthenticatedUtxo {
                utxo: utxo.deref().clone(),
                authenticated_txn,
                authenticated_spent_status,
                utxo_location,
                state_commitment_data,
            })
        } else {
            None
        }
    }

    pub fn get_spent_utxo_light(&self, addr: TxoSID) -> Option<UnAuthenticatedUtxo> {
        let utxo = self.status.get_spent_utxo(addr);
        if let Some(utxo) = utxo {
            let txn_location = *self.status.txo_to_txn_location.get(&addr).unwrap();
            let txn = self.get_transaction_light(txn_location.0).unwrap();
            let utxo_location = txn_location.1;
            Some(UnAuthenticatedUtxo {
                utxo: utxo.deref().clone(),
                txn,
                utxo_location,
            })
        } else {
            None
        }
    }

    pub fn get_utxos(&self, sid_list: &[TxoSID]) -> Vec<Option<AuthenticatedUtxo>> {
        let mut utxos = vec![];
        for sid in sid_list.iter() {
            let utxo = self.status.get_utxo(*sid);
            if let Some(utxo) = utxo {
                let txn_location = *self.status.txo_to_txn_location.get(sid).unwrap();
                let authenticated_txn = self.get_transaction(txn_location.0).unwrap();
                let authenticated_spent_status = self.get_utxo_status(*sid);
                let state_commitment_data =
                    self.status.state_commitment_data.as_ref().unwrap().clone();
                let utxo_location = txn_location.1;
                let auth_utxo = AuthenticatedUtxo {
                    utxo: utxo.deref().clone(),
                    authenticated_txn,
                    authenticated_spent_status,
                    utxo_location,
                    state_commitment_data,
                };
                utxos.push(Some(auth_utxo))
            } else {
                utxos.push(None)
            } // Should we just change this to return  Vec<AuthenticatedUtxo> ? and not return None for unknown utxos.
        }
        utxos
    }

    pub fn get_utxos_light(
        &self,
        sid_list: &[TxoSID],
    ) -> Result<Vec<Option<UnAuthenticatedUtxo>>> {
        let mut utxos = vec![];
        for sid in sid_list.iter() {
            let utxo = self.status.get_utxo(*sid);
            if let Some(utxo) = utxo {
                let txn_location = *self.status.txo_to_txn_location.get(sid).c(d!())?;
                let txn = self.get_transaction_light(txn_location.0).c(d!())?;
                let utxo_location = txn_location.1;
                let auth_utxo = UnAuthenticatedUtxo {
                    utxo: utxo.deref().clone(),
                    txn,
                    utxo_location,
                };
                utxos.push(Some(auth_utxo))
            } else {
                utxos.push(None)
            }
        }
        Ok(utxos)
    }

    pub fn get_owned_utxos(
        &self,
        addr: &XfrPublicKey,
    ) -> Result<BTreeMap<TxoSID, (Utxo, Option<OwnerMemo>)>> {
        let sids = self.status.get_owned_utxos(addr);
        let aus = self.get_utxos_light(&sids).c(d!())?;

        let res = sids
            .into_iter()
            .zip(aus.into_iter())
            .filter_map(|(sid, au)| au.map(|au| (sid, au)))
            .map(|(sid, au)| {
                (
                    sid,
                    (
                        au.utxo,
                        au.txn
                            .txn
                            .get_owner_memos_ref()
                            .get(au.utxo_location.0)
                            .map(|i| i.cloned())
                            .flatten(),
                    ),
                )
            })
            .collect();

        Ok(res)
    }

    pub fn get_issuance_num(&self, code: &AssetTypeCode) -> Option<u64> {
        self.status.get_issuance_num(code)
    }

    pub fn get_asset_type(&self, code: &AssetTypeCode) -> Option<AssetType> {
        self.status.get_asset_type(code).map(|v| v.deref().clone())
    }

    pub fn get_block_commit_count(&self) -> u64 {
        self.status.block_commit_count
    }

    pub fn get_state_commitment(&self) -> (HashOf<Option<StateCommitmentData>>, u64) {
        let block_count = self.status.block_commit_count;
        let commitment = self
            .status
            .state_commitment_versions
            .last()
            .map(|v| v.deref().clone())
            .unwrap_or_else(|| HashOf::new(&None));
        (commitment, block_count)
    }

    pub fn public_key(&self) -> &XfrPublicKey {
        self.signing_key.get_pk_ref()
    }

    pub fn sign_message<T: Serialize + serde::de::DeserializeOwned>(
        &self,
        msg: &T,
    ) -> SignatureOf<T> {
        SignatureOf::new(&self.signing_key, msg)
    }

    pub fn get_utxo_status(&self, addr: TxoSID) -> AuthenticatedUtxoStatus {
        let state_commitment_data = self.status.state_commitment_data.as_ref().unwrap();
        let utxo_map_bytes: Option<SparseMapBytes>;
        let status;
        if addr.0 < state_commitment_data.txo_count {
            utxo_map_bytes = Some(self.utxo_map.serialize(0));
            let utxo_map =
                SparseMap::new(&utxo_map_bytes.as_ref().unwrap().clone()).unwrap();
            status = if utxo_map.query(addr.0).unwrap() {
                UtxoStatus::Unspent
            } else {
                UtxoStatus::Spent
            };
        } else {
            status = UtxoStatus::Nonexistent;
            utxo_map_bytes = None;
        }

        AuthenticatedUtxoStatus {
            status,
            state_commitment_data: state_commitment_data.clone(),
            state_commitment: state_commitment_data.compute_commitment(),
            utxo_sid: addr,
            utxo_map_bytes,
        }
    }

    pub fn get_staking(&self) -> Value<Staking> {
        pnk!(self.status.staking.get(0).c(d!()))
    }

    pub fn get_transaction(&self, id: TxnSID) -> Result<AuthenticatedTransaction> {
        self.get_transaction_light(id).c(d!()).and_then(|tx| {
            let state_commitment_data =
                self.status.state_commitment_data.as_ref().c(d!())?.clone();
            let merkle = &self.txn_merkle;
            let proof = ProofOf::new(merkle.get_proof(tx.merkle_id, 0).c(d!())?);

            Ok(AuthenticatedTransaction {
                finalized_txn: tx,
                txn_inclusion_proof: proof,
                state_commitment_data: state_commitment_data.clone(),
                state_commitment: state_commitment_data.compute_commitment(),
            })
        })
    }

    pub fn get_transaction_light(&self, id: TxnSID) -> Result<FinalizedTransaction> {
        self.tx_to_block_location
            .get(&id)
            .c(d!())
            .map(|v| *v.deref())
            .and_then(|[block_idx, tx_idx]| {
                self.blocks
                    .get(block_idx)
                    .c(d!())
                    .and_then(|b| b.txns.get(tx_idx).cloned().c(d!()))
            })
    }

    pub fn get_block(&self, addr: BlockSID) -> Option<AuthenticatedBlock> {
        match self.blocks.get(addr.0) {
            None => None,
            Some(finalized_block) => {
                let block_inclusion_proof = ProofOf::new(
                    self.block_merkle
                        .get_proof(finalized_block.merkle_id, 0)
                        .unwrap(),
                );
                let state_commitment_data =
                    self.status.state_commitment_data.as_ref().unwrap().clone();
                Some(AuthenticatedBlock {
                    block: finalized_block.deref().clone(),
                    block_inclusion_proof,
                    state_commitment_data: state_commitment_data.clone(),
                    state_commitment: state_commitment_data.compute_commitment(),
                })
            }
        }
    }

    pub fn get_block_count(&self) -> usize {
        self.blocks.len()
    }
    pub fn get_transaction_count(&self) -> usize {
        self.get_next_txn().0
    }
    pub fn get_utxo_map(&self) -> &BitMap {
        &self.utxo_map
    }
    pub fn serialize_utxo_map(&mut self) -> Vec<u8> {
        self.utxo_map.serialize(self.get_transaction_count())
    }

    pub fn get_utxo_checksum(&self, version: u64) -> Option<BitDigest> {
        for pair in self.status.utxo_map_versions.iter() {
            if (pair.0).0 as u64 == version {
                return Some(pair.1);
            }
        }

        None
    }

    pub fn get_state_commitment_at_block_height(
        &self,
        block_height: u64,
    ) -> Option<HashOf<Option<StateCommitmentData>>> {
        self.status
            .state_commitment_versions
            .get((block_height - 1) as usize)
            .map(|v| v.deref().clone())
    }
}

pub mod helpers {
    use super::*;
    use crate::data_model::{
        Asset, AssetRules, ConfidentialMemo, DefineAsset, DefineAssetBody,
        IssuerPublicKey, Memo,
    };
    use std::fmt::Debug;
    use zei::setup::PublicParams;
    use zei::xfr::asset_record::AssetRecordType;
    use zei::xfr::asset_record::{build_blind_asset_record, open_blind_asset_record};
    use zei::xfr::sig::{XfrKeyPair, XfrPublicKey};
    use zei::xfr::structs::{AssetRecord, AssetRecordTemplate};

    pub fn create_definition_transaction(
        code: &AssetTypeCode,
        keypair: &XfrKeyPair,
        asset_rules: AssetRules,
        memo: Option<Memo>,
        seq_id: u64,
    ) -> Result<Transaction> {
        let issuer_key = IssuerPublicKey {
            key: *keypair.get_pk_ref(),
        };
        let asset_body =
            DefineAssetBody::new(&code, &issuer_key, asset_rules, memo, None).c(d!())?;
        let asset_create =
            DefineAsset::new(asset_body, &IssuerKeyPair { keypair: &keypair })
                .c(d!())?;
        Ok(Transaction::from_operation(
            Operation::DefineAsset(asset_create),
            seq_id,
        ))
    }

    pub fn build_keys<R: CryptoRng + RngCore>(prng: &mut R) -> XfrKeyPair {
        XfrKeyPair::generate(prng)
    }

    pub fn asset_creation_body(
        token_code: &AssetTypeCode,
        issuer_key: &XfrPublicKey,
        asset_rules: AssetRules,
        memo: Option<Memo>,
        confidential_memo: Option<ConfidentialMemo>,
    ) -> DefineAssetBody {
        let mut token = Asset {
            code: *token_code,
            issuer: IssuerPublicKey { key: *issuer_key },
            asset_rules,
            ..Default::default()
        };

        if let Some(memo) = memo {
            token.memo = memo;
        } else {
            token.memo = Memo(String::from(""));
        }

        if let Some(confidential_memo) = confidential_memo {
            token.confidential_memo = confidential_memo;
        } else {
            token.confidential_memo = ConfidentialMemo {};
        }

        DefineAssetBody {
            asset: Box::new(token),
        }
    }

    pub fn asset_creation_operation(
        asset_body: &DefineAssetBody,
        iss_key: &XfrKeyPair,
    ) -> DefineAsset {
        let signature = SignatureOf::new(iss_key, asset_body);
        DefineAsset {
            body: asset_body.clone(),
            pubkey: IssuerPublicKey {
                key: *iss_key.get_pk_ref(),
            },
            signature,
        }
    }

    pub fn apply_transaction(
        ledger: &mut LedgerState,
        tx: Transaction,
    ) -> (TxnSID, Vec<TxoSID>) {
        match TxnEffect::compute_effect(tx) {
            Ok(effect) => {
                let mut block = ledger.start_block().unwrap();
                let temp_sid =
                    ledger.apply_transaction(&mut block, effect, false).unwrap();
                ledger
                    .finish_block(block)
                    .unwrap()
                    .remove(&temp_sid)
                    .unwrap()
            }
            Err(e) => {
                fn unwrap_failed(msg: &str, error: impl Debug) -> ! {
                    panic!("{}: {:?}", msg, error)
                }
                unwrap_failed("apply_transaction: error in compute_effect", e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_issue_and_transfer_txn(
        ledger: &mut LedgerState,
        params: &PublicParams,
        code: &AssetTypeCode,
        amount: u64,
        issuer_keys: &XfrKeyPair,
        recipient_pk: &XfrPublicKey,
        seq_num: u64,
    ) -> (Transaction, AssetRecord) {
        // issue operation
        let ar_template = AssetRecordTemplate::with_no_asset_tracing(
            amount,
            code.val,
            AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
            issuer_keys.get_pk(),
        );
        let (ba, _tracer_memo, owner_memo) = build_blind_asset_record(
            ledger.get_prng(),
            &params.pc_gens,
            &ar_template,
            vec![],
        );

        let asset_issuance_body = IssueAssetBody::new(
            &code,
            seq_num,
            &[(
                TxOutput {
                    id: None,
                    record: ba.clone(),
                    lien: None,
                },
                None,
            )],
        )
        .unwrap();
        let asset_issuance_operation = IssueAsset::new(
            asset_issuance_body,
            &IssuerKeyPair {
                keypair: &issuer_keys,
            },
        )
        .unwrap();

        let issue_op = Operation::IssueAsset(asset_issuance_operation);

        // transfer operation
        let ar_template = AssetRecordTemplate::with_no_asset_tracing(
            amount,
            code.val,
            AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
            *recipient_pk,
        );
        let ar = AssetRecord::from_template_no_identity_tracing(
            ledger.get_prng(),
            &ar_template,
        )
        .unwrap();
        let mut transfer = pnk!(TransferAsset::new(pnk!(TransferAssetBody::new(
            ledger.get_prng(),
            vec![TxoRef::Relative(0)],
            &[AssetRecord::from_open_asset_record_no_asset_tracing(
                open_blind_asset_record(&ba, &owner_memo, &issuer_keys).unwrap()
            )],
            &[ar.clone()],
            None,
            vec![],
            TransferType::Standard,
        )),));

        transfer.sign(&issuer_keys);
        let seq_id = ledger.get_block_commit_count();
        let mut tx = Transaction::from_operation(issue_op, seq_id);
        tx.add_operation(Operation::TransferAsset(transfer));
        (tx, ar)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_issue_and_transfer_txn_with_asset_tracing(
        ledger: &mut LedgerState,
        params: &PublicParams,
        code: &AssetTypeCode,
        amount: u64,
        issuer_keys: &XfrKeyPair,
        recipient_pk: &XfrPublicKey,
        seq_num: u64,
        tracing_policy: TracingPolicy,
    ) -> (Transaction, AssetRecord) {
        let tracing_policies = TracingPolicies::from_policy(tracing_policy);
        let xfr_note_policies = XfrNotePolicies::new(
            vec![tracing_policies.clone()],
            vec![None],
            vec![tracing_policies.clone()],
            vec![None],
        );
        // issue operation
        let ar_template = AssetRecordTemplate::with_asset_tracing(
            amount,
            code.val,
            AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
            issuer_keys.get_pk(),
            tracing_policies.clone(),
        );
        let (ba, _tracer_memo, owner_memo) = build_blind_asset_record(
            ledger.get_prng(),
            &params.pc_gens,
            &ar_template,
            vec![vec![]],
        );

        let asset_issuance_body = IssueAssetBody::new(
            &code,
            seq_num,
            &[(
                TxOutput {
                    id: None,
                    record: ba.clone(),
                    lien: None,
                },
                None,
            )],
        )
        .unwrap();
        let asset_issuance_operation = IssueAsset::new(
            asset_issuance_body,
            &IssuerKeyPair {
                keypair: &issuer_keys,
            },
        )
        .unwrap();

        let issue_op = Operation::IssueAsset(asset_issuance_operation);

        // transfer operation
        let ar_template = AssetRecordTemplate::with_asset_tracing(
            amount,
            code.val,
            AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
            *recipient_pk,
            tracing_policies.clone(),
        );
        let ar = AssetRecord::from_template_no_identity_tracing(
            ledger.get_prng(),
            &ar_template,
        )
        .unwrap();
        let tar =
            AssetRecord::from_open_asset_record_with_asset_tracing_but_no_identity(
                ledger.get_prng(),
                open_blind_asset_record(&ba, &owner_memo, &issuer_keys).unwrap(),
                tracing_policies,
            )
            .unwrap();
        let mut transfer = TransferAsset::new(
            TransferAssetBody::new(
                ledger.get_prng(),
                vec![TxoRef::Relative(0)],
                &[tar],
                &[ar.clone()],
                Some(xfr_note_policies),
                vec![],
                TransferType::Standard,
            )
            .unwrap(),
        )
        .unwrap();

        transfer.sign(&issuer_keys);
        // FIXME: `from_operation` takes a no_replay_token, but only two operations need them.
        // IssueAsset does not, so we use a default
        let seq_id = ledger.get_block_commit_count();
        let mut tx = Transaction::from_operation(issue_op, seq_id);
        tx.add_operation(Operation::TransferAsset(transfer));
        (tx, ar)
    }

    pub fn create_issuance_txn(
        ledger: &mut LedgerState,
        params: &PublicParams,
        code: &AssetTypeCode,
        amount: u64,
        seq_num: u64,
        record_type: AssetRecordType,
        issuer_keys: &XfrKeyPair,
    ) -> Transaction {
        // issue operation
        let ar_template = AssetRecordTemplate::with_no_asset_tracing(
            amount,
            code.val,
            record_type,
            issuer_keys.get_pk(),
        );
        let (ba, _tracer_memo, _owner_memo) = build_blind_asset_record(
            ledger.get_prng(),
            &params.pc_gens,
            &ar_template,
            vec![],
        );

        let asset_issuance_body = IssueAssetBody::new(
            &code,
            seq_num,
            &[(
                TxOutput {
                    id: None,
                    record: ba,
                    lien: None,
                },
                None,
            )],
        )
        .unwrap();
        let asset_issuance_operation = IssueAsset::new(
            asset_issuance_body,
            &IssuerKeyPair {
                keypair: &issuer_keys,
            },
        )
        .unwrap();
        let seq_id = ledger.get_block_commit_count();
        Transaction::from_operation(
            Operation::IssueAsset(asset_issuance_operation),
            seq_id,
        )
    }
}

/// Define and Issue FRA.
/// Currently this should only be used for tests.
pub fn fra_gen_initial_tx(fra_owner_kp: &XfrKeyPair) -> Transaction {
    /*
     * Define FRA
     **/

    let fra_code = AssetTypeCode {
        val: ASSET_TYPE_FRA,
    };

    let mut tx = pnk!(helpers::create_definition_transaction(
        &fra_code,
        fra_owner_kp,
        AssetRules {
            max_units: Some(1000 + FRA_TOTAL_AMOUNT),
            decimals: FRA_DECIMALS,
            ..AssetRules::default()
        },
        Some(Memo("FRA".to_owned())),
        0,
    ));

    /*
     * Issue FRA
     **/

    let template = AssetRecordTemplate::with_no_asset_tracing(
        FRA_TOTAL_AMOUNT / 2,
        fra_code.val,
        AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
        fra_owner_kp.get_pk(),
    );

    let params = PublicParams::default();

    let outputs = (0..2)
        .map(|_| {
            let (ba, _, _) = build_blind_asset_record(
                &mut ChaChaRng::from_entropy(),
                &params.pc_gens,
                &template,
                vec![],
            );
            (
                TxOutput {
                    id: None,
                    record: ba,
                    lien: None,
                },
                None,
            )
        })
        .collect::<Vec<_>>();
    let asset_issuance_body = IssueAssetBody::new(&fra_code, 0, &outputs).unwrap();

    let asset_issuance_operation = IssueAsset::new(
        asset_issuance_body,
        &IssuerKeyPair {
            keypair: fra_owner_kp,
        },
    )
    .unwrap();

    tx.add_operation(Operation::IssueAsset(asset_issuance_operation));

    tx
}

#[cfg(test)]
mod tests {
    use super::helpers::*;
    use super::*;
    use crate::data_model::{ASSET_TYPE_FRA, BLACK_HOLE_PUBKEY, TX_FEE_MIN};
    use rand_core::SeedableRng;
    use tempfile::tempdir;
    use zei::setup::PublicParams;
    use zei::xfr::asset_record::{
        build_blind_asset_record, open_blind_asset_record, AssetRecordType,
    };
    use zei::xfr::sig::XfrKeyPair;
    use zei::xfr::structs::{AssetRecord, AssetRecordTemplate, AssetTracerKeyPair};

    #[test]
    fn test_load_fake_transaction_log() {
        // Verify that loading transaction fails with incorrect path
        let result_err = LedgerState::load_transaction_log("incorrect/path");
        assert!(result_err.is_err());
    }

    #[test]
    fn test_save_utxo_map_version() {
        let mut ledger_state = LedgerState::test_ledger();
        let digest = BitDigest { 0: [0_u8; 32] };
        ledger_state.status.utxo_map_versions =
            vec![(TxnSID(0), digest); MAX_VERSION - 1]
                .into_iter()
                .collect();

        // Verify that save_utxo_map_version increases the size of utxo_map_versions by 1 if its length < MAX_VERSION
        ledger_state.save_utxo_map_version();
        assert_eq!(ledger_state.status.utxo_map_versions.len(), MAX_VERSION);

        // Verify that save_utxo_map_version doesn't change the size of utxo_map_versions if its length >= MAX_VERSION
        ledger_state
            .status
            .utxo_map_versions
            .push_back((TxnSID(0), digest));
        assert_eq!(ledger_state.status.utxo_map_versions.len(), MAX_VERSION + 1);
        ledger_state.save_utxo_map_version();
        assert_eq!(ledger_state.status.utxo_map_versions.len(), MAX_VERSION + 1);

        // Verify that the element pushed to the back is as expected
        let back = ledger_state.status.utxo_map_versions.get(MAX_VERSION);
        assert_eq!(
            back,
            Some(&(
                TxnSID(ledger_state.status.next_txn.get(0).unwrap().0),
                ledger_state.utxo_map.compute_checksum()
            ))
        );
    }

    #[test]
    fn test_compute_and_save_block_hash() {
        let mut ledger_state = LedgerState::test_ledger();
        let mut data = StateCommitmentData {
            bitmap: ledger_state.utxo_map.compute_checksum(),
            block_merkle: ledger_state.block_merkle.get_root_hash(),
            txns_in_block_hash: HashOf::new(&vec![]),
            previous_state_commitment: HashOf::new(&None),
            transaction_merkle_commitment: ledger_state.txn_merkle.get_root_hash(),
            air_commitment: BitDigest::from_slice(&[0; 32][..]).unwrap(),
            txo_count: 0,
            pulse_count: 0,
        };

        // dbg!(&data);
        let count_original = ledger_state.status.block_commit_count;

        let b = ledger_state.start_block().unwrap();
        ledger_state.finish_block(b).unwrap();
        data.block_merkle = ledger_state.block_merkle.get_root_hash();

        let first_hash = data.compute_commitment();

        // dbg!(&ledger_state.status.state_commitment_data);

        assert_eq!(
            ledger_state
                .status
                .state_commitment_data
                .clone()
                .unwrap()
                .compute_commitment(),
            first_hash
        );
        assert_eq!(
            ledger_state
                .get_state_commitment_at_block_height(1)
                .unwrap(),
            first_hash
        );
        assert_eq!(ledger_state.status.block_commit_count, count_original + 1);
    }

    #[test]
    fn test_init_merkle_log() {
        let tmp_dir = tempdir().unwrap();
        let buf = tmp_dir.path().join("test_merkle");
        let path = buf.to_str().unwrap();

        // Verify that opening a non-existing Merkle tree fails
        let result_open_err = LedgerState::init_merkle_log(path, false);
        assert!(result_open_err.is_err());

        // Verify that creating a non-existing Merkle tree succeeds
        let result_create_ok = LedgerState::init_merkle_log(path, true);
        assert!(result_create_ok.is_ok());

        // Verify that opening an existing Merkle tree succeeds
        let result_open_ok = LedgerState::init_merkle_log(path, false);
        assert!(result_open_ok.is_ok());

        // // Verify that creating an existing Merkle tree fails
        // let result_create_err = LedgerState::init_merkle_log(path, true);
        // assert!(result_create_err.is_err());

        tmp_dir.close().unwrap();
    }

    #[test]
    fn test_init_utxo_map() {
        let tmp_dir = tempdir().unwrap();
        let buf = tmp_dir.path().join("test_init_bitmap");
        let path = buf.to_str().unwrap();

        // Verify that opening a non-existing bitmap fails
        let result_open_err = LedgerState::init_utxo_map(path, false);
        assert!(result_open_err.is_err());

        // Verify that creating a non-existing bitmap succeeds
        let result_create_ok = LedgerState::init_utxo_map(path, true);
        assert!(result_create_ok.is_ok());

        // Verify that creating an existing bitmap succeeds
        let result_open_ok = LedgerState::init_utxo_map(path, false);
        assert!(result_open_ok.is_ok());

        // // Verify that opening an existing bitmap fails
        // let result_create_err = LedgerState::init_utxo_map(path, true);
        // assert!(result_create_err.is_err());

        tmp_dir.close().unwrap();
    }

    #[test]
    fn test_checkpoint() {
        let mut ledger_state = LedgerState::test_ledger();

        let digest = BitDigest { 0: [0_u8; 32] };
        ledger_state.status.utxo_map_versions =
            vec![(TxnSID(0), digest); MAX_VERSION - 1]
                .into_iter()
                .collect();

        // Verify that checkpoint increases the size of utxo_map_versions by 1 if its length < MAX_VERSION
        pnk!(ledger_state.checkpoint(&BlockEffect::new()));
        assert_eq!(ledger_state.status.utxo_map_versions.len(), MAX_VERSION);

        let count_original = ledger_state.status.block_commit_count;
        let (commitment1, v1) = ledger_state.get_state_commitment();

        // Verify that end_commit doesn't change the size of utxo_map_versions if its length >= MAX_VERSION
        ledger_state
            .status
            .utxo_map_versions
            .push_back((TxnSID(0), digest));
        assert_eq!(ledger_state.status.utxo_map_versions.len(), MAX_VERSION + 1);
        pnk!(ledger_state.checkpoint(&BlockEffect::new()));
        assert_eq!(ledger_state.status.utxo_map_versions.len(), MAX_VERSION + 1);
        let (commitment2, v2) = ledger_state.get_state_commitment();

        // Verify that the element pushed to the back is as expected
        let back = ledger_state.status.utxo_map_versions.get(MAX_VERSION);
        assert_eq!(
            back,
            Some(&(
                TxnSID(ledger_state.status.next_txn.get(0).unwrap().0),
                ledger_state.utxo_map.compute_checksum()
            ))
        );

        // Verify that the status is saved as expected
        assert_eq!(
            ledger_state.status.txns_in_block_hash.clone().unwrap(),
            BlockEffect::new().compute_txns_in_block_hash()
        );
        assert_eq!(ledger_state.status.block_commit_count, count_original + 1);
        // Check state commitment history
        assert_eq!(
            ledger_state
                .get_state_commitment_at_block_height(v1)
                .unwrap(),
            commitment1
        );
        assert_eq!(
            ledger_state
                .get_state_commitment_at_block_height(v2)
                .unwrap(),
            commitment2
        );
    }

    /*
      #[test]
      fn test_create_merkle_log() {
        let tmp_dir = tempdir().c(d!())?;
        let buf = tmp_dir.path().join("merkle_log");
        let base_path = buf.to_str().c(d!())?;

        let result = LedgerState::create_merkle_log(base_path.to_string(), 0);
        assert!(result.is_ok());

        let path = base_path.to_owned() + "-log-0";
        assert!(fs::metadata(path).is_ok());

        tmp_dir.close().c(d!())?;
      }
    */

    #[test]
    fn test_asset_creation_valid() {
        let mut prng = ChaChaRng::from_entropy();
        let mut state = LedgerState::test_ledger();

        let token_code1 = AssetTypeCode::gen_random();
        let keypair = build_keys(&mut prng);

        let asset_body = asset_creation_body(
            &token_code1,
            keypair.get_pk_ref(),
            AssetRules::default(),
            None,
            None,
        );
        let asset_create = asset_creation_operation(&asset_body, &keypair);
        let seq_id = state.get_block_commit_count();
        let tx =
            Transaction::from_operation(Operation::DefineAsset(asset_create), seq_id);
        let effect = TxnEffect::compute_effect(tx).unwrap();
        {
            let mut block = state.start_block().unwrap();
            state.apply_transaction(&mut block, effect, false).unwrap();
            state.finish_block(block).unwrap();
        }

        assert!(state.get_asset_type(&token_code1).is_some());

        assert_eq!(
            *asset_body.asset,
            state.get_asset_type(&token_code1).unwrap().properties
        );

        assert_eq!(0, state.get_asset_type(&token_code1).unwrap().units);
    }

    // Change the signature to have the wrong public key
    #[test]
    fn test_asset_creation_invalid_public_key() {
        // Create a valid asset creation operation.
        let token_code1 = AssetTypeCode::gen_random();
        let mut prng = ChaChaRng::from_entropy();
        let keypair = build_keys(&mut prng);
        let asset_body = asset_creation_body(
            &token_code1,
            keypair.get_pk_ref(),
            AssetRules::default(),
            None,
            None,
        );
        let mut asset_create = asset_creation_operation(&asset_body, &keypair);

        // Now re-sign the operation with the wrong key.
        let mut prng = ChaChaRng::from_seed([1u8; 32]);
        let keypair = build_keys(&mut prng);

        asset_create.pubkey.key = *keypair.get_pk_ref();
        let tx = Transaction::from_operation(Operation::DefineAsset(asset_create), 0);

        assert!(TxnEffect::compute_effect(tx).is_err());
    }

    #[test]
    fn test_asset_transfer() {
        let mut ledger = LedgerState::test_ledger();
        let params = PublicParams::default();

        let code = AssetTypeCode::gen_random();
        let mut prng = ChaChaRng::from_entropy();
        let key_pair = XfrKeyPair::generate(&mut prng);
        let key_pair_adversary = XfrKeyPair::generate(ledger.get_prng());

        let tx = create_definition_transaction(
            &code,
            &key_pair,
            AssetRules::default(),
            None,
            ledger.get_block_commit_count(),
        )
        .unwrap();

        let effect = TxnEffect::compute_effect(tx).unwrap();
        {
            let mut block = ledger.start_block().unwrap();
            ledger.apply_transaction(&mut block, effect, false).unwrap();
            ledger.finish_block(block).unwrap();
        }

        // Issuance with two outputs
        let art = AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType;
        let template = AssetRecordTemplate::with_no_asset_tracing(
            100,
            code.val,
            art,
            key_pair.get_pk(),
        );
        let (ba, _, _) = build_blind_asset_record(
            ledger.get_prng(),
            &params.pc_gens,
            &template,
            vec![],
        );
        let second_ba = ba.clone();

        let asset_issuance_body = IssueAssetBody::new(
            &code,
            0,
            &[
                (
                    TxOutput {
                        id: None,
                        record: ba,
                        lien: None,
                    },
                    None,
                ),
                (
                    TxOutput {
                        id: None,
                        record: second_ba,
                        lien: None,
                    },
                    None,
                ),
            ],
        )
        .unwrap();
        let asset_issuance_operation =
            IssueAsset::new(asset_issuance_body, &IssuerKeyPair { keypair: &key_pair })
                .unwrap();

        let issue_op = Operation::IssueAsset(asset_issuance_operation);

        let tx = Transaction::from_operation(issue_op, ledger.get_block_commit_count());

        // Commit issuance to block
        let effect = TxnEffect::compute_effect(tx).unwrap();

        let mut block = ledger.start_block().unwrap();
        let temp_sid = ledger.apply_transaction(&mut block, effect, false).unwrap();

        let (_txn_sid, txos) = ledger
            .finish_block(block)
            .unwrap()
            .remove(&temp_sid)
            .unwrap();
        let state_commitment = ledger.get_state_commitment().0;

        for txo_id in &txos {
            assert!(ledger.status.utxos.contains_key(&txo_id));
            let utxo_status = ledger.get_utxo_status(*txo_id);
            assert!(utxo_status.is_valid(state_commitment.clone()));
            assert!(utxo_status.status == UtxoStatus::Unspent);
        }

        // Store txo_sids for subsequent transfers
        let txo_sid = txos[0];
        let second_txo_id = txos[1];

        // Construct transfer operation
        let input_bar_proof = ledger.get_utxo(txo_sid).unwrap();
        let input_bar = (input_bar_proof.clone().utxo.0).record;
        let input_oar = open_blind_asset_record(&input_bar, &None, &key_pair).unwrap();
        assert!(input_bar_proof.is_valid(state_commitment));

        let output_template = AssetRecordTemplate::with_no_asset_tracing(
            100,
            code.val,
            art,
            key_pair_adversary.get_pk(),
        );
        let output_ar = AssetRecord::from_template_no_identity_tracing(
            ledger.get_prng(),
            &output_template,
        )
        .unwrap();
        let input_ar = AssetRecord::from_open_asset_record_no_asset_tracing(input_oar);

        let mut transfer = TransferAsset::new(
            TransferAssetBody::new(
                ledger.get_prng(),
                vec![TxoRef::Absolute(txo_sid)],
                &[input_ar],
                &[output_ar],
                None,
                vec![],
                TransferType::Standard,
            )
            .unwrap(),
        )
        .unwrap();

        let mut second_transfer = transfer.clone();
        transfer.sign(&key_pair);
        let tx = Transaction::from_operation(
            Operation::TransferAsset(transfer),
            ledger.get_block_commit_count(),
        );

        // Commit first transfer
        let effect = TxnEffect::compute_effect(tx).unwrap();
        let mut block = ledger.start_block().unwrap();
        let temp_sid = ledger.apply_transaction(&mut block, effect, false).unwrap();

        let (_txn_sid, _txos) = ledger
            .finish_block(block)
            .unwrap()
            .remove(&temp_sid)
            .unwrap();
        // Ensure that previous txo is now spent
        let state_commitment = ledger.get_state_commitment().0;
        let utxo_status = ledger.get_utxo_status(TxoSID(0));
        assert!(utxo_status.is_valid(state_commitment.clone()));
        assert!(!input_bar_proof.is_valid(state_commitment));
        assert!(utxo_status.status == UtxoStatus::Spent);

        // Adversary will attempt to spend the same blind asset record at another index
        second_transfer.body.inputs = vec![TxoRef::Absolute(second_txo_id)];

        // Submit spend of same asset at second sid without signature
        second_transfer.body_signatures = Vec::new();
        let seq_id = ledger.get_block_commit_count();
        let tx = Transaction::from_operation(
            Operation::TransferAsset(second_transfer),
            seq_id,
        );

        let effect = TxnEffect::compute_effect(tx);
        assert!(effect.is_err());
    }

    // Sign with the wrong key.
    #[test]
    fn test_asset_creation_invalid_signature() {
        // Create a valid operation.
        let token_code1 = AssetTypeCode::gen_random();

        let mut prng = ChaChaRng::from_entropy();
        let keypair1 = build_keys(&mut prng);

        let asset_body = asset_creation_body(
            &token_code1,
            keypair1.get_pk_ref(),
            AssetRules::default(),
            None,
            None,
        );
        let mut asset_create = asset_creation_operation(&asset_body, &keypair1);

        // Re-sign the operation with the wrong key.
        let mut prng = ChaChaRng::from_seed([1u8; 32]);
        let keypair2 = build_keys(&mut prng);

        asset_create.pubkey.key = *keypair2.get_pk_ref();
        let tx = Transaction::from_operation(Operation::DefineAsset(asset_create), 0); // OK because no ledger interaction

        assert!(TxnEffect::compute_effect(tx).is_err());
    }

    #[test]
    fn asset_issued() {
        let mut ledger = LedgerState::test_ledger();

        let params = PublicParams::default();

        assert!(ledger.get_state_commitment() == (HashOf::new(&None), 0));
        let token_code1 = AssetTypeCode::gen_random();
        let keypair = build_keys(&mut ledger.get_prng());
        let seq_id = ledger.get_block_commit_count();
        let tx = create_definition_transaction(
            &token_code1,
            &keypair,
            AssetRules::default(),
            None,
            seq_id,
        )
        .unwrap();

        let effect = TxnEffect::compute_effect(tx).unwrap();
        {
            let mut block = ledger.start_block().unwrap();
            ledger.apply_transaction(&mut block, effect, false).unwrap();
            ledger.finish_block(block).unwrap();
        }

        let art = AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType;
        let ar = AssetRecordTemplate::with_no_asset_tracing(
            100,
            token_code1.val,
            art,
            *keypair.get_pk_ref(),
        );

        let (ba, _, _) =
            build_blind_asset_record(ledger.get_prng(), &params.pc_gens, &ar, vec![]);
        let asset_issuance_body = IssueAssetBody::new(
            &token_code1,
            0,
            &[(
                TxOutput {
                    id: None,
                    record: ba,
                    lien: None,
                },
                None,
            )],
        )
        .unwrap();
        let asset_issuance_operation =
            IssueAsset::new(asset_issuance_body, &IssuerKeyPair { keypair: &keypair })
                .unwrap();

        let issue_op = Operation::IssueAsset(asset_issuance_operation);

        let seq_id = ledger.get_block_commit_count();
        let tx = Transaction::from_operation(issue_op, seq_id);
        let second_tx = tx.clone();

        let effect = TxnEffect::compute_effect(tx).unwrap();

        let mut block = ledger.start_block().unwrap();
        let temp_sid = ledger.apply_transaction(&mut block, effect, false).unwrap();

        let (txn_sid, txos) = ledger
            .finish_block(block)
            .unwrap()
            .remove(&temp_sid)
            .unwrap();

        // shouldn't be able to replay issuance
        let effect = TxnEffect::compute_effect(second_tx).unwrap();
        let mut block = ledger.start_block().unwrap();
        let result = ledger.apply_transaction(&mut block, effect, false);
        assert!(result.is_err());
        ledger.abort_block(block);

        let transaction = ledger.get_transaction(txn_sid).unwrap();
        let txn_id = transaction.finalized_txn.tx_id;
        let state_commitment_and_version = ledger.get_state_commitment();

        println!("utxos = {:?}", ledger.status.utxos);
        for txo_id in txos {
            assert!(ledger.status.utxos.contains_key(&txo_id));
            let utxo_status = ledger.get_utxo_status(txo_id);
            assert!(utxo_status.is_valid(state_commitment_and_version.0.clone()));
            assert!(utxo_status.status == UtxoStatus::Unspent);
        }

        match ledger.get_block(BlockSID(0)) {
            Some(authenticated_block) => {
                assert!(
                    authenticated_block.is_valid(state_commitment_and_version.0.clone())
                );
            }
            None => panic!("get_proof failed for block id 0"),
        }

        match ledger.get_transaction(txn_id) {
            Ok(authenticated_txn) => {
                assert!(
                    authenticated_txn.txn_inclusion_proof.0.proof.tx_id
                        == authenticated_txn.finalized_txn.merkle_id
                );
                assert!(
                    authenticated_txn.is_valid(state_commitment_and_version.0.clone())
                );
                assert!(transaction.finalized_txn == authenticated_txn.finalized_txn);
            }
            Err(_) => {
                panic!(
                    "get_proof failed for tx_id {}, merkle_id {}, block state {}, transaction state {}",
                    transaction.finalized_txn.tx_id.0,
                    transaction.finalized_txn.merkle_id,
                    ledger.block_merkle.state(),
                    ledger.txn_merkle.state()
                );
            }
        }

        // We don't actually have anything to commmit yet,
        // but this will save the empty checksum, which is
        // enough for a bit of a test.
        assert!(
            state_commitment_and_version
                == (
                    ledger
                        .status
                        .state_commitment_data
                        .clone()
                        .unwrap()
                        .compute_commitment(),
                    2
                )
        );
        let query_result = ledger
            .get_utxo_checksum(pnk!(ledger.status.next_txn.get(0).c(d!())).0 as u64)
            .unwrap();
        let compute_result = ledger.utxo_map.compute_checksum();
        println!(
            "query_result = {:?}, compute_result = {:?}",
            query_result, compute_result
        );

        assert!(query_result == compute_result);
    }

    #[test]
    pub fn test_transferable() {
        let mut ledger = LedgerState::test_ledger();
        let params = PublicParams::default();
        let issuer = XfrKeyPair::generate(&mut ledger.get_prng());
        let alice = XfrKeyPair::generate(&mut ledger.get_prng());
        let bob = XfrKeyPair::generate(&mut ledger.get_prng());

        // Define fiat token
        let code = AssetTypeCode::gen_random();
        let seq_id = ledger.get_block_commit_count();
        let tx = create_definition_transaction(
            &code,
            &issuer,
            AssetRules::default().set_transferable(false).clone(),
            Some(Memo("test".to_string())),
            seq_id,
        )
        .unwrap();
        apply_transaction(&mut ledger, tx);
        let (tx, _) = create_issue_and_transfer_txn(
            &mut ledger,
            &params,
            &code,
            100,
            &issuer,
            alice.get_pk_ref(),
            0,
        );
        let (_, sids) = apply_transaction(&mut ledger, tx);
        let sid = sids[0];

        let bar = ledger.get_utxo_light(sid).unwrap().utxo.0.record;

        let transfer_template = AssetRecordTemplate::with_no_asset_tracing(
            100,
            code.val,
            AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
            bob.get_pk(),
        );
        let record = AssetRecord::from_template_no_identity_tracing(
            ledger.get_prng(),
            &transfer_template,
        )
        .unwrap();

        // Cant transfer non-transferable asset
        let mut transfer = TransferAsset::new(
            TransferAssetBody::new(
                ledger.get_prng(),
                vec![TxoRef::Absolute(sid)],
                &[AssetRecord::from_open_asset_record_no_asset_tracing(
                    open_blind_asset_record(&bar, &None, &alice).unwrap(),
                )],
                &[record],
                None,
                vec![],
                TransferType::Standard,
            )
            .unwrap(),
        )
        .unwrap();
        transfer.sign(&alice);
        let seq_id = ledger.get_block_commit_count();
        let tx = Transaction::from_operation(Operation::TransferAsset(transfer), seq_id);
        let effect = TxnEffect::compute_effect(tx).unwrap();

        let mut block = ledger.start_block().unwrap();
        let res = ledger.apply_transaction(&mut block, effect, false);
        assert!(res.is_err());
        // Cant transfer by making asset confidential
        let transfer_template = AssetRecordTemplate::with_no_asset_tracing(
            100,
            code.val,
            AssetRecordType::ConfidentialAmount_ConfidentialAssetType,
            bob.get_pk(),
        );
        let record = AssetRecord::from_template_no_identity_tracing(
            ledger.get_prng(),
            &transfer_template,
        )
        .unwrap();

        // Cant transfer non-transferable asset
        let mut transfer = TransferAsset::new(
            TransferAssetBody::new(
                ledger.get_prng(),
                vec![TxoRef::Absolute(sid)],
                &[AssetRecord::from_open_asset_record_no_asset_tracing(
                    open_blind_asset_record(&bar, &None, &alice).unwrap(),
                )],
                &[record],
                None,
                vec![],
                TransferType::Standard,
            )
            .unwrap(),
        )
        .unwrap();
        transfer.sign(&alice);
        let seq_id = ledger.get_block_commit_count();
        let tx = Transaction::from_operation(Operation::TransferAsset(transfer), seq_id);
        let effect = TxnEffect::compute_effect(tx).unwrap();

        let res = ledger.apply_transaction(&mut block, effect, false);
        assert!(res.is_err());
        // Cant transfer non-transferable asset through some intermediate operation
        // In this case, alice attempts to spend her non-transferable asset in the same transaction it
        // was issued.
        let second_transfer_template = AssetRecordTemplate::with_no_asset_tracing(
            100,
            code.val,
            AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
            bob.get_pk(),
        );
        let second_record = AssetRecord::from_template_no_identity_tracing(
            ledger.get_prng(),
            &second_transfer_template,
        )
        .unwrap();
        let (mut tx, ar) = create_issue_and_transfer_txn(
            &mut ledger,
            &params,
            &code,
            100,
            &issuer,
            alice.get_pk_ref(),
            1,
        );
        let mut transfer = TransferAsset::new(
            TransferAssetBody::new(
                ledger.get_prng(),
                vec![TxoRef::Relative(0)],
                &[AssetRecord::from_open_asset_record_no_asset_tracing(
                    ar.open_asset_record,
                )],
                &[second_record],
                None,
                vec![],
                TransferType::Standard,
            )
            .unwrap(),
        )
        .unwrap();
        transfer.sign(&alice);
        tx.body.operations.push(Operation::TransferAsset(transfer));
        let effect = TxnEffect::compute_effect(tx).unwrap();
        let res = ledger.apply_transaction(&mut block, effect, false);
        assert!(res.is_err());
    }

    #[test]
    pub fn test_tracing_policy() {
        let mut ledger = LedgerState::test_ledger();
        let params = PublicParams::default();

        let issuer = XfrKeyPair::generate(&mut ledger.get_prng());
        let recipient = XfrKeyPair::generate(&mut ledger.get_prng());

        // Set tracing policies
        let tracer_kp = AssetTracerKeyPair::generate(&mut ledger.get_prng());
        let tracing_policy = TracingPolicy {
            enc_keys: tracer_kp.enc_key.clone(),
            asset_tracing: true,
            identity_tracing: None,
        };
        let unmatched_tracing_policy = TracingPolicy {
            enc_keys: tracer_kp.enc_key,
            asset_tracing: false,
            identity_tracing: None,
        };

        // Define an asset without a tracing policy
        let code = AssetTypeCode::gen_random();
        let seq_id = ledger.get_block_commit_count();
        let tx = create_definition_transaction(
            &code,
            &issuer,
            AssetRules::default(),
            Some(Memo("test".to_string())),
            seq_id,
        )
        .unwrap();
        apply_transaction(&mut ledger, tx);

        // Issue and transfer the asset without a tracing policy
        // Should succeed
        let (tx, _) = create_issue_and_transfer_txn(
            &mut ledger,
            &params,
            &code,
            100,
            &issuer,
            recipient.get_pk_ref(),
            0,
        );
        apply_transaction(&mut ledger, tx);

        // Define an asset with the tracing policy
        let code = AssetTypeCode::gen_random();
        let seq_id = ledger.get_block_commit_count();
        let tx = create_definition_transaction(
            &code,
            &issuer,
            AssetRules::default()
                .add_tracing_policy(tracing_policy.clone())
                .clone(),
            Some(Memo("test".to_string())),
            seq_id,
        )
        .unwrap();
        apply_transaction(&mut ledger, tx);

        // Issue and transfer the asset without a tracing policy
        // Should fail
        let (tx, _) = create_issue_and_transfer_txn(
            &mut ledger,
            &params,
            &code,
            100,
            &issuer,
            recipient.get_pk_ref(),
            0,
        );
        let mut block = ledger.start_block().unwrap();
        let effect = TxnEffect::compute_effect(tx).unwrap();
        let res = ledger.apply_transaction(&mut block, effect, false);
        assert!(res.is_err());

        // Issue and transfer the asset to with the unmatched tracing policy
        // Should fail
        let (tx, _) = create_issue_and_transfer_txn_with_asset_tracing(
            &mut ledger,
            &params,
            &code,
            100,
            &issuer,
            recipient.get_pk_ref(),
            0,
            unmatched_tracing_policy,
        );
        let effect = TxnEffect::compute_effect(tx).unwrap();
        let res = ledger.apply_transaction(&mut block, effect, false);
        assert!(res.is_err());

        // Issue and transfer the asset with the correct tracing policy
        // Should pass
        let (tx, _) = create_issue_and_transfer_txn_with_asset_tracing(
            &mut ledger,
            &params,
            &code,
            100,
            &issuer,
            recipient.get_pk_ref(),
            0,
            tracing_policy,
        );
        let effect = TxnEffect::compute_effect(tx).unwrap();
        let res = ledger.apply_transaction(&mut block, effect, false);

        assert!(res.is_ok());
    }

    #[test]
    pub fn test_max_units() {
        let mut ledger = LedgerState::test_ledger();
        let params = PublicParams::default();

        let issuer = XfrKeyPair::generate(&mut ledger.get_prng());

        // Define fiat token
        let code = AssetTypeCode::gen_random();
        let seq_id = ledger.get_block_commit_count();
        let tx = create_definition_transaction(
            &code,
            &issuer,
            AssetRules::default().set_max_units(Some(100)).clone(),
            Some(Memo("test".to_string())),
            seq_id,
        )
        .unwrap();
        apply_transaction(&mut ledger, tx);
        let tx = create_issuance_txn(
            &mut ledger,
            &params,
            &code,
            50,
            0,
            AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
            &issuer,
        );
        apply_transaction(&mut ledger, tx);
        {
            // Ensure that a single overlfowing transaction fails
            let tx = create_issuance_txn(
                &mut ledger,
                &params,
                &code,
                51,
                1,
                AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
                &issuer,
            );
            let effect = TxnEffect::compute_effect(tx).unwrap();

            let mut block = ledger.start_block().unwrap();
            let res = ledger.apply_transaction(&mut block, effect, false);
            assert!(res.is_err());

            // Ensure that cap can be reached
            let tx = create_issuance_txn(
                &mut ledger,
                &params,
                &code,
                50,
                1,
                AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
                &issuer,
            );
            let effect = TxnEffect::compute_effect(tx).unwrap();
            ledger.apply_transaction(&mut block, effect, false).unwrap();
            ledger.finish_block(block).unwrap();

            // Cant try to exceed asset cap by issuing confidentially
            let tx = create_issuance_txn(
                &mut ledger,
                &params,
                &code,
                1,
                2,
                AssetRecordType::ConfidentialAmount_NonConfidentialAssetType,
                &issuer,
            );
            let effect = TxnEffect::compute_effect(tx).unwrap();
            let mut block = ledger.start_block().unwrap();
            let res = ledger.apply_transaction(&mut block, effect, false);
            assert!(res.is_err());
        }
    }

    // Co_signers is a array of (signs, weight) pairs representing cosigners. If signs is true, that cosigner signs the
    // transaction.
    fn cosignature_transfer_succeeds(
        co_signers: &[(bool, u64)],
        threshold: u64,
        confidential: bool,
    ) -> bool {
        let mut ledger = LedgerState::test_ledger();
        let params = PublicParams::default();

        let code = AssetTypeCode::gen_random();
        let mut prng = ChaChaRng::from_entropy();
        let keys: Vec<XfrKeyPair> = (0..co_signers.len())
            .map(|_| XfrKeyPair::generate(&mut prng))
            .collect();
        let alice = XfrKeyPair::generate(&mut prng); // Asset owner
        let bob = XfrKeyPair::generate(&mut prng); // Asset recipient

        let sig_rules = SignatureRules {
            threshold,
            weights: co_signers
                .iter()
                .zip(keys.iter())
                .map(|((_, weight), kp)| (*kp.get_pk_ref(), *weight))
                .collect(),
        };

        let seq_id = ledger.get_block_commit_count();
        let tx = create_definition_transaction(
            &code,
            &alice,
            AssetRules::default()
                .set_transfer_multisig_rules(Some(sig_rules))
                .clone(),
            None,
            seq_id,
        )
        .unwrap();

        let effect = TxnEffect::compute_effect(tx).unwrap();
        {
            let mut block = ledger.start_block().unwrap();
            ledger.apply_transaction(&mut block, effect, false).unwrap();
            ledger.finish_block(block).unwrap();
        }

        // Issuance with two outputs
        let art = if let true = confidential {
            AssetRecordType::ConfidentialAmount_ConfidentialAssetType
        } else {
            AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType
        };
        let template = AssetRecordTemplate::with_no_asset_tracing(
            100,
            code.val,
            AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
            alice.get_pk(),
        );
        let (ba, _, _) = build_blind_asset_record(
            ledger.get_prng(),
            &params.pc_gens,
            &template,
            vec![],
        );

        let asset_issuance_body = IssueAssetBody::new(
            &code,
            0,
            &[(
                TxOutput {
                    id: None,
                    record: ba,
                    lien: None,
                },
                None,
            )],
        )
        .unwrap();
        let asset_issuance_operation =
            IssueAsset::new(asset_issuance_body, &IssuerKeyPair { keypair: &alice })
                .unwrap();

        let issue_op = Operation::IssueAsset(asset_issuance_operation);

        let seq_id = ledger.get_block_commit_count();
        let tx = Transaction::from_operation(issue_op, seq_id);

        // Commit issuance to block
        let effect = TxnEffect::compute_effect(tx).unwrap();

        let mut block = ledger.start_block().unwrap();
        let temp_sid = ledger.apply_transaction(&mut block, effect, false).unwrap();

        let (_txn_sid, txos) = ledger
            .finish_block(block)
            .unwrap()
            .remove(&temp_sid)
            .unwrap();
        let txo_sid = txos[0];

        // Construct transfer operation
        let mut block = ledger.start_block().unwrap();
        let input_bar = ledger.get_utxo_light(txo_sid).unwrap().utxo.0.record;
        let input_oar = open_blind_asset_record(&input_bar, &None, &alice).unwrap();

        let output_template =
            AssetRecordTemplate::with_no_asset_tracing(100, code.val, art, bob.get_pk());
        let output_ar = AssetRecord::from_template_no_identity_tracing(
            ledger.get_prng(),
            &output_template,
        )
        .unwrap();

        let mut transfer = TransferAsset::new(
            TransferAssetBody::new(
                ledger.get_prng(),
                vec![TxoRef::Absolute(txo_sid)],
                &[AssetRecord::from_open_asset_record_no_asset_tracing(
                    input_oar,
                )],
                &[output_ar],
                None,
                vec![],
                TransferType::Standard,
            )
            .unwrap(),
        )
        .unwrap();

        transfer.sign(&alice);
        for (i, (signs, _)) in co_signers.iter().enumerate() {
            if *signs {
                transfer.sign_cosignature(&keys[i], 0);
            }
        }
        let seq_id = ledger.get_block_commit_count();
        let tx = Transaction::from_operation(Operation::TransferAsset(transfer), seq_id);
        let effect = TxnEffect::compute_effect(tx).unwrap();
        ledger.apply_transaction(&mut block, effect, false).is_ok()
    }

    #[test]
    pub fn test_cosignature_restrictions() {
        // Simple
        assert!(!cosignature_transfer_succeeds(
            &[(false, 1), (false, 1)],
            1,
            false
        ));
        assert!(!cosignature_transfer_succeeds(
            &[(false, 1), (false, 1)],
            1,
            true
        ));
        assert!(cosignature_transfer_succeeds(
            &[(false, 1), (true, 1)],
            1,
            false
        ));
        assert!(cosignature_transfer_succeeds(&[(true, 1)], 1, false));
        assert!(cosignature_transfer_succeeds(&[], 0, false));

        // More complex
        assert!(!cosignature_transfer_succeeds(
            &[(false, 1), (true, 1), (false, 5), (true, 10), (false, 18)],
            16,
            false
        ));
        assert!(cosignature_transfer_succeeds(
            &[(false, 1), (true, 1), (true, 5), (true, 10), (false, 18)],
            16,
            false
        ));
        // Needlessly complex
        assert!(cosignature_transfer_succeeds(
            &[
                (false, 18888888),
                (true, 1),
                (true, 5),
                (false, 12320),
                (true, 13220),
                (true, 100000),
                (true, 12320),
                (true, 134440),
                (false, 18)
            ],
            232323,
            false
        ));
    }

    fn gen_fee_operation(
        l: &mut LedgerState,
        txo_sid: TxoSID,
        fra_owner_kp: &XfrKeyPair,
    ) -> Operation {
        let fra_code = &AssetTypeCode {
            val: ASSET_TYPE_FRA,
        };

        let input_bar_proof = l.get_utxo_light(txo_sid).unwrap();
        let input_bar = (input_bar_proof.utxo.0).record;
        let input_oar =
            open_blind_asset_record(&input_bar, &None, &fra_owner_kp).unwrap();

        let output_template = AssetRecordTemplate::with_no_asset_tracing(
            input_oar.amount - TX_FEE_MIN,
            fra_code.val,
            AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
            fra_owner_kp.get_pk(),
        );
        let output_ar = AssetRecord::from_template_no_identity_tracing(
            l.get_prng(),
            &output_template,
        )
        .unwrap();

        let output_template = AssetRecordTemplate::with_no_asset_tracing(
            TX_FEE_MIN,
            fra_code.val,
            AssetRecordType::NonConfidentialAmount_NonConfidentialAssetType,
            *BLACK_HOLE_PUBKEY,
        );
        let output_ar_fee = AssetRecord::from_template_no_identity_tracing(
            l.get_prng(),
            &output_template,
        )
        .unwrap();

        let input_ar = AssetRecord::from_open_asset_record_no_asset_tracing(input_oar);

        let mut transfer = TransferAsset::new(
            TransferAssetBody::new(
                l.get_prng(),
                vec![TxoRef::Absolute(txo_sid)],
                &[input_ar],
                &[output_ar, output_ar_fee],
                None,
                vec![],
                TransferType::Standard,
            )
            .unwrap(),
        )
        .unwrap();

        transfer.sign(&fra_owner_kp);

        Operation::TransferAsset(transfer)
    }

    #[test]
    fn test_check_fee_with_ledger() {
        let mut ledger = LedgerState::test_ledger();
        let fra_owner_kp = XfrKeyPair::generate(&mut ChaChaRng::from_entropy());

        let tx = fra_gen_initial_tx(&fra_owner_kp);
        assert!(tx.check_fee());

        let effect = TxnEffect::compute_effect(tx.clone()).unwrap();
        let mut block = ledger.start_block().unwrap();
        let tmp_sid = ledger.apply_transaction(&mut block, effect, false).unwrap();
        let txo_sid = ledger
            .finish_block(block)
            .unwrap()
            .remove(&tmp_sid)
            .unwrap()
            .1[0];

        let tx2 = Transaction::from_operation(
            gen_fee_operation(&mut ledger, txo_sid, &fra_owner_kp),
            1,
        );
        assert!(tx2.check_fee());

        let effect = TxnEffect::compute_effect(tx2).unwrap();
        let mut block = ledger.start_block().unwrap();
        ledger.apply_transaction(&mut block, effect, false).unwrap();
        ledger.finish_block(block).unwrap();

        // Ensure that FRA can only be defined only once.
        let effect = TxnEffect::compute_effect(tx).unwrap();
        let mut block = ledger.start_block().unwrap();
        assert!(ledger.apply_transaction(&mut block, effect, false).is_err());
        ledger.abort_block(block);
    }
}
