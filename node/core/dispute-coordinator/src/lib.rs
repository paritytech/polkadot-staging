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

use polkadot_node_primitives::{CandidateVotes, SignedDisputeStatement};
use polkadot_node_subsystem::{
	messages::{
		DisputeCoordinatorMessage, ChainApiMessage, DisputeParticipationMessage,
	},
	Subsystem, SubsystemContext, FromOverseer, OverseerSignal, SpawnedSubsystem,
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
use parity_scale_codec::Error as CodecError;
use sc_keystore::LocalKeystore;

use db::v1::ActiveDisputes;

mod db;

const LOG_TARGET: &str = "parachain::dispute-coordinator";

// It would be nice to draw this from the chain state, but we have no tools for it right now.
// On Polkadot this is 1 day, and on Kusama it's 6 hours.
const DISPUTE_WINDOW: SessionIndex = 6;

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

impl<Context> Subsystem<Context> for DisputeCoordinatorSubsystem
	where Context: SubsystemContext<Message = DisputeCoordinatorMessage>
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = run(self, ctx)
			.map(|_| Ok(()))
			.boxed();

		SpawnedSubsystem {
			name: "dispute-coordinator-subsystem",
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

async fn run<Context>(subsystem: DisputeCoordinatorSubsystem, mut ctx: Context)
	where Context: SubsystemContext<Message = DisputeCoordinatorMessage>
{
	loop {
		let res = run_iteration(&mut ctx, &subsystem).await;
		match res {
			Err(e) => {
				e.trace();

				if let Error::Subsystem(SubsystemError::Context(_)) = e {
					break;
				}
			}
			Ok(true) => {
				tracing::info!(target: LOG_TARGET, "received `Conclude` signal, exiting");
				break;
			}
			Ok(false) => continue,
		}
	}
}

// Run the subsystem until an error is encountered or a `conclude` signal is received.
// Most errors are non-fatal and should lead to another call to this function.
//
// A return value of `true` indicates that an exit should be made, while a return value of
// `false` indicates that another iteration should be performed.
async fn run_iteration<Context>(ctx: &mut Context, subsystem: &DisputeCoordinatorSubsystem)
	-> Result<bool, Error>
	where Context: SubsystemContext<Message = DisputeCoordinatorMessage>
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
				return Ok(true)
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
				).await?
			}
		}
	}
}

async fn handle_new_activations(
	ctx: &mut impl SubsystemContext,
	store: &dyn KeyValueDB,
	state: &mut State,
	config: &Config,
	new_activations: impl IntoIterator<Item = Hash>,
) -> Result<(), Error> {
	for new_leaf in new_activations {
		let block_header = {
			let (tx, rx) = oneshot::channel();

			ctx.send_message(
				ChainApiMessage::BlockHeader(new_leaf, tx).into()
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

		// TODO [after https://github.com/paritytech/polkadot/issues/3160]: chain rollbacks
	}

	Ok(())
}

async fn handle_incoming(
	ctx: &mut impl SubsystemContext,
	store: &dyn KeyValueDB,
	state: &mut State,
	config: &Config,
	message: DisputeCoordinatorMessage,
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
			).await?;
		}
		DisputeCoordinatorMessage::ActiveDisputes(rx) => {
			let active_disputes = db::v1::load_active_disputes(store, &config.column_config())?
				.map(|d| d.disputed)
				.unwrap_or_default();

			let _ = rx.send(active_disputes);
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
			).await?;
		}
		DisputeCoordinatorMessage::DetermineUndisputedChain {
			base_number,
			block_descriptions,
			rx,
		} => {
			let undisputed_chain = determine_undisputed_chain(
				store,
				&config,
				base_number,
				block_descriptions
			)?;

			let _ = rx.send(undisputed_chain);
		}
	}

	Ok(())
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
) -> Result<(), Error> {
	if state.highest_session.map_or(true, |h| session + DISPUTE_WINDOW < h) {
		return Ok(());
	}

	let n_validators = match state.rolling_session_window.session_info(session) {
		None => {
			tracing::warn!(
				target: LOG_TARGET,
				session,
				"Missing info for session which has an active dispute",
			);

			return Ok(())
		}
		Some(info) => info.validators.len(),
	};

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

	let was_undisputed = votes.valid.len() == 0 || votes.invalid.len() == 0;

	// Update candidate votes.
	for (statement, val_index) in statements {
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
	let freshly_disputed = is_disputed && was_undisputed;
	let already_disputed = is_disputed && !was_undisputed;
	let concluded_valid = votes.valid.len() >= supermajority_threshold;

	let mut tx = db::v1::Transaction::default();

	if freshly_disputed && !concluded_valid {
		// add to active disputes and begin local participation.
		update_active_disputes(
			store,
			config,
			&mut tx,
			|active| active.insert(session, candidate_hash),
		)?;

		let voted_indices = votes.voted_indices();

		ctx.send_message(DisputeParticipationMessage::Participate {
			candidate_hash,
			candidate_receipt,
			session,
			voted_indices,
		}.into()).await;
	}

	if concluded_valid && already_disputed {
		// remove from active disputes.
		update_active_disputes(
			store,
			config,
			&mut tx,
			|active| active.delete(session, candidate_hash),
		)?;
	}

	tx.put_candidate_votes(session, candidate_hash, votes.into());
	tx.write(store, &config.column_config())?;

	Ok(())
}

fn update_active_disputes(
	store: &dyn KeyValueDB,
	config: &Config,
	tx: &mut db::v1::Transaction,
	with_active: impl FnOnce(&mut ActiveDisputes) -> bool,
) -> Result<(), Error> {
	let mut active_disputes = db::v1::load_active_disputes(store, &config.column_config())?
		.unwrap_or_default();

	if with_active(&mut active_disputes) {
		tx.put_active_disputes(active_disputes);
	}

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
	let active_disputes = match db::v1::load_active_disputes(store, &config.column_config())? {
		None => return Ok(last),
		Some(a) if a.disputed.is_empty() => return Ok(last),
		Some(a) => a,
	};

	for (i, (_, session, candidates)) in block_descriptions.iter().enumerate() {
		if candidates.iter().any(|c| active_disputes.contains(*session, *c)) {
			if i == 0 {
				return Ok(None);
			} else {
				return Ok(Some((
					base_number + i as BlockNumber,
					block_descriptions[base_number as usize + i].0,
				)));
			}
		}
	}

	Ok(last)
}

#[cfg(test)]
mod tests {
	use super::*;
	use polkadot_primitives::v1::{BlakeTwo256, HashT, ValidatorId, Header, SessionInfo};
	use polkadot_node_subsystem::{jaeger, ActiveLeavesUpdate, ActivatedLeaf};
	use polkadot_node_subsystem::messages::{
		AllMessages, ChainApiMessage, RuntimeApiMessage, RuntimeApiRequest,
	};
	use polkadot_node_subsystem_test_helpers::{make_subsystem_context, TestSubsystemContextHandle};
	use sp_core::testing::TaskExecutor;
	use sp_keyring::Sr25519Keyring;
	use sp_keystore::{SyncCryptoStore, SyncCryptoStorePtr};
	use futures::future::{self, BoxFuture};
	use parity_scale_codec::Encode;
	use assert_matches::assert_matches;

	// sets up a keystore with the given keyring accounts.
	fn make_keystore(accounts: &[Sr25519Keyring]) -> LocalKeystore {
		let store = LocalKeystore::in_memory();

		for s in accounts.iter().copied().map(|k| k.to_seed()) {
			store.sr25519_generate_new(
				polkadot_primitives::v1::PARACHAIN_KEY_TYPE_ID,
				Some(s.as_str()),
			).unwrap();
		}

		store
	}

	fn session_to_hash(session: SessionIndex, extra: impl Encode) -> Hash {
		BlakeTwo256::hash_of(&(session, extra))
	}

	type VirtualOverseer = TestSubsystemContextHandle<DisputeCoordinatorMessage>;

	struct TestState {
		validators: Vec<Sr25519Keyring>,
		validator_public: Vec<ValidatorId>,
		validator_groups: Vec<Vec<ValidatorIndex>>,
		master_keystore: Arc<sc_keystore::LocalKeystore>,
		subsystem_keystore: Arc<sc_keystore::LocalKeystore>,
		db: Arc<dyn KeyValueDB>,
		config: Config,
	}

	impl Default for TestState {
		fn default() -> TestState {
			let validators = vec![
				Sr25519Keyring::Alice,
				Sr25519Keyring::Bob,
				Sr25519Keyring::Charlie,
				Sr25519Keyring::Dave,
				Sr25519Keyring::Eve,
				Sr25519Keyring::One,
			];

			let validator_public = validators.iter()
				.map(|k| ValidatorId::from(k.public()))
				.collect();

			let validator_groups = vec![
				vec![ValidatorIndex(0), ValidatorIndex(1)],
				vec![ValidatorIndex(2), ValidatorIndex(3)],
				vec![ValidatorIndex(4), ValidatorIndex(5)],
			];

			let master_keystore = make_keystore(&validators).into();
			let subsystem_keystore = make_keystore(&[Sr25519Keyring::Alice]).into();

			let db = Arc::new(kvdb_memorydb::create(1));
			let config = Config {
				col_data: 0,
			};

			TestState {
				validators,
				validator_public,
				validator_groups,
				master_keystore,
				subsystem_keystore,
				db,
				config,
			}
		}
	}

	impl TestState {
		async fn activate_leaf_at_session(
			&self,
			virtual_overseer: &mut VirtualOverseer,
			session: SessionIndex,
			block_number: BlockNumber,
		) {
			assert!(block_number > 0);

			let parent_hash = session_to_hash(session, b"parent");
			let block_header = Header {
				parent_hash,
				number: block_number,
				digest: Default::default(),
				state_root: Default::default(),
				extrinsics_root: Default::default(),
			};
			let block_hash = block_header.hash();

			virtual_overseer.send(FromOverseer::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::start_work(ActivatedLeaf {
					hash: block_hash,
					span: Arc::new(jaeger::Span::Disabled),
					number: block_number,
				})
			))).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::ChainApi(ChainApiMessage::BlockHeader(h, tx)) => {
					assert_eq!(h, block_hash);
					let _ = tx.send(Ok(Some(block_header)));
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					h,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(h, parent_hash);
					let _ = tx.send(Ok(session));
				}
			);

			loop {
				// answer session info queries until the current session is reached.
				assert_matches!(
					virtual_overseer.recv().await,
					AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						h,
						RuntimeApiRequest::SessionInfo(session_index, tx),
					)) => {
						assert_eq!(h, block_hash);

						let _ = tx.send(Ok(Some(self.session_info())));
						if session_index == session { break }
					}
				)
			}
		}

		fn session_info(&self) -> SessionInfo {
			let discovery_keys = self.validators.iter()
				.map(|k| <_>::from(k.public()))
				.collect();

			let assignment_keys = self.validators.iter()
				.map(|k| <_>::from(k.public()))
				.collect();

			SessionInfo {
				validators: self.validator_public.clone(),
				discovery_keys,
				assignment_keys,
				validator_groups: self.validator_groups.clone(),
				n_cores: self.validator_groups.len() as _,
				zeroth_delay_tranche_width: 0,
				relay_vrf_modulo_samples: 1,
				n_delay_tranches: 100,
				no_show_slots: 1,
				needed_approvals: 10,
			}
		}

		async fn issue_statement_with_index(
			&self,
			index: usize,
			candidate_hash: CandidateHash,
			session: SessionIndex,
			valid: bool,
		) -> SignedDisputeStatement {
			let public = self.validator_public[index].clone();

			let keystore = self.master_keystore.clone() as SyncCryptoStorePtr;

			SignedDisputeStatement::sign_explicit(
				&keystore,
				valid,
				candidate_hash,
				session,
				public,
			).await.unwrap().unwrap()
		}
	}

	fn test_harness<F>(test: F)
		where F: FnOnce(TestState, VirtualOverseer) -> BoxFuture<'static, ()>
	{
		let (ctx, ctx_handle) = make_subsystem_context(TaskExecutor::new());

		let state = TestState::default();
		let subsystem = DisputeCoordinatorSubsystem::new(
			state.db.clone(),
			state.config.clone(),
			state.subsystem_keystore.clone(),
		);

		let subsystem_task = run(subsystem, ctx);
		let test_task = test(state, ctx_handle);

		futures::executor::block_on(future::join(subsystem_task, test_task));
	}

	#[test]
	fn conflicting_votes_lead_to_dispute_participation() {
		test_harness(|test_state, mut virtual_overseer| Box::pin(async move {
			let session = 1;

			let candidate_receipt = CandidateReceipt::default();
			let candidate_hash = candidate_receipt.hash();

			test_state.activate_leaf_at_session(
				&mut virtual_overseer,
				session,
				1,
			).await;

			let valid_vote = test_state.issue_statement_with_index(
				0,
				candidate_hash,
				session,
				true,
			).await;

			let invalid_vote = test_state.issue_statement_with_index(
				1,
				candidate_hash,
				session,
				false,
			).await;

			let invalid_vote_2 = test_state.issue_statement_with_index(
				2,
				candidate_hash,
				session,
				false,
			).await;

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::ImportStatements {
					candidate_hash,
					candidate_receipt: candidate_receipt.clone(),
					session,
					statements: vec![
						(valid_vote, ValidatorIndex(0)),
						(invalid_vote, ValidatorIndex(1)),
					],
				},
			}).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::DisputeParticipation(DisputeParticipationMessage::Participate {
					candidate_hash: c_hash,
					candidate_receipt: c_receipt,
					session: s,
					voted_indices,
				}) => {
					assert_eq!(c_hash, candidate_hash);
					assert_eq!(c_receipt, candidate_receipt);
					assert_eq!(s, session);
					assert_eq!(voted_indices, vec![ValidatorIndex(0), ValidatorIndex(1)]);
				}
			);

			{
				let (tx, rx) = oneshot::channel();
				virtual_overseer.send(FromOverseer::Communication {
					msg: DisputeCoordinatorMessage::ActiveDisputes(tx),
				}).await;

				assert_eq!(rx.await.unwrap(), vec![(session, candidate_hash)]);

				let (tx, rx) = oneshot::channel();
				virtual_overseer.send(FromOverseer::Communication {
					msg: DisputeCoordinatorMessage::QueryCandidateVotes(
						session,
						candidate_hash,
						tx,
					),
				}).await;

				let votes = rx.await.unwrap().unwrap();
				assert_eq!(votes.valid.len(), 1);
				assert_eq!(votes.invalid.len(), 1);
			}

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::ImportStatements {
					candidate_hash,
					candidate_receipt: candidate_receipt.clone(),
					session,
					statements: vec![
						(invalid_vote_2, ValidatorIndex(2)),
					],
				},
			}).await;

			{
				let (tx, rx) = oneshot::channel();
				virtual_overseer.send(FromOverseer::Communication {
					msg: DisputeCoordinatorMessage::QueryCandidateVotes(
						session,
						candidate_hash,
						tx,
					),
				}).await;

				let votes = rx.await.unwrap().unwrap();
				assert_eq!(votes.valid.len(), 1);
				assert_eq!(votes.invalid.len(), 2);
			}

			virtual_overseer.send(FromOverseer::Signal(OverseerSignal::Conclude)).await;

			// This confirms that the second vote doesn't lead to participation again.
			assert!(virtual_overseer.try_recv().await.is_none());
		}));
	}

	#[test]
	fn positive_votes_dont_trigger_participation() {
		test_harness(|test_state, mut virtual_overseer| Box::pin(async move {
			let session = 1;

			let candidate_receipt = CandidateReceipt::default();
			let candidate_hash = candidate_receipt.hash();

			test_state.activate_leaf_at_session(
				&mut virtual_overseer,
				session,
				1,
			).await;

			let valid_vote = test_state.issue_statement_with_index(
				0,
				candidate_hash,
				session,
				true,
			).await;

			let valid_vote_2 = test_state.issue_statement_with_index(
				1,
				candidate_hash,
				session,
				true,
			).await;

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::ImportStatements {
					candidate_hash,
					candidate_receipt: candidate_receipt.clone(),
					session,
					statements: vec![
						(valid_vote, ValidatorIndex(0)),
					],
				},
			}).await;

			{
				let (tx, rx) = oneshot::channel();
				virtual_overseer.send(FromOverseer::Communication {
					msg: DisputeCoordinatorMessage::ActiveDisputes(tx),
				}).await;

				assert!(rx.await.unwrap().is_empty());

				let (tx, rx) = oneshot::channel();
				virtual_overseer.send(FromOverseer::Communication {
					msg: DisputeCoordinatorMessage::QueryCandidateVotes(
						session,
						candidate_hash,
						tx,
					),
				}).await;

				let votes = rx.await.unwrap().unwrap();
				assert_eq!(votes.valid.len(), 1);
				assert!(votes.invalid.is_empty());
			}

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::ImportStatements {
					candidate_hash,
					candidate_receipt: candidate_receipt.clone(),
					session,
					statements: vec![
						(valid_vote_2, ValidatorIndex(1)),
					],
				},
			}).await;

			{
				let (tx, rx) = oneshot::channel();
				virtual_overseer.send(FromOverseer::Communication {
					msg: DisputeCoordinatorMessage::ActiveDisputes(tx),
				}).await;

				assert!(rx.await.unwrap().is_empty());

				let (tx, rx) = oneshot::channel();
				virtual_overseer.send(FromOverseer::Communication {
					msg: DisputeCoordinatorMessage::QueryCandidateVotes(
						session,
						candidate_hash,
						tx,
					),
				}).await;

				let votes = rx.await.unwrap().unwrap();
				assert_eq!(votes.valid.len(), 2);
				assert!(votes.invalid.is_empty());
			}

			virtual_overseer.send(FromOverseer::Signal(OverseerSignal::Conclude)).await;

			// This confirms that no participation request is made.
			assert!(virtual_overseer.try_recv().await.is_none());
		}));
	}

	#[test]
	fn finality_votes_ignore_disputed_candidates() {

	}

	#[test]
	fn supermajority_valid_dispute_may_be_finalized() {

	}
}
