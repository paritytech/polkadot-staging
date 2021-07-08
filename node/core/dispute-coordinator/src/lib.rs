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

use polkadot_node_primitives::{CandidateVotes, SignedDisputeStatement};
use polkadot_node_subsystem::{
	overseer,
	messages::{
		DisputeCoordinatorMessage, ChainApiMessage, DisputeParticipationMessage,
	},
	SubsystemContext, FromOverseer, OverseerSignal, SpawnedSubsystem,
	SubsystemError,
	errors::{ChainApiError, RuntimeApiError},
};
use polkadot_node_subsystem_util::rolling_session_window::{
	RollingSessionWindow, SessionWindowUpdate,
};
use polkadot_primitives::v1::{
	SessionIndex, CandidateHash, Hash, CandidateReceipt, DisputeStatement, ValidatorIndex,
	ValidatorSignature, BlockNumber, ValidatorPair,
};

use futures::prelude::*;
use futures::channel::oneshot;
use kvdb::KeyValueDB;
use parity_scale_codec::{Encode, Decode, Error as CodecError};
use sc_keystore::LocalKeystore;

use db::v1::RecentDisputes;

mod db;

#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "parachain::dispute-coordinator";

// It would be nice to draw this from the chain state, but we have no tools for it right now.
// On Polkadot this is 1 day, and on Kusama it's 6 hours.
const DISPUTE_WINDOW: SessionIndex = 6;

// The choice here is fairly arbitrary. But any dispute that concluded more than a few minutes ago
// is not worth considering anymore. Changing this value has little to no bearing on consensus,
// and really only affects the work that the node might do on startup during periods of many disputes.
const ACTIVE_DURATION_SECS: Timestamp = 180;

/// Timestamp based on the 1 Jan 1970 UNIX base, which is persistent across node restarts and OS reboots.
type Timestamp = u64;

struct State {
	keystore: Arc<LocalKeystore>,
	highest_session: Option<SessionIndex>,
	rolling_session_window: RollingSessionWindow,
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
		let future = run(self, ctx, Box::new(SystemClock))
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
	/// The dispute has been concluded agains the candidate
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

async fn run<Context>(
	subsystem: DisputeCoordinatorSubsystem,
	mut ctx: Context,
	clock: Box<dyn Clock>,
)
where
	Context: overseer::SubsystemContext<Message = DisputeCoordinatorMessage>,
	Context: SubsystemContext<Message = DisputeCoordinatorMessage>
{
	loop {
		let res = run_iteration(&mut ctx, &subsystem, &*clock).await;
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
async fn run_iteration<Context>(
	ctx: &mut Context,
	subsystem: &DisputeCoordinatorSubsystem,
	clock: &dyn Clock,
)
	-> Result<(), Error>
where
	Context: overseer::SubsystemContext<Message = DisputeCoordinatorMessage>,
	Context: SubsystemContext<Message = DisputeCoordinatorMessage>
{
	let DisputeCoordinatorSubsystem { ref store, ref keystore, ref config } = *subsystem;
	let mut state = State {
		keystore: keystore.clone(),
		highest_session: None,
		rolling_session_window: RollingSessionWindow::new(DISPUTE_WINDOW),
	};

	loop {
		match ctx.recv().await? {
			FromOverseer::Signal(OverseerSignal::Conclude) => {
				return Ok(())
			}
			FromOverseer::Signal(OverseerSignal::ActiveLeaves(update)) => {
				handle_new_activations(
					ctx,
					&**store,
					&mut state,
					config,
					update.activated.into_iter().map(|a| a.hash),
				).await?
			}
			FromOverseer::Signal(OverseerSignal::BlockFinalized(_, _)) => {},
			FromOverseer::Communication { msg } => {
				handle_incoming(
					ctx,
					&**store,
					&mut state,
					config,
					msg,
					clock.now(),
				).await?
			}
		}
	}
}

async fn handle_new_activations(
	ctx: &mut (impl SubsystemContext<Message = DisputeCoordinatorMessage> + overseer::SubsystemContext<Message = DisputeCoordinatorMessage>),
	store: &dyn KeyValueDB,
	state: &mut State,
	config: &Config,
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

					db::v1::note_current_session(
						store,
						&config.column_config(),
						session,
					)?;
				}
			}
			_ => {}
		}
	}

	Ok(())
}

async fn handle_incoming(
	ctx: &mut impl SubsystemContext,
	store: &dyn KeyValueDB,
	state: &mut State,
	config: &Config,
	message: DisputeCoordinatorMessage,
	now: Timestamp,
) -> Result<(), Error> {
	match message {
		DisputeCoordinatorMessage::ImportStatements {
			candidate_hash,
			candidate_receipt,
			session,
			statements,
		} => {
			handle_import_statements(
				ctx,
				store,
				state,
				config,
				candidate_hash,
				candidate_receipt,
				session,
				statements,
				now,
			).await?;
		}
		DisputeCoordinatorMessage::RecentDisputes(rx) => {
			let recent_disputes = db::v1::load_recent_disputes(store, &config.column_config())?
				.unwrap_or_default();

			let _ = rx.send(recent_disputes.keys().cloned().collect());
		}
		DisputeCoordinatorMessage::ActiveDisputes(rx) => {
			let recent_disputes = db::v1::load_recent_disputes(store, &config.column_config())?
				.unwrap_or_default();

			let _ = rx.send(collect_active(recent_disputes, now));
		}
		DisputeCoordinatorMessage::QueryCandidateVotes(
			session,
			candidate_hash,
			rx
		) => {
			let candidate_votes = db::v1::load_candidate_votes(
				store,
				&config.column_config(),
				session,
				&candidate_hash,
			)?;

			let _ = rx.send(candidate_votes.map(Into::into));
		}
		DisputeCoordinatorMessage::IssueLocalStatement(
			session,
			candidate_hash,
			candidate_receipt,
			valid,
		) => {
			issue_local_statement(
				ctx,
				state,
				store,
				config,
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
				store,
				&config,
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
	store: &dyn KeyValueDB,
	state: &mut State,
	config: &Config,
	candidate_hash: CandidateHash,
	candidate_receipt: CandidateReceipt,
	session: SessionIndex,
	statements: Vec<(SignedDisputeStatement, ValidatorIndex)>,
	now: Timestamp,
) -> Result<(), Error> {
	if state.highest_session.map_or(true, |h| session + DISPUTE_WINDOW < h) {
		return Ok(());
	}

	let validators = match state.rolling_session_window.session_info(session) {
		None => {
			tracing::warn!(
				target: LOG_TARGET,
				session,
				"Missing info for session which has an active dispute",
			);

			return Ok(())
		}
		Some(info) => info.validators.clone(),
	};

	let n_validators = validators.len();

	let supermajority_threshold = polkadot_primitives::v1::supermajority_threshold(n_validators);

	let mut votes = db::v1::load_candidate_votes(
		store,
		&config.column_config(),
		session,
		&candidate_hash
	)?
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

	let mut recent_disputes = db::v1::load_recent_disputes(store, &config.column_config())?
		.unwrap_or_default();

	let mut tx = db::v1::Transaction::default();

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
		// Only write when updated.
		tx.put_recent_disputes(recent_disputes);

		// This branch is only hit when the candidate is freshly disputed -
		// status was previously `None`, and now is not.
		if prev_status.is_none() {
			// No matter what, if the dispute is new, we participate.
			ctx.send_message(DisputeParticipationMessage::Participate {
				candidate_hash,
				candidate_receipt,
				session,
				n_validators: n_validators as u32,
			}).await;
		}
	}

	tx.put_candidate_votes(session, candidate_hash, votes.into());
	tx.write(store, &config.column_config())?;

	Ok(())
}

async fn issue_local_statement(
	ctx: &mut impl SubsystemContext,
	state: &mut State,
	store: &dyn KeyValueDB,
	config: &Config,
	candidate_hash: CandidateHash,
	candidate_receipt: CandidateReceipt,
	session: SessionIndex,
	valid: bool,
	now: Timestamp,
) -> Result<(), Error> {
	// Load session info.
	let validators = match state.rolling_session_window.session_info(session) {
		None => {
			tracing::warn!(
				target: LOG_TARGET,
				session,
				"Missing info for session which has an active dispute",
			);

			return Ok(())
		}
		Some(info) => info.validators.clone(),
	};

	let votes = db::v1::load_candidate_votes(
		store,
		&config.column_config(),
		session,
		&candidate_hash
	)?
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

	// Do import
	if !statements.is_empty() {
		handle_import_statements(
			ctx,
			store,
			state,
			config,
			candidate_hash,
			candidate_receipt,
			session,
			statements,
			now,
		).await?;
	}

	Ok(())
}

fn determine_undisputed_chain(
	store: &dyn KeyValueDB,
	config: &Config,
	base_number: BlockNumber,
	block_descriptions: Vec<(Hash, SessionIndex, Vec<CandidateHash>)>,
) -> Result<Option<(BlockNumber, Hash)>, Error> {
	let last = block_descriptions.last()
		.map(|e| (base_number + block_descriptions.len() as BlockNumber, e.0));

	// Fast path for no disputes.
	let recent_disputes = match db::v1::load_recent_disputes(store, &config.column_config())? {
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
