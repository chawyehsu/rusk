// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use crate::database;
use crate::database::Ledger;
use anyhow::anyhow;
use dusk_consensus::quorum::verifiers;
use dusk_consensus::quorum::verifiers::QuorumResult;
use dusk_consensus::user::committee::CommitteeSet;
use dusk_consensus::user::provisioners::{ContextProvisioners, Provisioners};
use node_data::ledger::to_str;
use node_data::ledger::Signature;
use node_data::{ledger, StepName};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

/// An implementation of the all validation checks of a candidate block header
/// according to current context
pub(crate) struct Validator<'a, DB: database::DB> {
    pub(crate) db: Arc<RwLock<DB>>,
    prev_header: &'a ledger::Header,
    provisioners: &'a ContextProvisioners,
}

impl<'a, DB: database::DB> Validator<'a, DB> {
    pub fn new(
        db: Arc<RwLock<DB>>,
        prev_header: &'a ledger::Header,
        provisioners: &'a ContextProvisioners,
    ) -> Self {
        Self {
            db,
            prev_header,
            provisioners,
        }
    }

    /// Executes check points to make sure a candidate header is fully valid
    ///
    /// * `disable_winner_cert_check` - disables the check of the winning
    /// certificate
    pub async fn execute_checks(
        &self,
        candidate_block: &'a ledger::Header,
        disable_winner_cert_check: bool,
    ) -> anyhow::Result<bool> {
        self.verify_basic_fields(candidate_block).await?;
        self.verify_prev_block_cert(candidate_block).await?;

        if !disable_winner_cert_check {
            self.verify_winning_cert(candidate_block).await?;
        }

        self.verify_failed_iterations(candidate_block).await
    }

    /// Verifies any non-certificate field
    pub async fn verify_basic_fields(
        &self,
        candidate_block: &'a ledger::Header,
    ) -> anyhow::Result<()> {
        if candidate_block.version > 0 {
            return Err(anyhow!("unsupported block version"));
        }

        if candidate_block.hash == [0u8; 32] {
            return Err(anyhow!("empty block hash"));
        }

        if candidate_block.height != self.prev_header.height + 1 {
            return Err(anyhow!(
                "invalid block height block_height: {:?}, curr_height: {:?}",
                candidate_block.height,
                self.prev_header.height,
            ));
        }

        if candidate_block.prev_block_hash != self.prev_header.hash {
            return Err(anyhow!("invalid previous block hash"));
        }

        // Ensure block is not already in the ledger
        self.db.read().await.view(|v| {
            if Ledger::get_block_exists(&v, &candidate_block.hash)? {
                return Err(anyhow!("block already exists"));
            }

            Ok(())
        })?;

        Ok(())
    }

    pub async fn verify_prev_block_cert(
        &self,
        candidate_block: &'a ledger::Header,
    ) -> anyhow::Result<()> {
        if self.prev_header.height == 0 {
            return Ok(());
        }

        let prev_block_seed = self.db.read().await.view(|v| {
            let prev_block =
                Ledger::fetch_block_by_height(&v, self.prev_header.height - 1)?
                    .ok_or_else(|| anyhow::anyhow!("could not fetch block"))?;

            Ok::<_, anyhow::Error>(prev_block.header().seed)
        })?;

        verify_block_cert(
            prev_block_seed,
            self.provisioners.prev(),
            self.prev_header.hash,
            self.prev_header.height,
            &candidate_block.prev_block_cert,
            self.prev_header.iteration,
            true,
        )
        .await?;

        Ok(())
    }

    pub async fn verify_failed_iterations(
        &self,
        candidate_block: &'a ledger::Header,
    ) -> anyhow::Result<bool> {
        // Verify Failed iterations
        let mut attested = true;

        for (iter, cert) in candidate_block
            .failed_iterations
            .cert_list
            .iter()
            .enumerate()
        {
            if let Some((cert, pk)) = cert {
                info!(event = "verify_cert", cert_type = "failed_cert", iter);
                let expected_pk = self.provisioners.current().get_generator(
                    iter as u8,
                    self.prev_header.seed,
                    candidate_block.height,
                );
                if pk != &expected_pk {
                    anyhow::bail!("Invalid generator. Expected {expected_pk:?}, actual {pk:?}");
                }

                let quorums = verify_block_cert(
                    self.prev_header.seed,
                    self.provisioners.current(),
                    [0u8; 32],
                    candidate_block.height,
                    cert,
                    iter as u8,
                    true,
                )
                .await?;

                attested = attested
                    && quorums.0.quorum_reached()
                    && quorums.1.quorum_reached();
            } else {
                attested = false;
            }
        }

        Ok(attested)
    }

    pub async fn verify_winning_cert(
        &self,
        candidate_block: &'a ledger::Header,
    ) -> anyhow::Result<()> {
        verify_block_cert(
            self.prev_header.seed,
            self.provisioners.current(),
            candidate_block.hash,
            candidate_block.height,
            &candidate_block.cert,
            candidate_block.iteration,
            true,
        )
        .await?;

        Ok(())
    }
}

pub async fn verify_block_cert(
    curr_seed: Signature,
    curr_eligible_provisioners: &Provisioners,
    block_hash: [u8; 32],
    height: u64,
    cert: &ledger::Certificate,
    iteration: u8,
    enable_quorum_check: bool,
) -> anyhow::Result<(QuorumResult, QuorumResult)> {
    let committee = RwLock::new(CommitteeSet::new(curr_eligible_provisioners));

    let hdr = node_data::message::Header {
        topic: node_data::message::Topics::Unknown,
        pubkey_bls: node_data::bls::PublicKey::default(),
        round: height,
        iteration,
        block_hash,
    };

    let mut result = (QuorumResult::default(), QuorumResult::default());

    // Verify validation
    match verifiers::verify_step_votes(
        &cert.validation,
        &committee,
        curr_seed,
        &hdr,
        StepName::Validation,
        enable_quorum_check,
    )
    .await
    {
        Ok(validation_quorum_result) => {
            result.0 = validation_quorum_result;
        }
        Err(e) => {
            return Err(anyhow!(
        "invalid validation, hash = {}, round = {}, iter = {}, seed = {},  sv = {:?}, err = {}",
        to_str(&hdr.block_hash),
        hdr.round,
        iteration,
        to_str(&curr_seed.inner()),
        cert.validation,
        e
    ));
        }
    };

    // Verify ratification
    match verifiers::verify_step_votes(
        &cert.ratification,
        &committee,
        curr_seed,
        &hdr,
        StepName::Ratification,
        enable_quorum_check,
    )
    .await
    {
        Ok(ratification_quorum_result) => {
            result.1 = ratification_quorum_result;
        }
        Err(e) => {
            return Err(anyhow!(
        "invalid ratification, hash = {}, round = {}, iter = {}, seed = {},  sv = {:?}, err = {}",
        to_str(&hdr.block_hash),
        hdr.round,
        iteration,
        to_str(&curr_seed.inner()),
        cert.ratification,
        e,
    ));
        }
    }

    Ok(result)
}