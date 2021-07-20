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
use super::approval_db::v1::Config;
use super::backend::{Backend, BackendWriteOp};
use polkadot_primitives::v1::{
	CandidateDescriptor, CoreIndex, GroupIndex, ValidatorSignature,
	DisputeStatement, ValidDisputeStatementKind,
};
use polkadot_node_primitives::approval::{
	AssignmentCert, AssignmentCertKind, VRFOutput, VRFProof,
	RELAY_VRF_MODULO_CONTEXT, DelayTranche,
};
use polkadot_node_subsystem_test_helpers::make_subsystem_context;
use polkadot_node_subsystem::messages::{AllMessages, HighestApprovedAncestorBlock};
use sp_core::testing::TaskExecutor;

use parking_lot::Mutex;
use bitvec::order::Lsb0 as BitOrderLsb0;
use std::pin::Pin;
use std::sync::Arc;
use sp_keyring::sr25519::Keyring as Sr25519Keyring;
use assert_matches::assert_matches;

const SLOT_DURATION_MILLIS: u64 = 5000;

const DATA_COL: u32 = 0;
const NUM_COLUMNS: u32 = 1;

const TEST_CONFIG: Config = Config {
	col_data: DATA_COL,
};

fn make_db() -> DbBackend {
	let db_writer: Arc<dyn KeyValueDB> = Arc::new(kvdb_memorydb::create(NUM_COLUMNS));
	DbBackend::new(db_writer.clone(), TEST_CONFIG)
}

fn overlay_txn<T, F>(db: &mut T, mut f: F)
	where
		T: Backend,
		F: FnMut(&mut OverlayedBackend<'_, T>)
{
	let mut overlay_db = OverlayedBackend::new(db);
	f(&mut overlay_db);
	let write_ops = overlay_db.into_write_ops();
	db.write(write_ops).unwrap();
}

fn slot_to_tick(t: impl Into<Slot>) -> crate::time::Tick {
	crate::time::slot_number_to_tick(SLOT_DURATION_MILLIS, t.into())
}

#[derive(Default, Clone)]
struct MockClock {
	inner: Arc<Mutex<MockClockInner>>,
}

impl MockClock {
	fn new(tick: Tick) -> Self {
		let me = Self::default();
		me.inner.lock().set_tick(tick);
		me
	}
}

impl Clock for MockClock {
	fn tick_now(&self) -> Tick {
		self.inner.lock().tick
	}

	fn wait(&self, tick: Tick) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
		let rx = self.inner.lock().register_wakeup(tick, true);

		Box::pin(async move {
			rx.await.expect("i exist in a timeless void. yet, i remain");
		})
	}
}

// This mock clock allows us to manipulate the time and
// be notified when wakeups have been triggered.
#[derive(Default)]
struct MockClockInner {
	tick: Tick,
	wakeups: Vec<(Tick, oneshot::Sender<()>)>,
}

impl MockClockInner {
	fn set_tick(&mut self, tick: Tick) {
		self.tick = tick;
		self.wakeup_all(tick);
	}

	fn wakeup_all(&mut self, up_to: Tick) {
		// This finds the position of the first wakeup after
		// the given tick, or the end of the map.
		let drain_up_to = self.wakeups.binary_search_by_key(
			&(up_to + 1),
			|w| w.0,
		).unwrap_or_else(|i| i);

		for (_, wakeup) in self.wakeups.drain(..drain_up_to) {
			let _ = wakeup.send(());
		}
	}

	// If `pre_emptive` is true, we compare the given tick to the internal
	// tick of the clock for an early return.
	//
	// Otherwise, the wakeup will only trigger alongside another wakeup of
	// equal or greater tick.
	//
	// When the pre-emptive wakeup is disabled, this can be used in combination with
	// a preceding call to `set_tick` to wait until some other wakeup at that same tick
	//  has been triggered.
	fn register_wakeup(&mut self, tick: Tick, pre_emptive: bool) -> oneshot::Receiver<()> {
		let (tx, rx) = oneshot::channel();

		let pos = self.wakeups.binary_search_by_key(
			&tick,
			|w| w.0,
		).unwrap_or_else(|i| i);

		self.wakeups.insert(pos, (tick, tx));

		if pre_emptive {
			// if `tick > self.tick`, this won't wake up the new
			// listener.
			self.wakeup_all(self.tick);
		}

		rx
	}
}

struct MockAssignmentCriteria<Compute, Check>(Compute, Check);

impl<Compute, Check> AssignmentCriteria for MockAssignmentCriteria<Compute, Check>
where
	Compute: Fn() -> HashMap<polkadot_primitives::v1::CoreIndex, criteria::OurAssignment>,
	Check: Fn() -> Result<DelayTranche, criteria::InvalidAssignment>
{
	fn compute_assignments(
		&self,
		_keystore: &LocalKeystore,
		_relay_vrf_story: polkadot_node_primitives::approval::RelayVRFStory,
		_config: &criteria::Config,
		_leaving_cores: Vec<(CandidateHash, polkadot_primitives::v1::CoreIndex, polkadot_primitives::v1::GroupIndex)>,
	) -> HashMap<polkadot_primitives::v1::CoreIndex, criteria::OurAssignment> {
		self.0()
	}

	fn check_assignment_cert(
		&self,
		_claimed_core_index: polkadot_primitives::v1::CoreIndex,
		_validator_index: ValidatorIndex,
		_config: &criteria::Config,
		_relay_vrf_story: polkadot_node_primitives::approval::RelayVRFStory,
		_assignment: &polkadot_node_primitives::approval::AssignmentCert,
		_backing_group: polkadot_primitives::v1::GroupIndex,
	) -> Result<polkadot_node_primitives::approval::DelayTranche, criteria::InvalidAssignment> {
		self.1()
	}
}

impl<F> MockAssignmentCriteria<
	fn() -> HashMap<polkadot_primitives::v1::CoreIndex, criteria::OurAssignment>,
	F,
> {
	fn check_only(f: F) -> Self {
		MockAssignmentCriteria(Default::default, f)
	}
}

fn blank_state() -> State {
	State {
		session_window: RollingSessionWindow::new(APPROVAL_SESSIONS),
		keystore: Arc::new(LocalKeystore::in_memory()),
		slot_duration_millis: SLOT_DURATION_MILLIS,
		clock: Box::new(MockClock::default()),
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| { Ok(0) })),
	}
}

fn single_session_state(index: SessionIndex, info: SessionInfo) -> State {
	State {
		session_window: RollingSessionWindow::with_session_info(
			APPROVAL_SESSIONS,
			index,
			vec![info],
		),
		..blank_state()
	}
}

fn garbage_assignment_cert(kind: AssignmentCertKind) -> AssignmentCert {
	let ctx = schnorrkel::signing_context(RELAY_VRF_MODULO_CONTEXT);
	let msg = b"test-garbage";
	let mut prng = rand_core::OsRng;
	let keypair = schnorrkel::Keypair::generate_with(&mut prng);
	let (inout, proof, _) = keypair.vrf_sign(ctx.bytes(msg));
	let out = inout.to_output();

	AssignmentCert {
		kind,
		vrf: (VRFOutput(out), VRFProof(proof)),
	}
}

fn sign_approval(
	key: Sr25519Keyring,
	candidate_hash: CandidateHash,
	session_index: SessionIndex,
) -> ValidatorSignature {
	key.sign(&ApprovalVote(candidate_hash).signing_payload(session_index)).into()
}

#[derive(Clone)]
struct StateConfig {
	session_index: SessionIndex,
	slot: Slot,
	tick: Tick,
	validators: Vec<Sr25519Keyring>,
	validator_groups: Vec<Vec<ValidatorIndex>>,
	needed_approvals: u32,
	no_show_slots: u32,
	candidate_hash: Option<CandidateHash>,
}

impl Default for StateConfig {
	fn default() -> Self {
		StateConfig {
			session_index: 1,
			slot: Slot::from(0),
			tick: 0,
			validators: vec![Sr25519Keyring::Alice, Sr25519Keyring::Bob],
			validator_groups: vec![vec![ValidatorIndex(0)], vec![ValidatorIndex(1)]],
			needed_approvals: 1,
			no_show_slots: 2,
			candidate_hash: None,
		}
	}
}

// one block with one candidate. Alice and Bob are in the assignment keys.
fn some_state(config: StateConfig, db: &mut DbBackend) -> State {
	let StateConfig {
		session_index,
		slot,
		tick,
		validators,
		validator_groups,
		needed_approvals,
		no_show_slots,
		candidate_hash,
	} = config;

	let n_validators = validators.len();

	let state = State {
		clock: Box::new(MockClock::new(tick)),
		..single_session_state(session_index, SessionInfo {
			validators: validators.iter().map(|v| v.public().into()).collect(),
			discovery_keys: validators.iter().map(|v| v.public().into()).collect(),
			assignment_keys: validators.iter().map(|v| v.public().into()).collect(),
			validator_groups: validator_groups.clone(),
			n_cores: validator_groups.len() as _,
			zeroth_delay_tranche_width: 5,
			relay_vrf_modulo_samples: 3,
			n_delay_tranches: 50,
			no_show_slots,
			needed_approvals,
			..Default::default()
		})
	};
	let core_index = 0.into();

	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = candidate_hash.unwrap_or_else(|| CandidateHash(Hash::repeat_byte(0xCC)));

	add_block(
		db,
		block_hash,
		session_index,
		slot,
	);

	add_candidate_to_block(
		db,
		block_hash,
		candidate_hash,
		n_validators,
		core_index,
		GroupIndex(0),
		None,
	);

	state
}

fn add_block(
	db: &mut DbBackend,
	block_hash: Hash,
	session: SessionIndex,
	slot: Slot,
) {
	overlay_txn(db, |overlay_db| overlay_db.write_block_entry(approval_db::v1::BlockEntry {
		block_hash,
		parent_hash: Default::default(),
		block_number: 0,
		session,
		slot,
		candidates: Vec::new(),
		relay_vrf_story: Default::default(),
		approved_bitfield: Default::default(),
		children: Default::default(),
	}.into()));
}

fn add_candidate_to_block(
	db: &mut DbBackend,
	block_hash: Hash,
	candidate_hash: CandidateHash,
	n_validators: usize,
	core: CoreIndex,
	backing_group: GroupIndex,
	candidate_receipt: Option<CandidateReceipt>,
) {
	let mut block_entry = db.load_block_entry(&block_hash).unwrap().unwrap();

	let mut candidate_entry = db.load_candidate_entry(&candidate_hash).unwrap()
		.unwrap_or_else(|| approval_db::v1::CandidateEntry {
			session: block_entry.session(),
			block_assignments: Default::default(),
			candidate: candidate_receipt.unwrap_or_default(),
			approvals: bitvec::bitvec![BitOrderLsb0, u8; 0; n_validators],
		}.into());

	block_entry.add_candidate(core, candidate_hash);

	candidate_entry.add_approval_entry(
		block_hash,
		approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			backing_group,
			our_assignment: None,
			our_approval_sig: None,
			assignments: bitvec::bitvec![BitOrderLsb0, u8; 0; n_validators],
			approved: false,
		}.into(),
	);

	overlay_txn(db, |overlay_db| {
		overlay_db.write_block_entry(block_entry.clone());
		overlay_db.write_candidate_entry(candidate_entry.clone());
	})
}

fn import_assignment<F>(
	db: &mut DbBackend,
	candidate_hash: &CandidateHash,
	block_hash: &Hash,
	validator_index: ValidatorIndex,
	mut f: F,
)
	where F: FnMut(&mut CandidateEntry)
{
	let mut candidate_entry = db.load_candidate_entry(candidate_hash).unwrap().unwrap();
	candidate_entry.approval_entry_mut(block_hash).unwrap()
		.import_assignment(0, validator_index, 0);

	f(&mut candidate_entry);

	overlay_txn(db, |overlay_db| overlay_db.write_candidate_entry(candidate_entry.clone()));
}

fn set_our_assignment<F>(
	db: &mut DbBackend,
	candidate_hash: &CandidateHash,
	block_hash: &Hash,
	tranche: DelayTranche,
	mut f: F,
)
	where F: FnMut(&mut CandidateEntry)
{
	let mut candidate_entry = db.load_candidate_entry(&candidate_hash).unwrap().unwrap();
	let approval_entry = candidate_entry.approval_entry_mut(&block_hash).unwrap();

	approval_entry.set_our_assignment(approval_db::v1::OurAssignment {
		cert: garbage_assignment_cert(
			AssignmentCertKind::RelayVRFModulo { sample: 0 }
		),
		tranche,
		validator_index: ValidatorIndex(0),
		triggered: false,
	}.into());

	f(&mut candidate_entry);

	overlay_txn(db, |overlay_db| overlay_db.write_candidate_entry(candidate_entry.clone()));
}

#[test]
fn rejects_bad_assignment() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();
	let assignment_good = IndirectAssignmentCert {
		block_hash,
		validator: ValidatorIndex(0),
		cert: garbage_assignment_cert(
			AssignmentCertKind::RelayVRFModulo {
				sample: 0,
			},
		),
	};
	let mut state = some_state(StateConfig {
		candidate_hash: Some(candidate_hash),
		..Default::default()
	}, &mut db);
	let candidate_index = 0;

	let mut overlay_db = OverlayedBackend::new(&db);
	let res = check_and_import_assignment(
		&mut state,
		&mut overlay_db,
		assignment_good.clone(),
		candidate_index,
	).unwrap();
	assert_eq!(res.0, AssignmentCheckResult::Accepted);
	// Check that the assignment's been imported.
	assert_eq!(res.1.len(), 1);

	let write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();
	assert_eq!(write_ops.len(), 1);
	assert_matches!(write_ops.get(0).unwrap(), BackendWriteOp::WriteCandidateEntry(..));
	db.write(write_ops).unwrap();

	// unknown hash
	let unknown_hash = Hash::repeat_byte(0x02);
	let assignment = IndirectAssignmentCert {
		block_hash: unknown_hash,
		validator: ValidatorIndex(0),
		cert: garbage_assignment_cert(
			AssignmentCertKind::RelayVRFModulo {
				sample: 0,
			},
		),
	};

	let mut overlay_db = OverlayedBackend::new(&db);
	let res = check_and_import_assignment(
		&mut state,
		&mut overlay_db,
		assignment,
		candidate_index,
	).unwrap();
	assert_eq!(res.0, AssignmentCheckResult::Bad(AssignmentCheckError::UnknownBlock(unknown_hash)));
	assert_eq!(overlay_db.into_write_ops().count(), 0);

	let mut state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Err(criteria::InvalidAssignment)
		})),
		..some_state(StateConfig {
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	// same assignment, but this time rejected
	let mut overlay_db = OverlayedBackend::new(&db);
	let res = check_and_import_assignment(
		&mut state,
		&mut overlay_db,
		assignment_good,
		candidate_index,
	).unwrap();
	assert_eq!(res.0, AssignmentCheckResult::Bad(AssignmentCheckError::InvalidCert(ValidatorIndex(0))));
	assert_eq!(overlay_db.into_write_ops().count(), 0);
}

#[test]
fn rejects_assignment_in_future() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_index = 0;
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();
	let assignment = IndirectAssignmentCert {
		block_hash,
		validator: ValidatorIndex(0),
		cert: garbage_assignment_cert(
			AssignmentCertKind::RelayVRFModulo {
				sample: 0,
			},
		),
	};

	let tick = 9;
	let mut state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(move || {
			Ok((tick + 20) as _)
		})),
		..some_state(StateConfig {
			tick,
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	let mut overlay_db = OverlayedBackend::new(&db);
	let res = check_and_import_assignment(
		&mut state,
		&mut overlay_db,
		assignment.clone(),
		candidate_index,
	).unwrap();
	assert_eq!(res.0, AssignmentCheckResult::TooFarInFuture);

	let write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();

	assert_eq!(write_ops.len(), 0);
	db.write(write_ops).unwrap();

	let mut state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(move || {
			Ok((tick + 20 - 1) as _)
		})),
		..some_state(StateConfig {
			tick,
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	let mut overlay_db = OverlayedBackend::new(&db);
	let res = check_and_import_assignment(
		&mut state,
		&mut overlay_db,
		assignment.clone(),
		candidate_index,
	).unwrap();
	assert_eq!(res.0, AssignmentCheckResult::Accepted);

	let write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();
	assert_eq!(write_ops.len(), 1);

	assert_matches!(
		write_ops.get(0).unwrap(),
		BackendWriteOp::WriteCandidateEntry(..)
	);
}

#[test]
fn rejects_assignment_with_unknown_candidate() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_index = 1;
	let assignment = IndirectAssignmentCert {
		block_hash,
		validator: ValidatorIndex(0),
		cert: garbage_assignment_cert(
			AssignmentCertKind::RelayVRFModulo {
				sample: 0,
			},
		),
	};

	let mut state = some_state(Default::default(), &mut db);

	let mut overlay_db = OverlayedBackend::new(&db);
	let res = check_and_import_assignment(
		&mut state,
		&mut overlay_db,
		assignment.clone(),
		candidate_index,
	).unwrap();
	assert_eq!(res.0, AssignmentCheckResult::Bad(AssignmentCheckError::InvalidCandidateIndex(candidate_index)));
	assert_eq!(overlay_db.into_write_ops().count(), 0);
}

#[test]
fn assignment_import_updates_candidate_entry_and_schedules_wakeup() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();

	let candidate_index = 0;
	let assignment = IndirectAssignmentCert {
		block_hash,
		validator: ValidatorIndex(0),
		cert: garbage_assignment_cert(
			AssignmentCertKind::RelayVRFModulo {
				sample: 0,
			},
		),
	};

	let mut state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Ok(0)
		})),
		..some_state(StateConfig {
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	let mut overlay_db = OverlayedBackend::new(&db);
	let (res, actions) = check_and_import_assignment(
		&mut state,
		&mut overlay_db,
		assignment.clone(),
		candidate_index,
	).unwrap();

	assert_eq!(res, AssignmentCheckResult::Accepted);
	assert_eq!(actions.len(), 1);

	assert_matches!(
		actions.get(0).unwrap(),
		Action::ScheduleWakeup {
			block_hash: b,
			candidate_hash: c,
			tick,
			..
		} => {
			assert_eq!(b, &block_hash);
			assert_eq!(c, &candidate_hash);
			assert_eq!(tick, &slot_to_tick(0 + 2)); // current tick + no-show-duration.
		}
	);

	let write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();
	assert_eq!(1, write_ops.len());
	assert_matches!(
		write_ops.get(0).unwrap(),
		BackendWriteOp::WriteCandidateEntry(ref c_entry) => {
			assert_eq!(&c_entry.candidate.hash(), &candidate_hash);
			assert!(c_entry.approval_entry(&block_hash).unwrap().is_assigned(ValidatorIndex(0)));
		}
	);
}

#[test]
fn rejects_approval_before_assignment() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();

	let state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Ok(0)
		})),
		..some_state(StateConfig {
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	let vote = IndirectSignedApprovalVote {
		block_hash,
		candidate_index: 0,
		validator: ValidatorIndex(0),
		signature: sign_approval(Sr25519Keyring::Alice, candidate_hash, 1),
	};

	let mut overlay_db = OverlayedBackend::new(&db);
	let (actions, res) = check_and_import_approval(
		&state,
		&mut overlay_db,
		&Metrics(None),
		vote,
		|r| r
	).unwrap();

	assert_eq!(res, ApprovalCheckResult::Bad(ApprovalCheckError::NoAssignment(ValidatorIndex(0))));
	assert!(actions.is_empty());
	assert_eq!(overlay_db.into_write_ops().count(), 0);
}

#[test]
fn rejects_approval_if_no_candidate_entry() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();

	let state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Ok(0)
		})),
		..some_state(StateConfig {
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	let vote = IndirectSignedApprovalVote {
		block_hash,
		candidate_index: 0,
		validator: ValidatorIndex(0),
		signature: sign_approval(Sr25519Keyring::Alice, candidate_hash, 1),
	};


	overlay_txn(&mut db, |overlay_db| overlay_db.delete_candidate_entry(&candidate_hash));

	let mut overlay_db = OverlayedBackend::new(&db);
	let (actions, res) = check_and_import_approval(
		&state,
		&mut overlay_db,
		&Metrics(None),
		vote,
		|r| r
	).unwrap();

	assert_eq!(res, ApprovalCheckResult::Bad(ApprovalCheckError::InvalidCandidate(0, candidate_hash)));
	assert!(actions.is_empty());
	assert_eq!(overlay_db.into_write_ops().count(), 0);
}

#[test]
fn rejects_approval_if_no_block_entry() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();
	let validator_index = ValidatorIndex(0);

	let state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Ok(0)
		})),
		..some_state(StateConfig {
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	let vote = IndirectSignedApprovalVote {
		block_hash,
		candidate_index: 0,
		validator: ValidatorIndex(0),
		signature: sign_approval(Sr25519Keyring::Alice, candidate_hash, 1),
	};

	import_assignment(&mut db, &candidate_hash, &block_hash, validator_index, |_| {});

	overlay_txn(&mut db, |overlay_db| overlay_db.delete_block_entry(&block_hash));

	let mut overlay_db = OverlayedBackend::new(&db);
	let (actions, res) = check_and_import_approval(
		&state,
		&mut overlay_db,
		&Metrics(None),
		vote,
		|r| r
	).unwrap();

	assert_eq!(res, ApprovalCheckResult::Bad(ApprovalCheckError::UnknownBlock(block_hash)));
	assert!(actions.is_empty());
	assert_eq!(overlay_db.into_write_ops().count(), 0);
}

#[test]
fn accepts_and_imports_approval_after_assignment() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();
	let validator_index = ValidatorIndex(0);

	let candidate_index = 0;
	let state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Ok(0)
		})),
		..some_state(StateConfig {
			validators: vec![Sr25519Keyring::Alice, Sr25519Keyring::Bob, Sr25519Keyring::Charlie],
			validator_groups: vec![vec![ValidatorIndex(0), ValidatorIndex(1)], vec![ValidatorIndex(2)]],
			needed_approvals: 2,
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	let vote = IndirectSignedApprovalVote {
		block_hash,
		candidate_index,
		validator: validator_index,
		signature: sign_approval(Sr25519Keyring::Alice, candidate_hash, 1),
	};

	import_assignment(&mut db, &candidate_hash, &block_hash, validator_index, |_| {});

	let mut overlay_db = OverlayedBackend::new(&db);
	let (actions, res) = check_and_import_approval(
		&state,
		&mut overlay_db,
		&Metrics(None),
		vote,
		|r| r
	).unwrap();

	assert_eq!(res, ApprovalCheckResult::Accepted);

	assert_eq!(actions.len(), 1);

	assert_matches!(
		&actions[0],
		Action::InformDisputeCoordinator {
			dispute_statement,
			candidate_hash: c_hash,
			validator_index: v,
			..
		} => {
			assert_matches!(
				dispute_statement.statement(),
				&DisputeStatement::Valid(ValidDisputeStatementKind::ApprovalChecking)
			);

			assert_eq!(c_hash, &candidate_hash);
			assert_eq!(v, &validator_index);
		}
	);

	let write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();
	assert_eq!(write_ops.len(), 1);
	assert_matches!(
		write_ops.get(0).unwrap(),
		BackendWriteOp::WriteCandidateEntry(ref c_entry) => {
			assert_eq!(&c_entry.candidate.hash(), &candidate_hash);
			assert!(!c_entry.approval_entry(&block_hash).unwrap().is_approved());
		}
	);
}

#[test]
fn second_approval_import_only_schedules_wakeups() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();
	let validator_index = ValidatorIndex(0);
	let validator_index_b = ValidatorIndex(1);

	let candidate_index = 0;
	let state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Ok(0)
		})),
		..some_state(StateConfig {
			validators: vec![Sr25519Keyring::Alice, Sr25519Keyring::Bob, Sr25519Keyring::Charlie],
			validator_groups: vec![vec![ValidatorIndex(0), ValidatorIndex(1)], vec![ValidatorIndex(2)]],
			needed_approvals: 2,
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	let vote = IndirectSignedApprovalVote {
		block_hash,
		candidate_index,
		validator: validator_index,
		signature: sign_approval(Sr25519Keyring::Alice, candidate_hash, 1),
	};

	import_assignment(&mut db, &candidate_hash, &block_hash, validator_index, |candidate_entry| {
		assert!(!candidate_entry.mark_approval(validator_index));
	});

	// There is only one assignment, so nothing to schedule if we double-import.

	let mut overlay_db = OverlayedBackend::new(&db);
	let (actions, res) = check_and_import_approval(
		&state,
		&mut overlay_db,
		&Metrics(None),
		vote.clone(),
		|r| r
	).unwrap();

	assert_eq!(res, ApprovalCheckResult::Accepted);
	assert!(actions.is_empty());
	assert_eq!(overlay_db.into_write_ops().count(), 0);

	// After adding a second assignment, there should be a schedule wakeup action.

	import_assignment(&mut db, &candidate_hash, &block_hash, validator_index_b, |_| {});

	let mut overlay_db = OverlayedBackend::new(&db);
	let (actions, res) = check_and_import_approval(
		&state,
		&mut overlay_db,
		&Metrics(None),
		vote,
		|r| r
	).unwrap();

	assert_eq!(res, ApprovalCheckResult::Accepted);
	assert_eq!(actions.len(), 1);
	assert_eq!(overlay_db.into_write_ops().count(), 0);

	assert_matches!(
		actions.get(0).unwrap(),
		Action::ScheduleWakeup { .. } => {}
	);
}

#[test]
fn import_checked_approval_updates_entries_and_schedules() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();
	let validator_index_a = ValidatorIndex(0);
	let validator_index_b = ValidatorIndex(1);

	let state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Ok(0)
		})),
		..some_state(StateConfig {
			validators: vec![Sr25519Keyring::Alice, Sr25519Keyring::Bob, Sr25519Keyring::Charlie],
			validator_groups: vec![vec![ValidatorIndex(0), ValidatorIndex(1)], vec![ValidatorIndex(2)]],
			needed_approvals: 2,
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	import_assignment(&mut db, &candidate_hash, &block_hash, validator_index_a, |_| {});
	import_assignment(&mut db, &candidate_hash, &block_hash, validator_index_b, |_| {});

	{
		let mut overlay_db = OverlayedBackend::new(&db);
		let actions = import_checked_approval(
			&state,
			&mut overlay_db,
			&Metrics(None),
			db.load_block_entry(&block_hash).unwrap().unwrap(),
			candidate_hash,
			db.load_candidate_entry(&candidate_hash).unwrap().unwrap(),
			ApprovalSource::Remote(validator_index_a),
		);

		assert_eq!(actions.len(), 1);
		assert_matches!(
			actions.get(0).unwrap(),
			Action::ScheduleWakeup {
				block_hash: b_hash,
				candidate_hash: c_hash,
				..
			} => {
				assert_eq!(b_hash, &block_hash);
				assert_eq!(c_hash, &candidate_hash);
			}
		);

		let mut write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();
		assert_eq!(write_ops.len(), 1);
		assert_matches!(
			write_ops.get_mut(0).unwrap(),
			BackendWriteOp::WriteCandidateEntry(ref mut c_entry) => {
				assert!(!c_entry.approval_entry(&block_hash).unwrap().is_approved());
				assert!(c_entry.mark_approval(validator_index_a));
			}
		);

		db.write(write_ops).unwrap();
	}

	{
		let mut overlay_db = OverlayedBackend::new(&db);
		let actions = import_checked_approval(
			&state,
			&mut overlay_db,
			&Metrics(None),
			db.load_block_entry(&block_hash).unwrap().unwrap(),
			candidate_hash,
			db.load_candidate_entry(&candidate_hash).unwrap().unwrap(),
			ApprovalSource::Remote(validator_index_b),
		);
		assert_eq!(actions.len(), 1);

		let mut write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();
		assert_eq!(write_ops.len(), 2);

		assert_matches!(
			write_ops.get(0).unwrap(),
			BackendWriteOp::WriteBlockEntry(b_entry) => {
				assert_eq!(b_entry.block_hash(), block_hash);
				assert!(b_entry.is_fully_approved());
				assert!(b_entry.is_candidate_approved(&candidate_hash));
			}
		);

		assert_matches!(
			write_ops.get_mut(1).unwrap(),
			BackendWriteOp::WriteCandidateEntry(ref mut c_entry) => {
				assert_eq!(&c_entry.candidate.hash(), &candidate_hash);
				assert!(c_entry.approval_entry(&block_hash).unwrap().is_approved());
				assert!(c_entry.mark_approval(validator_index_b));
			}
		);
	}
}

#[test]
fn assignment_triggered_by_all_with_less_than_threshold() {
	let block_hash = Hash::repeat_byte(0x01);

	let mut candidate_entry: CandidateEntry = {
		let approval_entry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			backing_group: GroupIndex(0),
			our_assignment: Some(approval_db::v1::OurAssignment {
				cert: garbage_assignment_cert(
					AssignmentCertKind::RelayVRFModulo { sample: 0 }
				),
				tranche: 1,
				validator_index: ValidatorIndex(4),
				triggered: false,
			}),
			our_approval_sig: None,
			assignments: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
			approved: false,
		};

		approval_db::v1::CandidateEntry {
			candidate: Default::default(),
			session: 1,
			block_assignments: vec![(block_hash, approval_entry)].into_iter().collect(),
			approvals: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
		}.into()
	};

	// 1-of-4
	candidate_entry
		.approval_entry_mut(&block_hash)
		.unwrap()
		.import_assignment(0, ValidatorIndex(0), 0);

	candidate_entry.mark_approval(ValidatorIndex(0));

	let tranche_now = 1;
	assert!(should_trigger_assignment(
		candidate_entry.approval_entry(&block_hash).unwrap(),
		&candidate_entry,
		RequiredTranches::All,
		tranche_now,
	));
}

#[test]
fn assignment_not_triggered_by_all_with_threshold() {
	let block_hash = Hash::repeat_byte(0x01);

	let mut candidate_entry: CandidateEntry = {
		let approval_entry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			backing_group: GroupIndex(0),
			our_assignment: Some(approval_db::v1::OurAssignment {
				cert: garbage_assignment_cert(
					AssignmentCertKind::RelayVRFModulo { sample: 0 }
				),
				tranche: 1,
				validator_index: ValidatorIndex(4),
				triggered: false,
			}),
			our_approval_sig: None,
			assignments: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
			approved: false,
		};

		approval_db::v1::CandidateEntry {
			candidate: Default::default(),
			session: 1,
			block_assignments: vec![(block_hash, approval_entry)].into_iter().collect(),
			approvals: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
		}.into()
	};

	// 2-of-4
	candidate_entry
		.approval_entry_mut(&block_hash)
		.unwrap()
		.import_assignment(0, ValidatorIndex(0), 0);

	candidate_entry
		.approval_entry_mut(&block_hash)
		.unwrap()
		.import_assignment(0, ValidatorIndex(1), 0);

	candidate_entry.mark_approval(ValidatorIndex(0));
	candidate_entry.mark_approval(ValidatorIndex(1));

	let tranche_now = 1;
	assert!(!should_trigger_assignment(
		candidate_entry.approval_entry(&block_hash).unwrap(),
		&candidate_entry,
		RequiredTranches::All,
		tranche_now,
	));
}

#[test]
fn assignment_not_triggered_if_already_triggered() {
	let block_hash = Hash::repeat_byte(0x01);

	let candidate_entry: CandidateEntry = {
		let approval_entry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			backing_group: GroupIndex(0),
			our_assignment: Some(approval_db::v1::OurAssignment {
				cert: garbage_assignment_cert(
					AssignmentCertKind::RelayVRFModulo { sample: 0 }
				),
				tranche: 1,
				validator_index: ValidatorIndex(4),
				triggered: true,
			}),
			our_approval_sig: None,
			assignments: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
			approved: false,
		};

		approval_db::v1::CandidateEntry {
			candidate: Default::default(),
			session: 1,
			block_assignments: vec![(block_hash, approval_entry)].into_iter().collect(),
			approvals: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
		}.into()
	};

	let tranche_now = 1;
	assert!(!should_trigger_assignment(
		candidate_entry.approval_entry(&block_hash).unwrap(),
		&candidate_entry,
		RequiredTranches::All,
		tranche_now,
	));
}

#[test]
fn assignment_not_triggered_by_exact() {
	let block_hash = Hash::repeat_byte(0x01);

	let candidate_entry: CandidateEntry = {
		let approval_entry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			backing_group: GroupIndex(0),
			our_assignment: Some(approval_db::v1::OurAssignment {
				cert: garbage_assignment_cert(
					AssignmentCertKind::RelayVRFModulo { sample: 0 }
				),
				tranche: 1,
				validator_index: ValidatorIndex(4),
				triggered: false,
			}),
			our_approval_sig: None,
			assignments: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
			approved: false,
		};

		approval_db::v1::CandidateEntry {
			candidate: Default::default(),
			session: 1,
			block_assignments: vec![(block_hash, approval_entry)].into_iter().collect(),
			approvals: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
		}.into()
	};

	let tranche_now = 1;
	assert!(!should_trigger_assignment(
		candidate_entry.approval_entry(&block_hash).unwrap(),
		&candidate_entry,
		RequiredTranches::Exact { needed: 2, next_no_show: None, tolerated_missing: 0 },
		tranche_now,
	));
}

#[test]
fn assignment_not_triggered_more_than_maximum() {
	let block_hash = Hash::repeat_byte(0x01);
	let maximum_broadcast = 10;

	let candidate_entry: CandidateEntry = {
		let approval_entry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			backing_group: GroupIndex(0),
			our_assignment: Some(approval_db::v1::OurAssignment {
				cert: garbage_assignment_cert(
					AssignmentCertKind::RelayVRFModulo { sample: 0 }
				),
				tranche: maximum_broadcast + 1,
				validator_index: ValidatorIndex(4),
				triggered: false,
			}),
			our_approval_sig: None,
			assignments: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
			approved: false,
		};

		approval_db::v1::CandidateEntry {
			candidate: Default::default(),
			session: 1,
			block_assignments: vec![(block_hash, approval_entry)].into_iter().collect(),
			approvals: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
		}.into()
	};

	let tranche_now = 50;
	assert!(!should_trigger_assignment(
		candidate_entry.approval_entry(&block_hash).unwrap(),
		&candidate_entry,
		RequiredTranches::Pending {
			maximum_broadcast,
			clock_drift: 0,
			considered: 10,
			next_no_show: None,
		},
		tranche_now,
	));
}

#[test]
fn assignment_triggered_if_at_maximum() {
	let block_hash = Hash::repeat_byte(0x01);
	let maximum_broadcast = 10;

	let candidate_entry: CandidateEntry = {
		let approval_entry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			backing_group: GroupIndex(0),
			our_assignment: Some(approval_db::v1::OurAssignment {
				cert: garbage_assignment_cert(
					AssignmentCertKind::RelayVRFModulo { sample: 0 }
				),
				tranche: maximum_broadcast,
				validator_index: ValidatorIndex(4),
				triggered: false,
			}),
			our_approval_sig: None,
			assignments: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
			approved: false,
		};

		approval_db::v1::CandidateEntry {
			candidate: Default::default(),
			session: 1,
			block_assignments: vec![(block_hash, approval_entry)].into_iter().collect(),
			approvals: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
		}.into()
	};

	let tranche_now = maximum_broadcast;
	assert!(should_trigger_assignment(
		candidate_entry.approval_entry(&block_hash).unwrap(),
		&candidate_entry,
		RequiredTranches::Pending {
			maximum_broadcast,
			clock_drift: 0,
			considered: 10,
			next_no_show: None,
		},
		tranche_now,
	));
}

#[test]
fn assignment_not_triggered_if_at_maximum_but_clock_is_before() {
	let block_hash = Hash::repeat_byte(0x01);
	let maximum_broadcast = 10;

	let candidate_entry: CandidateEntry = {
		let approval_entry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			backing_group: GroupIndex(0),
			our_assignment: Some(approval_db::v1::OurAssignment {
				cert: garbage_assignment_cert(
					AssignmentCertKind::RelayVRFModulo { sample: 0 }
				),
				tranche: maximum_broadcast,
				validator_index: ValidatorIndex(4),
				triggered: false,
			}),
			our_approval_sig: None,
			assignments: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
			approved: false,
		};

		approval_db::v1::CandidateEntry {
			candidate: Default::default(),
			session: 1,
			block_assignments: vec![(block_hash, approval_entry)].into_iter().collect(),
			approvals: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
		}.into()
	};

	let tranche_now = 9;
	assert!(!should_trigger_assignment(
		candidate_entry.approval_entry(&block_hash).unwrap(),
		&candidate_entry,
		RequiredTranches::Pending {
			maximum_broadcast,
			clock_drift: 0,
			considered: 10,
			next_no_show: None,
		},
		tranche_now,
	));
}

#[test]
fn assignment_not_triggered_if_at_maximum_but_clock_is_before_with_drift() {
	let block_hash = Hash::repeat_byte(0x01);
	let maximum_broadcast = 10;

	let candidate_entry: CandidateEntry = {
		let approval_entry = approval_db::v1::ApprovalEntry {
			tranches: Vec::new(),
			backing_group: GroupIndex(0),
			our_assignment: Some(approval_db::v1::OurAssignment {
				cert: garbage_assignment_cert(
					AssignmentCertKind::RelayVRFModulo { sample: 0 }
				),
				tranche: maximum_broadcast,
				validator_index: ValidatorIndex(4),
				triggered: false,
			}),
			our_approval_sig: None,
			assignments: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
			approved: false,
		};

		approval_db::v1::CandidateEntry {
			candidate: Default::default(),
			session: 1,
			block_assignments: vec![(block_hash, approval_entry)].into_iter().collect(),
			approvals: bitvec::bitvec![BitOrderLsb0, u8; 0; 4],
		}.into()
	};

	let tranche_now = 10;
	assert!(!should_trigger_assignment(
		candidate_entry.approval_entry(&block_hash).unwrap(),
		&candidate_entry,
		RequiredTranches::Pending {
			maximum_broadcast,
			clock_drift: 1,
			considered: 10,
			next_no_show: None,
		},
		tranche_now,
	));
}

#[test]
fn wakeups_next() {
	let mut wakeups = Wakeups::default();

	let b_a = Hash::repeat_byte(0);
	let b_b = Hash::repeat_byte(1);

	let c_a = CandidateHash(Hash::repeat_byte(2));
	let c_b = CandidateHash(Hash::repeat_byte(3));

	wakeups.schedule(b_a, 0, c_a, 1);
	wakeups.schedule(b_a, 0, c_b, 4);
	wakeups.schedule(b_b, 1, c_b, 3);

	assert_eq!(wakeups.first().unwrap(), 1);

	let clock = MockClock::new(0);
	let clock_aux = clock.clone();

	let test_fut = Box::pin(async move {
		assert_eq!(wakeups.next(&clock).await, (1, b_a, c_a));
		assert_eq!(wakeups.next(&clock).await, (3, b_b, c_b));
		assert_eq!(wakeups.next(&clock).await, (4, b_a, c_b));
		assert!(wakeups.first().is_none());
		assert!(wakeups.wakeups.is_empty());

		assert_eq!(
			wakeups.block_numbers.get(&0).unwrap(),
			&vec![b_a].into_iter().collect::<HashSet<_>>(),
		);
		assert_eq!(
			wakeups.block_numbers.get(&1).unwrap(),
			&vec![b_b].into_iter().collect::<HashSet<_>>(),
		);

		wakeups.prune_finalized_wakeups(0);

		assert!(wakeups.block_numbers.get(&0).is_none());
		assert_eq!(
			wakeups.block_numbers.get(&1).unwrap(),
			&vec![b_b].into_iter().collect::<HashSet<_>>(),
		);

		wakeups.prune_finalized_wakeups(1);

		assert!(wakeups.block_numbers.get(&0).is_none());
		assert!(wakeups.block_numbers.get(&1).is_none());
	});

	let aux_fut = Box::pin(async move {
		clock_aux.inner.lock().set_tick(1);
		// skip direct set to 3.
		clock_aux.inner.lock().set_tick(4);
	});

	futures::executor::block_on(futures::future::join(test_fut, aux_fut));
}

#[test]
fn wakeup_earlier_supersedes_later() {
	let mut wakeups = Wakeups::default();

	let b_a = Hash::repeat_byte(0);
	let c_a = CandidateHash(Hash::repeat_byte(2));

	wakeups.schedule(b_a, 0, c_a, 4);
	wakeups.schedule(b_a, 0, c_a, 2);
	wakeups.schedule(b_a, 0, c_a, 3);

	let clock = MockClock::new(0);
	let clock_aux = clock.clone();

	let test_fut = Box::pin(async move {
		assert_eq!(wakeups.next(&clock).await, (2, b_a, c_a));
		assert!(wakeups.first().is_none());
		assert!(wakeups.reverse_wakeups.is_empty());
	});

	let aux_fut = Box::pin(async move {
		clock_aux.inner.lock().set_tick(2);
	});

	futures::executor::block_on(futures::future::join(test_fut, aux_fut));
}

#[test]
fn import_checked_approval_sets_one_block_bit_at_a_time() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();
	let candidate_receipt_2 = CandidateReceipt::<Hash> {
		descriptor: CandidateDescriptor::default(),
		commitments_hash: Hash::repeat_byte(0x02),
	};
	let candidate_hash_2 = candidate_receipt_2.hash();

	let validator_index_a = ValidatorIndex(0);
	let validator_index_b = ValidatorIndex(1);

	let state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Ok(0)
		})),
		..some_state(StateConfig {
			validators: vec![Sr25519Keyring::Alice, Sr25519Keyring::Bob, Sr25519Keyring::Charlie],
			validator_groups: vec![vec![ValidatorIndex(0), ValidatorIndex(1)], vec![ValidatorIndex(2)]],
			needed_approvals: 2,
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	add_candidate_to_block(
		&mut db,
		block_hash,
		candidate_hash_2,
		3,
		CoreIndex(1),
		GroupIndex(1),
		Some(candidate_receipt_2),
	);

	let setup_candidate = |db: &mut DbBackend, c_hash| {
		import_assignment(db, &c_hash, &block_hash, validator_index_a, |candidate_entry| {
			let approval_entry = candidate_entry.approval_entry_mut(&block_hash).unwrap();
			approval_entry.import_assignment(0, validator_index_b, 0);
			assert!(!candidate_entry.mark_approval(validator_index_a));
		})
	};

	setup_candidate(&mut db, candidate_hash);
	setup_candidate(&mut db, candidate_hash_2);

	let mut overlay_db = OverlayedBackend::new(&db);
	let actions = import_checked_approval(
		&state,
		&mut overlay_db,
		&Metrics(None),
		db.load_block_entry(&block_hash).unwrap().unwrap(),
		candidate_hash,
		db.load_candidate_entry(&candidate_hash).unwrap().unwrap(),
		ApprovalSource::Remote(validator_index_b),
	);

	assert_eq!(actions.len(), 0);

	let write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();
	assert_eq!(write_ops.len(), 2);

	assert_matches!(
		write_ops.get(0).unwrap(),
		BackendWriteOp::WriteBlockEntry(b_entry) => {
			assert_eq!(b_entry.block_hash(), block_hash);
			assert!(!b_entry.is_fully_approved());
			assert!(b_entry.is_candidate_approved(&candidate_hash));
			assert!(!b_entry.is_candidate_approved(&candidate_hash_2));
		}
	);

	assert_matches!(
		write_ops.get(1).unwrap(),
		BackendWriteOp::WriteCandidateEntry(c_entry) => {
			assert_eq!(&c_entry.candidate.hash(), &candidate_hash);
			assert!(c_entry.approval_entry(&block_hash).unwrap().is_approved());
		}
	);

	db.write(write_ops).unwrap();

	let mut overlay_db = OverlayedBackend::new(&db);
	let actions = import_checked_approval(
		&state,
		&mut overlay_db,
		&Metrics(None),
		db.load_block_entry(&block_hash).unwrap().unwrap(),
		candidate_hash_2,
		db.load_candidate_entry(&candidate_hash_2).unwrap().unwrap(),
		ApprovalSource::Remote(validator_index_b),
	);

	assert_eq!(actions.len(), 1);

	let write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();
	assert_eq!(write_ops.len(), 2);

	assert_matches!(
		write_ops.get(0).unwrap(),
		BackendWriteOp::WriteBlockEntry(b_entry) => {
			assert_eq!(b_entry.block_hash(), block_hash);
			assert!(b_entry.is_fully_approved());
			assert!(b_entry.is_candidate_approved(&candidate_hash));
			assert!(b_entry.is_candidate_approved(&candidate_hash_2));
		}
	);

	assert_matches!(
		write_ops.get(1).unwrap(),
		BackendWriteOp::WriteCandidateEntry(c_entry) => {
			assert_eq!(&c_entry.candidate.hash(), &candidate_hash_2);
			assert!(c_entry.approval_entry(&block_hash).unwrap().is_approved());
		}
	);
}

#[test]
fn approved_ancestor_all_approved() {
	let mut db = make_db();

	let block_hash_1 = Hash::repeat_byte(0x01);
	let block_hash_2 = Hash::repeat_byte(0x02);
	let block_hash_3 = Hash::repeat_byte(0x03);
	let block_hash_4 = Hash::repeat_byte(0x04);

	let candidate_hash = CandidateHash(Hash::repeat_byte(0xCC));

	let slot = Slot::from(1);
	let session_index = 1;

	let add_block = |db: &mut DbBackend, block_hash, approved| {
		add_block(
			db,
			block_hash,
			session_index,
			slot,
		);

		let mut block_entry = db.load_block_entry(&block_hash).unwrap().unwrap();
		block_entry.add_candidate(CoreIndex(0), candidate_hash);
		if approved {
			block_entry.mark_approved_by_hash(&candidate_hash);
		}

		overlay_txn(db, |overlay_db| overlay_db.write_block_entry(block_entry.clone()));
	};

	add_block(&mut db, block_hash_1, true);
	add_block(&mut db, block_hash_2, true);
	add_block(&mut db, block_hash_3, true);
	add_block(&mut db, block_hash_4, true);

	let pool = TaskExecutor::new();
	let (mut ctx, mut handle) = make_subsystem_context::<(), _>(pool.clone());

	let test_fut = Box::pin(async move {
		let overlay_db = OverlayedBackend::new(&db);
		assert_matches!(
			handle_approved_ancestor(&mut ctx, &overlay_db, block_hash_4, 0, &Default::default())
				.await,
			Ok(Some(HighestApprovedAncestorBlock { hash, number, .. } )) => {
				assert_eq!((block_hash_4, 4), (hash, number));
			}
		)
	});

	let aux_fut = Box::pin(async move {
		assert_matches!(
			handle.recv().await,
			AllMessages::ChainApi(ChainApiMessage::BlockNumber(target, tx)) => {
				assert_eq!(target, block_hash_4);
				let _ = tx.send(Ok(Some(4)));
			}
		);

		assert_matches!(
			handle.recv().await,
			AllMessages::ChainApi(ChainApiMessage::Ancestors {
				hash,
				k,
				response_channel: tx,
			}) => {
				assert_eq!(hash, block_hash_4);
				assert_eq!(k, 4 - (0 + 1));
				let _ = tx.send(Ok(vec![block_hash_3, block_hash_2, block_hash_1]));
			}
		);
	});

	futures::executor::block_on(futures::future::join(test_fut, aux_fut));
}

#[test]
fn approved_ancestor_missing_approval() {
	let mut db = make_db();

	let block_hash_1 = Hash::repeat_byte(0x01);
	let block_hash_2 = Hash::repeat_byte(0x02);
	let block_hash_3 = Hash::repeat_byte(0x03);
	let block_hash_4 = Hash::repeat_byte(0x04);

	let candidate_hash = CandidateHash(Hash::repeat_byte(0xCC));

	let slot = Slot::from(1);
	let session_index = 1;

	let add_block = |db: &mut DbBackend, block_hash, approved| {
		add_block(
			db,
			block_hash,
			session_index,
			slot,
		);

		let mut block_entry = db.load_block_entry(&block_hash).unwrap().unwrap();
		block_entry.add_candidate(CoreIndex(0), candidate_hash);
		if approved {
			block_entry.mark_approved_by_hash(&candidate_hash);
		}

		overlay_txn(db, |overlay_db| overlay_db.write_block_entry(block_entry.clone()));
	};

	add_block(&mut db, block_hash_1, true);
	add_block(&mut db, block_hash_2, true);
	add_block(&mut db, block_hash_3, false);
	add_block(&mut db, block_hash_4, true);

	let pool = TaskExecutor::new();
	let (mut ctx, mut handle) = make_subsystem_context::<(), _>(pool.clone());

	let test_fut = Box::pin(async move {
		let overlay_db = OverlayedBackend::new(&db);
		assert_matches!(
			handle_approved_ancestor(&mut ctx, &overlay_db, block_hash_4, 0, &Default::default())
				.await,
			Ok(Some(HighestApprovedAncestorBlock { hash, number, .. })) => {
				assert_eq!((block_hash_2, 2), (hash, number));
			}
		)
	});

	let aux_fut = Box::pin(async move {
		assert_matches!(
			handle.recv().await,
			AllMessages::ChainApi(ChainApiMessage::BlockNumber(target, tx)) => {
				assert_eq!(target, block_hash_4);
				let _ = tx.send(Ok(Some(4)));
			}
		);

		assert_matches!(
			handle.recv().await,
			AllMessages::ChainApi(ChainApiMessage::Ancestors {
				hash,
				k,
				response_channel: tx,
			}) => {
				assert_eq!(hash, block_hash_4);
				assert_eq!(k, 4 - (0 + 1));
				let _ = tx.send(Ok(vec![block_hash_3, block_hash_2, block_hash_1]));
			}
		);
	});

	futures::executor::block_on(futures::future::join(test_fut, aux_fut));
}

#[test]
fn process_wakeup_trigger_assignment_launch_approval() {
	let mut db = make_db();

	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();
	let slot = Slot::from(1);
	let session_index = 1;

	let state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Ok(0)
		})),
		..some_state(StateConfig {
			validators: vec![Sr25519Keyring::Alice, Sr25519Keyring::Bob],
			validator_groups: vec![vec![ValidatorIndex(0)], vec![ValidatorIndex(1)]],
			needed_approvals: 2,
			session_index,
			slot,
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	let mut overlay_db = OverlayedBackend::new(&db);
	let actions = process_wakeup(
		&state,
		&mut overlay_db,
		block_hash,
		candidate_hash,
		1,
	).unwrap();

	assert!(actions.is_empty());
	assert_eq!(overlay_db.into_write_ops().count(), 0);

	set_our_assignment(&mut db, &candidate_hash, &block_hash, 0, |_| {});

	let mut overlay_db = OverlayedBackend::new(&db);
	let actions = process_wakeup(
		&state,
		&mut overlay_db,
		block_hash,
		candidate_hash,
		1,
	).unwrap();

	assert_eq!(actions.len(), 2);

	assert_matches!(
		actions.get(0).unwrap(),
		Action::LaunchApproval {
			candidate_index,
			..
		} => {
			assert_eq!(candidate_index, &0);
		}
	);

	assert_matches!(
		actions.get(1).unwrap(),
		Action::ScheduleWakeup {
			tick,
			..
		} => {
			assert_eq!(tick, &slot_to_tick(0 + 2));
		}
	);

	let write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();
	assert_eq!(write_ops.len(), 1);

	assert_matches!(
		write_ops.get(0).unwrap(),
		BackendWriteOp::WriteCandidateEntry(c_entry) => {
			assert_eq!(&c_entry.candidate.hash(), &candidate_hash);
			assert!(c_entry
				.approval_entry(&block_hash)
				.unwrap()
				.our_assignment()
				.unwrap()
				.triggered()
			);
		}
	);
}

#[test]
fn process_wakeup_schedules_wakeup() {
	let mut db = make_db();

	let block_hash = Hash::repeat_byte(0x01);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();
	let slot = Slot::from(1);
	let session_index = 1;

	let state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Ok(10)
		})),
		..some_state(StateConfig {
			validators: vec![Sr25519Keyring::Alice, Sr25519Keyring::Bob],
			validator_groups: vec![vec![ValidatorIndex(0)], vec![ValidatorIndex(1)]],
			needed_approvals: 2,
			session_index,
			slot,
			candidate_hash: Some(candidate_hash),
			..Default::default()
		}, &mut db)
	};

	set_our_assignment(&mut db, &candidate_hash, &block_hash, 10, |_| {});

	let mut overlay_db = OverlayedBackend::new(&db);
	let actions = process_wakeup(
		&state,
		&mut overlay_db,
		block_hash,
		candidate_hash,
		1,
	).unwrap();

	assert_eq!(actions.len(), 1);
	assert_matches!(
		actions.get(0).unwrap(),
		Action::ScheduleWakeup { block_hash: b, candidate_hash: c, tick, .. } => {
			assert_eq!(b, &block_hash);
			assert_eq!(c, &candidate_hash);
			assert_eq!(tick, &(slot_to_tick(slot) + 10));
		}
	);
	assert_eq!(overlay_db.into_write_ops().count(), 0);
}

#[test]
fn triggered_assignment_leads_to_recovery_and_validation() {

}

#[test]
fn finalization_event_prunes() {

}

#[test]
fn local_approval_import_always_updates_approval_entry() {
	let mut db = make_db();
	let block_hash = Hash::repeat_byte(0x01);
	let block_hash_2 = Hash::repeat_byte(0x02);
	let candidate_hash = CandidateReceipt::<Hash>::default().hash();
	let validator_index = ValidatorIndex(0);

	let state_config = StateConfig {
		validators: vec![Sr25519Keyring::Alice, Sr25519Keyring::Bob, Sr25519Keyring::Charlie],
		validator_groups: vec![vec![ValidatorIndex(0), ValidatorIndex(1)], vec![ValidatorIndex(2)]],
		needed_approvals: 2,
		candidate_hash: Some(candidate_hash),
		..Default::default()
	};

	let state = State {
		assignment_criteria: Box::new(MockAssignmentCriteria::check_only(|| {
			Ok(0)
		})),
		..some_state(state_config.clone(), &mut db)
	};

	add_block(
		&mut db,
		block_hash_2,
		state_config.session_index,
		state_config.slot,
	);

	add_candidate_to_block(
		&mut db,
		block_hash_2,
		candidate_hash,
		state_config.validators.len(),
		1.into(),
		GroupIndex(1),
		None,
	);

	let sig_a = sign_approval(Sr25519Keyring::Alice, candidate_hash, 1);
	let sig_b = sign_approval(Sr25519Keyring::Alice, candidate_hash, 1);

	{
		let mut import_local_assignment = |block_hash: Hash| {
			set_our_assignment(&mut db, &candidate_hash, &block_hash, 0, |candidate_entry| {
				let approval_entry = candidate_entry.approval_entry_mut(&block_hash).unwrap();
				assert!(approval_entry.trigger_our_assignment(0).is_some());
				assert!(approval_entry.local_statements().0.is_some());
			});
		};

		import_local_assignment(block_hash);
		import_local_assignment(block_hash_2);
	}

	{
		let mut overlay_db = OverlayedBackend::new(&db);
		let actions = import_checked_approval(
			&state,
			&mut overlay_db,
			&Metrics(None),
			db.load_block_entry(&block_hash).unwrap().unwrap(),
			candidate_hash,
			db.load_candidate_entry(&candidate_hash).unwrap().unwrap(),
			ApprovalSource::Local(validator_index, sig_a.clone()),
		);

		assert_eq!(actions.len(), 0);

		let mut write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();
		assert_eq!(write_ops.len(), 1);

		assert_matches!(
			write_ops.get_mut(0).unwrap(),
			BackendWriteOp::WriteCandidateEntry(ref mut c_entry) => {
				assert_eq!(&c_entry.candidate.hash(), &candidate_hash);
				assert_eq!(
					c_entry.approval_entry(&block_hash).unwrap().local_statements().1,
					Some(sig_a),
				);
				assert!(c_entry.mark_approval(validator_index));
			}
		);

		db.write(write_ops).unwrap();
	}

	{
		let mut overlay_db = OverlayedBackend::new(&db);
		let actions = import_checked_approval(
			&state,
			&mut overlay_db,
			&Metrics(None),
			db.load_block_entry(&block_hash_2).unwrap().unwrap(),
			candidate_hash,
			db.load_candidate_entry(&candidate_hash).unwrap().unwrap(),
			ApprovalSource::Local(validator_index, sig_b.clone()),
		);

		assert_eq!(actions.len(), 0);

		let mut write_ops = overlay_db.into_write_ops().collect::<Vec<BackendWriteOp>>();
		assert_eq!(write_ops.len(), 1);

		assert_matches!(
			write_ops.get_mut(0).unwrap(),
			BackendWriteOp::WriteCandidateEntry(ref mut c_entry) => {
				assert_eq!(&c_entry.candidate.hash(), &candidate_hash);
				assert_eq!(
					c_entry.approval_entry(&block_hash_2).unwrap().local_statements().1,
					Some(sig_b),
				);
				assert!(c_entry.mark_approval(validator_index));
			}
		);
	}
}

// TODO [now]: handling `BecomeActive` action broadcasts everything.
