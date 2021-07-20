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


use super::*;
use polkadot_primitives::v1::{BlakeTwo256, HashT, ValidatorId, Header, SessionInfo};
use polkadot_node_subsystem::{jaeger, ActiveLeavesUpdate, ActivatedLeaf, LeafStatus};
use polkadot_node_subsystem::messages::{
	AllMessages, ChainApiMessage, RuntimeApiMessage, RuntimeApiRequest,
	BlockDescription,
};
use polkadot_node_subsystem_test_helpers::{make_subsystem_context, TestSubsystemContextHandle};
use sp_core::testing::TaskExecutor;
use sp_keyring::Sr25519Keyring;
use sp_keystore::{SyncCryptoStore, SyncCryptoStorePtr};
use futures::{
	channel::oneshot,
	future::{self, BoxFuture},
};
use parity_scale_codec::Encode;
use assert_matches::assert_matches;

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

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

#[derive(Clone)]
struct MockClock {
	time: Arc<AtomicU64>,
}

impl Default for MockClock {
	fn default() -> Self {
		MockClock { time: Arc::new(AtomicU64::default()) }
	}
}

impl Clock for MockClock {
	fn now(&self) -> Timestamp {
		self.time.load(AtomicOrdering::SeqCst)
	}
}

impl MockClock {
	fn set(&self, to: Timestamp) {
		self.time.store(to, AtomicOrdering::SeqCst)
	}
}

struct TestState {
	validators: Vec<Sr25519Keyring>,
	validator_public: Vec<ValidatorId>,
	validator_groups: Vec<Vec<ValidatorIndex>>,
	master_keystore: Arc<sc_keystore::LocalKeystore>,
	subsystem_keystore: Arc<sc_keystore::LocalKeystore>,
	db: Arc<dyn KeyValueDB>,
	config: Config,
	clock: MockClock,
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
			clock: MockClock::default(),
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
				status: LeafStatus::Fresh,
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

	let backend = DbBackend::new(state.db.clone(), state.config.column_config());
	let subsystem_task = run(subsystem, ctx, backend, Box::new(state.clock.clone()));
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

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements: vec![
					(valid_vote, ValidatorIndex(0)),
					(invalid_vote, ValidatorIndex(1)),
				],
				pending_confirmation,
			},
		}).await;
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::DisputeParticipation(DisputeParticipationMessage::Participate {
				candidate_hash: c_hash,
				candidate_receipt: c_receipt,
				session: s,
				n_validators,
				report_availability,
			}) => {
				assert_eq!(c_hash, candidate_hash);
				assert_eq!(c_receipt, candidate_receipt);
				assert_eq!(s, session);
				assert_eq!(n_validators, test_state.validators.len() as u32);
				report_availability.send(true).unwrap();
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
					vec![(session, candidate_hash)],
					tx,
				),
			}).await;

			let (_, _, votes) = rx.await.unwrap().get(0).unwrap().clone();
			assert_eq!(votes.valid.len(), 1);
			assert_eq!(votes.invalid.len(), 1);
		}

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements: vec![
					(invalid_vote_2, ValidatorIndex(2)),
				],
				pending_confirmation,
			},
		}).await;

		{
			let (tx, rx) = oneshot::channel();
			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::QueryCandidateVotes(
					vec![(session, candidate_hash)],
					tx,
				),
			}).await;

			let (_, _, votes) = rx.await.unwrap().get(0).unwrap().clone();
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

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements: vec![
					(valid_vote, ValidatorIndex(0)),
				],
				pending_confirmation,
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
					vec![(session, candidate_hash)],
					tx,
				),
			}).await;

			let (_, _, votes) = rx.await.unwrap().get(0).unwrap().clone();
			assert_eq!(votes.valid.len(), 1);
			assert!(votes.invalid.is_empty());
		}

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements: vec![
					(valid_vote_2, ValidatorIndex(1)),
				],
				pending_confirmation,
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
					vec![(session, candidate_hash)],
					tx,
				),
			}).await;

			let (_, _, votes) = rx.await.unwrap().get(0).unwrap().clone();
			assert_eq!(votes.valid.len(), 2);
			assert!(votes.invalid.is_empty());
		}

		virtual_overseer.send(FromOverseer::Signal(OverseerSignal::Conclude)).await;

		// This confirms that no participation request is made.
		assert!(virtual_overseer.try_recv().await.is_none());
	}));
}

#[test]
fn wrong_validator_index_is_ignored() {
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

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements: vec![
					(valid_vote, ValidatorIndex(1)),
					(invalid_vote, ValidatorIndex(0)),
				],
				pending_confirmation,
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
					vec![(session, candidate_hash)],
					tx,
				),
			}).await;

			let (_, _, votes) = rx.await.unwrap().get(0).unwrap().clone();
			assert!(votes.valid.is_empty());
			assert!(votes.invalid.is_empty());
		}

		virtual_overseer.send(FromOverseer::Signal(OverseerSignal::Conclude)).await;

		// This confirms that no participation request is made.
		assert!(virtual_overseer.try_recv().await.is_none());
	}));
}

#[test]
fn finality_votes_ignore_disputed_candidates() {
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

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements: vec![
					(valid_vote, ValidatorIndex(0)),
					(invalid_vote, ValidatorIndex(1)),
				],
				pending_confirmation,
			},
		}).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::DisputeParticipation(
				DisputeParticipationMessage::Participate {
					report_availability,
					..
				}
			) => {
				report_availability.send(true).unwrap();
			}
		);

		{
			let (tx, rx) = oneshot::channel();

			let block_hash_a = Hash::repeat_byte(0x0a);
			let block_hash_b = Hash::repeat_byte(0x0b);

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::DetermineUndisputedChain{
					base_number: 10,
					block_descriptions: vec![
						BlockDescription { block_hash: block_hash_a, session, candidates: vec![candidate_hash] },
					],
					tx,
				},
			}).await;

			assert!(rx.await.unwrap().is_none());

			let (tx, rx) = oneshot::channel();
			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::DetermineUndisputedChain {
					base_number: 10,
					block_descriptions: vec![
						BlockDescription { block_hash: block_hash_a, session, candidates: vec![] },
						BlockDescription { block_hash: block_hash_b, session, candidates: vec![candidate_hash] },
					],
					tx,
				},
			}).await;

			assert_eq!(rx.await.unwrap(), Some((11, block_hash_a)));
		}

		virtual_overseer.send(FromOverseer::Signal(OverseerSignal::Conclude)).await;
		assert!(virtual_overseer.try_recv().await.is_none());
	}));
}

#[test]
fn supermajority_valid_dispute_may_be_finalized() {
	test_harness(|test_state, mut virtual_overseer| Box::pin(async move {
		let session = 1;

		let candidate_receipt = CandidateReceipt::default();
		let candidate_hash = candidate_receipt.hash();

		test_state.activate_leaf_at_session(
			&mut virtual_overseer,
			session,
			1,
		).await;

		let supermajority_threshold = polkadot_primitives::v1::supermajority_threshold(
			test_state.validators.len()
		);

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

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements: vec![
					(valid_vote, ValidatorIndex(0)),
					(invalid_vote, ValidatorIndex(1)),
				],
				pending_confirmation,
			},
		}).await;

		let _ = virtual_overseer.recv().await;

		let mut statements = Vec::new();
		for i in (0..supermajority_threshold - 1).map(|i| i + 2) {
			let vote = test_state.issue_statement_with_index(
				i,
				candidate_hash,
				session,
				true,
			).await;

			statements.push((vote, ValidatorIndex(i as _)));
		};

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements,
				pending_confirmation,
			},
		}).await;

		{
			let (tx, rx) = oneshot::channel();

			let block_hash_a = Hash::repeat_byte(0x0a);
			let block_hash_b = Hash::repeat_byte(0x0b);

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::DetermineUndisputedChain {
					base_number: 10,
					block_descriptions: vec![
						BlockDescription { block_hash: block_hash_a, session, candidates: vec![candidate_hash] },
					],
					tx,
				},
			}).await;

			assert_eq!(rx.await.unwrap(), Some((11, block_hash_a)));

			let (tx, rx) = oneshot::channel();
			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::DetermineUndisputedChain {
					base_number: 10,
					block_descriptions: vec![
						BlockDescription { block_hash: block_hash_a, session, candidates: vec![] },
						BlockDescription { block_hash: block_hash_b, session, candidates: vec![candidate_hash] },
					],
					tx,
				},
			}).await;

			assert_eq!(rx.await.unwrap(), Some((12, block_hash_b)));
		}

		virtual_overseer.send(FromOverseer::Signal(OverseerSignal::Conclude)).await;
		assert!(virtual_overseer.try_recv().await.is_none());
	}));
}

#[test]
fn concluded_supermajority_for_non_active_after_time() {
	test_harness(|test_state, mut virtual_overseer| Box::pin(async move {
		let session = 1;

		let candidate_receipt = CandidateReceipt::default();
		let candidate_hash = candidate_receipt.hash();

		test_state.activate_leaf_at_session(
			&mut virtual_overseer,
			session,
			1,
		).await;

		let supermajority_threshold = polkadot_primitives::v1::supermajority_threshold(
			test_state.validators.len()
		);

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

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements: vec![
					(valid_vote, ValidatorIndex(0)),
					(invalid_vote, ValidatorIndex(1)),
				],
				pending_confirmation,
			},
		}).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::DisputeParticipation(
				DisputeParticipationMessage::Participate {
					report_availability,
					..
				}
			) => {
				report_availability.send(true).unwrap();
			}
		);

		let mut statements = Vec::new();
		for i in (0..supermajority_threshold - 1).map(|i| i + 2) {
			let vote = test_state.issue_statement_with_index(
				i,
				candidate_hash,
				session,
				true,
			).await;

			statements.push((vote, ValidatorIndex(i as _)));
		};

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements,
				pending_confirmation,
			},
		}).await;

		test_state.clock.set(ACTIVE_DURATION_SECS + 1);

		{
			let (tx, rx) = oneshot::channel();

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::ActiveDisputes(tx),
			}).await;

			assert!(rx.await.unwrap().is_empty());

			let (tx, rx) = oneshot::channel();

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::RecentDisputes(tx),
			}).await;

			assert_eq!(rx.await.unwrap().len(), 1);
		}

		virtual_overseer.send(FromOverseer::Signal(OverseerSignal::Conclude)).await;
		assert!(virtual_overseer.try_recv().await.is_none());
	}));
}

#[test]
fn concluded_supermajority_against_non_active_after_time() {
	test_harness(|test_state, mut virtual_overseer| Box::pin(async move {
		let session = 1;

		let candidate_receipt = CandidateReceipt::default();
		let candidate_hash = candidate_receipt.hash();

		test_state.activate_leaf_at_session(
			&mut virtual_overseer,
			session,
			1,
		).await;

		let supermajority_threshold = polkadot_primitives::v1::supermajority_threshold(
			test_state.validators.len()
		);

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

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements: vec![
					(valid_vote, ValidatorIndex(0)),
					(invalid_vote, ValidatorIndex(1)),
				],
				pending_confirmation,
			},
		}).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::DisputeParticipation(
				DisputeParticipationMessage::Participate {
					report_availability,
					..
				}
			) => {
				report_availability.send(true).unwrap();
			}
		);

		let mut statements = Vec::new();
		for i in (0..supermajority_threshold - 1).map(|i| i + 2) {
			let vote = test_state.issue_statement_with_index(
				i,
				candidate_hash,
				session,
				false,
			).await;

			statements.push((vote, ValidatorIndex(i as _)));
		};

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements,
				pending_confirmation,
			},
		}).await;

		test_state.clock.set(ACTIVE_DURATION_SECS + 1);

		{
			let (tx, rx) = oneshot::channel();

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::ActiveDisputes(tx),
			}).await;

			assert!(rx.await.unwrap().is_empty());

			let (tx, rx) = oneshot::channel();

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::RecentDisputes(tx),
			}).await;

			assert_eq!(rx.await.unwrap().len(), 1);
		}

		virtual_overseer.send(FromOverseer::Signal(OverseerSignal::Conclude)).await;
		assert!(virtual_overseer.try_recv().await.is_none());
	}));
}

#[test]
fn fresh_dispute_ignored_if_unavailable() {
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

		let (pending_confirmation, _confirmation_rx) = oneshot::channel();
		virtual_overseer.send(FromOverseer::Communication {
			msg: DisputeCoordinatorMessage::ImportStatements {
				candidate_hash,
				candidate_receipt: candidate_receipt.clone(),
				session,
				statements: vec![
					(valid_vote, ValidatorIndex(0)),
					(invalid_vote, ValidatorIndex(1)),
				],
				pending_confirmation,
			},
		}).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::DisputeParticipation(
				DisputeParticipationMessage::Participate {
					report_availability,
					..
				}
			) => {
				report_availability.send(false).unwrap();
			}
		);

		{
			let (tx, rx) = oneshot::channel();

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::ActiveDisputes(tx),
			}).await;

			assert!(rx.await.unwrap().is_empty());

			let (tx, rx) = oneshot::channel();

			virtual_overseer.send(FromOverseer::Communication {
				msg: DisputeCoordinatorMessage::RecentDisputes(tx),
			}).await;

			assert!(rx.await.unwrap().is_empty());
		}

		virtual_overseer.send(FromOverseer::Signal(OverseerSignal::Conclude)).await;
		assert!(virtual_overseer.try_recv().await.is_none());
	}));
}
