use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::select;
use tokio::sync::oneshot;
use tokio::task::{self, JoinHandle};
use tonic_primitives::Signer;
use tracing::{debug, error, info};

use crate::backend::{BlockService, Broadcast, ValidatorManager};
use crate::types::{
    digest_block, CommitMessage, CommitMessageSigned, CommitSeals, FinalizedBlock,
    IBFTBroadcastMessage, IBFTError, PrepareMessage, PrepareMessageSigned, PreparedCertificate,
    PreparedProposed, ProposalMessage, ProposalMessageSigned, RawBlock, RoundChangeCertificate,
    RoundChangeMessage, RoundChangeMessageSigned,
};
use crate::{MAX_ROUND, ROUND_ARRAY_SIZE};

use super::messages::ConsensusMessages;
use super::types::View;

#[derive(Clone)]
pub struct IBFT<V, B, BS>
where
    V: ValidatorManager,
    B: Broadcast,
    BS: BlockService,
{
    messages: ConsensusMessages,
    validator_manager: V,
    broadcast: B,
    signer: Signer,
    block_service: BS,
    base_round_time: Duration,
}

impl<V, B, BS> IBFT<V, B, BS>
where
    V: ValidatorManager,
    B: Broadcast,
    BS: BlockService,
{
    pub fn new(
        messages: ConsensusMessages,
        validator_manager: V,
        broadcast: B,
        block_service: BS,
        signer: Signer,
        base_round_time: Duration,
    ) -> Self {
        Self {
            messages,
            validator_manager,
            broadcast,
            block_service,
            signer,
            base_round_time,
        }
    }

    pub async fn run(
        &self,
        height: u64,
        mut cancel: oneshot::Receiver<()>,
    ) -> Option<FinalizedBlock> {
        info!("Running consensus height #{}", height);

        let quorum = self.validator_manager.quorum(height);
        assert_ne!(quorum, 0, "Quorum must be greater than 0");

        let mut round = 0;
        // Tracking self sent latest round change message separately to be able to track
        // latest prepared proposed in an optimized manner.
        let mut latest_self_round_change = None;
        loop {
            if round > MAX_ROUND {
                panic!("Not producing blocks for a long time, chain is dead");
            }

            let view = View::new(height, round);
            let state = SharedRunState::new(view);

            info!("Round: #{}", view.round);

            let timeout = tokio::time::sleep(self.get_round_timeout(view.round));
            let (future_proposal_rx, future_proposal_task) = self.monitor_future_proposal(view);
            let (future_rcc_rx, future_rcc_task) = self.monitor_future_rcc(view);
            let (round_finished, round_task) = self
                .start_ibft_round(state.clone(), latest_self_round_change.as_ref())
                .await;

            let abort = move || {
                round_task.abort();
                future_proposal_task.abort();
                future_rcc_task.abort();
            };

            select! {
                biased;
                _ = &mut cancel => {
                    info!("Received cancel signal, stopping consensus...");
                    abort();
                    return None;
                }
                Ok(commit_seals) = round_finished => {
                    info!("Finished IBFT round");
                    abort();

                    let finalized_block = self.handle_round_finish(view, commit_seals).await;
                    return Some(finalized_block);
                }
                Ok(new_round) = future_proposal_rx => {
                    info!("Received future proposal");
                    abort();
                    // We just move to the proposal round. When new round starts, the already verified
                    // proposal will be queried and immediately will jump to the prepare state.
                    round = new_round;
                }
                Ok(new_round) = future_rcc_rx => {
                    info!("Got enough round changes to create certificate");
                    abort();
                    // We just move to the rcc round, if we are the proposer in the new round,
                    // we will create the certificate, and we don't need rcc if we are not the proposer
                    round = new_round;
                }
                _ = timeout => {
                    info!("Round timeout");
                    abort();

                    self.handle_timeout(state, &mut latest_self_round_change).await;
                    round += 1;
                }
            }
        }
    }

    async fn start_ibft_round(
        &self,
        state: SharedRunState,
        latest_self_round_change: Option<&RoundChangeMessageSigned>,
    ) -> (oneshot::Receiver<CommitSeals>, JoinHandle<()>) {
        let view = state.view;
        let ibft = self.clone();
        let (tx, rx) = oneshot::channel();

        let task = if view.round == 0 {
            task::spawn(async move {
                match ibft.run_ibft_round_0(state).await {
                    Ok(commit_seals) => {
                        let _ = tx.send(commit_seals);
                    }
                    Err(err) => {
                        // TODO: think about what to do here
                        error!("Error occurred during IBFT run: {err}");
                    }
                }
            })
        } else {
            if let Some(round_change) = latest_self_round_change {
                // Add self round change to messages only if view matches and we are proposer
                if round_change.view() == view
                    && self
                        .validator_manager
                        .is_proposer(self.signer.address(), view)
                {
                    self.messages
                        .add_round_change_message(round_change.clone(), self.signer.address())
                        .await;
                }
            }

            task::spawn(async move {
                match ibft.run_ibft_round_1(state).await {
                    Ok(commit_seals) => {
                        let _ = tx.send(commit_seals);
                    }
                    Err(err) => {
                        // TODO: think about what to do here
                        error!("Error occurred during IBFT run: {err}");
                    }
                }
            })
        };

        (rx, task)
    }

    async fn run_ibft_round_0(&self, state: SharedRunState) -> Result<CommitSeals, IBFTError> {
        let view = state.view;
        let quorum = self.validator_manager.quorum(view.height);

        assert_eq!(view.round, 0, "Round must be 0");
        assert_eq!(
            state.get(),
            RunState::Proposal,
            "Initial run state must be Proposal"
        );

        let proposed_block_digest = self.run_proposal_0(view).await?;

        state.update(RunState::Prepare);
        self.run_prepare(view, proposed_block_digest, quorum).await;

        state.update(RunState::Commit);
        let commit_seals = self.run_commit(view, proposed_block_digest, quorum).await;

        Ok(commit_seals)
    }

    async fn run_ibft_round_1(&self, state: SharedRunState) -> Result<CommitSeals, IBFTError> {
        let view = state.view;
        let quorum = self.validator_manager.quorum(view.height);

        assert_ne!(view.round, 0, "Round must be greater than 0");
        assert_eq!(
            state.get(),
            RunState::Proposal,
            "Initial run state must be Proposal"
        );

        let proposed_block_digest = self.run_proposal_1(view, quorum).await?;

        state.update(RunState::Prepare);
        self.run_prepare(view, proposed_block_digest, quorum).await;

        state.update(RunState::Commit);
        let commit_seals = self.run_commit(view, proposed_block_digest, quorum).await;

        Ok(commit_seals)
    }

    async fn run_proposal_0(&self, view: View) -> Result<[u8; 32], IBFTError> {
        let should_propose = self
            .validator_manager
            .is_proposer(self.signer.address(), view);

        if should_propose {
            info!("We are the block proposer");

            let raw_block = self
                .block_service
                .build_block(view.height)
                .map_err(|e| IBFTError::BlockBuild(e.to_string()))?;
            debug!("Built the proposal block");

            let proposal = ProposalMessage::new(view, raw_block, None).into_signed(&self.signer);
            let proposed_block_digest = proposal.proposed_block_digest();

            self.broadcast
                .broadcast_message(IBFTBroadcastMessage::Proposal(&proposal))
                .await;

            self.messages
                .add_proposal_message(proposal, self.signer.address())
                .await;

            return Ok(proposed_block_digest);
        }

        let proposal_verify_fn = |proposal: &ProposalMessageSigned| {
            if !proposal.verify_block_digest() {
                return Err(IBFTError::IncorrectProposalDigest);
            }

            if let Err(err) = self
                .block_service
                .verify_block(proposal.proposed_block().raw_block())
            {
                return Err(IBFTError::InvalidProposalBlock(err.to_string()));
            }

            Ok(())
        };

        // First subscribe so we don't miss the notification in the brief time we query the proposal.
        let mut proposal_rx = self.messages.subscribe_proposal();

        let digest = self
            .messages
            .get_valid_proposal_digest(view, proposal_verify_fn)
            .await;

        let digest = match digest {
            Some(digest) => digest?,
            None => {
                // Wait until we receive a proposal from peers for the given view
                loop {
                    let proposal_view = proposal_rx
                        .recv()
                        .await
                        .expect("Proposal subscriber channel should not close");
                    if proposal_view == view {
                        break;
                    }
                }

                self.messages
                    .get_valid_proposal_digest(view, proposal_verify_fn)
                    .await
                    .expect(
                        "At this state, nothing else should be pruning or taking the proposal",
                    )?
            }
        };

        debug!("Received valid proposal message");

        let prepare = PrepareMessage::new(view, digest).into_signed(&self.signer);

        self.broadcast
            .broadcast_message(IBFTBroadcastMessage::Prepare(&prepare))
            .await;

        self.messages
            .add_prepare_message(prepare, self.signer.address())
            .await;

        Ok(digest)
    }

    async fn run_proposal_1(&self, view: View, quorum: usize) -> Result<[u8; 32], IBFTError> {
        let should_propose = self
            .validator_manager
            .is_proposer(self.signer.address(), view);

        if should_propose {
            info!("We are the block proposer");

            let (rcc, raw_block) = self.wait_rcc(view, quorum).await;
            let proposal = match raw_block {
                Some(raw_block) => {
                    debug!("Found previously proposed block in round change certificate");
                    ProposalMessage::new(view, raw_block, Some(rcc))
                }
                None => {
                    debug!("No proposed block in round change certificate");

                    let raw_block = self
                        .block_service
                        .build_block(view.height)
                        .map_err(|e| IBFTError::BlockBuild(e.to_string()))?;
                    debug!("Built the proposal block");

                    ProposalMessage::new(view, raw_block, Some(rcc))
                }
            };

            let proposal = proposal.into_signed(&self.signer);
            let proposed_block_digest = proposal.proposed_block_digest();

            self.broadcast
                .broadcast_message(IBFTBroadcastMessage::Proposal(&proposal))
                .await;

            self.messages
                .add_proposal_message(proposal, self.signer.address())
                .await;

            return Ok(proposed_block_digest);
        }

        let proposal_verify_fn = self.proposal_1_verify_fn(quorum);

        // First subscribe so we don't miss the notification in the brief time we query the proposal.
        let mut proposal_rx = self.messages.subscribe_proposal();

        let digest = self
            .messages
            .get_valid_proposal_digest(view, &proposal_verify_fn)
            .await;

        let digest = match digest {
            Some(digest) => digest?,
            None => {
                // Wait until we receive a proposal from peers for the given view
                loop {
                    let proposal_view = proposal_rx
                        .recv()
                        .await
                        .expect("Proposal subscriber channel should not close");
                    if proposal_view == view {
                        break;
                    }
                }

                self.messages
                    .get_valid_proposal_digest(view, proposal_verify_fn)
                    .await
                    .expect(
                        "At this state, nothing else should be pruning or taking the proposal",
                    )?
            }
        };

        debug!("Received valid proposal message");

        let prepare = PrepareMessage::new(view, digest).into_signed(&self.signer);

        self.broadcast
            .broadcast_message(IBFTBroadcastMessage::Prepare(&prepare))
            .await;

        self.messages
            .add_prepare_message(prepare, self.signer.address())
            .await;

        Ok(digest)
    }

    async fn run_prepare(&self, view: View, proposed_block_digest: [u8; 32], quorum: usize) {
        debug!("Started prepare state");
        // We only need to verify the proposed block digest, signature check is enforced by `MessageHandler`,
        // and querying by view also ensures height and round matches.
        let prepare_verify_fn = |prepare: &PrepareMessageSigned| -> bool {
            prepare.proposed_block_digest() == proposed_block_digest
        };
        let mut prepare_rx = self.messages.subscribe_prepare();
        let mut prepare_count = self
            .messages
            .get_valid_prepare_count(view, prepare_verify_fn)
            .await;
        // Wait for new prepare messages until we hit quorum - 1 (quorum - 1 because proposer does not broadcast prepare)
        while prepare_count < quorum - 1 {
            let new_prepare_view = prepare_rx
                .recv()
                .await
                .expect("Prepare subscriber channel should not close");
            if new_prepare_view == view {
                prepare_count += 1;
                if prepare_count == quorum - 1 {
                    prepare_count = self
                        .messages
                        .get_valid_prepare_count(view, prepare_verify_fn)
                        .await;
                }
            }
        }
        debug!("Received quorum prepare messages");

        let commit =
            CommitMessage::new(view, proposed_block_digest, &self.signer).into_signed(&self.signer);

        self.broadcast
            .broadcast_message(IBFTBroadcastMessage::Commit(&commit))
            .await;

        self.messages
            .add_commit_message(commit, self.signer.address())
            .await;
    }

    async fn run_commit(
        &self,
        view: View,
        proposed_block_digest: [u8; 32],
        quorum: usize,
    ) -> CommitSeals {
        debug!("Started commit state");

        // We only need to verify the proposed block digest, signature check is enforced by `MessageHandler`,
        // and querying by view also ensures height and round matches.
        let commit_verify_fn = |commit: &CommitMessageSigned| -> bool {
            commit.proposed_block_digest() == proposed_block_digest
        };
        let mut commit_rx = self.messages.subscribe_commit();
        let (mut commit_seals, mut commit_count) = self
            .messages
            .take_valid_commit_seals(view, commit_verify_fn, quorum)
            .await;
        // Wait for new commit messages until we hit quorum
        while commit_count < quorum {
            let new_commit_view = commit_rx
                .recv()
                .await
                .expect("Commit subscriber channel should not close");
            if new_commit_view == view {
                commit_count += 1;
                if commit_count == quorum {
                    (commit_seals, commit_count) = self
                        .messages
                        .take_valid_commit_seals(view, commit_verify_fn, quorum)
                        .await;
                }
            }
        }
        debug!("Received quorum commit messages");

        commit_seals.expect("Must have value when reached quorum")
    }

    async fn wait_rcc(
        &self,
        view: View,
        quorum: usize,
    ) -> (RoundChangeCertificate, Option<RawBlock>) {
        let round_change_verify_fn = self.round_change_verify_fn(view.height);

        let mut round_change_rx = self.messages.subscribe_round_change();
        let (mut round_changes, mut rc_count) = self
            .messages
            .take_valid_round_change_messages(view, &round_change_verify_fn, quorum)
            .await;
        // Wait for new round change messages until we hit quorum
        while rc_count < quorum {
            let new_rc_view = round_change_rx
                .recv()
                .await
                .expect("Round change subscriber channel should not close");
            if new_rc_view == view {
                rc_count += 1;
                if rc_count == quorum {
                    (round_changes, rc_count) = self
                        .messages
                        .take_valid_round_change_messages(view, &round_change_verify_fn, quorum)
                        .await;
                }
            }
        }
        debug!("Received quorum round change messages");

        let mut round_changes = round_changes.expect("Must have value when reached quorum");

        let prepared_proposed_pos = round_changes
            .iter()
            .position(|round_change| round_change.latest_prepared_proposed().is_some());

        let (metadata_list, raw_block) = match prepared_proposed_pos {
            Some(pos) => {
                let round_change = round_changes.swap_remove(pos);
                let (Some(proposed_block), metadata) = round_change.into_metadata() else {
                    panic!("Impossible to not have block while having prepared proposed");
                };

                let mut metadata_list = round_changes
                    .into_iter()
                    .map(|rc| rc.into_metadata().1)
                    .collect::<Vec<_>>();

                metadata_list.push(metadata);

                (metadata_list, Some(proposed_block.into_raw_block()))
            }
            None => {
                // No prepared certificate is found in any of the messages
                let metadata_list = round_changes
                    .into_iter()
                    .map(|rc| rc.into_metadata().1)
                    .collect::<Vec<_>>();
                (metadata_list, None)
            }
        };

        (
            RoundChangeCertificate {
                round_change_messages: metadata_list,
            },
            raw_block,
        )
    }

    fn monitor_future_rcc(&self, view: View) -> (oneshot::Receiver<u8>, JoinHandle<()>) {
        let ibft = self.clone();
        let (tx, rx) = oneshot::channel();
        let task = task::spawn(async move {
            let rcc_round = ibft.wait_future_rcc(view).await;
            let _ = tx.send(rcc_round);
        });

        (rx, task)
    }

    async fn wait_future_rcc(&self, view: View) -> u8 {
        let quorum = self.validator_manager.quorum(view.height);

        let round_change_verify_fn = self.round_change_verify_fn(view.height);
        let mut round_change_rx = self.messages.subscribe_round_change();
        let mut message_count_by_round =
            self.messages.round_change_count_by_round(view.height).await;
        // Check if any of the future rounds has quorum valid round change messages, and return early
        for (idx, count) in message_count_by_round.iter_mut().enumerate().rev() {
            let round = idx as u8;
            if round <= view.round {
                break;
            }

            if *count >= quorum {
                *count = self
                    .messages
                    .get_valid_round_change_count(
                        View::new(view.height, round),
                        &round_change_verify_fn,
                    )
                    .await;
                if *count >= quorum {
                    return round;
                }
            }
        }

        // Wait until receiving quorum round changes in one of the future rounds
        loop {
            let new_view = round_change_rx
                .recv()
                .await
                .expect("Round change subscription should not close");

            if new_view.height == view.height && new_view.round > view.round {
                let count = &mut message_count_by_round[new_view.round as usize];
                *count += 1;
                if *count >= quorum {
                    *count = self
                        .messages
                        .get_valid_round_change_count(new_view, &round_change_verify_fn)
                        .await;
                    if *count >= quorum {
                        return new_view.round;
                    }
                }
            }
        }
    }

    fn monitor_future_proposal(&self, view: View) -> (oneshot::Receiver<u8>, JoinHandle<()>) {
        let ibft = self.clone();
        let (tx, rx) = oneshot::channel();
        let task = task::spawn(async move {
            let round = ibft.wait_future_proposal(view).await;
            let _ = tx.send(round);
        });

        (rx, task)
    }

    async fn wait_future_proposal(&self, view: View) -> u8 {
        let quorum = self.validator_manager.quorum(view.height);

        let proposal_verify_fn = self.proposal_1_verify_fn(quorum);
        let mut proposal_rx = self.messages.subscribe_proposal();
        let round_has_proposal = self.messages.has_proposal_by_round(view.height).await;
        for (idx, has_proposal) in round_has_proposal.into_iter().enumerate().rev() {
            let round = idx as u8;
            if round <= view.round {
                break;
            }

            if has_proposal {
                if let Some(Ok(_)) = self
                    .messages
                    .get_valid_proposal_digest(View::new(view.height, round), &proposal_verify_fn)
                    .await
                {
                    return round;
                }
            }
        }

        // Wait until receiving a valid proposal for a future round
        loop {
            let new_view = proposal_rx
                .recv()
                .await
                .expect("Proposal channel should not close");

            if new_view.height == view.height && new_view.round > view.round {
                if let Some(Ok(_)) = self
                    .messages
                    .get_valid_proposal_digest(new_view, &proposal_verify_fn)
                    .await
                {
                    return new_view.round;
                }
            }
        }
    }

    async fn handle_round_finish(&self, view: View, commit_seals: CommitSeals) -> FinalizedBlock {
        let proposed_block = self
            .messages
            .take_proposal_message(view)
            .await
            .expect("Proposal must exist when round finished")
            .into_proposed_block();
        let finalized_block = FinalizedBlock::new(proposed_block, commit_seals);

        self.broadcast.broadcast_block(&finalized_block).await;

        finalized_block
    }

    async fn handle_timeout(
        &self,
        state: SharedRunState,
        latest_self_round_change: &mut Option<RoundChangeMessageSigned>,
    ) {
        let view = state.view;
        let run_state = state.get();

        match run_state {
            // If commit, broadcast round change message with the newly created prepared proposed
            RunState::Commit => {
                debug!("Able to compose a new prepared proposed for round change");

                let proposal = self
                    .messages
                    .take_proposal_message(view)
                    .await
                    .expect("Proposal must exist when past prepare state");
                let prepare_verify_fn = |prepare: &PrepareMessageSigned| -> bool {
                    prepare.proposed_block_digest() == proposal.proposed_block_digest()
                };
                let prepares = self
                    .messages
                    .take_valid_prepare_messages(view, prepare_verify_fn)
                    .await;

                let quorum = self.validator_manager.quorum(view.height);
                assert!(
                    prepares.len() >= quorum - 1,
                    "Got {} prepares while prepare quorum is {} in timeout handler",
                    prepares.len(),
                    quorum - 1,
                );

                let prepared_proposed = PreparedProposed::new(proposal, prepares);

                let round_change = RoundChangeMessage::new(
                    View::new(view.height, view.round + 1),
                    Some(prepared_proposed),
                )
                .into_signed(&self.signer);

                self.broadcast
                    .broadcast_message(IBFTBroadcastMessage::RoundChange(&round_change))
                    .await;

                *latest_self_round_change = Some(round_change);
            }
            // Else, broadcast round change message with either none or previously created prepared proposed
            _ => match latest_self_round_change {
                Some(round_change) => {
                    debug!("Using previously created round change");

                    round_change.update_and_resign(view.round + 1, &self.signer);

                    self.broadcast
                        .broadcast_message(IBFTBroadcastMessage::RoundChange(round_change))
                        .await;
                }
                None => {
                    debug!("No previously sent round change, creating new one");

                    let round_change =
                        RoundChangeMessage::new(View::new(view.height, view.round + 1), None)
                            .into_signed(&self.signer);

                    self.broadcast
                        .broadcast_message(IBFTBroadcastMessage::RoundChange(&round_change))
                        .await;

                    *latest_self_round_change = Some(round_change);
                }
            },
        }
    }

    fn proposal_1_verify_fn(
        &self,
        quorum: usize,
    ) -> impl Fn(&ProposalMessageSigned) -> Result<(), IBFTError> + '_ {
        move |proposal: &ProposalMessageSigned| {
            if !proposal.verify_block_digest() {
                return Err(IBFTError::IncorrectProposalDigest);
            }

            let Some(rcc) = proposal.round_change_certificate() else {
                return Err(IBFTError::MissingRoundChangeCertificate);
            };
            let round_changes = rcc.round_change_messages.as_slice();

            if round_changes.len() < quorum {
                return Err(IBFTError::RoundChangeCertificateQuorumNotReached);
            }

            let proposal_view = proposal.view();

            let mut highest_round = 0;
            let mut highest_round_pc: Option<&PreparedCertificate> = None;
            let mut seen_validators = HashSet::with_capacity(round_changes.len());
            for round_change in round_changes {
                if round_change.view() != proposal_view {
                    return Err(IBFTError::InvalidRoundChangeInCertificate);
                }

                let Ok(validator) = round_change.recover_signer() else {
                    return Err(IBFTError::InvalidRoundChangeInCertificate);
                };
                if !self
                    .validator_manager
                    .is_validator(validator, proposal_view.height)
                {
                    return Err(IBFTError::InvalidRoundChangeInCertificate);
                }

                let inserted = seen_validators.insert(validator);
                if !inserted {
                    return Err(IBFTError::DuplicateRoundChangeInCertificate);
                }

                if let Some(pc) = round_change.latest_prepared_certificate() {
                    let pc_round = pc.proposal().view().round;
                    if pc_round > highest_round {
                        highest_round = pc_round;
                        highest_round_pc = Some(pc);
                    }
                }
            }

            let proposed_block = proposal.proposed_block();

            if let Some(pc) = highest_round_pc {
                // Proposed block must correspond to the highest round prepared certificate
                // within the round change messages
                let expected_digest = digest_block(proposed_block.raw_block(), highest_round);
                if expected_digest != pc.proposal().proposed_block_digest() {
                    return Err(IBFTError::InvalidRoundChangeInCertificate);
                }

                if !self.verify_prepared_certificate(pc, proposal_view.height, proposal_view.round)
                {
                    return Err(IBFTError::InvalidRoundChangeInCertificate);
                }
            } else {
                // There are no prepared certificates in any of the round change messages.
                // Verify the newly proposed block.
                if let Err(err) = self.block_service.verify_block(proposed_block.raw_block()) {
                    return Err(IBFTError::InvalidProposalBlock(err.to_string()));
                }
            }

            Ok(())
        }
    }

    fn round_change_verify_fn(
        &self,
        height: u64,
    ) -> impl Fn(&RoundChangeMessageSigned) -> bool + '_ {
        move |round_change: &RoundChangeMessageSigned| {
            let Some(prepared_proposed) = round_change.latest_prepared_proposed() else {
                return true;
            };

            let prepared_certificate = prepared_proposed.prepared_certificate();
            let proposed_block_digest = prepared_proposed.proposed_block().digest();
            if prepared_certificate.proposal().proposed_block_digest() != proposed_block_digest {
                return false;
            }

            self.verify_prepared_certificate(
                prepared_certificate,
                height,
                round_change.view().round,
            )
        }
    }

    fn verify_prepared_certificate(
        &self,
        prepared_certificate: &PreparedCertificate,
        height: u64,
        round_limit: u8,
    ) -> bool {
        let proposal = prepared_certificate.proposal();
        let proposal_view = proposal.view();
        if proposal_view.height != height || proposal_view.round >= round_limit {
            return false;
        }
        let Ok(proposer) = proposal.recover_signer() else {
            return false;
        };
        if !self.validator_manager.is_proposer(proposer, proposal_view) {
            return false;
        }

        let prepares = prepared_certificate.prepare_messages();
        let quorum = self.validator_manager.quorum(height);
        if prepares.len() < quorum - 1 {
            return false;
        }

        let proposed_block_digest = proposal.proposed_block_digest();
        let mut seen_validators = HashSet::with_capacity(prepares.len());
        for prepare in prepares {
            if proposal_view != prepare.view() {
                return false;
            }
            if prepare.proposed_block_digest() != proposed_block_digest {
                return false;
            }
            let Ok(validator) = prepare.recover_signer() else {
                return false;
            };
            if validator == proposer
                || !self
                    .validator_manager
                    .is_validator(validator, proposal_view.height)
            {
                return false;
            }

            let inserted = seen_validators.insert(validator);
            if !inserted {
                // Duplicate validator
                return false;
            }
        }

        true
    }

    fn get_round_timeout(&self, round: u8) -> Duration {
        const TIMEOUT_MULTIPLIER: [u32; ROUND_ARRAY_SIZE] = {
            let mut arr = [0; ROUND_ARRAY_SIZE];
            let mut i = 0;
            while i < ROUND_ARRAY_SIZE {
                arr[i] = 1 << i;
                i += 1;
            }
            arr
        };

        self.base_round_time
            .saturating_mul(TIMEOUT_MULTIPLIER[round as usize])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
enum RunState {
    Proposal = 0,
    Prepare = 1,
    Commit = 2,
}

impl RunState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Proposal,
            1 => Self::Prepare,
            2 => Self::Commit,
            _ => panic!("Should never be called with invalid value"),
        }
    }
}

#[derive(Debug, Clone)]
struct SharedRunState {
    view: View,
    run_state: Arc<AtomicU8>,
}

impl SharedRunState {
    fn new(view: View) -> Self {
        Self {
            view,
            run_state: Arc::new(AtomicU8::new(RunState::Proposal as u8)),
        }
    }

    fn update(&self, new_state: RunState) {
        self.run_state.store(new_state as u8, Ordering::SeqCst);
    }

    fn get(&self) -> RunState {
        RunState::from_u8(self.run_state.load(Ordering::SeqCst))
    }
}
