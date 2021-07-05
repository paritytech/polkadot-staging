// Copyright 2020-2021 Parity Technologies (UK) Ltd.
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
use polkadot_node_subsystem_test_helpers as test_helpers;
use polkadot_primitives::v1::{HeadData, UpwardMessage, ValidationCode};
use sp_core::testing::TaskExecutor;
use futures::executor;
use assert_matches::assert_matches;
use sp_keyring::Sr25519Keyring;

fn collator_sign(descriptor: &mut CandidateDescriptor, collator: Sr25519Keyring) {
	descriptor.collator = collator.public().into();
	let payload = polkadot_primitives::v1::collator_signature_payload(
		&descriptor.relay_parent,
		&descriptor.para_id,
		&descriptor.persisted_validation_data_hash,
		&descriptor.pov_hash,
		&descriptor.validation_code_hash,
	);

	descriptor.signature = collator.sign(&payload[..]).into();
	assert!(descriptor.check_collator_signature().is_ok());
}

#[test]
fn correctly_checks_included_assumption() {
	let validation_data: PersistedValidationData = Default::default();
	let validation_code_and_hash = ValidationCodeAndHash::compute_from_code(vec![1, 2, 3].into());

	let persisted_validation_data_hash = validation_data.hash();
	let relay_parent = [2; 32].into();
	let para_id = 5.into();

	let mut candidate = CandidateDescriptor::default();
	candidate.relay_parent = relay_parent;
	candidate.persisted_validation_data_hash = persisted_validation_data_hash;
	candidate.para_id = para_id;

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = test_helpers::make_subsystem_context(pool.clone());

	let (check_fut, check_result) = check_assumption_validation_data(
		&mut ctx,
		&candidate,
		OccupiedCoreAssumption::Included,
	).remote_handle();

	let test_fut = async move {
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::PersistedValidationData(
					p,
					OccupiedCoreAssumption::Included,
					tx
				),
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_data.clone())));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::ValidationCode(p, OccupiedCoreAssumption::Included, tx)
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_code_and_hash.clone())));
			}
		);

		assert_matches!(check_result.await.unwrap(), AssumptionCheckOutcome::Matches(o, v) => {
			assert_eq!(o, validation_data);
			assert_eq!(v, validation_code_and_hash);
		});
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);
}

#[test]
fn correctly_checks_timed_out_assumption() {
	let validation_data: PersistedValidationData = Default::default();
	let validation_code_and_hash = ValidationCodeAndHash::compute_from_code(vec![1, 2, 3].into());

	let persisted_validation_data_hash = validation_data.hash();
	let relay_parent = [2; 32].into();
	let para_id = 5.into();

	let mut candidate = CandidateDescriptor::default();
	candidate.relay_parent = relay_parent;
	candidate.persisted_validation_data_hash = persisted_validation_data_hash;
	candidate.para_id = para_id;

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = test_helpers::make_subsystem_context(pool.clone());

	let (check_fut, check_result) = check_assumption_validation_data(
		&mut ctx,
		&candidate,
		OccupiedCoreAssumption::TimedOut,
	).remote_handle();

	let test_fut = async move {
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::PersistedValidationData(
					p,
					OccupiedCoreAssumption::TimedOut,
					tx
				),
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_data.clone())));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::ValidationCode(p, OccupiedCoreAssumption::TimedOut, tx)
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_code_and_hash.clone())));
			}
		);

		assert_matches!(check_result.await.unwrap(), AssumptionCheckOutcome::Matches(o, v) => {
			assert_eq!(o, validation_data);
			assert_eq!(v, validation_code_and_hash);
		});
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);
}

#[test]
fn check_is_bad_request_if_no_validation_data() {
	let validation_data: PersistedValidationData = Default::default();
	let persisted_validation_data_hash = validation_data.hash();
	let relay_parent = [2; 32].into();
	let para_id = 5.into();

	let mut candidate = CandidateDescriptor::default();
	candidate.relay_parent = relay_parent;
	candidate.persisted_validation_data_hash = persisted_validation_data_hash;
	candidate.para_id = para_id;

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = test_helpers::make_subsystem_context(pool.clone());

	let (check_fut, check_result) = check_assumption_validation_data(
		&mut ctx,
		&candidate,
		OccupiedCoreAssumption::Included,
	).remote_handle();

	let test_fut = async move {
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::PersistedValidationData(
					p,
					OccupiedCoreAssumption::Included,
					tx
				),
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(None));
			}
		);

		assert_matches!(check_result.await.unwrap(), AssumptionCheckOutcome::BadRequest);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);
}

#[test]
fn check_is_bad_request_if_no_validation_code() {
	let validation_data: PersistedValidationData = Default::default();
	let persisted_validation_data_hash = validation_data.hash();
	let relay_parent = [2; 32].into();
	let para_id = 5.into();

	let mut candidate = CandidateDescriptor::default();
	candidate.relay_parent = relay_parent;
	candidate.persisted_validation_data_hash = persisted_validation_data_hash;
	candidate.para_id = para_id;

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = test_helpers::make_subsystem_context(pool.clone());

	let (check_fut, check_result) = check_assumption_validation_data(
		&mut ctx,
		&candidate,
		OccupiedCoreAssumption::TimedOut,
	).remote_handle();

	let test_fut = async move {
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::PersistedValidationData(
					p,
					OccupiedCoreAssumption::TimedOut,
					tx
				),
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_data.clone())));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::ValidationCode(p, OccupiedCoreAssumption::TimedOut, tx)
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(None));
			}
		);

		assert_matches!(check_result.await.unwrap(), AssumptionCheckOutcome::BadRequest);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);
}

#[test]
fn check_does_not_match() {
	let validation_data: PersistedValidationData = Default::default();
	let relay_parent = [2; 32].into();
	let para_id = 5.into();

	let mut candidate = CandidateDescriptor::default();
	candidate.relay_parent = relay_parent;
	candidate.persisted_validation_data_hash = [3; 32].into();
	candidate.para_id = para_id;

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = test_helpers::make_subsystem_context(pool.clone());

	let (check_fut, check_result) = check_assumption_validation_data(
		&mut ctx,
		&candidate,
		OccupiedCoreAssumption::Included,
	).remote_handle();

	let test_fut = async move {
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::PersistedValidationData(
					p,
					OccupiedCoreAssumption::Included,
					tx
				),
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_data.clone())));
			}
		);

		assert_matches!(check_result.await.unwrap(), AssumptionCheckOutcome::DoesNotMatch);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);
}

struct MockValidatorBackend {
	result: Result<WasmValidationResult, ValidationError>,
}

impl MockValidatorBackend {
	fn with_hardcoded_result(result: Result<WasmValidationResult, ValidationError>) -> Self {
		Self {
			result,
		}
	}
}

#[async_trait]
impl ValidationBackend for MockValidatorBackend {
	async fn validate_candidate(
		&mut self,
		_raw_validation_code: Vec<u8>,
		_params: ValidationParams
	) -> Result<WasmValidationResult, ValidationError> {
		self.result.clone()
	}
}

#[test]
fn candidate_validation_ok_is_ok() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };

	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);
	let validation_code_and_hash = ValidationCodeAndHash::compute_from_code(vec![2; 16].into());

	let mut descriptor = CandidateDescriptor::default();
	descriptor.pov_hash = pov.hash();
	descriptor.para_head = head_data.hash();
	descriptor.validation_code_hash = validation_code_and_hash.hash().clone();
	collator_sign(&mut descriptor, Sr25519Keyring::Alice);

	let check = perform_basic_checks(
		&descriptor,
		validation_data.max_pov_size,
		&pov,
		&validation_code_and_hash.hash(),
	);
	assert!(check.is_ok());

	let validation_result = WasmValidationResult {
		head_data,
		new_validation_code: Some(vec![2, 2, 2].into()),
		upward_messages: Vec::new(),
		horizontal_messages: Vec::new(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};

	let v = executor::block_on(validate_candidate_exhaustive(
		MockValidatorBackend::with_hardcoded_result(Ok(validation_result)),
		validation_data.clone(),
		validation_code_and_hash,
		descriptor,
		Arc::new(pov),
		&Default::default(),
	))
	.unwrap()
	.unwrap();

	assert_matches!(v, ValidationResult::Valid(outputs, used_validation_data) => {
		assert_eq!(outputs.head_data, HeadData(vec![1, 1, 1]));
		assert_eq!(outputs.upward_messages, Vec::<UpwardMessage>::new());
		assert_eq!(outputs.horizontal_messages, Vec::new());
		assert_eq!(outputs.new_validation_code, Some(vec![2, 2, 2].into()));
		assert_eq!(outputs.hrmp_watermark, 0);
		assert_eq!(used_validation_data, validation_data);
	});
}

#[test]
fn candidate_validation_bad_return_is_invalid() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };

	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let validation_code_and_hash = ValidationCodeAndHash::compute_from_code(vec![2; 16].into());

	let mut descriptor = CandidateDescriptor::default();
	descriptor.pov_hash = pov.hash();
	descriptor.validation_code_hash = validation_code_and_hash.hash().clone();
	collator_sign(&mut descriptor, Sr25519Keyring::Alice);

	let check = perform_basic_checks(
		&descriptor,
		validation_data.max_pov_size,
		&pov,
		&validation_code_and_hash.hash(),
	);
	assert!(check.is_ok());

	let v = executor::block_on(validate_candidate_exhaustive(
		MockValidatorBackend::with_hardcoded_result(
			Err(ValidationError::InvalidCandidate(WasmInvalidCandidate::AmbigiousWorkerDeath))
		),
		validation_data,
		validation_code_and_hash,
		descriptor,
		Arc::new(pov),
		&Default::default(),
	))
	.unwrap()
	.unwrap();

	assert_matches!(v, ValidationResult::Invalid(InvalidCandidate::ExecutionError(_)));
}

#[test]
fn candidate_validation_timeout_is_internal_error() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };

	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let validation_code_and_hash = ValidationCodeAndHash::compute_from_code(vec![2; 16].into());

	let mut descriptor = CandidateDescriptor::default();
	descriptor.pov_hash = pov.hash();
	descriptor.validation_code_hash = validation_code_and_hash.hash().clone();
	collator_sign(&mut descriptor, Sr25519Keyring::Alice);

	let check = perform_basic_checks(
		&descriptor,
		validation_data.max_pov_size,
		&pov,
		&validation_code_and_hash.hash(),
	);
	assert!(check.is_ok());

	let v = executor::block_on(validate_candidate_exhaustive(
		MockValidatorBackend::with_hardcoded_result(
			Err(ValidationError::InvalidCandidate(WasmInvalidCandidate::HardTimeout)),
		),
		validation_data,
		validation_code_and_hash,
		descriptor,
		Arc::new(pov),
		&Default::default(),
	))
	.unwrap();

	assert_matches!(v, Ok(ValidationResult::Invalid(InvalidCandidate::Timeout)));
}

#[test]
fn candidate_validation_code_mismatch_is_invalid() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };

	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let validation_code_and_hash = ValidationCodeAndHash::compute_from_code(vec![2; 16].into());

	let mut descriptor = CandidateDescriptor::default();
	descriptor.pov_hash = pov.hash();
	descriptor.validation_code_hash = ValidationCode(vec![1; 16]).hash();
	collator_sign(&mut descriptor, Sr25519Keyring::Alice);

	let check = perform_basic_checks(
		&descriptor,
		validation_data.max_pov_size,
		&pov,
		&validation_code_and_hash.hash(),
	);
	assert_matches!(check, Err(InvalidCandidate::CodeHashMismatch));

	let v = executor::block_on(validate_candidate_exhaustive(
		MockValidatorBackend::with_hardcoded_result(
			Err(ValidationError::InvalidCandidate(WasmInvalidCandidate::HardTimeout)),
		),
		validation_data,
		validation_code_and_hash,
		descriptor,
		Arc::new(pov),
		&Default::default(),
	))
	.unwrap()
	.unwrap();

	assert_matches!(v, ValidationResult::Invalid(InvalidCandidate::CodeHashMismatch));
}

#[test]
fn compressed_code_works() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };
	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);

	let raw_code = vec![2u8; 16];
	let validation_code_and_hash = sp_maybe_compressed_blob::compress(
		&raw_code,
		VALIDATION_CODE_BOMB_LIMIT,
	)
		.map(ValidationCode)
		.map(ValidationCodeAndHash::compute_from_code)
		.unwrap();

	let mut descriptor = CandidateDescriptor::default();
	descriptor.pov_hash = pov.hash();
	descriptor.para_head = head_data.hash();
	descriptor.validation_code_hash = validation_code_and_hash.hash().clone();
	collator_sign(&mut descriptor, Sr25519Keyring::Alice);

	let validation_result = WasmValidationResult {
		head_data,
		new_validation_code: None,
		upward_messages: Vec::new(),
		horizontal_messages: Vec::new(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};

	let v = executor::block_on(validate_candidate_exhaustive(
		MockValidatorBackend::with_hardcoded_result(Ok(validation_result)),
		validation_data,
		validation_code_and_hash,
		descriptor,
		Arc::new(pov),
		&Default::default(),
	))
	.unwrap();

	assert_matches!(v, Ok(ValidationResult::Valid(_, _)));
}

#[test]
fn code_decompression_failure_is_invalid() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };
	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);

	let raw_code = vec![2u8; VALIDATION_CODE_BOMB_LIMIT + 1];
	let validation_code_and_hash = sp_maybe_compressed_blob::compress(
		&raw_code,
		VALIDATION_CODE_BOMB_LIMIT + 1,
	)
		.map(ValidationCode)
		.map(ValidationCodeAndHash::compute_from_code)
		.unwrap();

	let mut descriptor = CandidateDescriptor::default();
	descriptor.pov_hash = pov.hash();
	descriptor.para_head = head_data.hash();
	descriptor.validation_code_hash = validation_code_and_hash.hash().clone();
	collator_sign(&mut descriptor, Sr25519Keyring::Alice);

	let validation_result = WasmValidationResult {
		head_data,
		new_validation_code: None,
		upward_messages: Vec::new(),
		horizontal_messages: Vec::new(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};

	let v = executor::block_on(validate_candidate_exhaustive(
		MockValidatorBackend::with_hardcoded_result(Ok(validation_result)),
		validation_data,
		validation_code_and_hash,
		descriptor,
		Arc::new(pov),
		&Default::default(),
	))
	.unwrap();

	assert_matches!(
		v,
		Ok(ValidationResult::Invalid(InvalidCandidate::CodeDecompressionFailure))
	);
}

#[test]
fn pov_decompression_failure_is_invalid() {
	let validation_data = PersistedValidationData {
		max_pov_size: POV_BOMB_LIMIT as u32,
		..Default::default()
	 };
	let head_data = HeadData(vec![1, 1, 1]);

	let raw_block_data = vec![2u8; POV_BOMB_LIMIT + 1];
	let pov = sp_maybe_compressed_blob::compress(
		&raw_block_data,
		POV_BOMB_LIMIT + 1,
	)
		.map(|raw| PoV { block_data: BlockData(raw) })
		.unwrap();

	let validation_code_and_hash = ValidationCodeAndHash::compute_from_code(vec![2; 16].into());

	let mut descriptor = CandidateDescriptor::default();
	descriptor.pov_hash = pov.hash();
	descriptor.para_head = head_data.hash();
	descriptor.validation_code_hash = validation_code_and_hash.hash().clone();
	collator_sign(&mut descriptor, Sr25519Keyring::Alice);

	let validation_result = WasmValidationResult {
		head_data,
		new_validation_code: None,
		upward_messages: Vec::new(),
		horizontal_messages: Vec::new(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};

	let v = executor::block_on(validate_candidate_exhaustive(
		MockValidatorBackend::with_hardcoded_result(Ok(validation_result)),
		validation_data,
		validation_code_and_hash,
		descriptor,
		Arc::new(pov),
		&Default::default(),
	))
	.unwrap();

	assert_matches!(
		v,
		Ok(ValidationResult::Invalid(InvalidCandidate::PoVDecompressionFailure))
	);
}
