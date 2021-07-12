// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Implements the dispute participation subsystem.
//!
//! This subsystem is responsible for actually participating in disputes: when
//! notified of a dispute, we recover the candidate data, validate the
//! candidate, and cast our vote in the dispute.

use futures::channel::oneshot;
use futures::prelude::*;

use polkadot_node_primitives::ValidationResult;
use polkadot_node_subsystem::{
	errors::{RecoveryError, RuntimeApiError},
	overseer,
	messages::{
		AvailabilityRecoveryMessage, AvailabilityStoreMessage,
		CandidateValidationMessage, DisputeCoordinatorMessage, DisputeParticipationMessage,
		RuntimeApiMessage, RuntimeApiRequest,
	},
	ActiveLeavesUpdate, FromOverseer, OverseerSignal, SpawnedSubsystem,
	SubsystemContext, SubsystemError,
};
use polkadot_primitives::v1::{BlockNumber, CandidateHash, CandidateReceipt, Hash, SessionIndex};

#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "parachain::dispute-participation";

struct State {
	recent_block: Option<(BlockNumber, Hash)>,
}

/// An implementation of the dispute participation subsystem.
pub struct DisputeParticipationSubsystem;

impl DisputeParticipationSubsystem {
	/// Create a new instance of the subsystem.
	pub fn new() -> Self {
		DisputeParticipationSubsystem
	}
}

impl<Context> overseer::overseer::Subsystem<Context> for DisputeParticipationSubsystem
where
	Context: overseer::SubsystemContext<Message = DisputeParticipationMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = run(ctx).map(|_| Ok(())).boxed();

		SpawnedSubsystem {
			name: "dispute-participation-subsystem",
			future,
		}
	}
}

#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum Error {
	#[error(transparent)]
	RuntimeApi(#[from] RuntimeApiError),

	#[error(transparent)]
	Subsystem(#[from] SubsystemError),

	#[error(transparent)]
	Oneshot(#[from] oneshot::Canceled),

	#[error("Oneshot receiver died")]
	OneshotSendFailed,

	#[error(transparent)]
	Participation(#[from] ParticipationError),
}

#[derive(Debug, thiserror::Error)]
pub enum ParticipationError {
	#[error("Missing recent block state to participate in dispute")]
	MissingRecentBlockState,
	#[error("Failed to recover available data for candidate {0}")]
	MissingAvailableData(CandidateHash),
	#[error("Failed to recover validation code for candidate {0}")]
	MissingValidationCode(CandidateHash),
}

impl Error {
	fn trace(&self) {
		match self {
			// don't spam the log with spurious errors
			Self::RuntimeApi(_) | Self::Oneshot(_) => {
				tracing::debug!(target: LOG_TARGET, err = ?self)
			}
			// it's worth reporting otherwise
			_ => tracing::warn!(target: LOG_TARGET, err = ?self),
		}
	}
}

async fn run<Context>(mut ctx: Context)
where
	Context: overseer::SubsystemContext<Message = DisputeParticipationMessage>,
{
	let mut state = State { recent_block: None };

	loop {
		match ctx.recv().await {
			Err(_) => return,
			Ok(FromOverseer::Signal(OverseerSignal::Conclude)) => {
				tracing::info!(target: LOG_TARGET, "Received `Conclude` signal, exiting");
				return;
			}
			Ok(FromOverseer::Signal(OverseerSignal::BlockFinalized(_, _))) => {}
			Ok(FromOverseer::Signal(OverseerSignal::ActiveLeaves(update))) => {
				update_state(&mut state, update);
			}
			Ok(FromOverseer::Communication { msg }) => {
				if let Err(err) = handle_incoming(&mut ctx, &mut state, msg).await {
					err.trace();
					if let Error::Subsystem(SubsystemError::Context(_)) = err {
						return;
					}
				}
			}
		}
	}
}

fn update_state(state: &mut State, update: ActiveLeavesUpdate) {
	for active in update.activated {
		if state.recent_block.map_or(true, |s| active.number > s.0) {
			state.recent_block = Some((active.number, active.hash));
		}
	}
}

async fn handle_incoming(
	ctx: &mut impl SubsystemContext,
	state: &mut State,
	message: DisputeParticipationMessage,
) -> Result<(), Error> {
	match message {
		DisputeParticipationMessage::Participate {
			candidate_hash,
			candidate_receipt,
			session,
			n_validators,
			report_availability,
		} => {
			if let Some((_, block_hash)) = state.recent_block {
				participate(
					ctx,
					block_hash,
					candidate_hash,
					candidate_receipt,
					session,
					n_validators,
					report_availability,
				)
				.await
			} else {
				return Err(ParticipationError::MissingRecentBlockState.into());
			}
		}
	}
}

async fn participate(
	ctx: &mut impl SubsystemContext,
	block_hash: Hash,
	candidate_hash: CandidateHash,
	candidate_receipt: CandidateReceipt,
	session: SessionIndex,
	n_validators: u32,
	report_availability: oneshot::Sender<bool>,
) -> Result<(), Error> {
	let (recover_available_data_tx, recover_available_data_rx) = oneshot::channel();
	let (code_tx, code_rx) = oneshot::channel();
	let (store_available_data_tx, store_available_data_rx) = oneshot::channel();
	let (validation_tx, validation_rx) = oneshot::channel();

	// in order to validate a candidate we need to start by recovering the
	// available data
	ctx.send_message(
		AvailabilityRecoveryMessage::RecoverAvailableData(
			candidate_receipt.clone(),
			session,
			None,
			recover_available_data_tx,
		)
	)
	.await;

	let available_data = match recover_available_data_rx.await? {
		Ok(data) => {
			report_availability.send(true).map_err(|_| Error::OneshotSendFailed)?;
			data
		}
		Err(RecoveryError::Invalid) => {
			report_availability.send(true).map_err(|_| Error::OneshotSendFailed)?;

			// the available data was recovered but it is invalid, therefore we'll
			// vote negatively for the candidate dispute
			cast_invalid_vote(ctx, candidate_hash, candidate_receipt, session).await;
			return Ok(());
		}
		Err(RecoveryError::Unavailable) => {
			report_availability.send(false).map_err(|_| Error::OneshotSendFailed)?;

			return Err(ParticipationError::MissingAvailableData(candidate_hash).into());
		}
	};

	// we also need to fetch the validation code which we can reference by its
	// hash as taken from the candidate descriptor
	ctx.send_message(
		RuntimeApiMessage::Request(
			block_hash,
			RuntimeApiRequest::ValidationCodeByHash(
				candidate_receipt.descriptor.validation_code_hash,
				code_tx,
			),
		)
	)
	.await;

	let validation_code = match code_rx.await?? {
		Some(code) => code,
		None => {
			tracing::warn!(
				target: LOG_TARGET,
				"Validation code unavailable for code hash {:?} in the state of block {:?}",
				candidate_receipt.descriptor.validation_code_hash,
				block_hash,
			);

			return Err(ParticipationError::MissingValidationCode(candidate_hash).into());
		}
	};

	// we dispatch a request to store the available data for the candidate. we
	// want to maximize data availability for other potential checkers involved
	// in the dispute
	ctx.send_message(
		AvailabilityStoreMessage::StoreAvailableData(
			candidate_hash,
			None,
			n_validators,
			available_data.clone(),
			store_available_data_tx,
		)
	)
	.await;

	match store_available_data_rx.await? {
		Err(_) => {
			tracing::warn!(
				target: LOG_TARGET,
				"Failed to store available data for candidate {:?}",
				candidate_hash,
			);
		}
		Ok(()) => {}
	}

	// we issue a request to validate the candidate with the provided exhaustive
	// parameters
	ctx.send_message(
		CandidateValidationMessage::ValidateFromExhaustive(
			available_data.validation_data,
			validation_code,
			candidate_receipt.descriptor.clone(),
			available_data.pov,
			validation_tx,
		)
	)
	.await;

	// we cast votes (either positive or negative) depending on the outcome of
	// the validation and if valid, whether the commitments hash matches
	match validation_rx.await? {
		Err(err) => {
			tracing::warn!(
				target: LOG_TARGET,
				"Candidate {:?} validation failed with: {:?}",
				candidate_receipt.hash(),
				err,
			);

			cast_invalid_vote(ctx, candidate_hash, candidate_receipt, session).await;
		}
		Ok(ValidationResult::Invalid(invalid)) => {
			tracing::warn!(
				target: LOG_TARGET,
				"Candidate {:?} considered invalid: {:?}",
				candidate_hash,
				invalid,
			);

			cast_invalid_vote(ctx, candidate_hash, candidate_receipt, session).await;
		}
		Ok(ValidationResult::Valid(commitments, _)) => {
			if commitments.hash() != candidate_receipt.commitments_hash {
				tracing::warn!(
					target: LOG_TARGET,
					expected = ?candidate_receipt.commitments_hash,
					got = ?commitments.hash(),
					"Candidate is valid but commitments hash doesn't match",
				);

				cast_invalid_vote(ctx, candidate_hash, candidate_receipt, session).await;
			} else {
				cast_valid_vote(ctx, candidate_hash, candidate_receipt, session).await;
			}
		}
	}

	Ok(())
}

async fn cast_valid_vote(
	ctx: &mut impl SubsystemContext,
	candidate_hash: CandidateHash,
	candidate_receipt: CandidateReceipt,
	session: SessionIndex,
) {
	tracing::info!(
		target: LOG_TARGET,
		"Casting valid vote in dispute for candidate {:?}",
		candidate_hash,
	);

	issue_local_statement(ctx, candidate_hash, candidate_receipt, session, true).await;
}

async fn cast_invalid_vote(
	ctx: &mut impl SubsystemContext,
	candidate_hash: CandidateHash,
	candidate_receipt: CandidateReceipt,
	session: SessionIndex,
) {
	tracing::info!(
		target: LOG_TARGET,
		"Casting invalid vote in dispute for candidate {:?}",
		candidate_hash,
	);

	issue_local_statement(ctx, candidate_hash, candidate_receipt, session, false).await;
}

async fn issue_local_statement(
	ctx: &mut impl SubsystemContext,
	candidate_hash: CandidateHash,
	candidate_receipt: CandidateReceipt,
	session: SessionIndex,
	valid: bool,
) {
	ctx.send_message(
		DisputeCoordinatorMessage::IssueLocalStatement(
			session,
			candidate_hash,
			candidate_receipt,
			valid,
		),
	)
	.await
}
