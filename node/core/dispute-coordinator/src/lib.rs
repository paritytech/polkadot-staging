// Copyright 2020 Parity Technologies (UK) Ltd.
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

//! Implements the dispute coordinator subsystem.
//!
//! This is the central subsystem of the node-side components which participate in disputes.
//! This subsystem wraps a database which tracks all statements observed by all validators over some window of sessions.
//! Votes older than this session window are pruned.
//!
//! This subsystem will be the point which produce dispute votes, either positive or negative, based on locally-observed
//! validation results as well as a sink for votes received by other subsystems. When importing a dispute vote from
//! another node, this will trigger the dispute participation subsystem to recover and validate the block and call
//! back to this subsystem.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use polkadot_node_primitives::{CandidateVotes, DISPUTE_WINDOW, DisputeMessage, SignedDisputeStatement, DisputeMessageCheckError};
use polkadot_node_subsystem::{
	overseer, SubsystemContext, FromOverseer, OverseerSignal, SpawnedSubsystem, SubsystemError,
	errors::{ChainApiError, RuntimeApiError},
	messages::{
		ChainApiMessage, DisputeCoordinatorMessage, DisputeDistributionMessage,
		DisputeParticipationMessage, ImportStatementsResult,
	}
};
use polkadot_node_subsystem_util::rolling_session_window::{
	RollingSessionWindow, SessionWindowUpdate,
};
use polkadot_primitives::v1::{
	BlockNumber, CandidateHash, CandidateReceipt, DisputeStatement, Hash,
	SessionIndex, SessionInfo, ValidatorIndex, ValidatorPair, ValidatorSignature
};

use futures::prelude::*;
use futures::channel::oneshot;
use kvdb::KeyValueDB;
use parity_scale_codec::{Encode, Decode, Error as CodecError};
use sc_keystore::LocalKeystore;

use db::v1::{RecentDisputes, DbBackend};
use backend::{Backend, OverlayedBackend};

mod db;
mod backend;

#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "parachain::dispute-coordinator";

// The choice here is fairly arbitrary. But any dispute that concluded more than a few minutes ago
// is not worth considering anymore. Changing this value has little to no bearing on consensus,
// and really only affects the work that the node might do on startup during periods of many disputes.
const ACTIVE_DURATION_SECS: Timestamp = 180;

/// Timestamp based on the 1 Jan 1970 UNIX base, which is persistent across node restarts and OS reboots.
type Timestamp = u64;

#[derive(Eq, PartialEq)]
enum Participation {
	Pending,
	Complete,
}

impl Participation {
	fn complete(&mut self) -> bool {
		let complete = *self == Participation::Complete;
		if !complete {
			*self = Participation::Complete
		}
		complete
	}
}

struct State {
	keystore: Arc<LocalKeystore>,
	highest_session: Option<SessionIndex>,
	rolling_session_window: RollingSessionWindow,
	recovery_state: Participation,
}

/// Configuration for the dispute coordinator subsystem.
#[derive(Debug, Clone, Copy)]
pub struct Config {
	/// The data column in the store to use for dispute data.
	pub col_data: u32,
}

impl Config {
	fn column_config(&self) -> db::v1::ColumnConfiguration {
		db::v1::ColumnConfiguration { col_data: self.col_data }
	}
}

/// An implementation of the dispute coordinator subsystem.
pub struct DisputeCoordinatorSubsystem {
	config: Config,
	store: Arc<dyn KeyValueDB>,
	keystore: Arc<LocalKeystore>,
}

impl DisputeCoordinatorSubsystem {
	/// Create a new instance of the subsystem.
	pub fn new(
		store: Arc<dyn KeyValueDB>,
		config: Config,
		keystore: Arc<LocalKeystore>,
	) -> Self {
		DisputeCoordinatorSubsystem { store, config, keystore }
	}
}

impl<Context> overseer::Subsystem<Context, SubsystemError> for DisputeCoordinatorSubsystem
where
	Context: SubsystemContext<Message = DisputeCoordinatorMessage>,
	Context: overseer::SubsystemContext<Message = DisputeCoordinatorMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let backend = DbBackend::new(self.store.clone(), self.config.column_config());
		let future = run(self, ctx, backend, Box::new(SystemClock))
			.map(|_| Ok(()))
			.boxed();

		SpawnedSubsystem {
			name: "dispute-coordinator-subsystem",
			future,
		}
	}
}

trait Clock: Send + Sync {
	fn now(&self) -> Timestamp;
}

struct SystemClock;

impl Clock for SystemClock {
	fn now(&self) -> Timestamp {
		// `SystemTime` is notoriously non-monotonic, so our timers might not work
		// exactly as expected.
		//
		// Regardless, disputes are considered active based on an order of minutes,
		// so a few seconds of slippage in either direction shouldn't affect the
		// amount of work the node is doing significantly.
		match SystemTime::now().duration_since(UNIX_EPOCH) {
			Ok(d) => d.as_secs(),
			Err(e) => {
				tracing::warn!(
					target: LOG_TARGET,
					err = ?e,
					"Current time is before unix epoch. Validation will not work correctly."
				);

				0
			}
		}
	}
}

#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum Error {
	#[error(transparent)]
	RuntimeApi(#[from] RuntimeApiError),

	#[error(transparent)]
	ChainApi(#[from] ChainApiError),

	#[error(transparent)]
	Io(#[from] std::io::Error),

	#[error(transparent)]
	Oneshot(#[from] oneshot::Canceled),

	#[error("Oneshot send failed")]
	OneshotSend,

	#[error(transparent)]
	Subsystem(#[from] SubsystemError),

	#[error(transparent)]
	Codec(#[from] CodecError),
}

impl From<db::v1::Error> for Error {
	fn from(err: db::v1::Error) -> Self {
		match err {
			db::v1::Error::Io(io) => Self::Io(io),
			db::v1::Error::Codec(e) => Self::Codec(e),
		}
	}
}

impl Error {
	fn trace(&self) {
		match self {
			// don't spam the log with spurious errors
			Self::RuntimeApi(_) |
			Self::Oneshot(_) => tracing::debug!(target: LOG_TARGET, err = ?self),
			// it's worth reporting otherwise
			_ => tracing::warn!(target: LOG_TARGET, err = ?self),
		}
	}
}

/// The status of dispute. This is a state machine which can be altered by the
/// helper methods.
#[derive(Debug, Clone, Copy, Encode, Decode, PartialEq)]
pub enum DisputeStatus {
	/// The dispute is active and unconcluded.
	#[codec(index = 0)]
	Active,
	/// The dispute has been concluded in favor of the candidate
	/// since the given timestamp.
	#[codec(index = 1)]
	ConcludedFor(Timestamp),
	/// The dispute has been concluded against the candidate
	/// since the given timestamp.
	///
	/// This takes precedence over `ConcludedFor` in the case that
	/// both are true, which is impossible unless a large amount of
	/// validators are participating on both sides.
	#[codec(index = 2)]
	ConcludedAgainst(Timestamp),
}

impl DisputeStatus {
	/// Initialize the status to the active state.
	pub fn active() -> DisputeStatus {
		DisputeStatus::Active
	}

	/// Transition the status to a new status after observing the dispute has concluded for the candidate.
	/// This may be a no-op if the status was already concluded.
	pub fn concluded_for(self, now: Timestamp) -> DisputeStatus {
		match self {
			DisputeStatus::Active => DisputeStatus::ConcludedFor(now),
			DisputeStatus::ConcludedFor(at) => DisputeStatus::ConcludedFor(std::cmp::min(at, now)),
			against => against,
		}
	}

	/// Transition the status to a new status after observing the dispute has concluded against the candidate.
	/// This may be a no-op if the status was already concluded.
	pub fn concluded_against(self, now: Timestamp) -> DisputeStatus {
		match self {
			DisputeStatus::Active => DisputeStatus::ConcludedAgainst(now),
			DisputeStatus::ConcludedFor(at) => DisputeStatus::ConcludedAgainst(std::cmp::min(at, now)),
			DisputeStatus::ConcludedAgainst(at) => DisputeStatus::ConcludedAgainst(std::cmp::min(at, now)),
		}
	}

	/// Whether the disputed candidate is possibly invalid.
	pub fn is_possibly_invalid(&self) -> bool {
		match self {
			DisputeStatus::Active | DisputeStatus::ConcludedAgainst(_) => true,
			DisputeStatus::ConcludedFor(_) => false,
		}
	}

	/// Yields the timestamp this dispute concluded at, if any.
	pub fn concluded_at(&self) -> Option<Timestamp> {
		match self {
			DisputeStatus::Active => None,
			DisputeStatus::ConcludedFor(at) | DisputeStatus::ConcludedAgainst(at) => Some(*at),
		}
	}
}

async fn run<B, Context>(
	subsystem: DisputeCoordinatorSubsystem,
	mut ctx: Context,
	mut backend: B,
	clock: Box<dyn Clock>,
)
where
	Context: overseer::SubsystemContext<Message = DisputeCoordinatorMessage>,
	Context: SubsystemContext<Message = DisputeCoordinatorMessage>,
	B: Backend,
{
	loop {
		let res = run_until_error(&mut ctx, &subsystem, &mut backend, &*clock).await;
		match res {
			Err(e) => {
				e.trace();

				if let Error::Subsystem(SubsystemError::Context(_)) = e {
					break;
				}
			}
			Ok(()) => {
				tracing::info!(target: LOG_TARGET, "received `Conclude` signal, exiting");
				break;
			}
		}
	}
}

// Run the subsystem until an error is encountered or a `conclude` signal is received.
// Most errors are non-fatal and should lead to another call to this function.
//
// A return value of `Ok` indicates that an exit should be made, while non-fatal errors
// lead to another call to this function.
async fn run_until_error<B, Context>(
	ctx: &mut Context,
	subsystem: &DisputeCoordinatorSubsystem,
	backend: &mut B,
	clock: &dyn Clock,
) -> Result<(), Error>
where
	Context: overseer::SubsystemContext<Message = DisputeCoordinatorMessage>,
	Context: SubsystemContext<Message = DisputeCoordinatorMessage>,
	B: Backend,
{
	let mut state = State {
		keystore: subsystem.keystore.clone(),
		highest_session: None,
		rolling_session_window: RollingSessionWindow::new(DISPUTE_WINDOW),
		recovery_state: Participation::Pending,
	};

	loop {
		let mut overlay_db = OverlayedBackend::new(backend);
		match ctx.recv().await? {
			FromOverseer::Signal(OverseerSignal::Conclude) => {
				return Ok(())
			}
			FromOverseer::Signal(OverseerSignal::ActiveLeaves(update)) => {
				handle_new_activations(
					ctx,
					&mut overlay_db,
					&mut state,
					update.activated.into_iter().map(|a| a.hash),
				).await?;
				if !state.recovery_state.complete() {
					handle_startup(
						ctx,
						&mut overlay_db,
						&mut state,
					).await?;
				}
			}
			FromOverseer::Signal(OverseerSignal::BlockFinalized(_, _)) => {},
			FromOverseer::Communication { msg } => {
				handle_incoming(
					ctx,
					&mut overlay_db,
					&mut state,
					msg,
					clock.now(),
				).await?
			}
		}

		if !overlay_db.is_empty() {
			let ops = overlay_db.into_write_ops();
			backend.write(ops)?;
		}
	}
}

// Restores the subsystem's state before proceeding with the main event loop. Primarily, this
// repopulates the rolling session window the relevant session information to handle incoming
// import statement requests.
//
// This method also retransmits a `DisputeParticiationMessage::Participate` for any non-concluded
// disputes for which the subsystem doesn't have a local statement, ensuring it eventually makes an
// arbitration on the dispute.
async fn handle_startup<Context>(
	ctx: &mut Context,
	overlay_db: &mut OverlayedBackend<'_, impl Backend>,
	state: &mut State,
) -> Result<(), Error>
where
	Context: overseer::SubsystemContext<Message = DisputeCoordinatorMessage>,
	Context: SubsystemContext<Message = DisputeCoordinatorMessage>,
{
	let recent_disputes = match overlay_db.load_recent_disputes() {
		Ok(Some(disputes)) => disputes,
		Ok(None) => return Ok(()),
		Err(e) => {
			tracing::error!(target: LOG_TARGET, "Failed initial load of recent disputes: {:?}", e);
			return Err(e.into());
		},
	};

	// Filter out disputes that have already concluded.
	let active_disputes = recent_disputes.into_iter()
		.filter(|(_, status)| *status == DisputeStatus::Active)
		.collect::<RecentDisputes>();

	for ((session, ref candidate_hash), _) in active_disputes.into_iter() {
		let votes: CandidateVotes = match overlay_db.load_candidate_votes(session, candidate_hash) {
			Ok(Some(votes)) => votes.into(),
			Ok(None) => continue,
			Err(e) => {
				tracing::error!(target: LOG_TARGET, "Failed initial load of candidate votes: {:?}", e);
				continue
			},
		};

		let validators = match state.rolling_session_window.session_info(session) {
			None => {
				tracing::warn!(
					target: LOG_TARGET,
					session,
					"Missing info for session which has an active dispute",
				);
				continue
			}
			Some(info) => info.validators.clone(),
		};

		let n_validators = validators.len();
		let voted_indices: HashSet<_> = votes.voted_indices().into_iter().collect();

		// Determine if there are any missing local statements for this dispute. Validators are
		// filtered if:
		//  1) their statement already exists, or
		//  2) the validator key is not in the local keystore (i.e. the validator is remote).
		// The remaining set only contains local validators that are also missing statements.
		let missing_local_statement = validators.iter()
			.enumerate()
			.map(|(index, validator)| (ValidatorIndex(index as _), validator))
			.any(|(index, validator)|
				!voted_indices.contains(&index) &&
				state.keystore
					.key_pair::<ValidatorPair>(validator)
					.ok()
					.map_or(false, |v| v.is_some())
			);

		// Send a `DisputeParticipationMessage` for all non-concluded disputes which do not have a
		// recorded local statement.
		if missing_local_statement {
			let (report_availability, receive_availability) = oneshot::channel();
			ctx.send_message(DisputeParticipationMessage::Participate {
				candidate_hash: *candidate_hash,
				candidate_receipt: votes.candidate_receipt.clone(),
				session,
				n_validators: n_validators as u32,
				report_availability,
			}).await;

			if !receive_availability.await? {
				tracing::debug!(target: LOG_TARGET, "Participation failed. Candidate not available");
			}
		}
	}

	Ok(())
}

async fn handle_new_activations(
	ctx: &mut (impl SubsystemContext<Message = DisputeCoordinatorMessage> + overseer::SubsystemContext<Message = DisputeCoordinatorMessage>),
	overlay_db: &mut OverlayedBackend<'_, impl Backend>,
	state: &mut State,
	new_activations: impl IntoIterator<Item = Hash>,
) -> Result<(), Error> {
	for new_leaf in new_activations {
		let block_header = {
			let (tx, rx) = oneshot::channel();

			ctx.send_message(
				ChainApiMessage::BlockHeader(new_leaf, tx)
			).await;

			match rx.await?? {
				None => continue,
				Some(header) => header,
			}
		};

		match state.rolling_session_window.cache_session_info_for_head(
			ctx,
			new_leaf,
			&block_header,
		).await {
			Err(e) => {
				tracing::warn!(
					target: LOG_TARGET,
					err = ?e,
					"Failed to update session cache for disputes",
				);

				continue
			}
			Ok(SessionWindowUpdate::Initialized { window_end, .. })
				| Ok(SessionWindowUpdate::Advanced { new_window_end: window_end, .. })
			=> {
				let session = window_end;
				if state.highest_session.map_or(true, |s| s < session) {
					tracing::trace!(
						target: LOG_TARGET,
						session,
						"Observed new session. Pruning",
					);

					state.highest_session = Some(session);

					db::v1::note_current_session(overlay_db, session)?;
				}
			}
			_ => {}
		}
	}

	Ok(())
}

async fn handle_incoming(
	ctx: &mut impl SubsystemContext,
	overlay_db: &mut OverlayedBackend<'_, impl Backend>,
	state: &mut State,
	message: DisputeCoordinatorMessage,
	now: Timestamp,
) -> Result<(), Error> {
	match message {
		DisputeCoordinatorMessage::ImportStatements {
			candidate_hash,
			candidate_receipt,
			session,
			statements,
			pending_confirmation,
		} => {
			handle_import_statements(
				ctx,
				overlay_db,
				state,
				candidate_hash,
				candidate_receipt,
				session,
				statements,
				now,
				pending_confirmation,
			).await?;
		}
		DisputeCoordinatorMessage::RecentDisputes(rx) => {
			let recent_disputes = overlay_db.load_recent_disputes()?.unwrap_or_default();
			let _ = rx.send(recent_disputes.keys().cloned().collect());
		}
		DisputeCoordinatorMessage::ActiveDisputes(rx) => {
			let recent_disputes = overlay_db.load_recent_disputes()?.unwrap_or_default();
			let _ = rx.send(collect_active(recent_disputes, now));
		}
		DisputeCoordinatorMessage::QueryCandidateVotes(
			query,
			rx
		) => {
			let mut query_output = Vec::new();
			for (session_index, candidate_hash) in query.into_iter() {
				if let Some(v) = overlay_db.load_candidate_votes(
					session_index,
					&candidate_hash,
				)? {
					query_output.push((session_index, candidate_hash, v.into()));
				} else {
					tracing::debug!(
						target: LOG_TARGET,
						session_index,
						"No votes found for candidate",
					);
				}
			}
			let _ = rx.send(query_output);
		}
		DisputeCoordinatorMessage::IssueLocalStatement(
			session,
			candidate_hash,
			candidate_receipt,
			valid,
		) => {
			issue_local_statement(
				ctx,
				overlay_db,
				state,
				candidate_hash,
				candidate_receipt,
				session,
				valid,
				now,
			).await?;
		}
		DisputeCoordinatorMessage::DetermineUndisputedChain {
			base_number,
			block_descriptions,
			tx,
		} => {
			let undisputed_chain = determine_undisputed_chain(
				overlay_db,
				base_number,
				block_descriptions
			)?;

			let _ = tx.send(undisputed_chain);
		}
	}

	Ok(())
}

fn collect_active(recent_disputes: RecentDisputes, now: Timestamp) -> Vec<(SessionIndex, CandidateHash)> {
	recent_disputes.iter().filter_map(|(disputed, status)|
		status.concluded_at().filter(|at| at + ACTIVE_DURATION_SECS < now).map_or(
			Some(*disputed),
			|_| None,
		)
	).collect()
}

fn insert_into_statement_vec<T>(
	vec: &mut Vec<(T, ValidatorIndex, ValidatorSignature)>,
	tag: T,
	val_index: ValidatorIndex,
	val_signature: ValidatorSignature,
) {
	let pos = match vec.binary_search_by_key(&val_index, |x| x.1) {
		Ok(_) => return, // no duplicates needed.
		Err(p) => p,
	};

	vec.insert(pos, (tag, val_index, val_signature));
}

async fn handle_import_statements(
	ctx: &mut impl SubsystemContext,
	overlay_db: &mut OverlayedBackend<'_, impl Backend>,
	state: &mut State,
	candidate_hash: CandidateHash,
	candidate_receipt: CandidateReceipt,
	session: SessionIndex,
	statements: Vec<(SignedDisputeStatement, ValidatorIndex)>,
	now: Timestamp,
	pending_confirmation: oneshot::Sender<ImportStatementsResult>,
) -> Result<(), Error> {
	if state.highest_session.map_or(true, |h| session + DISPUTE_WINDOW < h) {

		// It is not valid to participate in an ancient dispute (spam?).
		pending_confirmation.send(ImportStatementsResult::InvalidImport).map_err(|_| Error::OneshotSend)?;

		return Ok(());
	}

	let validators = match state.rolling_session_window.session_info(session) {
		None => {
			tracing::warn!(
				target: LOG_TARGET,
				session,
				"Missing info for session which has an active dispute",
			);

			pending_confirmation
				.send(ImportStatementsResult::InvalidImport)
				.map_err(|_| Error::OneshotSend)?;

			return Ok(())
		}
		Some(info) => info.validators.clone(),
	};

	let n_validators = validators.len();

	let supermajority_threshold = polkadot_primitives::v1::supermajority_threshold(n_validators);

	let mut votes = overlay_db.load_candidate_votes(session, &candidate_hash)?
		.map(CandidateVotes::from)
		.unwrap_or_else(|| CandidateVotes {
			candidate_receipt: candidate_receipt.clone(),
			valid: Vec::new(),
			invalid: Vec::new(),
		});

	// Update candidate votes.
	for (statement, val_index) in statements {
		if validators.get(val_index.0 as usize)
			.map_or(true, |v| v != statement.validator_public())
		{
			tracing::debug!(
				target: LOG_TARGET,
				?val_index,
				session,
				claimed_key = ?statement.validator_public(),
				"Validator index doesn't match claimed key",
			);

			continue
		}

		match statement.statement().clone() {
			DisputeStatement::Valid(valid_kind) => {
				insert_into_statement_vec(
					&mut votes.valid,
					valid_kind,
					val_index,
					statement.validator_signature().clone(),
				);
			}
			DisputeStatement::Invalid(invalid_kind) => {
				insert_into_statement_vec(
					&mut votes.invalid,
					invalid_kind,
					val_index,
					statement.validator_signature().clone(),
				);
			}
		}
	}

	// Check if newly disputed.
	let is_disputed = !votes.valid.is_empty() && !votes.invalid.is_empty();
	let concluded_valid = votes.valid.len() >= supermajority_threshold;
	let concluded_invalid = votes.invalid.len() >= supermajority_threshold;

	let mut recent_disputes = overlay_db.load_recent_disputes()?.unwrap_or_default();

	let prev_status = recent_disputes.get(&(session, candidate_hash)).map(|x| x.clone());

	let status = if is_disputed {
		let status = recent_disputes
			.entry((session, candidate_hash))
			.or_insert(DisputeStatus::active());

		// Note: concluded-invalid overwrites concluded-valid,
		// so we do this check first. Dispute state machine is
		// non-commutative.
		if concluded_valid {
			*status = status.concluded_for(now);
		}

		if concluded_invalid {
			*status = status.concluded_against(now);
		}

		Some(*status)
	} else {
		None
	};

	if status != prev_status {
		// This branch is only hit when the candidate is freshly disputed -
		// status was previously `None`, and now is not.
		if prev_status.is_none() {
			// No matter what, if the dispute is new, we participate.
			//
			// We also block the coordinator while awaiting our determination
			// of whether the vote is available.
			let (report_availability, receive_availability) = oneshot::channel();
			ctx.send_message(DisputeParticipationMessage::Participate {
				candidate_hash,
				candidate_receipt,
				session,
				n_validators: n_validators as u32,
				report_availability,
			}).await;

			if !receive_availability.await.map_err(Error::Oneshot)? {
				// If the data is not available, we disregard the dispute votes.
				// This is an indication that the dispute does not correspond to any included
				// candidate and that it should be ignored.
				//
				// We expect that if the candidate is truly disputed that the higher-level network
				// code will retry.
				pending_confirmation.send(ImportStatementsResult::InvalidImport)
					.map_err(|_| Error::OneshotSend)?;

				tracing::debug!(
					target: LOG_TARGET,
					"Recovering availability failed - invalid import."
				);
				return Ok(())
			}
		}

		// Only write when updated and vote is available.
		overlay_db.write_recent_disputes(recent_disputes);
	}

	overlay_db.write_candidate_votes(session, candidate_hash, votes.into());

	pending_confirmation
		.send(ImportStatementsResult::ValidImport)
		.map_err(|_| Error::OneshotSend)?;

	Ok(())
}

async fn issue_local_statement(
	ctx: &mut impl SubsystemContext,
	overlay_db: &mut OverlayedBackend<'_, impl Backend>,
	state: &mut State,
	candidate_hash: CandidateHash,
	candidate_receipt: CandidateReceipt,
	session: SessionIndex,
	valid: bool,
	now: Timestamp,
) -> Result<(), Error> {
	// Load session info.
	let info = match state.rolling_session_window.session_info(session) {
		None => {
			tracing::warn!(
				target: LOG_TARGET,
				session,
				"Missing info for session which has an active dispute",
			);

			return Ok(())
		}
		Some(info) => info,
	};

	let validators = info.validators.clone();

	let votes = overlay_db.load_candidate_votes(session, &candidate_hash)?
		.map(CandidateVotes::from)
		.unwrap_or_else(|| CandidateVotes {
			candidate_receipt: candidate_receipt.clone(),
			valid: Vec::new(),
			invalid: Vec::new(),
		});

	// Sign a statement for each validator index we control which has
	// not already voted. This should generally be maximum 1 statement.
	let voted_indices = votes.voted_indices();
	let mut statements = Vec::new();

	let voted_indices: HashSet<_> = voted_indices.into_iter().collect();
	for (index, validator) in validators.iter().enumerate() {
		let index = ValidatorIndex(index as _);
		if voted_indices.contains(&index) { continue }
		if state.keystore.key_pair::<ValidatorPair>(validator).ok().flatten().is_none() {
			continue
		}

		let keystore = state.keystore.clone() as Arc<_>;
		let res = SignedDisputeStatement::sign_explicit(
			&keystore,
			valid,
			candidate_hash,
			session,
			validator.clone(),
		).await;

		match res {
			Ok(Some(signed_dispute_statement)) => {
				statements.push((signed_dispute_statement, index));
			}
			Ok(None) => {}
			Err(e) => {
				tracing::error!(
					target: LOG_TARGET,
					err = ?e,
					"Encountered keystore error while signing dispute statement",
				);
			}
		}
	}

	// Get our message out:
	for (statement, index) in &statements {
		let dispute_message = match make_dispute_message(info, &votes, statement.clone(), *index) {
			Err(err) => {
				tracing::debug!(
					target: LOG_TARGET,
					?err,
					"Creating dispute message failed."
				);
				continue
			}
			Ok(dispute_message) => dispute_message,
		};

		ctx.send_message(DisputeDistributionMessage::SendDispute(dispute_message)).await;
	}


	// Do import
	if !statements.is_empty() {
		let (pending_confirmation, _rx) = oneshot::channel();
		handle_import_statements(
			ctx,
			overlay_db,
			state,
			candidate_hash,
			candidate_receipt,
			session,
			statements,
			now,
			pending_confirmation,
		).await?;
	}

	Ok(())
}

#[derive(Debug, thiserror::Error)]
enum MakeDisputeMessageError {
	#[error("There was no opposite vote available")]
	NoOppositeVote,
	#[error("Found vote had an invalid validator index that could not be found")]
	InvalidValidatorIndex,
	#[error("Statement found in votes had invalid signature.")]
	InvalidStoredStatement,
	#[error(transparent)]
	InvalidStatementCombination(DisputeMessageCheckError),
}

fn make_dispute_message(
	info: &SessionInfo,
	votes: &CandidateVotes,
	our_vote: SignedDisputeStatement,
	our_index: ValidatorIndex
) -> Result<DisputeMessage, MakeDisputeMessageError> {

	let validators = &info.validators;

	let (valid_statement, valid_index, invalid_statement, invalid_index) =
		if let DisputeStatement::Valid(_) = our_vote.statement() {
			let (statement_kind, validator_index, validator_signature)
				= votes.invalid.get(0).ok_or(MakeDisputeMessageError::NoOppositeVote)?.clone();
			let other_vote = SignedDisputeStatement::new_checked(
				DisputeStatement::Invalid(statement_kind),
				our_vote.candidate_hash().clone(),
				our_vote.session_index(),
				validators.get(validator_index.0 as usize).ok_or(MakeDisputeMessageError::InvalidValidatorIndex)?.clone(),
				validator_signature,
			).map_err(|()| MakeDisputeMessageError::InvalidStoredStatement)?;
			(our_vote, our_index, other_vote, validator_index)
	} else {
		let (statement_kind, validator_index, validator_signature)
			= votes.valid.get(0).ok_or(MakeDisputeMessageError::NoOppositeVote)?.clone();
		let other_vote = SignedDisputeStatement::new_checked(
			DisputeStatement::Valid(statement_kind),
			our_vote.candidate_hash().clone(),
			our_vote.session_index(),
			validators.get(validator_index.0 as usize).ok_or(MakeDisputeMessageError::InvalidValidatorIndex)?.clone(),
			validator_signature,
		).map_err(|()| MakeDisputeMessageError::InvalidStoredStatement)?;
		(other_vote, validator_index, our_vote, our_index)
	};

	DisputeMessage::from_signed_statements(
		valid_statement, valid_index,
		invalid_statement, invalid_index,
		votes.candidate_receipt.clone(),
		info,
	).map_err(MakeDisputeMessageError::InvalidStatementCombination)
}

fn determine_undisputed_chain(
	overlay_db: &mut OverlayedBackend<'_, impl Backend>,
	base_number: BlockNumber,
	block_descriptions: Vec<(Hash, SessionIndex, Vec<CandidateHash>)>,
) -> Result<Option<(BlockNumber, Hash)>, Error> {
	let last = block_descriptions.last()

		.map(|e| (base_number + block_descriptions.len() as BlockNumber, e.0));

	// Fast path for no disputes.
	let recent_disputes = match overlay_db.load_recent_disputes()? {
		None => return Ok(last),
		Some(a) if a.is_empty() => return Ok(last),
		Some(a) => a,
	};

	let is_possibly_invalid = |session, candidate_hash| {
		recent_disputes.get(&(session, candidate_hash)).map_or(
			false,
			|status| status.is_possibly_invalid(),
		)
	};

	for (i, (_, session, candidates)) in block_descriptions.iter().enumerate() {
		if candidates.iter().any(|c| is_possibly_invalid(*session, *c)) {
			if i == 0 {
				return Ok(None);
			} else {
				return Ok(Some((
					base_number + i as BlockNumber,
					block_descriptions[i - 1].0,
				)));
			}
		}
	}

	Ok(last)
}
