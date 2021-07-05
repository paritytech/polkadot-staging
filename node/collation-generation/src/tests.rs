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

mod handle_new_activations {
	use super::super::*;
	use futures::{
		lock::Mutex,
		task::{Context as FuturesContext, Poll},
		Future,
	};
	use polkadot_node_primitives::{Collation, CollationResult, BlockData, PoV, POV_BOMB_LIMIT};
	use polkadot_node_subsystem::messages::{
		AllMessages, RuntimeApiMessage, RuntimeApiRequest,
	};
	use polkadot_node_subsystem_test_helpers::{
		subsystem_test_harness, TestSubsystemContextHandle,
	};
	use polkadot_primitives::v1::{
		CollatorPair, Id as ParaId, PersistedValidationData, ScheduledCore, ValidationCode,
		ValidationCodeAndHash,
	};
	use std::pin::Pin;

	fn test_collation() -> Collation {
		Collation {
			upward_messages: Default::default(),
			horizontal_messages: Default::default(),
			new_validation_code: Default::default(),
			head_data: Default::default(),
			proof_of_validity: PoV {
				block_data: BlockData(Vec::new()),
			},
			processed_downward_messages: Default::default(),
			hrmp_watermark: Default::default(),
		}
	}

	fn test_collation_compressed() -> Collation {
		let mut collation = test_collation();
		let compressed = PoV {
			block_data: BlockData(sp_maybe_compressed_blob::compress(
				&collation.proof_of_validity.block_data.0,
				POV_BOMB_LIMIT,
			).unwrap())
		};
		collation.proof_of_validity = compressed;
		collation
	}

	fn test_validation_data() -> PersistedValidationData {
		let mut persisted_validation_data: PersistedValidationData = Default::default();
		persisted_validation_data.max_pov_size = 1024;
		persisted_validation_data
	}

	// Box<dyn Future<Output = Collation> + Unpin + Send
	struct TestCollator;

	impl Future for TestCollator {
		type Output = Option<CollationResult>;

		fn poll(self: Pin<&mut Self>, _cx: &mut FuturesContext) -> Poll<Self::Output> {
			Poll::Ready(Some(CollationResult { collation: test_collation(), result_sender: None }))
		}
	}

	impl Unpin for TestCollator {}

	fn test_config<Id: Into<ParaId>>(para_id: Id) -> Arc<CollationGenerationConfig> {
		Arc::new(CollationGenerationConfig {
			key: CollatorPair::generate().0,
			collator: Box::new(|_: Hash, _vd: &PersistedValidationData| {
				TestCollator.boxed()
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
	fn requests_availability_per_relay_parent() {
		let activated_hashes: Vec<Hash> = vec![
			[1; 32].into(),
			[4; 32].into(),
			[9; 32].into(),
			[16; 32].into(),
		];

		let requested_availability_cores = Arc::new(Mutex::new(Vec::new()));

		let overseer_requested_availability_cores = requested_availability_cores.clone();
		let overseer = |mut handle: TestSubsystemContextHandle<CollationGenerationMessage>| async move {
			loop {
				match handle.try_recv().await {
					None => break,
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
				test_config(123u32),
				subsystem_activated_hashes,
				&mut ctx,
				Metrics(None),
				&tx,
			)
			.await
			.unwrap();
		});

		let mut requested_availability_cores = Arc::try_unwrap(requested_availability_cores)
			.expect("overseer should have shut down by now")
			.into_inner();
		requested_availability_cores.sort();

		assert_eq!(requested_availability_cores, activated_hashes);
	}

	#[test]
	fn requests_validation_data_for_scheduled_matches() {
		let activated_hashes: Vec<Hash> = vec![
			Hash::repeat_byte(1),
			Hash::repeat_byte(4),
			Hash::repeat_byte(9),
			Hash::repeat_byte(16),
		];

		let requested_validation_data = Arc::new(Mutex::new(Vec::new()));

		let overseer_requested_validation_data = requested_validation_data.clone();
		let overseer = |mut handle: TestSubsystemContextHandle<CollationGenerationMessage>| async move {
			loop {
				match handle.try_recv().await {
					None => break,
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
						RuntimeApiRequest::PersistedValidationData(
							_para_id,
							_occupied_core_assumption,
							tx,
						),
					))) => {
						overseer_requested_validation_data
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
			handle_new_activations(test_config(16), activated_hashes, &mut ctx, Metrics(None), &tx)
				.await
				.unwrap();
		});

		let requested_validation_data = Arc::try_unwrap(requested_validation_data)
			.expect("overseer should have shut down by now")
			.into_inner();

		// the only activated hash should be from the 4 hash:
		// each activated hash generates two scheduled cores: one with its value * 4, one with its value * 5
		// given that the test configuration has a para_id of 16, there's only one way to get that value: with the 4
		// hash.
		assert_eq!(requested_validation_data, vec![[4; 32].into()]);
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
						RuntimeApiRequest::PersistedValidationData(
							_para_id,
							_occupied_core_assumption,
							tx,
						),
					))) => {
						tx.send(Ok(Some(test_validation_data()))).unwrap();
					}
					Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						_hash,
						RuntimeApiRequest::Validators(tx),
					))) => {
						tx.send(Ok(vec![Default::default(); 3])).unwrap();
					}
					Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						_hash,
						RuntimeApiRequest::ValidationCodeHash(
							_para_id,
							OccupiedCoreAssumption::Free,
							tx,
						),
					))) => {
						tx.send(Ok(Some(ValidationCodeAndHash::compute_from_code(
							vec![1, 2, 3].into()
						).into_parts().1))).unwrap();
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
			handle_new_activations(subsystem_config, activated_hashes, &mut ctx, Metrics(None), &tx)
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
		let expect_pov_hash = test_collation_compressed().proof_of_validity.hash();
		let expect_validation_data_hash = test_validation_data().hash();
		let expect_relay_parent = Hash::repeat_byte(4);
		let expect_validation_code_hash = ValidationCode(vec![1, 2, 3]).hash();
		let expect_payload = collator_signature_payload(
			&expect_relay_parent,
			&config.para_id,
			&expect_validation_data_hash,
			&expect_pov_hash,
			&expect_validation_code_hash,
		);
		let expect_descriptor = CandidateDescriptor {
			signature: config.key.sign(&expect_payload),
			para_id: config.para_id,
			relay_parent: expect_relay_parent,
			collator: config.key.public(),
			persisted_validation_data_hash: expect_validation_data_hash,
			pov_hash: expect_pov_hash,
			erasure_root: Default::default(), // this isn't something we're checking right now
			para_head: test_collation().head_data.hash(),
			validation_code_hash: expect_validation_code_hash,
		};

		assert_eq!(sent_messages.len(), 1);
		match &sent_messages[0] {
			AllMessages::CollatorProtocol(CollatorProtocolMessage::DistributeCollation(
				CandidateReceipt { descriptor, .. },
				_pov,
				..
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
						&descriptor.persisted_validation_data_hash,
						&descriptor.pov_hash,
						&descriptor.validation_code_hash,
					)
					.as_ref(),
					&descriptor.collator,
				));
				let expect_descriptor = {
					let mut expect_descriptor = expect_descriptor;
					expect_descriptor.signature = descriptor.signature.clone();
					expect_descriptor.erasure_root = descriptor.erasure_root.clone();
					expect_descriptor
				};
				assert_eq!(descriptor, &expect_descriptor);
			}
			_ => panic!("received wrong message type"),
		}
	}
}
