// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use std::path::Path;
use std::sync::{mpsc, Arc, LazyLock};
use std::{fs, io};

use sha3::{Digest, Sha3_256};

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
    STAKE_CONTRACT, TRANSFER_CONTRACT,
};
use rusk_profile::to_rusk_state_id_path;

use super::{coinbase_value, emission_amount, Rusk};
use crate::{Error, Result};

pub static DUSK_KEY: LazyLock<BlsPublicKey> = LazyLock::new(|| {
    let dusk_cpk_bytes = include_bytes!("../../assets/dusk.cpk");
    BlsPublicKey::from_slice(dusk_cpk_bytes)
        .expect("Dusk consensus public key to be valid")
});

impl Rusk {
    pub fn new<P: AsRef<Path>>(dir: P) -> Result<Self> {
        let dir = dir.as_ref();
        let commit_id_path = to_rusk_state_id_path(dir);

        let base_commit_bytes = fs::read(commit_id_path)?;
        if base_commit_bytes.len() != 32 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "Expected commit id to have 32 bytes, got {}",
                    base_commit_bytes.len()
                ),
            )
            .into());
        }
        let mut base_commit = [0u8; 32];
        base_commit.copy_from_slice(&base_commit_bytes);

        let vm = rusk_abi::new_vm(dir)?;

        Ok(Rusk(Arc::new(vm)))
    }

    pub fn execute_transactions<I: Iterator<Item = Transaction>>(
        &self,
        base_commit: [u8; 32],
        block_height: u64,
        block_gas_limit: u64,
        generator: &BlsPublicKey,
        txs: I,
        missed_generators: &[BlsPublicKey],
    ) -> Result<(Vec<SpentTransaction>, Vec<Transaction>, VerificationOutput)>
    {
        let mut session =
            rusk_abi::new_session(&self.0, base_commit, block_height)?;

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
                        session = rusk_abi::new_session(
                            &self.0,
                            base_commit,
                            block_height,
                        )?;

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
        base_commit: [u8; 32],
        block_height: u64,
        block_gas_limit: u64,
        generator: &BlsPublicKey,
        txs: &[Transaction],
        missed_generators: &[BlsPublicKey],
    ) -> Result<(Vec<SpentTransaction>, VerificationOutput)> {
        let mut session =
            rusk_abi::new_session(&self.0, base_commit, block_height)?;

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
        base_commit: [u8; 32],
        block_height: u64,
        block_gas_limit: u64,
        generator: BlsPublicKey,
        txs: Vec<Transaction>,
        consistency_check: Option<VerificationOutput>,
        missed_generators: &[BlsPublicKey],
    ) -> Result<(Vec<SpentTransaction>, VerificationOutput)> {
        let mut session =
            rusk_abi::new_session(&self.0, base_commit, block_height)?;

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

        session.commit()?;

        Ok((spent_txs, verification_output))
    }

    /// Finalize the given transactions.
    ///
    /// * `consistency_check` - represents a state_root, the caller expects to
    ///   be returned on successful transactions execution. Passing None value
    ///   disables the check.
    pub fn finalize_transactions(
        &self,
        base_commit: [u8; 32],
        block_height: u64,
        block_gas_limit: u64,
        generator: BlsPublicKey,
        txs: Vec<Transaction>,
        consistency_check: Option<VerificationOutput>,
        missed_generators: &[BlsPublicKey],
    ) -> Result<(Vec<SpentTransaction>, VerificationOutput)> {
        let mut session =
            rusk_abi::new_session(&self.0, base_commit, block_height)?;

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

        // TODO We should delete some commits at some point
        session.commit()?;

        Ok((spent_txs, verification_output))
    }

    /// Returns the nullifiers that already exist from a list of given
    /// `nullifiers`.
    pub fn existing_nullifiers(
        &self,
        base_commit: [u8; 32],
        nullifiers: &Vec<BlsScalar>,
    ) -> Result<Vec<BlsScalar>> {
        self.query(
            base_commit,
            TRANSFER_CONTRACT,
            "existing_nullifiers",
            nullifiers,
        )
    }

    /// Returns the stakes.
    pub fn provisioners(
        &self,
        base_commit: [u8; 32],
    ) -> Result<impl Iterator<Item = (BlsPublicKey, StakeData)>> {
        let (sender, receiver) = mpsc::channel();
        self.feeder_query(base_commit, STAKE_CONTRACT, "stakes", &(), sender)?;
        Ok(receiver.into_iter().map(|bytes| {
            rkyv::from_bytes::<(BlsPublicKey, StakeData)>(&bytes).expect(
                "The contract should only return (pk, stake_data) tuples",
            )
        }))
    }

    pub fn provisioner(
        &self,
        base_commit: [u8; 32],
        pk: &BlsPublicKey,
    ) -> Result<Option<StakeData>> {
        self.query(base_commit, STAKE_CONTRACT, "get_stake", pk)
    }
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
