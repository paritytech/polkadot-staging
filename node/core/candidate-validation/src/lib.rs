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

//! The Candidate Validation subsystem.
//!
//! This handles incoming requests from other subsystems to validate candidates
//! according to a validation function. This delegates validation to an underlying
//! pool of processes used for execution of the Wasm.

#![deny(unused_crate_dependencies, unused_results)]
#![warn(missing_docs)]

use polkadot_subsystem::{
	Subsystem, SubsystemContext, SpawnedSubsystem, SubsystemResult, SubsystemError,
	FromOverseer, OverseerSignal,
	messages::{
		AllMessages, CandidateValidationMessage, RuntimeApiMessage,
		ValidationFailed, RuntimeApiRequest,
	},
};
use polkadot_node_subsystem_util::metrics::{self, prometheus};
use polkadot_subsystem::errors::RuntimeApiError;
use polkadot_node_primitives::{
	VALIDATION_CODE_BOMB_LIMIT, POV_BOMB_LIMIT, ValidationResult, InvalidCandidate, PoV, BlockData,
};
use polkadot_primitives::v1::{
	ValidationCodeHash, ValidationCodeAndHash, CandidateDescriptor, PersistedValidationData,
	OccupiedCoreAssumption, Hash, CandidateCommitments,
};
use polkadot_parachain::primitives::{ValidationParams, ValidationResult as WasmValidationResult};
use polkadot_node_core_pvf::{Pvf, ValidationHost, ValidationError, InvalidCandidate as WasmInvalidCandidate};

use parity_scale_codec::Encode;

use futures::channel::oneshot;
use futures::prelude::*;

use std::sync::Arc;
use std::path::PathBuf;

use async_trait::async_trait;

#[cfg(test)]
mod tests;

const LOG_TARGET: &'static str = "parachain::candidate-validation";

/// Configuration for the candidate validation subsystem
#[derive(Clone)]
pub struct Config {
	/// The path where candidate validation can store compiled artifacts for PVFs.
	pub artifacts_cache_path: PathBuf,
	/// The path to the executable which can be used for spawning PVF compilation & validation
	/// workers.
	pub program_path: PathBuf,
}

/// The candidate validation subsystem.
pub struct CandidateValidationSubsystem {
	metrics: Metrics,
	config: Config,
}

impl CandidateValidationSubsystem {
	/// Create a new `CandidateValidationSubsystem` with the given task spawner and isolation
	/// strategy.
	///
	/// Check out [`IsolationStrategy`] to get more details.
	pub fn with_config(config: Config, metrics: Metrics) -> Self {
		CandidateValidationSubsystem { config, metrics, }
	}
}

impl<C> Subsystem<C> for CandidateValidationSubsystem where
	C: SubsystemContext<Message = CandidateValidationMessage>,
{
	fn start(self, ctx: C) -> SpawnedSubsystem {
		let future = run(ctx, self.metrics, self.config.artifacts_cache_path, self.config.program_path)
			.map_err(|e| SubsystemError::with_origin("candidate-validation", e))
			.boxed();
		SpawnedSubsystem {
			name: "candidate-validation-subsystem",
			future,
		}
	}
}

async fn run(
	mut ctx: impl SubsystemContext<Message = CandidateValidationMessage>,
	metrics: Metrics,
	cache_path: PathBuf,
	program_path: PathBuf,
) -> SubsystemResult<()> {
	let (mut validation_host, task) = polkadot_node_core_pvf::start(
		polkadot_node_core_pvf::Config::new(cache_path, program_path),
	);
	ctx.spawn_blocking("pvf-validation-host", task.boxed())?;

	loop {
		match ctx.recv().await? {
			FromOverseer::Signal(OverseerSignal::ActiveLeaves(_)) => {}
			FromOverseer::Signal(OverseerSignal::BlockFinalized(..)) => {}
			FromOverseer::Signal(OverseerSignal::Conclude) => return Ok(()),
			FromOverseer::Communication { msg } => match msg {
				CandidateValidationMessage::ValidateFromChainState(
					descriptor,
					pov,
					response_sender,
				) => {
					let _timer = metrics.time_validate_from_chain_state();

					let res = spawn_validate_from_chain_state(
						&mut ctx,
						&mut validation_host,
						descriptor,
						pov,
						&metrics,
					).await;

					match res {
						Ok(x) => {
							metrics.on_validation_event(&x);
							let _ = response_sender.send(x);
						}
						Err(e) => return Err(e),
					}
				}
				CandidateValidationMessage::ValidateFromExhaustive(
					persisted_validation_data,
					validation_code,
					descriptor,
					pov,
					response_sender,
				) => {
					let _timer = metrics.time_validate_from_exhaustive();

					let res = validate_candidate_exhaustive(
						&mut validation_host,
						persisted_validation_data,
						validation_code,
						descriptor,
						pov,
						&metrics,
					).await;

					match res {
						Ok(x) => {
							metrics.on_validation_event(&x);
							if let Err(_e) = response_sender.send(x) {
								tracing::warn!(
									target: LOG_TARGET,
									"Requester of candidate validation dropped",
								)
							}
						},
						Err(e) => return Err(e),
					}
				}
			}
		}
	}
}

async fn runtime_api_request<T>(
	ctx: &mut impl SubsystemContext<Message = CandidateValidationMessage>,
	relay_parent: Hash,
	request: RuntimeApiRequest,
	receiver: oneshot::Receiver<Result<T, RuntimeApiError>>,
) -> SubsystemResult<Result<T, RuntimeApiError>> {
	ctx.send_message(
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(
			relay_parent,
			request,
		))
	).await;

	receiver.await.map_err(Into::into)
}

#[derive(Debug)]
enum AssumptionCheckOutcome {
	Matches(PersistedValidationData, ValidationCodeAndHash),
	DoesNotMatch,
	BadRequest,
}

async fn check_assumption_validation_data(
	ctx: &mut impl SubsystemContext<Message = CandidateValidationMessage>,
	descriptor: &CandidateDescriptor,
	assumption: OccupiedCoreAssumption,
) -> SubsystemResult<AssumptionCheckOutcome> {
	let validation_data = {
		let (tx, rx) = oneshot::channel();
		let d = runtime_api_request(
			ctx,
			descriptor.relay_parent,
			RuntimeApiRequest::PersistedValidationData(
				descriptor.para_id,
				assumption,
				tx,
			),
			rx,
		).await?;

		match d {
			Ok(None) | Err(_) => {
				return Ok(AssumptionCheckOutcome::BadRequest);
			}
			Ok(Some(d)) => d,
		}
	};

	let persisted_validation_data_hash = validation_data.hash();

	SubsystemResult::Ok(if descriptor.persisted_validation_data_hash == persisted_validation_data_hash {
		let (code_tx, code_rx) = oneshot::channel();
		let validation_code = runtime_api_request(
			ctx,
			descriptor.relay_parent,
			RuntimeApiRequest::ValidationCode(
				descriptor.para_id,
				assumption,
				code_tx,
			),
			code_rx,
		).await?;

		match validation_code {
			Ok(None) | Err(_) => AssumptionCheckOutcome::BadRequest,
			Ok(Some(v)) => AssumptionCheckOutcome::Matches(validation_data, v),
		}
	} else {
		AssumptionCheckOutcome::DoesNotMatch
	})
}

async fn find_assumed_validation_data(
	ctx: &mut impl SubsystemContext<Message = CandidateValidationMessage>,
	descriptor: &CandidateDescriptor,
) -> SubsystemResult<AssumptionCheckOutcome> {
	// The candidate descriptor has a `persisted_validation_data_hash` which corresponds to
	// one of up to two possible values that we can derive from the state of the
	// relay-parent. We can fetch these values by getting the persisted validation data
	// based on the different `OccupiedCoreAssumption`s.

	const ASSUMPTIONS: &[OccupiedCoreAssumption] = &[
		OccupiedCoreAssumption::Included,
		OccupiedCoreAssumption::TimedOut,
		// `TimedOut` and `Free` both don't perform any speculation and therefore should be the same
		// for our purposes here. In other words, if `TimedOut` matched then the `Free` must be
		// matched as well.
	];

	// Consider running these checks in parallel to reduce validation latency.
	for assumption in ASSUMPTIONS {
		let outcome = check_assumption_validation_data(ctx, descriptor, *assumption).await?;

		match outcome {
			AssumptionCheckOutcome::Matches(_, _) => return Ok(outcome),
			AssumptionCheckOutcome::BadRequest => return Ok(outcome),
			AssumptionCheckOutcome::DoesNotMatch => continue,
		}
	}

	Ok(AssumptionCheckOutcome::DoesNotMatch)
}

async fn spawn_validate_from_chain_state(
	ctx: &mut impl SubsystemContext<Message = CandidateValidationMessage>,
	validation_host: &mut ValidationHost,
	descriptor: CandidateDescriptor,
	pov: Arc<PoV>,
	metrics: &Metrics,
) -> SubsystemResult<Result<ValidationResult, ValidationFailed>> {
	let (validation_data, validation_code) =
		match find_assumed_validation_data(ctx, &descriptor).await? {
			AssumptionCheckOutcome::Matches(validation_data, validation_code) => {
				(validation_data, validation_code)
			}
			AssumptionCheckOutcome::DoesNotMatch => {
				// If neither the assumption of the occupied core having the para included or the assumption
				// of the occupied core timing out are valid, then the persisted_validation_data_hash in the descriptor
				// is not based on the relay parent and is thus invalid.
				return Ok(Ok(ValidationResult::Invalid(InvalidCandidate::BadParent)));
			}
			AssumptionCheckOutcome::BadRequest => {
				return Ok(Err(ValidationFailed("Assumption Check: Bad request".into())));
			}
		};

	let validation_result = validate_candidate_exhaustive(
		validation_host,
		validation_data,
		validation_code,
		descriptor.clone(),
		pov,
		metrics,
	)
	.await;

	if let Ok(Ok(ValidationResult::Valid(ref outputs, _))) = validation_result {
		let (tx, rx) = oneshot::channel();
		match runtime_api_request(
			ctx,
			descriptor.relay_parent,
			RuntimeApiRequest::CheckValidationOutputs(descriptor.para_id, outputs.clone(), tx),
			rx,
		)
		.await?
		{
			Ok(true) => {}
			Ok(false) => {
				return Ok(Ok(ValidationResult::Invalid(
					InvalidCandidate::InvalidOutputs,
				)));
			}
			Err(_) => {
				return Ok(Err(ValidationFailed("Check Validation Outputs: Bad request".into())));
			}
		}
	}

	validation_result
}

async fn validate_candidate_exhaustive(
	mut validation_backend: impl ValidationBackend,
	persisted_validation_data: PersistedValidationData,
	validation_code: ValidationCodeAndHash,
	descriptor: CandidateDescriptor,
	pov: Arc<PoV>,
	metrics: &Metrics,
) -> SubsystemResult<Result<ValidationResult, ValidationFailed>> {
	let _timer = metrics.time_validate_candidate_exhaustive();

	if let Err(e) = perform_basic_checks(
		&descriptor,
		persisted_validation_data.max_pov_size,
		&*pov,
		validation_code.hash(),
	) {
		return Ok(Ok(ValidationResult::Invalid(e)));
	}

	let raw_validation_code = match sp_maybe_compressed_blob::decompress(
		&validation_code.code().0,
		VALIDATION_CODE_BOMB_LIMIT,
	) {
		Ok(code) => code,
		Err(e) => {
			tracing::debug!(target: LOG_TARGET, err=?e, "Invalid validation code");

			// If the validation code is invalid, the candidate certainly is.
			return Ok(Ok(ValidationResult::Invalid(InvalidCandidate::CodeDecompressionFailure)));
		}
	};

	let raw_block_data = match sp_maybe_compressed_blob::decompress(
		&pov.block_data.0,
		POV_BOMB_LIMIT,
	) {
		Ok(block_data) => BlockData(block_data.to_vec()),
		Err(e) => {
			tracing::debug!(target: LOG_TARGET, err=?e, "Invalid PoV code");

			// If the PoV is invalid, the candidate certainly is.
			return Ok(Ok(ValidationResult::Invalid(InvalidCandidate::PoVDecompressionFailure)));
		}
	};

	let params = ValidationParams {
		parent_head: persisted_validation_data.parent_head.clone(),
		block_data: raw_block_data,
		relay_parent_number: persisted_validation_data.relay_parent_number,
		relay_parent_storage_root: persisted_validation_data.relay_parent_storage_root,
	};

	let result =
		validation_backend.validate_candidate(
			raw_validation_code.to_vec(),
			params
		)
		.await;

	if let Err(ref e) = result {
		tracing::debug!(
			target: LOG_TARGET,
			error = ?e,
			"Failed to validate candidate",
		);
	}

	let result = match result {
		Err(ValidationError::InternalError(e)) => Err(ValidationFailed(e)),

		Err(ValidationError::InvalidCandidate(WasmInvalidCandidate::HardTimeout)) =>
			Ok(ValidationResult::Invalid(InvalidCandidate::Timeout)),
		Err(ValidationError::InvalidCandidate(WasmInvalidCandidate::WorkerReportedError(e))) =>
			Ok(ValidationResult::Invalid(InvalidCandidate::ExecutionError(e))),
		Err(ValidationError::InvalidCandidate(WasmInvalidCandidate::AmbigiousWorkerDeath)) =>
			Ok(ValidationResult::Invalid(InvalidCandidate::ExecutionError("ambigious worker death".to_string()))),

		Ok(res) => {
			if res.head_data.hash() != descriptor.para_head {
				Ok(ValidationResult::Invalid(InvalidCandidate::ParaHeadHashMismatch))
			} else {
				let outputs = CandidateCommitments {
					head_data: res.head_data,
					upward_messages: res.upward_messages,
					horizontal_messages: res.horizontal_messages,
					new_validation_code: res.new_validation_code,
					processed_downward_messages: res.processed_downward_messages,
					hrmp_watermark: res.hrmp_watermark,
				};
				Ok(ValidationResult::Valid(outputs, persisted_validation_data))
			}
		}
	};

	Ok(result)
}

#[async_trait]
trait ValidationBackend {
	async fn validate_candidate(
		&mut self,
		raw_validation_code: Vec<u8>,
		params: ValidationParams
	) -> Result<WasmValidationResult, ValidationError>;
}

#[async_trait]
impl ValidationBackend for &'_ mut ValidationHost {
	async fn validate_candidate(
		&mut self,
		raw_validation_code: Vec<u8>,
		params: ValidationParams
	) -> Result<WasmValidationResult, ValidationError> {
		let (tx, rx) = oneshot::channel();
		if let Err(err) = self.execute_pvf(
			Pvf::from_code(raw_validation_code),
			params.encode(),
			polkadot_node_core_pvf::Priority::Normal,
			tx,
		).await {
			return Err(ValidationError::InternalError(format!("cannot send pvf to the validation host: {:?}", err)));
		}

		let validation_result = rx
			.await
			.map_err(|_| ValidationError::InternalError("validation was cancelled".into()))?;

		validation_result
	}
}

/// Does basic checks of a candidate. Provide the encoded PoV-block. Returns `Ok` if basic checks
/// are passed, `Err` otherwise.
fn perform_basic_checks(
	candidate: &CandidateDescriptor,
	max_pov_size: u32,
	pov: &PoV,
	validation_code_hash: &ValidationCodeHash,
) -> Result<(), InvalidCandidate> {
	let pov_hash = pov.hash();

	let encoded_pov_size = pov.encoded_size();
	if encoded_pov_size > max_pov_size as usize {
		return Err(InvalidCandidate::ParamsTooLarge(encoded_pov_size as u64));
	}

	if pov_hash != candidate.pov_hash {
		return Err(InvalidCandidate::PoVHashMismatch);
	}

	if *validation_code_hash != candidate.validation_code_hash {
		return Err(InvalidCandidate::CodeHashMismatch);
	}

	if let Err(()) = candidate.check_collator_signature() {
		return Err(InvalidCandidate::BadSignature);
	}

	Ok(())
}

#[derive(Clone)]
struct MetricsInner {
	validation_requests: prometheus::CounterVec<prometheus::U64>,
	validate_from_chain_state: prometheus::Histogram,
	validate_from_exhaustive: prometheus::Histogram,
	validate_candidate_exhaustive: prometheus::Histogram,
}

/// Candidate validation metrics.
#[derive(Default, Clone)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_validation_event(&self, event: &Result<ValidationResult, ValidationFailed>) {
		if let Some(metrics) = &self.0 {
			match event {
				Ok(ValidationResult::Valid(_, _)) => {
					metrics.validation_requests.with_label_values(&["valid"]).inc();
				},
				Ok(ValidationResult::Invalid(_)) => {
					metrics.validation_requests.with_label_values(&["invalid"]).inc();
				},
				Err(_) => {
					metrics.validation_requests.with_label_values(&["validation failure"]).inc();
				},
			}
		}
	}

	/// Provide a timer for `validate_from_chain_state` which observes on drop.
	fn time_validate_from_chain_state(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.validate_from_chain_state.start_timer())
	}

	/// Provide a timer for `validate_from_exhaustive` which observes on drop.
	fn time_validate_from_exhaustive(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.validate_from_exhaustive.start_timer())
	}

	/// Provide a timer for `validate_candidate_exhaustive` which observes on drop.
	fn time_validate_candidate_exhaustive(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.validate_candidate_exhaustive.start_timer())
	}
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry) -> Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			validation_requests: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"parachain_validation_requests_total",
						"Number of validation requests served.",
					),
					&["validity"],
				)?,
				registry,
			)?,
			validate_from_chain_state: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_candidate_validation_validate_from_chain_state",
						"Time spent within `candidate_validation::validate_from_chain_state`",
					)
				)?,
				registry,
			)?,
			validate_from_exhaustive: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_candidate_validation_validate_from_exhaustive",
						"Time spent within `candidate_validation::validate_from_exhaustive`",
					)
				)?,
				registry,
			)?,
			validate_candidate_exhaustive: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_candidate_validation_validate_candidate_exhaustive",
						"Time spent within `candidate_validation::validate_candidate_exhaustive`",
					)
				)?,
				registry,
			)?,
		};
		Ok(Metrics(Some(metrics)))
	}
}
