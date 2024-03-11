// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use std::path::Path;
use std::sync::{mpsc, Arc, LazyLock};
use std::{fs, io};

use parking_lot::RwLock;
use sha3::{Digest, Sha3_256};
use tokio::task;
use tracing::debug;

use dusk_bls12_381::BlsScalar;
use dusk_bls12_381_sign::PublicKey as BlsPublicKey;
use dusk_bytes::DeserializableSlice;
use dusk_consensus::operations::VerificationOutput;
use node_data::ledger::{SpentTransaction, Transaction};
use phoenix_core::transaction::StakeData;
use phoenix_core::Transaction as PhoenixTransaction;
use rusk_abi::dusk::Dusk;
use rusk_abi::{
    CallReceipt, ContractError, Error as PiecrustError, Event, Session,
    STAKE_CONTRACT, TRANSFER_CONTRACT, VM,
};
use rusk_profile::{to_rusk_epoch_id_path, to_rusk_state_id_path};
use stake_contract_types::EPOCH;

use super::{coinbase_value, emission_amount, Rusk, RuskTip};
use crate::{Error, Result};

pub static DUSK_KEY: LazyLock<BlsPublicKey> = LazyLock::new(|| {
    let dusk_cpk_bytes = include_bytes!("../../assets/dusk.cpk");
    BlsPublicKey::from_slice(dusk_cpk_bytes)
        .expect("Dusk consensus public key to be valid")
});

impl Rusk {
    pub fn new<P: AsRef<Path>>(dir: P) -> Result<Self> {
        let dir = dir.as_ref();

        let base_commit_path = to_rusk_state_id_path(dir);
        let base_commit = read_commit_id(base_commit_path)?;

        let epoch_commit_path = to_rusk_epoch_id_path(dir);
        let epoch = if epoch_commit_path.exists() {
            Some(read_commit_id(epoch_commit_path)?)
        } else {
            None
        };

        let vm = Arc::new(rusk_abi::new_vm(dir)?);

        let tip = Arc::new(RwLock::new(RuskTip {
            current: base_commit,
            base: base_commit,
            epoch,
        }));

        Ok(Self {
            tip,
            vm,
            dir: dir.into(),
        })
    }

    pub fn execute_transactions<I: Iterator<Item = Transaction>>(
        &self,
        block_height: u64,
        block_gas_limit: u64,
        generator: &BlsPublicKey,
        txs: I,
        missed_generators: &[BlsPublicKey],
    ) -> Result<(Vec<SpentTransaction>, Vec<Transaction>, VerificationOutput)>
    {
        let mut session = self.session(block_height, None)?;

        let mut block_gas_left = block_gas_limit;

        let mut spent_txs = Vec::<SpentTransaction>::new();
        let mut discarded_txs = vec![];

        let mut dusk_spent = 0;

        let mut event_hasher = Sha3_256::new();

        for unspent_tx in txs {
            let tx = unspent_tx.inner.clone();
            match execute(&mut session, &tx) {
                Ok(receipt) => {
                    let gas_spent = receipt.gas_spent;

                    // If the transaction went over the block gas limit we
                    // re-execute all spent transactions. We don't discard the
                    // transaction, since it is technically valid.
                    if gas_spent > block_gas_left {
                        session = self.session(block_height, None)?;

                        for spent_tx in &spent_txs {
                            // We know these transactions were correctly
                            // executed before, so we don't bother checking.
                            let _ =
                                execute(&mut session, &spent_tx.inner.inner);
                        }

                        continue;
                    }

                    for event in receipt.events {
                        update_hasher(&mut event_hasher, event);
                    }

                    block_gas_left -= gas_spent;
                    dusk_spent += gas_spent * tx.fee.gas_price;

                    spent_txs.push(SpentTransaction {
                        inner: unspent_tx.clone(),
                        gas_spent,
                        block_height,
                        // We're currently ignoring the result of successful
                        // calls
                        err: receipt.data.err().map(|e| format!("{e}")),
                    });
                }
                Err(_) => {
                    // An unspendable transaction should be discarded
                    discarded_txs.push(unspent_tx);
                    continue;
                }
            }
        }

        reward_slash_and_update_root(
            &mut session,
            block_height,
            dusk_spent,
            generator,
            missed_generators,
        )?;

        let state_root = session.root();
        let event_hash = event_hasher.finalize().into();

        Ok((
            spent_txs,
            discarded_txs,
            VerificationOutput {
                state_root,
                event_hash,
            },
        ))
    }

    /// Verify the given transactions are ok.
    pub fn verify_transactions(
        &self,
        block_height: u64,
        block_gas_limit: u64,
        generator: &BlsPublicKey,
        txs: &[Transaction],
        missed_generators: &[BlsPublicKey],
    ) -> Result<(Vec<SpentTransaction>, VerificationOutput)> {
        let mut session = self.session(block_height, None)?;

        accept(
            &mut session,
            block_height,
            block_gas_limit,
            generator,
            txs,
            missed_generators,
        )
    }

    /// Accept the given transactions.
    ///
    ///   * `consistency_check` - represents a state_root, the caller expects to
    ///   be returned on successful transactions execution. Passing a None
    ///   value disables the check.
    pub fn accept_transactions(
        &self,
        block_height: u64,
        block_gas_limit: u64,
        generator: BlsPublicKey,
        txs: Vec<Transaction>,
        consistency_check: Option<VerificationOutput>,
        missed_generators: &[BlsPublicKey],
    ) -> Result<(Vec<SpentTransaction>, VerificationOutput)> {
        let mut session = self.session(block_height, None)?;

        let (spent_txs, verification_output) = accept(
            &mut session,
            block_height,
            block_gas_limit,
            &generator,
            &txs[..],
            missed_generators,
        )?;

        if let Some(expected_verification) = consistency_check {
            if expected_verification != verification_output {
                // Drop the session if the resulting is inconsistent
                // with the callers one.
                return Err(Error::InconsistentState(verification_output));
            }
        }

        let commit = session.commit()?;
        self.set_current_commit(commit, block_height)?;

        Ok((spent_txs, verification_output))
    }

    /// Finalize the given transactions.
    ///
    /// * `consistency_check` - represents a state_root, the caller expects to
    ///   be returned on successful transactions execution. Passing None value
    ///   disables the check.
    pub fn finalize_transactions(
        &self,
        block_height: u64,
        block_gas_limit: u64,
        generator: BlsPublicKey,
        txs: Vec<Transaction>,
        consistency_check: Option<VerificationOutput>,
        missed_generators: &[BlsPublicKey],
    ) -> Result<(Vec<SpentTransaction>, VerificationOutput)> {
        let mut session = self.session(block_height, None)?;

        let (spent_txs, verification_output) = accept(
            &mut session,
            block_height,
            block_gas_limit,
            &generator,
            &txs[..],
            missed_generators,
        )?;

        if let Some(expected_verification) = consistency_check {
            if expected_verification != verification_output {
                // Drop the session if the result state root is inconsistent
                // with the callers one.
                return Err(Error::InconsistentState(verification_output));
            }
        }

        let commit = session.commit()?;
        self.set_base_and_delete(commit, block_height)?;

        let commit_id_path = to_rusk_state_id_path(&self.dir);
        fs::write(commit_id_path, commit)?;

        Ok((spent_txs, verification_output))
    }

    pub fn revert(&self, state_hash: [u8; 32]) -> Result<[u8; 32]> {
        let mut tip = self.tip.write();

        let commits = self.vm.commits();
        if !commits.contains(&state_hash) {
            return Err(Error::CommitNotFound(state_hash));
        }

        tip.current = state_hash;
        Ok(tip.current)
    }

    pub fn revert_to_base_root(&self) -> Result<[u8; 32]> {
        self.revert(self.base_root())
    }

    pub fn revert_to_epoch_root(&self) -> Result<[u8; 32]> {
        let mut tip = self.tip.write();

        match tip.epoch {
            Some(epoch_commit) => {
                let commits = self.vm.commits();
                if !commits.contains(&epoch_commit) {
                    return Err(Error::CommitNotFound(epoch_commit));
                }

                // When reverting to the epoch root we need to make sure that
                // the epoch root is set as a base on disk as
                // well, otherwise we risk restarting
                // from a different base than what we might expect on restart.
                let base_commit_path = to_rusk_state_id_path(&self.dir);
                write_commit_id(epoch_commit, base_commit_path)?;

                tip.current = epoch_commit;
                tip.base = epoch_commit;

                Ok(epoch_commit)
            }
            None => Err(Error::LastEpochNotFound),
        }
    }

    /// Get the base root.
    pub fn base_root(&self) -> [u8; 32] {
        self.tip.read().base
    }

    /// Get the current state root.
    pub fn state_root(&self) -> [u8; 32] {
        self.tip.read().current
    }

    /// Get the latest known epoch root.
    pub fn epoch_root(&self) -> Option<[u8; 32]> {
        self.tip.read().epoch
    }

    /// Returns the nullifiers that already exist from a list of given
    /// `nullifiers`.
    pub fn existing_nullifiers(
        &self,
        nullifiers: &Vec<BlsScalar>,
    ) -> Result<Vec<BlsScalar>> {
        self.query(TRANSFER_CONTRACT, "existing_nullifiers", nullifiers)
    }

    /// Returns the stakes.
    pub fn provisioners(
        &self,
        base_commit: Option<[u8; 32]>,
    ) -> Result<impl Iterator<Item = (BlsPublicKey, StakeData)>> {
        let (sender, receiver) = mpsc::channel();
        self.feeder_query(STAKE_CONTRACT, "stakes", &(), sender, base_commit)?;
        Ok(receiver.into_iter().map(|bytes| {
            rkyv::from_bytes::<(BlsPublicKey, StakeData)>(&bytes).expect(
                "The contract should only return (pk, stake_data) tuples",
            )
        }))
    }

    pub fn provisioner(&self, pk: &BlsPublicKey) -> Result<Option<StakeData>> {
        self.query(STAKE_CONTRACT, "get_stake", pk)
    }

    pub(crate) fn session(
        &self,
        block_height: u64,
        commit: Option<[u8; 32]>,
    ) -> Result<Session> {
        let commit = commit.unwrap_or_else(|| {
            let tip = self.tip.read();
            tip.current
        });

        let session = rusk_abi::new_session(&self.vm, commit, block_height)?;

        Ok(session)
    }

    pub(crate) fn set_current_commit(
        &self,
        commit: [u8; 32],
        block_height: u64,
    ) -> Result<()> {
        let mut tip = self.tip.write();

        // If the current block height is a multiple of the epoch, we need to
        // write the commit to the epoch commit file, and set the commit in the
        // tip.
        if block_height % EPOCH == 0 {
            let epoch_commit_path = to_rusk_epoch_id_path(&self.dir);
            write_commit_id(commit, epoch_commit_path)?;
            tip.epoch = Some(commit);
        }

        tip.current = commit;

        Ok(())
    }

    pub(crate) fn set_base_and_delete(
        &self,
        commit: [u8; 32],
        block_height: u64,
    ) -> Result<()> {
        let mut tip = self.tip.write();

        // If the current block height is a multiple of the epoch, we need to
        // write the commit to the epoch commit file, and set the commit in the
        // tip.
        if block_height % EPOCH == 0 {
            let epoch_commit_path = to_rusk_epoch_id_path(&self.dir);
            write_commit_id(commit, epoch_commit_path)?;
            tip.epoch = Some(commit);
        }

        let current_commit = tip.current;
        let base_commit = tip.base;

        tip.current = commit;
        tip.base = commit;

        // We will delete all commits except the previous base commit, the
        // previous current commit, the new commit and the epoch commit - if it
        // exists.
        let mut commits_to_delete = self.vm.commits();
        commits_to_delete.retain(|c| {
            *c != current_commit && *c != base_commit && *c != commit
        });

        // Delete all commits except the previous base commit, and the current
        // commit. Deleting commits is blocking, meaning it will wait until any
        // process using the commit is done. This includes any queries that are
        // currently executing.
        // Since we do want commits to be deleted, but don't want block
        // finalization to wait, we spawn a new task to delete the commits.
        task::spawn(delete_commits(self.vm.clone(), commits_to_delete));

        Ok(())
    }
}

async fn delete_commits(vm: Arc<VM>, commits: Vec<[u8; 32]>) {
    for commit in commits {
        if let Err(err) = vm.delete_commit(commit) {
            debug!("failed deleting commit {}: {err}", hex::encode(commit));
        }
    }
}

fn read_commit_id<P: AsRef<Path>>(path: P) -> Result<[u8; 32]> {
    let commit_bytes = fs::read(path)?;

    if commit_bytes.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "Expected commit to have 32 bytes, got {}",
                commit_bytes.len()
            ),
        )
        .into());
    }

    let mut commit = [0u8; 32];
    commit.copy_from_slice(&commit_bytes);

    Ok(commit)
}

fn write_commit_id<P: AsRef<Path>>(commit: [u8; 32], path: P) -> Result<()> {
    Ok(fs::write(path, commit)?)
}

fn accept(
    session: &mut Session,
    block_height: u64,
    block_gas_limit: u64,
    generator: &BlsPublicKey,
    txs: &[Transaction],
    missed_generators: &[BlsPublicKey],
) -> Result<(Vec<SpentTransaction>, VerificationOutput)> {
    let mut block_gas_left = block_gas_limit;

    let mut spent_txs = Vec::with_capacity(txs.len());
    let mut dusk_spent = 0;

    let mut event_hasher = Sha3_256::new();

    for unspent_tx in txs {
        let tx = &unspent_tx.inner;
        let receipt = execute(session, tx)?;

        for event in receipt.events {
            update_hasher(&mut event_hasher, event);
        }
        let gas_spent = receipt.gas_spent;

        dusk_spent += gas_spent * tx.fee.gas_price;
        block_gas_left = block_gas_left
            .checked_sub(gas_spent)
            .ok_or(Error::OutOfGas)?;

        spent_txs.push(SpentTransaction {
            inner: unspent_tx.clone(),
            gas_spent,
            block_height,
            // We're currently ignoring the result of successful calls
            err: receipt.data.err().map(|e| format!("{e}")),
        });
    }

    reward_slash_and_update_root(
        session,
        block_height,
        dusk_spent,
        generator,
        missed_generators,
    )?;

    let state_root = session.root();
    let event_hash = event_hasher.finalize().into();

    Ok((
        spent_txs,
        VerificationOutput {
            state_root,
            event_hash,
        },
    ))
}

/// Executes a transaction, returning the receipt of the call and the gas spent.
/// The following steps are performed:
///
/// 1. Call the "spend_and_execute" function on the transfer contract with
///    unlimited gas. If this fails, an error is returned. If an error is
///    returned the transaction should be considered unspendable/invalid, but no
///    re-execution of previous transactions is required.
///
/// 2. Call the "refund" function on the transfer contract with unlimited gas.
///    The amount charged depends on the gas spent by the transaction, and the
///    optional contract call in step 1.
fn execute(
    session: &mut Session,
    tx: &PhoenixTransaction,
) -> Result<CallReceipt<Result<Vec<u8>, ContractError>>, PiecrustError> {
    // Spend the inputs and execute the call. If this errors the transaction is
    // unspendable.
    let mut receipt = session.call::<_, Result<Vec<u8>, ContractError>>(
        TRANSFER_CONTRACT,
        "spend_and_execute",
        tx,
        tx.fee.gas_limit,
    )?;

    // Ensure all gas is consumed if there's an error in the contract call
    if receipt.data.is_err() {
        receipt.gas_spent = receipt.gas_limit;
    }

    // Refund the appropriate amount to the transaction. This call is guaranteed
    // to never error. If it does, then a programming error has occurred. As
    // such, the call to `Result::expect` is warranted.
    let refund_receipt = session
        .call::<_, ()>(
            TRANSFER_CONTRACT,
            "refund",
            &(tx.fee, receipt.gas_spent),
            u64::MAX,
        )
        .expect("Refunding must succeed");

    receipt.events.extend(refund_receipt.events);

    Ok(receipt)
}

fn update_hasher(hasher: &mut Sha3_256, event: Event) {
    hasher.update(event.source.as_bytes());
    hasher.update(event.topic.as_bytes());
    hasher.update(event.data);
}

fn reward_slash_and_update_root(
    session: &mut Session,
    block_height: u64,
    dusk_spent: Dusk,
    generator: &BlsPublicKey,
    slashing: &[BlsPublicKey],
) -> Result<()> {
    let (dusk_value, generator_value) =
        coinbase_value(block_height, dusk_spent);

    session.call::<_, ()>(
        STAKE_CONTRACT,
        "reward",
        &(*DUSK_KEY, dusk_value),
        u64::MAX,
    )?;
    session.call::<_, ()>(
        STAKE_CONTRACT,
        "reward",
        &(*generator, generator_value),
        u64::MAX,
    )?;
    let slash_amount = emission_amount(block_height);

    for to_slash in slashing {
        session.call::<_, ()>(
            STAKE_CONTRACT,
            "slash",
            &(*to_slash, slash_amount),
            u64::MAX,
        )?;
    }

    session.call::<_, ()>(TRANSFER_CONTRACT, "update_root", &(), u64::MAX)?;

    Ok(())
}
