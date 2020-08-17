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
	channel::{mpsc, oneshot},
	future::FutureExt,
	select,
	sink::SinkExt,
	stream::StreamExt,
};
use polkadot_node_primitives::CollationGenerationConfig;
use polkadot_node_subsystem::{
	errors::RuntimeApiError,
	messages::{AllMessages, CollationGenerationMessage, CollatorProtocolMessage},
	FromOverseer, SpawnedSubsystem, Subsystem, SubsystemContext, SubsystemError, SubsystemResult,
};
use polkadot_node_subsystem_util::{
	self as util, request_availability_cores_ctx, request_global_validation_data_ctx,
	request_local_validation_data_ctx, request_validators_ctx,
};
use polkadot_primitives::v1::{
	collator_signature_payload, validation_data_hash, AvailableData, CandidateCommitments,
	CandidateDescriptor, CandidateReceipt, CoreState, GlobalValidationData, Hash,
	LocalValidationData, OccupiedCoreAssumption, PoV,
};
use sp_core::crypto::Pair;
use std::sync::Arc;

/// Collation Generation Subsystem
pub struct CollationGenerationSubsystem {
	config: Option<Arc<CollationGenerationConfig>>,
}

impl CollationGenerationSubsystem {
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
		Context: SubsystemContext<Message = CollationGenerationMessage>,
	{
		// when we activate new leaves, we spawn a bunch of sub-tasks, each of which is
		// expected to generate precisely one message. We don't want to block the main loop
		// at any point waiting for them all, so instead, we create a channel on which they can
		// send those messages. We can then just monitor the channel and forward messages on it
		// to the overseer here, via the context.
		let (sender, mut receiver) = mpsc::channel(0);

		loop {
			select! {
				incoming = ctx.recv().fuse() => {
					if self.handle_incoming::<Context>(incoming, &mut ctx, &sender).await {
						break;
					}
				},
				msg = receiver.next().fuse() => {
					if let Some(msg) = msg {
						if let Err(err) = ctx.send_message(msg).await {
							log::warn!(target: "collation_generation", "failed to forward message to overseer: {:?}", err);
							break;
						}
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
		incoming: SubsystemResult<FromOverseer<Context::Message>>,
		ctx: &mut Context,
		sender: &mpsc::Sender<AllMessages>,
	) -> bool
	where
		Context: SubsystemContext<Message = CollationGenerationMessage>,
	{
		use polkadot_node_subsystem::ActiveLeavesUpdate;
		use polkadot_node_subsystem::FromOverseer::{Communication, Signal};
		use polkadot_node_subsystem::OverseerSignal::{ActiveLeaves, BlockFinalized, Conclude};

		match incoming {
			Ok(Signal(ActiveLeaves(ActiveLeavesUpdate { activated, .. }))) => {
				// follow the procedure from the guide
				if let Some(config) = &self.config {
					if let Err(err) =
						handle_new_activations(config.clone(), &activated, ctx, sender).await
					{
						log::warn!(target: "collation_generation", "failed to handle new activations: {:?}", err);
						return true;
					};
				}
				false
			}
			Ok(Signal(Conclude)) => true,
			Ok(Communication {
				msg: CollationGenerationMessage::Initialize(config),
			}) => {
				if self.config.is_some() {
					log::warn!(target: "collation_generation", "double initialization");
					true
				} else {
					self.config = Some(Arc::new(config));
					false
				}
			}
			Ok(Signal(BlockFinalized(_))) => false,
			Err(err) => {
				log::error!(target: "collation_generation", "error receiving message from subsystem context: {:?}", err);
				true
			}
		}
	}
}

impl<Context> Subsystem<Context> for CollationGenerationSubsystem
where
	Context: SubsystemContext<Message = CollationGenerationMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let subsystem = CollationGenerationSubsystem { config: None };

		let future = Box::pin(subsystem.run(ctx));

		SpawnedSubsystem {
			name: "CollationGenerationSubsystem",
			future,
		}
	}
}

#[derive(Debug, derive_more::From)]
enum Error {
	#[from]
	Subsystem(SubsystemError),
	#[from]
	OneshotRecv(oneshot::Canceled),
	#[from]
	Runtime(RuntimeApiError),
	#[from]
	Util(util::Error),
	#[from]
	Erasure(polkadot_erasure_coding::Error),
}

type Result<T> = std::result::Result<T, Error>;

async fn handle_new_activations<Context: SubsystemContext>(
	config: Arc<CollationGenerationConfig>,
	activated: &[Hash],
	ctx: &mut Context,
	sender: &mpsc::Sender<AllMessages>,
) -> Result<()> {
	// follow the procedure from the guide:
	// https://w3f.github.io/parachain-implementers-guide/node/collators/collation-generation.html

	for relay_parent in activated.iter().copied() {
		let global_validation_data = request_global_validation_data_ctx(relay_parent, ctx)
			.await?
			.await??;

		let availability_cores = request_availability_cores_ctx(relay_parent, ctx)
			.await?
			.await??;

		let n_validators = request_validators_ctx(relay_parent, ctx)
			.await?
			.await??
			.len();

		for core in availability_cores {
			let (scheduled_core, assumption) = match core {
				CoreState::Scheduled(scheduled_core) => {
					(scheduled_core, OccupiedCoreAssumption::Free)
				}
				CoreState::Occupied(_occupied_core) => {
					// TODO: https://github.com/paritytech/polkadot/issues/1573
					continue;
				}
				_ => continue,
			};

			if scheduled_core.para_id != config.para_id {
				continue;
			}

			// we get local validation data synchronously for each core instead of within the subtask loop,
			// because we have only a single mutable handle to the context, so the work can't really be distributed
			let local_validation_data = match request_local_validation_data_ctx(
				relay_parent,
				scheduled_core.para_id,
				assumption,
				ctx,
			)
			.await?
			.await??
			{
				Some(local_validation_data) => local_validation_data,
				None => continue,
			};

			let task_global_validation_data = global_validation_data.clone();
			let task_config = config.clone();
			let mut task_sender = sender.clone();
			ctx.spawn("collation generation collation builder", Box::pin(async move {
				let validation_data_hash =
					validation_data_hash(&task_global_validation_data, &local_validation_data);

				let collation = (task_config.collator)(&task_global_validation_data, &local_validation_data).await;

				let pov_hash = collation.proof_of_validity.hash();

				let signature_payload = collator_signature_payload(
					&relay_parent,
					&scheduled_core.para_id,
					&validation_data_hash,
					&pov_hash,
				);

				let erasure_root = match erasure_root(n_validators, local_validation_data, task_global_validation_data, collation.proof_of_validity.clone()) {
					Ok(erasure_root) => erasure_root,
					Err(err) => {
						log::error!(target: "collation_generation", "failed to calculate erasure root for para_id {}: {:?}", scheduled_core.para_id, err);
						return
					}
				};

				let commitments = CandidateCommitments {
					fees: collation.fees,
					upward_messages: collation.upward_messages,
					new_validation_code: collation.new_validation_code,
					head_data: collation.head_data,
					erasure_root,
				};

				let ccr = CandidateReceipt {
					commitments_hash: commitments.hash(),
					descriptor: CandidateDescriptor {
						signature: task_config.key.sign(&signature_payload),
						para_id: scheduled_core.para_id,
						relay_parent,
						collator: task_config.key.public(),
						validation_data_hash,
						pov_hash,
					},
				};

				if let Err(err) = task_sender.send(AllMessages::CollatorProtocol(
					CollatorProtocolMessage::DistributeCollation(ccr, collation.proof_of_validity)
				)).await {
					log::warn!(target: "collation_generation", "failed to send collation result for para_id {}: {:?}", scheduled_core.para_id, err);
				}
			})).await?;
		}
	}

	Ok(())
}

fn erasure_root(
	n_validators: usize,
	local_validation_data: LocalValidationData,
	global_validation_data: GlobalValidationData,
	pov: PoV,
) -> Result<Hash> {
	let omitted_validation = polkadot_primitives::v1::OmittedValidationData {
		global_validation: global_validation_data,
		local_validation: local_validation_data,
	};

	let available_data = AvailableData {
		omitted_validation,
		pov,
	};

	let chunks = polkadot_erasure_coding::obtain_chunks_v1(n_validators, &available_data)?;
	Ok(polkadot_erasure_coding::branches(&chunks).root())
}

#[cfg(test)]
mod tests {
	mod handle_new_activations {
		use super::super::*;
		use futures::{
			lock::Mutex,
			task::{Context as FuturesContext, Poll},
			Future,
		};
		use polkadot_node_primitives::Collation;
		use polkadot_node_subsystem::messages::{
			AllMessages, RuntimeApiMessage, RuntimeApiRequest,
		};
		use polkadot_node_subsystem_test_helpers::{
			subsystem_test_harness, TestSubsystemContextHandle,
		};
		use polkadot_primitives::v1::{
			BlockData, BlockNumber, CollatorPair, GlobalValidationData, Id as ParaId,
			LocalValidationData, PoV, ScheduledCore,
		};
		use std::pin::Pin;

		fn test_collation() -> Collation {
			Collation {
				fees: Default::default(),
				upward_messages: Default::default(),
				new_validation_code: Default::default(),
				head_data: Default::default(),
				proof_of_validity: PoV {
					block_data: BlockData(Vec::new()),
				},
			}
		}

		// Box<dyn Future<Output = Collation> + Unpin + Send
		struct TestCollator;

		impl Future for TestCollator {
			type Output = Collation;

			fn poll(self: Pin<&mut Self>, _cx: &mut FuturesContext) -> Poll<Self::Output> {
				Poll::Ready(test_collation())
			}
		}

		impl Unpin for TestCollator {}

		fn test_config<Id: Into<ParaId>>(para_id: Id) -> Arc<CollationGenerationConfig> {
			Arc::new(CollationGenerationConfig {
				key: CollatorPair::generate().0,
				collator: Box::new(|_gvd: &GlobalValidationData, _lvd: &LocalValidationData| {
					Box::new(TestCollator)
				}),
				para_id: para_id.into(),
			})
		}

		fn scheduled_core_for<Id: Into<ParaId>>(para_id: Id) -> ScheduledCore {
			ScheduledCore {
				para_id: para_id.into(),
				collator: None,
			}
		}

		#[test]
		fn requests_validation_and_availability_per_relay_parent() {
			let activated_hashes: Vec<Hash> = vec![
				[1; 32].into(),
				[4; 32].into(),
				[9; 32].into(),
				[16; 32].into(),
			];

			let requested_validation_data = Arc::new(Mutex::new(Vec::new()));
			let requested_availability_cores = Arc::new(Mutex::new(Vec::new()));

			let overseer_requested_validation_data = requested_validation_data.clone();
			let overseer_requested_availability_cores = requested_availability_cores.clone();
			let overseer = |mut handle: TestSubsystemContextHandle<CollationGenerationMessage>| async move {
				loop {
					match handle.try_recv().await {
						None => break,
						Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(hash, RuntimeApiRequest::GlobalValidationData(tx)))) => {
							overseer_requested_validation_data.lock().await.push(hash);
							tx.send(Ok(Default::default())).unwrap();
						}
						Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(hash, RuntimeApiRequest::AvailabilityCores(tx)))) => {
							overseer_requested_availability_cores.lock().await.push(hash);
							tx.send(Ok(vec![])).unwrap();
						}
						Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(_hash, RuntimeApiRequest::Validators(tx)))) => {
							tx.send(Ok(vec![Default::default(); 3])).unwrap();
						}
						Some(msg) => panic!("didn't expect any other overseer requests given no availability cores; got {:?}", msg),
					}
				}
			};

			let (tx, _rx) = mpsc::channel(0);

			let subsystem_activated_hashes = activated_hashes.clone();
			subsystem_test_harness(overseer, |mut ctx| async move {
				handle_new_activations(
					test_config(123),
					&subsystem_activated_hashes,
					&mut ctx,
					&tx,
				)
				.await
				.unwrap();
			});

			let mut requested_validation_data = Arc::try_unwrap(requested_validation_data)
				.expect("overseer should have shut down by now")
				.into_inner();
			requested_validation_data.sort();
			let mut requested_availability_cores = Arc::try_unwrap(requested_availability_cores)
				.expect("overseer should have shut down by now")
				.into_inner();
			requested_availability_cores.sort();

			assert_eq!(requested_validation_data, activated_hashes);
			assert_eq!(requested_availability_cores, activated_hashes);
		}

		#[test]
		fn requests_local_validation_for_scheduled_matches() {
			let activated_hashes: Vec<Hash> = vec![
				Hash::repeat_byte(1),
				Hash::repeat_byte(4),
				Hash::repeat_byte(9),
				Hash::repeat_byte(16),
			];

			let requested_local_validation_data = Arc::new(Mutex::new(Vec::new()));

			let overseer_requested_local_validation_data = requested_local_validation_data.clone();
			let overseer = |mut handle: TestSubsystemContextHandle<CollationGenerationMessage>| async move {
				loop {
					match handle.try_recv().await {
						None => break,
						Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
							_hash,
							RuntimeApiRequest::GlobalValidationData(tx),
						))) => {
							tx.send(Ok(Default::default())).unwrap();
						}
						Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
							hash,
							RuntimeApiRequest::AvailabilityCores(tx),
						))) => {
							tx.send(Ok(vec![
								CoreState::Free,
								// this is weird, see explanation below
								CoreState::Scheduled(scheduled_core_for(
									(hash.as_fixed_bytes()[0] * 4) as u32,
								)),
								CoreState::Scheduled(scheduled_core_for(
									(hash.as_fixed_bytes()[0] * 5) as u32,
								)),
							]))
							.unwrap();
						}
						Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
							hash,
							RuntimeApiRequest::LocalValidationData(
								_para_id,
								_occupied_core_assumption,
								tx,
							),
						))) => {
							overseer_requested_local_validation_data
								.lock()
								.await
								.push(hash);
							tx.send(Ok(Default::default())).unwrap();
						}
						Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
							_hash,
							RuntimeApiRequest::Validators(tx),
						))) => {
							tx.send(Ok(vec![Default::default(); 3])).unwrap();
						}
						Some(msg) => {
							panic!("didn't expect any other overseer requests; got {:?}", msg)
						}
					}
				}
			};

			let (tx, _rx) = mpsc::channel(0);

			subsystem_test_harness(overseer, |mut ctx| async move {
				handle_new_activations(test_config(16), &activated_hashes, &mut ctx, &tx)
					.await
					.unwrap();
			});

			let requested_local_validation_data = Arc::try_unwrap(requested_local_validation_data)
				.expect("overseer should have shut down by now")
				.into_inner();

			// the only activated hash should be from the 4 hash:
			// each activated hash generates two scheduled cores: one with its value * 4, one with its value * 5
			// given that the test configuration has a para_id of 16, there's only one way to get that value: with the 4
			// hash.
			assert_eq!(requested_local_validation_data, vec![[4; 32].into()]);
		}

		#[test]
		fn sends_distribute_collation_message() {
			let activated_hashes: Vec<Hash> = vec![
				Hash::repeat_byte(1),
				Hash::repeat_byte(4),
				Hash::repeat_byte(9),
				Hash::repeat_byte(16),
			];

			let overseer = |mut handle: TestSubsystemContextHandle<CollationGenerationMessage>| async move {
				loop {
					match handle.try_recv().await {
						None => break,
						Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
							_hash,
							RuntimeApiRequest::GlobalValidationData(tx),
						))) => {
							tx.send(Ok(Default::default())).unwrap();
						}
						Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
							hash,
							RuntimeApiRequest::AvailabilityCores(tx),
						))) => {
							tx.send(Ok(vec![
								CoreState::Free,
								// this is weird, see explanation below
								CoreState::Scheduled(scheduled_core_for(
									(hash.as_fixed_bytes()[0] * 4) as u32,
								)),
								CoreState::Scheduled(scheduled_core_for(
									(hash.as_fixed_bytes()[0] * 5) as u32,
								)),
							]))
							.unwrap();
						}
						Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
							_hash,
							RuntimeApiRequest::LocalValidationData(
								_para_id,
								_occupied_core_assumption,
								tx,
							),
						))) => {
							tx.send(Ok(Some(Default::default()))).unwrap();
						}
						Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
							_hash,
							RuntimeApiRequest::Validators(tx),
						))) => {
							tx.send(Ok(vec![Default::default(); 3])).unwrap();
						}
						Some(msg) => {
							panic!("didn't expect any other overseer requests; got {:?}", msg)
						}
					}
				}
			};

			let config = test_config(16);
			let subsystem_config = config.clone();

			let (tx, rx) = mpsc::channel(0);

			// empty vec doesn't allocate on the heap, so it's ok we throw it away
			let sent_messages = Arc::new(Mutex::new(Vec::new()));
			let subsystem_sent_messages = sent_messages.clone();
			subsystem_test_harness(overseer, |mut ctx| async move {
				handle_new_activations(subsystem_config, &activated_hashes, &mut ctx, &tx)
					.await
					.unwrap();

				std::mem::drop(tx);

				// collect all sent messages
				*subsystem_sent_messages.lock().await = rx.collect().await;
			});

			let sent_messages = Arc::try_unwrap(sent_messages)
				.expect("subsystem should have shut down by now")
				.into_inner();

			// we expect a single message to be sent, containing a candidate receipt.
			// we don't care too much about the commitments_hash right now, but let's ensure that we've calculated the
			// correct descriptor
			let expect_pov_hash = test_collation().proof_of_validity.hash();
			let expect_validation_data_hash =
				validation_data_hash::<BlockNumber>(&Default::default(), &Default::default());
			let expect_relay_parent = Hash::repeat_byte(4);
			let expect_payload = collator_signature_payload(
				&expect_relay_parent,
				&config.para_id,
				&expect_validation_data_hash,
				&expect_pov_hash,
			);
			let expect_descriptor = CandidateDescriptor {
				signature: config.key.sign(&expect_payload),
				para_id: config.para_id,
				relay_parent: expect_relay_parent,
				collator: config.key.public(),
				validation_data_hash: expect_validation_data_hash,
				pov_hash: expect_pov_hash,
			};

			assert_eq!(sent_messages.len(), 1);
			match &sent_messages[0] {
				AllMessages::CollatorProtocol(CollatorProtocolMessage::DistributeCollation(
					CandidateReceipt { descriptor, .. },
					_pov,
				)) => {
					// signature generation is non-deterministic, so we can't just assert that the
					// expected descriptor is correct. What we can do is validate that the produced
					// descriptor has a valid signature, then just copy in the generated signature
					// and check the rest of the fields for equality.
					assert!(CollatorPair::verify(
						&descriptor.signature,
						&collator_signature_payload(
							&descriptor.relay_parent,
							&descriptor.para_id,
							&descriptor.validation_data_hash,
							&descriptor.pov_hash,
						)
						.as_ref(),
						&descriptor.collator,
					));
					let expect_descriptor = {
						let mut expect_descriptor = expect_descriptor;
						expect_descriptor.signature = descriptor.signature.clone();
						expect_descriptor
					};
					assert_eq!(descriptor, &expect_descriptor);
				}
				_ => panic!("received wrong message type"),
			}
		}
	}
}
