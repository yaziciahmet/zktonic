use std::iter;
use std::sync::Arc;
use std::time::Duration;

use tokio::select;
use tokio::sync::{oneshot, RwLock};
use tokio::task::JoinHandle;
use tonic_signer::Signer;
use tracing::{error, info};

use crate::backend::{BlockBuilder, BlockVerifier, Broadcast, ValidatorManager};
use crate::types::{
    CommitMessage, CommitMessageSigned, CommitSeals, FinalizedBlock, IBFTBroadcastMessage,
    PrepareMessage, PrepareMessageSigned, ProposalMessage,
};

use super::messages::ConsensusMessages;
use super::types::View;

const TIMEOUT_TABLE: [Duration; 6] = [
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(32),
    Duration::from_secs(64),
    Duration::from_secs(128),
];

#[derive(Clone)]
pub struct IBFT<V, B, BV, BB>
where
    V: ValidatorManager,
    B: Broadcast,
    BV: BlockVerifier,
    BB: BlockBuilder,
{
    messages: ConsensusMessages,
    validator_manager: V,
    broadcast: B,
    signer: Signer,
    block_verifier: BV,
    block_builder: BB,
}

impl<V, B, BV, BB> IBFT<V, B, BV, BB>
where
    V: ValidatorManager,
    B: Broadcast,
    BV: BlockVerifier,
    BB: BlockBuilder,
{
    pub fn new(
        messages: ConsensusMessages,
        validator_manager: V,
        broadcast: B,
        block_verifier: BV,
        block_builder: BB,
        signer: Signer,
    ) -> Self {
        Self {
            messages,
            validator_manager,
            broadcast,
            block_verifier,
            block_builder,
            signer,
        }
    }

    pub async fn run(
        &self,
        height: u64,
        mut cancel: oneshot::Receiver<()>,
    ) -> Option<FinalizedBlock> {
        let mut view = View { height, round: 0 };

        info!("Running consensus height {}", view.height);
        loop {
            info!("Running consensus round {}", view.round);

            let state = SharedRunState::new(view);

            let timeout = tokio::time::sleep(get_round_timeout(view.round));
            let (future_proposal_rx, future_proposal_task) =
                self.watch_future_proposal(state.clone());
            let (rcc_rx, rcc_task) = self.watch_rcc(state.clone());
            let (round_finished, round_task) = self.start_ibft_round(state);

            let abort = move || {
                round_task.abort();
                future_proposal_task.abort();
                rcc_task.abort();
            };

            select! {
                biased;
                _ = &mut cancel => {
                    info!("Received cancel signal, stopping consensus...");
                    abort();
                    return None;
                }
                _ = timeout => {
                    info!("Round timeout");
                    abort();
                    view.round += 1;
                }
                _ = future_proposal_rx => {
                    info!("Received future proposal");
                    abort();
                }
                _ = rcc_rx => {
                    info!("Got enough round change messages to create round change certificate");
                    abort();
                }
                Ok(commit_seals) = round_finished => {
                    info!("Finished IBFT round");
                    abort();

                    let proposal = self.messages.take_proposal_message(view).await.expect("There must be a proposal when round is finished");
                    let finalized_block = FinalizedBlock::new(proposal.into_proposed_block(), commit_seals);
                    return Some(finalized_block);
                }
            }
        }
    }

    fn start_ibft_round(
        &self,
        state: SharedRunState,
    ) -> (oneshot::Receiver<CommitSeals>, JoinHandle<()>) {
        let ibft = self.clone();
        let (tx, rx) = oneshot::channel();

        let task = tokio::spawn(async move {
            match ibft.run_ibft_round0(state).await {
                Ok(commit_seals) => {
                    let _ = tx.send(commit_seals);
                }
                Err(err) => {
                    // TODO: think about what to do here
                    error!("Error occurred during IBFT run: {err}");
                }
            }
        });

        (rx, task)
    }

    async fn run_ibft_round0(&self, state: SharedRunState) -> Result<CommitSeals, IBFTError> {
        let view = state.view;

        assert_eq!(view.round, 0, "round must be 0");

        let proposal = if self
            .validator_manager
            .is_proposer(self.signer.address(), view)
        {
            info!("We are the block proposer");

            // Build a block
            let raw_eth_block = self
                .block_builder
                .build_block(view.height)
                .map_err(IBFTError::BlockBuild)?;
            let proposal =
                ProposalMessage::new(view, raw_eth_block, None).into_signed(&self.signer);

            // Broadcast proposal to peers
            self.broadcast
                .broadcast_message(IBFTBroadcastMessage::Proposal(&proposal))
                .await;

            proposal
        } else {
            // We first subscribe so we don't miss the notification in the brief time we query the proposal.
            let mut proposal_rx = self.messages.subscribe_proposal();
            let proposal = if let Some(proposal) = self.messages.take_proposal_message(view).await {
                proposal
            } else {
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
                    .take_proposal_message(view)
                    .await
                    .expect("Proposal message for the view must exist")
            };

            let proposed_block = proposal.proposed_block();
            // Verify proposed block's round
            if proposed_block.round() != view.round {
                return Err(IBFTError::IncorrectProposedBlockRound);
            }
            // Verify proposed block digest
            if !proposal.verify_digest() {
                return Err(IBFTError::IncorrectProposalDigest);
            }
            // Verify ethereum block
            if let Err(err) = self
                .block_verifier
                .verify_block(proposed_block.raw_eth_block())
            {
                return Err(IBFTError::InvalidBlock(err));
            }

            proposal
        };

        // Go to prepare state
        state.set_state(RunState::Prepare).await;

        let proposed_block_digest = proposal.proposed_block_digest();

        let quorum = self.validator_manager.quorum(view.height);
        assert_ne!(quorum, 0, "Quorum must be greater than 0");

        // Broadcast prepare to peers
        let prepare = PrepareMessage::new(view, proposed_block_digest).into_signed(&self.signer);
        self.broadcast
            .broadcast_message(IBFTBroadcastMessage::Prepare(&prepare))
            .await;

        // We only need to verify the proposed block digest, signature check is enforced by `MessageHandler`,
        // and querying by view also ensures height and round matches.
        let verify_prepare_fn = move |prepare: &PrepareMessageSigned| -> bool {
            prepare.proposed_block_digest() == proposed_block_digest
        };
        // Subscribe to prepare messages first
        let mut prepare_rx = self.messages.subscribe_prepare();
        let (mut prepares, mut need_more) = self
            .messages
            .try_collect_valid_prepare_messages(view, verify_prepare_fn, quorum - 1)
            .await;
        // Wait for new prepare messages until we hit quorum - 1 (+1 from us)
        while need_more > 0 {
            let new_prepare_view = prepare_rx
                .recv()
                .await
                .expect("Prepare subscriber channel should not close");
            if new_prepare_view == view {
                need_more -= 1;
                if need_more == 0 {
                    (prepares, need_more) = self
                        .messages
                        .try_collect_valid_prepare_messages(view, verify_prepare_fn, quorum - 1)
                        .await;
                }
            }
        }

        let mut prepares = prepares.expect("Must be some");
        prepares.push(prepare);

        // Go to commit state
        state.set_state(RunState::Commit).await;

        // Broadcast commit to peers
        let commit =
            CommitMessage::new(view, proposed_block_digest, &self.signer).into_signed(&self.signer);
        self.broadcast
            .broadcast_message(IBFTBroadcastMessage::Commit(&commit))
            .await;

        // We only need to verify the proposed block digest, signature check is enforced by `MessageHandler`,
        // and querying by view also ensures height and round matches.
        let verify_commit_fn = move |commit: &CommitMessageSigned| -> bool {
            commit.proposed_block_digest() == proposed_block_digest
        };
        // Subscribe to commit messages first
        let mut commit_rx = self.messages.subscribe_commit();
        let (mut commits, mut need_more) = self
            .messages
            .try_collect_valid_commit_messages(view, verify_commit_fn, quorum - 1)
            .await;
        // Wait for new commit messages until we hit quorum - 1 (+1 from us)
        while need_more > 0 {
            let new_commit_view = commit_rx
                .recv()
                .await
                .expect("Commit subscriber channel should not close");
            if new_commit_view == view {
                need_more -= 1;
                if need_more == 0 {
                    (commits, need_more) = self
                        .messages
                        .try_collect_valid_commit_messages(view, verify_commit_fn, quorum - 1)
                        .await;
                }
            }
        }

        let commit_seals = commits
            .expect("Must be some")
            .into_iter()
            .map(|commit| commit.commit_seal())
            .chain(iter::once(commit.commit_seal()))
            .collect();

        // Go to finalized state
        state.set_state(RunState::Finalized).await;

        Ok(commit_seals)
    }

    fn watch_rcc(&self, state: SharedRunState) -> (oneshot::Receiver<()>, JoinHandle<()>) {
        let (tx, rx) = oneshot::channel();
        let ibft = self.clone();
        let task = tokio::spawn(async move {
            ibft.wait_until_rcc(state).await;
            let _ = tx.send(());
        });

        (rx, task)
    }

    async fn wait_until_rcc(&self, state: SharedRunState) {
        let view = state.view;

        let round_change_rx = self.messages.subscribe_round_change();
        todo!()
    }

    fn watch_future_proposal(
        &self,
        _state: SharedRunState,
    ) -> (oneshot::Receiver<()>, JoinHandle<()>) {
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            // TODO: actually watch for future proposal
            tokio::time::sleep(Duration::from_secs(9999)).await;
            let _ = tx.send(());
        });

        (rx, task)
    }
}

fn get_round_timeout(mut round: u32) -> Duration {
    if round > 5 {
        round = 5;
    }

    TIMEOUT_TABLE[round as usize]
}

/// `SharedRunState` is the shared state of the currently running IBFT
/// consensus. Used for tracking the current step of the IBFT run.
#[derive(Clone, Debug)]
struct SharedRunState {
    view: View,
    state: Arc<RwLock<RunState>>,
}

impl SharedRunState {
    fn new(view: View) -> Self {
        Self {
            view,
            state: Default::default(),
        }
    }

    async fn set_state(&self, new_state: RunState) {
        *self.state.write().await = new_state;
    }

    async fn proposal_accepted(&self) -> bool {
        *self.state.read().await != RunState::Proposal
    }
}

#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum RunState {
    #[default]
    Proposal,
    Prepare,
    Commit,
    Finalized,
}

#[derive(Debug, thiserror::Error)]
enum IBFTError {
    #[error("Incorrect proposed block round")]
    IncorrectProposedBlockRound,
    #[error("Incorrect proposal digest")]
    IncorrectProposalDigest,
    #[error("Invalid block: {0}")]
    InvalidBlock(anyhow::Error),
    #[error("Block builder failed: {0}")]
    BlockBuild(anyhow::Error),
}
