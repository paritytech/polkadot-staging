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

//! The collation generation subsystem is the interface between polkadot and the collators.

#![deny(missing_docs)]

use futures::{
	channel::mpsc,
	future::FutureExt,
	join,
	select,
	sink::SinkExt,
	stream::StreamExt,
};
use polkadot_node_primitives::{
	CollationGenerationConfig, AvailableData, PoV,
};
use polkadot_node_subsystem::{
	ActiveLeavesUpdate,
	messages::{AllMessages, CollationGenerationMessage, CollatorProtocolMessage},
	SpawnedSubsystem, SubsystemContext, SubsystemResult,
	SubsystemError, FromOverseer, OverseerSignal,
	overseer,
};
use polkadot_node_subsystem_util::{
	request_availability_cores, request_persisted_validation_data,
	request_validators, request_validation_code,
	metrics::{self, prometheus},
};
use polkadot_primitives::v1::{
	collator_signature_payload, CandidateCommitments,
	CandidateDescriptor, CandidateReceipt, CoreState, Hash, OccupiedCoreAssumption,
	PersistedValidationData,
};
use parity_scale_codec::Encode;
use sp_core::crypto::Pair;
use std::sync::Arc;

mod error;

#[cfg(test)]
mod tests;

const LOG_TARGET: &'static str = "parachain::collation-generation";

/// Collation Generation Subsystem
pub struct CollationGenerationSubsystem {
	config: Option<Arc<CollationGenerationConfig>>,
	metrics: Metrics,
}

impl CollationGenerationSubsystem {
	/// Create a new instance of the `CollationGenerationSubsystem`.
	pub fn new(metrics: Metrics) -> Self {
		Self {
			config: None,
			metrics,
		}
	}

	/// Run this subsystem
	///
	/// Conceptually, this is very simple: it just loops forever.
	///
	/// - On incoming overseer messages, it starts or stops jobs as appropriate.
	/// - On other incoming messages, if they can be converted into Job::ToJob and
	///   include a hash, then they're forwarded to the appropriate individual job.
	/// - On outgoing messages from the jobs, it forwards them to the overseer.
	///
	/// If `err_tx` is not `None`, errors are forwarded onto that channel as they occur.
	/// Otherwise, most are logged and then discarded.
	async fn run<Context>(mut self, mut ctx: Context)
	where
		Context: overseer::SubsystemContext<Message = CollationGenerationMessage>,
	{
		// when we activate new leaves, we spawn a bunch of sub-tasks, each of which is
		// expected to generate precisely one message. We don't want to block the main loop
		// at any point waiting for them all, so instead, we create a channel on which they can
		// send those messages. We can then just monitor the channel and forward messages on it
		// to the overseer here, via the context.
		let (sender, receiver) = mpsc::channel(0);

		let mut receiver = receiver.fuse();
		loop {
			select! {
				incoming = ctx.recv().fuse() => {
					if self.handle_incoming::<Context>(incoming, &mut ctx, &sender).await {
						break;
					}
				},
				msg = receiver.next() => {
					if let Some(msg) = msg {
						ctx.send_message(msg).await;
					}
				},
			}
		}
	}

	// handle an incoming message. return true if we should break afterwards.
	// note: this doesn't strictly need to be a separate function; it's more an administrative function
	// so that we don't clutter the run loop. It could in principle be inlined directly into there.
	// it should hopefully therefore be ok that it's an async function mutably borrowing self.
	async fn handle_incoming<Context>(
		&mut self,
		incoming: SubsystemResult<FromOverseer<<Context as SubsystemContext>::Message>>,
		ctx: &mut Context,
		sender: &mpsc::Sender<AllMessages>,
	) -> bool
	where
		Context: overseer::SubsystemContext<Message = CollationGenerationMessage>,
	{
		match incoming {
			Ok(FromOverseer::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate { activated, .. }))) => {
				// follow the procedure from the guide
				if let Some(config) = &self.config {
					let metrics = self.metrics.clone();
					if let Err(err) = handle_new_activations(
						config.clone(),
						activated.into_iter().map(|v| v.hash),
						ctx,
						metrics,
						sender,
					).await {
						tracing::warn!(target: LOG_TARGET, err = ?err, "failed to handle new activations");
					}
				}

				false
			}
			Ok(FromOverseer::Signal(OverseerSignal::Conclude)) => true,
			Ok(FromOverseer::Communication {
				msg: CollationGenerationMessage::Initialize(config),
			}) => {
				if self.config.is_some() {
					tracing::error!(target: LOG_TARGET, "double initialization");
				} else {
					self.config = Some(Arc::new(config));
				}
				false
			}
			Ok(FromOverseer::Signal(OverseerSignal::BlockFinalized(..))) => false,
			Err(err) => {
				tracing::error!(
					target: LOG_TARGET,
					err = ?err,
					"error receiving message from subsystem context: {:?}",
					err
				);
				true
			}
		}
	}
}

impl<Context> overseer::overseer::Subsystem<Context> for CollationGenerationSubsystem
where
	Context: overseer::SubsystemContext<Message = CollationGenerationMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = async move {
			self.run(ctx).await;
			Ok(())
		}.boxed();

		SpawnedSubsystem {
			name: "collation-generation-subsystem",
			future,
		}
	}
}

async fn handle_new_activations<Context: SubsystemContext>(
	config: Arc<CollationGenerationConfig>,
	activated: impl IntoIterator<Item = Hash>,
	ctx: &mut Context,
	metrics: Metrics,
	sender: &mpsc::Sender<AllMessages>,
) -> crate::error::Result<()> {
	// follow the procedure from the guide:
	// https://w3f.github.io/parachain-implementers-guide/node/collators/collation-generation.html

	let _overall_timer = metrics.time_new_activations();

	for relay_parent in activated {
		let _relay_parent_timer = metrics.time_new_activations_relay_parent();

		let (availability_cores, validators) = join!(
			request_availability_cores(relay_parent, ctx.sender()).await,
			request_validators(relay_parent, ctx.sender()).await,
		);

		let availability_cores = availability_cores??;
		let n_validators = validators??.len();

		for (core_idx, core) in availability_cores.into_iter().enumerate() {
			let _availability_core_timer = metrics.time_new_activations_availability_core();

			let (scheduled_core, assumption) = match core {
				CoreState::Scheduled(scheduled_core) => {
					(scheduled_core, OccupiedCoreAssumption::Free)
				}
				CoreState::Occupied(_occupied_core) => {
					// TODO: https://github.com/paritytech/polkadot/issues/1573
					tracing::trace!(
						target: LOG_TARGET,
						core_idx = %core_idx,
						relay_parent = ?relay_parent,
						"core is occupied. Keep going.",
					);
					continue;
				}
				CoreState::Free => {
					tracing::trace!(
						target: LOG_TARGET,
						core_idx = %core_idx,
						"core is free. Keep going.",
					);
					continue
				}
			};

			if scheduled_core.para_id != config.para_id {
				tracing::trace!(
					target: LOG_TARGET,
					core_idx = %core_idx,
					relay_parent = ?relay_parent,
					our_para = %config.para_id,
					their_para = %scheduled_core.para_id,
					"core is not assigned to our para. Keep going.",
				);
				continue;
			}

			// we get validation data and validation code synchronously for each core instead of
			// within the subtask loop, because we have only a single mutable handle to the
			// context, so the work can't really be distributed

			let validation_data = match request_persisted_validation_data(
				relay_parent,
				scheduled_core.para_id,
				assumption,
				ctx.sender(),
			)
			.await
			.await??
			{
				Some(v) => v,
				None => {
					tracing::trace!(
						target: LOG_TARGET,
						core_idx = %core_idx,
						relay_parent = ?relay_parent,
						our_para = %config.para_id,
						their_para = %scheduled_core.para_id,
						"validation data is not available",
					);
					continue
				}
			};

			let validation_code = match request_validation_code(
				relay_parent,
				scheduled_core.para_id,
				assumption,
				ctx.sender(),
			)
			.await
			.await??
			{
				Some(v) => v,
				None => {
					tracing::trace!(
						target: LOG_TARGET,
						core_idx = %core_idx,
						relay_parent = ?relay_parent,
						our_para = %config.para_id,
						their_para = %scheduled_core.para_id,
						"validation code is not available",
					);
					continue
				}
			};
			let validation_code_hash = validation_code.hash();

			let task_config = config.clone();
			let mut task_sender = sender.clone();
			let metrics = metrics.clone();
			ctx.spawn("collation generation collation builder", Box::pin(async move {
				let persisted_validation_data_hash = validation_data.hash();

				let (collation, result_sender) = match (task_config.collator)(relay_parent, &validation_data).await {
					Some(collation) => collation.into_inner(),
					None => {
						tracing::debug!(
							target: LOG_TARGET,
							para_id = %scheduled_core.para_id,
							"collator returned no collation on collate",
						);
						return
					}
				};

				// Apply compression to the block data.
				let pov = {
					let pov = polkadot_node_primitives::maybe_compress_pov(collation.proof_of_validity);
					let encoded_size = pov.encoded_size();

					// As long as `POV_BOMB_LIMIT` is at least `max_pov_size`, this ensures
					// that honest collators never produce a PoV which is uncompressed.
					//
					// As such, honest collators never produce an uncompressed PoV which starts with
					// a compression magic number, which would lead validators to reject the collation.
					if encoded_size > validation_data.max_pov_size as usize {
						tracing::debug!(
							target: LOG_TARGET,
							para_id = %scheduled_core.para_id,
							size = encoded_size,
							max_size = validation_data.max_pov_size,
							"PoV exceeded maximum size"
						);

						return
					}

					pov
				};

				let pov_hash = pov.hash();

				let signature_payload = collator_signature_payload(
					&relay_parent,
					&scheduled_core.para_id,
					&persisted_validation_data_hash,
					&pov_hash,
					&validation_code_hash,
				);

				let erasure_root = match erasure_root(
					n_validators,
					validation_data,
					pov.clone(),
				) {
					Ok(erasure_root) => erasure_root,
					Err(err) => {
						tracing::error!(
							target: LOG_TARGET,
							para_id = %scheduled_core.para_id,
							err = ?err,
							"failed to calculate erasure root",
						);
						return
					}
				};

				let commitments = CandidateCommitments {
					upward_messages: collation.upward_messages,
					horizontal_messages: collation.horizontal_messages,
					new_validation_code: collation.new_validation_code,
					head_data: collation.head_data,
					processed_downward_messages: collation.processed_downward_messages,
					hrmp_watermark: collation.hrmp_watermark,
				};

				let ccr = CandidateReceipt {
					commitments_hash: commitments.hash(),
					descriptor: CandidateDescriptor {
						signature: task_config.key.sign(&signature_payload),
						para_id: scheduled_core.para_id,
						relay_parent,
						collator: task_config.key.public(),
						persisted_validation_data_hash,
						pov_hash,
						erasure_root,
						para_head: commitments.head_data.hash(),
						validation_code_hash: validation_code_hash,
					},
				};

				tracing::debug!(
					target: LOG_TARGET,
					candidate_hash = ?ccr.hash(),
					?pov_hash,
					?relay_parent,
					para_id = %scheduled_core.para_id,
					"candidate is generated",
				);
				metrics.on_collation_generated();

				if let Err(err) = task_sender.send(AllMessages::CollatorProtocol(
					CollatorProtocolMessage::DistributeCollation(ccr, pov, result_sender)
				)).await {
					tracing::warn!(
						target: LOG_TARGET,
						para_id = %scheduled_core.para_id,
						err = ?err,
						"failed to send collation result",
					);
				}
			}))?;
		}
	}

	Ok(())
}

fn erasure_root(
	n_validators: usize,
	persisted_validation: PersistedValidationData,
	pov: PoV,
) -> crate::error::Result<Hash> {
	let available_data = AvailableData {
		validation_data: persisted_validation,
		pov: Arc::new(pov),
	};

	let chunks = polkadot_erasure_coding::obtain_chunks_v1(n_validators, &available_data)?;
	Ok(polkadot_erasure_coding::branches(&chunks).root())
}

#[derive(Clone)]
struct MetricsInner {
	collations_generated_total: prometheus::Counter<prometheus::U64>,
	new_activations_overall: prometheus::Histogram,
	new_activations_per_relay_parent: prometheus::Histogram,
	new_activations_per_availability_core: prometheus::Histogram,
}

/// CollationGenerationSubsystem metrics.
#[derive(Default, Clone)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_collation_generated(&self) {
		if let Some(metrics) = &self.0 {
			metrics.collations_generated_total.inc();
		}
	}

	/// Provide a timer for new activations which updates on drop.
	fn time_new_activations(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.new_activations_overall.start_timer())
	}

	/// Provide a timer per relay parents which updates on drop.
	fn time_new_activations_relay_parent(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.new_activations_per_relay_parent.start_timer())
	}

	/// Provide a timer per availability core which updates on drop.
	fn time_new_activations_availability_core(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.new_activations_per_availability_core.start_timer())
	}
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry) -> Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			collations_generated_total: prometheus::register(
				prometheus::Counter::new(
					"parachain_collations_generated_total",
					"Number of collations generated."
				)?,
				registry,
			)?,
			new_activations_overall: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_collation_generation_new_activations",
						"Time spent within fn handle_new_activations",
					)
				)?,
				registry,
			)?,
			new_activations_per_relay_parent: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_collation_generation_per_relay_parent",
						"Time spent handling a particular relay parent within fn handle_new_activations"
					)
				)?,
				registry,
			)?,
			new_activations_per_availability_core: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_collation_generation_per_availability_core",
						"Time spent handling a particular availability core for a relay parent in fn handle_new_activations",
					)
				)?,
				registry,
			)?,
		};
		Ok(Metrics(Some(metrics)))
	}
}
