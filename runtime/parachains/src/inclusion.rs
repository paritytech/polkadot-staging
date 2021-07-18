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

//! The inclusion module is responsible for inclusion and availability of scheduled parachains
//! and parathreads.
//!
//! It is responsible for carrying candidates from being backable to being backed, and then from backed
//! to included.

use sp_std::prelude::*;
use primitives::v1::{
	CandidateCommitments, CandidateDescriptor, ValidatorIndex, Id as ParaId,
	AvailabilityBitfield as AvailabilityBitfield, UncheckedSignedAvailabilityBitfields, SigningContext,
	BackedCandidate, CoreIndex, GroupIndex, CommittedCandidateReceipt,
	CandidateReceipt, HeadData, CandidateHash,
};
use frame_support::{
	decl_storage, decl_module, decl_error, decl_event, ensure, dispatch::DispatchResult, IterableStorageMap,
	weights::Weight, traits::Get,
};
use parity_scale_codec::{Encode, Decode};
use bitvec::{order::Lsb0 as BitOrderLsb0, vec::BitVec};
use sp_runtime::{DispatchError, traits::{One, Saturating}};

use crate::{configuration, paras, dmp, ump, hrmp, shared, scheduler::CoreAssignment};

/// A bitfield signed by a validator indicating that it is keeping its piece of the erasure-coding
/// for any backed candidates referred to by a `1` bit available.
///
/// The bitfield's signature should be checked at the point of submission. Afterwards it can be
/// dropped.
#[derive(Encode, Decode)]
#[cfg_attr(test, derive(Debug))]
pub struct AvailabilityBitfieldRecord<N> {
	bitfield: AvailabilityBitfield, // one bit per core.
	submitted_at: N, // for accounting, as meaning of bits may change over time.
}

/// A backed candidate pending availability.
#[derive(Encode, Decode, PartialEq)]
#[cfg_attr(test, derive(Debug))]
pub struct CandidatePendingAvailability<H, N> {
	/// The availability core this is assigned to.
	core: CoreIndex,
	/// The candidate hash.
	hash: CandidateHash,
	/// The candidate descriptor.
	descriptor: CandidateDescriptor<H>,
	/// The received availability votes. One bit per validator.
	availability_votes: BitVec<BitOrderLsb0, u8>,
	/// The backers of the candidate pending availability.
	backers: BitVec<BitOrderLsb0, u8>,
	/// The block number of the relay-parent of the receipt.
	relay_parent_number: N,
	/// The block number of the relay-chain block this was backed in.
	backed_in_number: N,
	/// The group index backing this block.
	backing_group: GroupIndex,
}

impl<H, N> CandidatePendingAvailability<H, N> {
	/// Get the availability votes on the candidate.
	pub(crate) fn availability_votes(&self) -> &BitVec<BitOrderLsb0, u8> {
		&self.availability_votes
	}

	/// Get the relay-chain block number this was backed in.
	pub(crate) fn backed_in_number(&self) -> &N {
		&self.backed_in_number
	}

	/// Get the core index.
	pub(crate) fn core_occupied(&self)-> CoreIndex {
		self.core.clone()
	}

	/// Get the candidate hash.
	pub(crate) fn candidate_hash(&self) -> CandidateHash {
		self.hash
	}

	/// Get the candidate descriptor.
	pub(crate) fn candidate_descriptor(&self) -> &CandidateDescriptor<H> {
		&self.descriptor
	}
}

/// A hook for applying validator rewards
pub trait RewardValidators {
	// Reward the validators with the given indices for issuing backing statements.
	fn reward_backing(validators: impl IntoIterator<Item=ValidatorIndex>);
	// Reward the validators with the given indices for issuing availability bitfields.
	// Validators are sent to this hook when they have contributed to the availability
	// of a candidate by setting a bit in their bitfield.
	fn reward_bitfields(validators: impl IntoIterator<Item=ValidatorIndex>);
}

pub trait Config:
	frame_system::Config
	+ shared::Config
	+ paras::Config
	+ dmp::Config
	+ ump::Config
	+ hrmp::Config
	+ configuration::Config
{
	type Event: From<Event<Self>> + Into<<Self as frame_system::Config>::Event>;
	type RewardValidators: RewardValidators;
}

decl_storage! {
	trait Store for Module<T: Config> as ParaInclusion {
		/// The latest bitfield for each validator, referred to by their index in the validator set.
		AvailabilityBitfields: map hasher(twox_64_concat) ValidatorIndex
			=> Option<AvailabilityBitfieldRecord<T::BlockNumber>>;

		/// Candidates pending availability by `ParaId`.
		PendingAvailability: map hasher(twox_64_concat) ParaId
			=> Option<CandidatePendingAvailability<T::Hash, T::BlockNumber>>;

		/// The commitments of candidates pending availability, by `ParaId`.
		PendingAvailabilityCommitments: map hasher(twox_64_concat) ParaId
			=> Option<CandidateCommitments>;
	}
}

decl_error! {
	pub enum Error for Module<T: Config> {
		/// Availability bitfield has unexpected size.
		WrongBitfieldSize,
		/// Multiple bitfields submitted by same validator or validators out of order by index.
		BitfieldDuplicateOrUnordered,
		/// Validator index out of bounds.
		ValidatorIndexOutOfBounds,
		/// Invalid signature
		InvalidBitfieldSignature,
		/// Candidate submitted but para not scheduled.
		UnscheduledCandidate,
		/// Candidate scheduled despite pending candidate already existing for the para.
		CandidateScheduledBeforeParaFree,
		/// Candidate included with the wrong collator.
		WrongCollator,
		/// Scheduled cores out of order.
		ScheduledOutOfOrder,
		/// Head data exceeds the configured maximum.
		HeadDataTooLarge,
		/// Code upgrade prematurely.
		PrematureCodeUpgrade,
		/// Output code is too large
		NewCodeTooLarge,
		/// Candidate not in parent context.
		CandidateNotInParentContext,
		/// The bitfield contains a bit relating to an unassigned availability core.
		UnoccupiedBitInBitfield,
		/// Invalid group index in core assignment.
		InvalidGroupIndex,
		/// Insufficient (non-majority) backing.
		InsufficientBacking,
		/// Invalid (bad signature, unknown validator, etc.) backing.
		InvalidBacking,
		/// Collator did not sign PoV.
		NotCollatorSigned,
		/// The validation data hash does not match expected.
		ValidationDataHashMismatch,
		/// Internal error only returned when compiled with debug assertions.
		InternalError,
		/// The downward message queue is not processed correctly.
		IncorrectDownwardMessageHandling,
		/// At least one upward message sent does not pass the acceptance criteria.
		InvalidUpwardMessages,
		/// The candidate didn't follow the rules of HRMP watermark advancement.
		HrmpWatermarkMishandling,
		/// The HRMP messages sent by the candidate is not valid.
		InvalidOutboundHrmp,
		/// The validation code hash of the candidate is not valid.
		InvalidValidationCodeHash,
	}
}

decl_event! {
	pub enum Event<T> where <T as frame_system::Config>::Hash {
		/// A candidate was backed. `[candidate, head_data]`
		CandidateBacked(CandidateReceipt<Hash>, HeadData, CoreIndex, GroupIndex),
		/// A candidate was included. `[candidate, head_data]`
		CandidateIncluded(CandidateReceipt<Hash>, HeadData, CoreIndex, GroupIndex),
		/// A candidate timed out. `[candidate, head_data]`
		CandidateTimedOut(CandidateReceipt<Hash>, HeadData, CoreIndex),
	}
}

decl_module! {
	/// The parachain-candidate inclusion module.
	pub struct Module<T: Config>
		for enum Call where origin: <T as frame_system::Config>::Origin
	{
		type Error = Error<T>;

		fn deposit_event() = default;
	}
}

const LOG_TARGET: &str = "runtime::inclusion";

impl<T: Config> Module<T> {
	/// Block initialization logic, called by initializer.
	pub(crate) fn initializer_initialize(_now: T::BlockNumber) -> Weight { 0 }

	/// Block finalization logic, called by initializer.
	pub(crate) fn initializer_finalize() { }

	/// Handle an incoming session change.
	pub(crate) fn initializer_on_new_session(
		_notification: &crate::initializer::SessionChangeNotification<T::BlockNumber>
	) {
		// unlike most drain methods, drained elements are not cleared on `Drop` of the iterator
		// and require consumption.
		for _ in <PendingAvailabilityCommitments>::drain() { }
		for _ in <PendingAvailability<T>>::drain() { }
		for _ in <AvailabilityBitfields<T>>::drain() { }
	}

	/// Process a set of incoming bitfields. Return a `vec` of cores freed by candidates
	/// becoming available.
	pub(crate) fn process_bitfields(
		expected_bits: usize,
		unchecked_bitfields: UncheckedSignedAvailabilityBitfields,
		core_lookup: impl Fn(CoreIndex) -> Option<ParaId>,
	) -> Result<Vec<CoreIndex>, DispatchError> {
		let validators = shared::Module::<T>::active_validator_keys();
		let session_index = shared::Module::<T>::session_index();

		let mut assigned_paras_record: Vec<_> = (0..expected_bits)
			.map(|bit_index| core_lookup(CoreIndex::from(bit_index as u32)))
			.map(|core_para| core_para.map(|p| (p, PendingAvailability::<T>::get(&p))))
			.collect();


		// do sanity checks on the bitfields:
		// 1. no more than one bitfield per validator
		// 2. bitfields are ascending by validator index.
		// 3. each bitfield has exactly `expected_bits`
		// 4. signature is valid.
		let signed_bitfields = {
			let occupied_bitmask: BitVec<BitOrderLsb0, u8> = assigned_paras_record.iter()
				.map(|p| p.as_ref()
					.map_or(false, |(_id, pending_availability)| pending_availability.is_some())
				)
				.collect();

			let mut last_index = None;

			let signing_context = SigningContext {
				parent_hash: <frame_system::Pallet<T>>::parent_hash(),
				session_index,
			};

			let mut signed_bitfields = Vec::with_capacity(unchecked_bitfields.len());

			for unchecked_bitfield in unchecked_bitfields {
				ensure!(
					unchecked_bitfield.unchecked_payload().0.len() == expected_bits,
					Error::<T>::WrongBitfieldSize,
				);

				ensure!(
					last_index.map_or(true, |last| last < unchecked_bitfield.unchecked_validator_index()),
					Error::<T>::BitfieldDuplicateOrUnordered,
				);

				ensure!(
					(unchecked_bitfield.unchecked_validator_index().0 as usize) < validators.len(),
					Error::<T>::ValidatorIndexOutOfBounds,
				);

				ensure!(
					occupied_bitmask.clone() & unchecked_bitfield.unchecked_payload().0.clone() == unchecked_bitfield.unchecked_payload().0,
					Error::<T>::UnoccupiedBitInBitfield,
				);

				let validator_public = &validators[unchecked_bitfield.unchecked_validator_index().0 as usize];

				last_index = Some(unchecked_bitfield.unchecked_validator_index());

				signed_bitfields.push(
					unchecked_bitfield.try_into_checked(
						&signing_context,
						validator_public,
					).map_err(|_| Error::<T>::InvalidBitfieldSignature)?
				);
			}
			signed_bitfields
		};

		let now = <frame_system::Pallet<T>>::block_number();
		for signed_bitfield in signed_bitfields {
			for (bit_idx, _)
				in signed_bitfield.payload().0.iter().enumerate().filter(|(_, is_av)| **is_av)
			{
				let (_, pending_availability) = assigned_paras_record[bit_idx]
					.as_mut()
					.expect("validator bitfields checked not to contain bits corresponding to unoccupied cores; qed");

				// defensive check - this is constructed by loading the availability bitfield record,
				// which is always `Some` if the core is occupied - that's why we're here.
				let val_idx = signed_bitfield.validator_index().0 as usize;
				if let Some(mut bit) = pending_availability.as_mut()
					.and_then(|r| r.availability_votes.get_mut(val_idx))
				{
					*bit = true;
				} else if cfg!(debug_assertions) {
					ensure!(false, Error::<T>::InternalError);
				}
			}

			let validator_index = signed_bitfield.validator_index();
			let record = AvailabilityBitfieldRecord {
				bitfield: signed_bitfield.into_payload(),
				submitted_at: now,
			};

			<AvailabilityBitfields<T>>::insert(&validator_index, record);
		}

		let threshold = availability_threshold(validators.len());

		let mut freed_cores = Vec::with_capacity(expected_bits);
		for (para_id, pending_availability) in assigned_paras_record.into_iter()
			.filter_map(|x| x)
			.filter_map(|(id, p)| p.map(|p| (id, p)))
		{
			if pending_availability.availability_votes.count_ones() >= threshold {
				<PendingAvailability<T>>::remove(&para_id);
				let commitments = match PendingAvailabilityCommitments::take(&para_id) {
					Some(commitments) => commitments,
					None => {
						log::warn!(
							target: LOG_TARGET,
							"Inclusion::process_bitfields: PendingAvailability and PendingAvailabilityCommitments
							are out of sync, did someone mess with the storage?",
						);
						continue;
					}
				};

				let receipt = CommittedCandidateReceipt {
					descriptor: pending_availability.descriptor,
					commitments,
				};
				Self::enact_candidate(
					pending_availability.relay_parent_number,
					receipt,
					pending_availability.backers,
					pending_availability.availability_votes,
					pending_availability.core,
					pending_availability.backing_group,
				);

				freed_cores.push(pending_availability.core);
			} else {
				<PendingAvailability<T>>::insert(&para_id, &pending_availability);
			}
		}

		// TODO: pass available candidates onwards to validity module once implemented.
		// https://github.com/paritytech/polkadot/issues/1251

		Ok(freed_cores)
	}

	/// Process candidates that have been backed. Provide the relay storage root, a set of candidates
	/// and scheduled cores.
	///
	/// Both should be sorted ascending by core index, and the candidates should be a subset of
	/// scheduled cores. If these conditions are not met, the execution of the function fails.
	pub(crate) fn process_candidates(
		parent_storage_root: T::Hash,
		candidates: Vec<BackedCandidate<T::Hash>>,
		scheduled: Vec<CoreAssignment>,
		group_validators: impl Fn(GroupIndex) -> Option<Vec<ValidatorIndex>>,
	) -> Result<Vec<CoreIndex>, DispatchError> {
		ensure!(candidates.len() <= scheduled.len(), Error::<T>::UnscheduledCandidate);

		if scheduled.is_empty() {
			return Ok(Vec::new());
		}

		let validators = shared::Module::<T>::active_validator_keys();
		let parent_hash = <frame_system::Pallet<T>>::parent_hash();

		// At the moment we assume (and in fact enforce, below) that the relay-parent is always one
		// before of the block where we include a candidate (i.e. this code path).
		let now = <frame_system::Pallet<T>>::block_number();
		let relay_parent_number = now - One::one();
		let check_cx = CandidateCheckContext::<T>::new(now, relay_parent_number);

		// do all checks before writing storage.
		let core_indices_and_backers = {
			let mut skip = 0;
			let mut core_indices_and_backers = Vec::with_capacity(candidates.len());
			let mut last_core = None;

			let mut check_assignment_in_order = |assignment: &CoreAssignment| -> DispatchResult {
				ensure!(
					last_core.map_or(true, |core| assignment.core > core),
					Error::<T>::ScheduledOutOfOrder,
				);

				last_core = Some(assignment.core);
				Ok(())
			};

			let signing_context = SigningContext {
				parent_hash,
				session_index: shared::Module::<T>::session_index(),
			};

			// We combine an outer loop over candidates with an inner loop over the scheduled,
			// where each iteration of the outer loop picks up at the position
			// in scheduled just after the past iteration left off.
			//
			// If the candidates appear in the same order as they appear in `scheduled`,
			// then they should always be found. If the end of `scheduled` is reached,
			// then the candidate was either not scheduled or out-of-order.
			//
			// In the meantime, we do certain sanity checks on the candidates and on the scheduled
			// list.
			'a:
			for (candidate_idx, candidate) in candidates.iter().enumerate() {
				let para_id = candidate.descriptor().para_id;
				let mut backers = bitvec::bitvec![BitOrderLsb0, u8; 0; validators.len()];

				// we require that the candidate is in the context of the parent block.
				ensure!(
					candidate.descriptor().relay_parent == parent_hash,
					Error::<T>::CandidateNotInParentContext,
				);
				ensure!(
					candidate.descriptor().check_collator_signature().is_ok(),
					Error::<T>::NotCollatorSigned,
				);

				let validation_code_hash =
					<paras::Pallet<T>>::validation_code_hash_at(para_id, now, None)
					// A candidate for a parachain without current validation code is not scheduled.
					.ok_or_else(|| Error::<T>::UnscheduledCandidate)?;
				ensure!(
					candidate.descriptor().validation_code_hash == validation_code_hash,
					Error::<T>::InvalidValidationCodeHash,
				);

				if let Err(err) = check_cx
					.check_validation_outputs(
						para_id,
						&candidate.candidate.commitments.head_data,
						&candidate.candidate.commitments.new_validation_code,
						candidate.candidate.commitments.processed_downward_messages,
						&candidate.candidate.commitments.upward_messages,
						T::BlockNumber::from(candidate.candidate.commitments.hrmp_watermark),
						&candidate.candidate.commitments.horizontal_messages,
					)
				{
					log::debug!(
						target: LOG_TARGET,
						"Validation outputs checking during inclusion of a candidate {} for parachain `{}` failed: {:?}",
						candidate_idx,
						u32::from(para_id),
						err,
					);
					Err(err.strip_into_dispatch_err::<T>())?;
				};

				for (i, assignment) in scheduled[skip..].iter().enumerate() {
					check_assignment_in_order(assignment)?;

					if para_id == assignment.para_id {
						if let Some(required_collator) = assignment.required_collator() {
							ensure!(
								required_collator == &candidate.descriptor().collator,
								Error::<T>::WrongCollator,
							);
						}

						{
							// this should never fail because the para is registered
							let persisted_validation_data =
								match crate::util::make_persisted_validation_data::<T>(
									para_id,
									relay_parent_number,
									parent_storage_root,
								) {
									Some(l) => l,
									None => {
										// We don't want to error out here because it will
										// brick the relay-chain. So we return early without
										// doing anything.
										return Ok(Vec::new());
									}
								};

							let expected = persisted_validation_data.hash();

							ensure!(
								expected == candidate.descriptor().persisted_validation_data_hash,
								Error::<T>::ValidationDataHashMismatch,
							);
						}

						ensure!(
							<PendingAvailability<T>>::get(&para_id).is_none() &&
							<PendingAvailabilityCommitments>::get(&para_id).is_none(),
							Error::<T>::CandidateScheduledBeforeParaFree,
						);

						// account for already skipped, and then skip this one.
						skip = i + skip + 1;

						let group_vals = group_validators(assignment.group_idx)
							.ok_or_else(|| Error::<T>::InvalidGroupIndex)?;

						// check the signatures in the backing and that it is a majority.
						{
							let maybe_amount_validated
								= primitives::v1::check_candidate_backing(
									&candidate,
									&signing_context,
									group_vals.len(),
									|idx| group_vals.get(idx)
										.and_then(|i| validators.get(i.0 as usize))
										.map(|v| v.clone()),
								);

							match maybe_amount_validated {
								Ok(amount_validated) => ensure!(
									amount_validated * 2 > group_vals.len(),
									Error::<T>::InsufficientBacking,
								),
								Err(()) => { Err(Error::<T>::InvalidBacking)?; }
							}

							for (bit_idx, _) in candidate
								.validator_indices.iter()
								.enumerate().filter(|(_, signed)| **signed)
							{
								let val_idx = group_vals.get(bit_idx)
									.expect("this query done above; qed");

								backers.set(val_idx.0 as _, true);
							}
						}

						core_indices_and_backers.push((assignment.core, backers, assignment.group_idx));
						continue 'a;
					}
				}

				// end of loop reached means that the candidate didn't appear in the non-traversed
				// section of the `scheduled` slice. either it was not scheduled or didn't appear in
				// `candidates` in the correct order.
				ensure!(
					false,
					Error::<T>::UnscheduledCandidate,
				);
			}

			// check remainder of scheduled cores, if any.
			for assignment in scheduled[skip..].iter() {
				check_assignment_in_order(assignment)?;
			}

			core_indices_and_backers
		};

		// one more sweep for actually writing to storage.
		let core_indices = core_indices_and_backers.iter().map(|&(ref c, _, _)| c.clone()).collect();
		for (candidate, (core, backers, group)) in candidates.into_iter().zip(core_indices_and_backers) {
			let para_id = candidate.descriptor().para_id;

			// initialize all availability votes to 0.
			let availability_votes: BitVec<BitOrderLsb0, u8>
				= bitvec::bitvec![BitOrderLsb0, u8; 0; validators.len()];

			Self::deposit_event(Event::<T>::CandidateBacked(
				candidate.candidate.to_plain(),
				candidate.candidate.commitments.head_data.clone(),
				core,
				group,
			));

			let candidate_hash = candidate.candidate.hash();

			let (descriptor, commitments) = (
				candidate.candidate.descriptor,
				candidate.candidate.commitments,
			);

			<PendingAvailability<T>>::insert(&para_id, CandidatePendingAvailability {
				core,
				hash: candidate_hash,
				descriptor,
				availability_votes,
				relay_parent_number,
				backers,
				backed_in_number: check_cx.now,
				backing_group: group,
			});
			<PendingAvailabilityCommitments>::insert(&para_id, commitments);
		}

		Ok(core_indices)
	}

	/// Run the acceptance criteria checks on the given candidate commitments.
	pub(crate) fn check_validation_outputs_for_runtime_api(
		para_id: ParaId,
		validation_outputs: primitives::v1::CandidateCommitments,
	) -> bool {
		// This function is meant to be called from the runtime APIs against the relay-parent, hence
		// `relay_parent_number` is equal to `now`.
		let now = <frame_system::Pallet<T>>::block_number();
		let relay_parent_number = now;
		let check_cx = CandidateCheckContext::<T>::new(now, relay_parent_number);

		if let Err(err) = check_cx.check_validation_outputs(
			para_id,
			&validation_outputs.head_data,
			&validation_outputs.new_validation_code,
			validation_outputs.processed_downward_messages,
			&validation_outputs.upward_messages,
			T::BlockNumber::from(validation_outputs.hrmp_watermark),
			&validation_outputs.horizontal_messages,
		) {
			log::debug!(
				target: LOG_TARGET,
				"Validation outputs checking for parachain `{}` failed: {:?}",
				u32::from(para_id),
				err,
			);
			false
		} else {
			true
		}
	}

	fn enact_candidate(
		relay_parent_number: T::BlockNumber,
		receipt: CommittedCandidateReceipt<T::Hash>,
		backers: BitVec<BitOrderLsb0, u8>,
		availability_votes: BitVec<BitOrderLsb0, u8>,
		core_index: CoreIndex,
		backing_group: GroupIndex,
	) -> Weight {
		let plain = receipt.to_plain();
		let commitments = receipt.commitments;
		let config = <configuration::Module<T>>::config();

		T::RewardValidators::reward_backing(backers.iter().enumerate()
			.filter(|(_, backed)| **backed)
			.map(|(i, _)| ValidatorIndex(i as _))
		);

		T::RewardValidators::reward_bitfields(availability_votes.iter().enumerate()
			.filter(|(_, voted)| **voted)
			.map(|(i, _)| ValidatorIndex(i as _))
		);

		// initial weight is config read.
		let mut weight = T::DbWeight::get().reads_writes(1, 0);
		if let Some(new_code) = commitments.new_validation_code {
			weight += <paras::Pallet<T>>::schedule_code_upgrade(
				receipt.descriptor.para_id,
				new_code,
				relay_parent_number,
				&config,
			);
		}

		// enact the messaging facet of the candidate.
		weight += <dmp::Module<T>>::prune_dmq(
			receipt.descriptor.para_id,
			commitments.processed_downward_messages,
		);
		weight += <ump::Module<T>>::receive_upward_messages(
			receipt.descriptor.para_id,
			commitments.upward_messages,
		);
		weight += <hrmp::Module<T>>::prune_hrmp(
			receipt.descriptor.para_id,
			T::BlockNumber::from(commitments.hrmp_watermark),
		);
		weight += <hrmp::Module<T>>::queue_outbound_hrmp(
			receipt.descriptor.para_id,
			commitments.horizontal_messages,
		);

		Self::deposit_event(
			Event::<T>::CandidateIncluded(plain, commitments.head_data.clone(), core_index, backing_group)
		);

		weight + <paras::Pallet<T>>::note_new_head(
			receipt.descriptor.para_id,
			commitments.head_data,
			relay_parent_number,
		)
	}

	/// Cleans up all paras pending availability that the predicate returns true for.
	///
	/// The predicate accepts the index of the core and the block number the core has been occupied
	/// since (i.e. the block number the candidate was backed at in this fork of the relay chain).
	///
	/// Returns a vector of cleaned-up core IDs.
	pub(crate) fn collect_pending(pred: impl Fn(CoreIndex, T::BlockNumber) -> bool) -> Vec<CoreIndex> {
		let mut cleaned_up_ids = Vec::new();
		let mut cleaned_up_cores = Vec::new();

		for (para_id, pending_record) in <PendingAvailability<T>>::iter() {
			if pred(pending_record.core, pending_record.backed_in_number) {
				cleaned_up_ids.push(para_id);
				cleaned_up_cores.push(pending_record.core);
			}
		}

		for para_id in cleaned_up_ids {
			let pending = <PendingAvailability<T>>::take(&para_id);
			let commitments = <PendingAvailabilityCommitments>::take(&para_id);

			if let (Some(pending), Some(commitments)) = (pending, commitments) {
				// defensive: this should always be true.
				let candidate = CandidateReceipt {
					descriptor: pending.descriptor,
					commitments_hash: commitments.hash(),
				};

				Self::deposit_event(Event::<T>::CandidateTimedOut(
					candidate,
					commitments.head_data,
					pending.core,
				));
			}
		}

		cleaned_up_cores
	}

	/// Forcibly enact the candidate with the given ID as though it had been deemed available
	/// by bitfields.
	///
	/// Is a no-op if there is no candidate pending availability for this para-id.
	/// This should generally not be used but it is useful during execution of Runtime APIs,
	/// where the changes to the state are expected to be discarded directly after.
	pub(crate) fn force_enact(para: ParaId) {
		let pending = <PendingAvailability<T>>::take(&para);
		let commitments = <PendingAvailabilityCommitments>::take(&para);

		if let (Some(pending), Some(commitments)) = (pending, commitments) {
			let candidate = CommittedCandidateReceipt {
				descriptor: pending.descriptor,
				commitments,
			};

			Self::enact_candidate(
				pending.relay_parent_number,
				candidate,
				pending.backers,
				pending.availability_votes,
				pending.core,
				pending.backing_group,
			);
		}
	}

	/// Returns the `CommittedCandidateReceipt` pending availability for the para provided, if any.
	pub(crate) fn candidate_pending_availability(para: ParaId)
		-> Option<CommittedCandidateReceipt<T::Hash>>
	{
		<PendingAvailability<T>>::get(&para)
			.map(|p| p.descriptor)
			.and_then(|d| <PendingAvailabilityCommitments>::get(&para).map(move |c| (d, c)))
			.map(|(d, c)| CommittedCandidateReceipt { descriptor: d, commitments: c })
	}

	/// Returns the metadata around the candidate pending availability for the
	/// para provided, if any.
	pub(crate) fn pending_availability(para: ParaId)
		-> Option<CandidatePendingAvailability<T::Hash, T::BlockNumber>>
	{
		<PendingAvailability<T>>::get(&para)
	}
}

const fn availability_threshold(n_validators: usize) -> usize {
	let mut threshold = (n_validators * 2) / 3;
	threshold += (n_validators * 2) % 3;
	threshold
}

#[derive(derive_more::From, Debug)]
enum AcceptanceCheckErr<BlockNumber> {
	HeadDataTooLarge,
	PrematureCodeUpgrade,
	NewCodeTooLarge,
	ProcessedDownwardMessages(dmp::ProcessedDownwardMessagesAcceptanceErr),
	UpwardMessages(ump::AcceptanceCheckErr),
	HrmpWatermark(hrmp::HrmpWatermarkAcceptanceErr<BlockNumber>),
	OutboundHrmp(hrmp::OutboundHrmpAcceptanceErr),
}

impl<BlockNumber> AcceptanceCheckErr<BlockNumber> {
	/// Returns the same error so that it can be threaded through a needle of `DispatchError` and
	/// ultimately returned from a `Dispatchable`.
	fn strip_into_dispatch_err<T: Config>(self) -> Error<T> {
		use AcceptanceCheckErr::*;
		match self {
			HeadDataTooLarge => Error::<T>::HeadDataTooLarge,
			PrematureCodeUpgrade => Error::<T>::PrematureCodeUpgrade,
			NewCodeTooLarge => Error::<T>::NewCodeTooLarge,
			ProcessedDownwardMessages(_) => Error::<T>::IncorrectDownwardMessageHandling,
			UpwardMessages(_) => Error::<T>::InvalidUpwardMessages,
			HrmpWatermark(_) => Error::<T>::HrmpWatermarkMishandling,
			OutboundHrmp(_) => Error::<T>::InvalidOutboundHrmp,
		}
	}
}

/// A collection of data required for checking a candidate.
struct CandidateCheckContext<T: Config> {
	config: configuration::HostConfiguration<T::BlockNumber>,
	now: T::BlockNumber,
	relay_parent_number: T::BlockNumber,
}

impl<T: Config> CandidateCheckContext<T> {
	fn new(now: T::BlockNumber, relay_parent_number: T::BlockNumber) -> Self {
		Self {
			config: <configuration::Module<T>>::config(),
			now,
			relay_parent_number,
		}
	}

	/// Check the given outputs after candidate validation on whether it passes the acceptance
	/// criteria.
	fn check_validation_outputs(
		&self,
		para_id: ParaId,
		head_data: &HeadData,
		new_validation_code: &Option<primitives::v1::ValidationCode>,
		processed_downward_messages: u32,
		upward_messages: &[primitives::v1::UpwardMessage],
		hrmp_watermark: T::BlockNumber,
		horizontal_messages: &[primitives::v1::OutboundHrmpMessage<ParaId>],
	) -> Result<(), AcceptanceCheckErr<T::BlockNumber>> {
		ensure!(
			head_data.0.len() <= self.config.max_head_data_size as _,
			AcceptanceCheckErr::HeadDataTooLarge,
		);

		// if any, the code upgrade attempt is allowed.
		if let Some(new_validation_code) = new_validation_code {
			let valid_upgrade_attempt = <paras::Pallet<T>>::last_code_upgrade(para_id, true)
				.map_or(true, |last| {
					last <= self.relay_parent_number
						&& self.relay_parent_number.saturating_sub(last)
							>= self.config.validation_upgrade_frequency
				});
			ensure!(
				valid_upgrade_attempt,
				AcceptanceCheckErr::PrematureCodeUpgrade,
			);
			ensure!(
				new_validation_code.0.len() <= self.config.max_code_size as _,
				AcceptanceCheckErr::NewCodeTooLarge,
			);
		}

		// check if the candidate passes the messaging acceptance criteria
		<dmp::Module<T>>::check_processed_downward_messages(
			para_id,
			processed_downward_messages,
		)?;
		<ump::Module<T>>::check_upward_messages(&self.config, para_id, upward_messages)?;
		<hrmp::Module<T>>::check_hrmp_watermark(
			para_id,
			self.relay_parent_number,
			hrmp_watermark,
		)?;
		<hrmp::Module<T>>::check_outbound_hrmp(&self.config, para_id, horizontal_messages)?;

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use std::sync::Arc;
	use futures::executor::block_on;
	use primitives::{v0::PARACHAIN_KEY_TYPE_ID, v1::UncheckedSignedAvailabilityBitfield};
	use primitives::v1::{BlockNumber, Hash};
	use primitives::v1::{
		SignedAvailabilityBitfield, CompactStatement as Statement, ValidityAttestation, CollatorId,
		CandidateCommitments, SignedStatement, CandidateDescriptor, ValidationCode, ValidatorId,
	};
	use sp_keystore::{SyncCryptoStorePtr, SyncCryptoStore};
	use frame_support::traits::{OnFinalize, OnInitialize};
	use keyring::Sr25519Keyring;
	use sc_keystore::LocalKeystore;
	use crate::mock::{
		new_test_ext, Configuration, Paras, System, Inclusion,
		MockGenesisConfig, Test, Shared,
	};
	use crate::initializer::SessionChangeNotification;
	use crate::configuration::HostConfiguration;
	use crate::paras::ParaGenesisArgs;
	use crate::scheduler::AssignmentKind;

	fn default_config() -> HostConfiguration<BlockNumber> {
		let mut config = HostConfiguration::default();
		config.parathread_cores = 1;
		config.max_code_size = 3;
		config
	}

	fn genesis_config(paras: Vec<(ParaId, bool)>) -> MockGenesisConfig {
		MockGenesisConfig {
			paras: paras::GenesisConfig {
				paras: paras.into_iter().map(|(id, is_chain)| (id, ParaGenesisArgs {
					genesis_head: Vec::new().into(),
					validation_code: Vec::new().into(),
					parachain: is_chain,
				})).collect(),
				..Default::default()
			},
			configuration: configuration::GenesisConfig {
				config: default_config(),
				..Default::default()
			},
			..Default::default()
		}
	}

	#[derive(Debug, Clone, Copy, PartialEq)]
	enum BackingKind {
		#[allow(unused)]
		Unanimous,
		Threshold,
		Lacking,
	}

	fn collator_sign_candidate(
		collator: Sr25519Keyring,
		candidate: &mut CommittedCandidateReceipt,
	) {
		candidate.descriptor.collator = collator.public().into();

		let payload = primitives::v1::collator_signature_payload(
			&candidate.descriptor.relay_parent,
			&candidate.descriptor.para_id,
			&candidate.descriptor.persisted_validation_data_hash,
			&candidate.descriptor.pov_hash,
			&candidate.descriptor.validation_code_hash,
		);

		candidate.descriptor.signature = collator.sign(&payload[..]).into();
		assert!(candidate.descriptor().check_collator_signature().is_ok());
	}

	async fn back_candidate(
		candidate: CommittedCandidateReceipt,
		validators: &[Sr25519Keyring],
		group: &[ValidatorIndex],
		keystore: &SyncCryptoStorePtr,
		signing_context: &SigningContext,
		kind: BackingKind,
	) -> BackedCandidate {
		let mut validator_indices = bitvec::bitvec![BitOrderLsb0, u8; 0; group.len()];
		let threshold = (group.len() / 2) + 1;

		let signing = match kind {
			BackingKind::Unanimous => group.len(),
			BackingKind::Threshold => threshold,
			BackingKind::Lacking => threshold.saturating_sub(1),
		};

		let mut validity_votes = Vec::with_capacity(signing);
		let candidate_hash = candidate.hash();

		for (idx_in_group, val_idx) in group.iter().enumerate().take(signing) {
			let key: Sr25519Keyring = validators[val_idx.0 as usize];
			*validator_indices.get_mut(idx_in_group).unwrap() = true;

			let signature = SignedStatement::sign(
				&keystore,
				Statement::Valid(candidate_hash),
				signing_context,
				*val_idx,
				&key.public().into(),
			).await.unwrap().unwrap().signature().clone();

			validity_votes.push(ValidityAttestation::Explicit(signature).into());
		}

		let backed = BackedCandidate {
			candidate,
			validity_votes,
			validator_indices,
		};

		let should_pass = match kind {
			BackingKind::Unanimous | BackingKind::Threshold => true,
			BackingKind::Lacking => false,
		};

		let successfully_backed = primitives::v1::check_candidate_backing(
			&backed,
			signing_context,
			group.len(),
			|i| Some(validators[group[i].0 as usize].public().into()),
		).ok().unwrap_or(0) * 2 > group.len();

		if should_pass {
			assert!(successfully_backed);
		} else {
			assert!(!successfully_backed);
		}

		backed
	}

	fn run_to_block(
		to: BlockNumber,
		new_session: impl Fn(BlockNumber) -> Option<SessionChangeNotification<BlockNumber>>,
	) {
		while System::block_number() < to {
			let b = System::block_number();

			Inclusion::initializer_finalize();
			Paras::initializer_finalize();
			Shared::initializer_finalize();

			if let Some(notification) = new_session(b + 1) {
				Shared::initializer_on_new_session(
					notification.session_index,
					notification.random_seed,
					&notification.new_config,
					notification.validators.clone(),
				);
				Paras::initializer_on_new_session(&notification);
				Inclusion::initializer_on_new_session(&notification);
			}

			System::on_finalize(b);

			System::on_initialize(b + 1);
			System::set_block_number(b + 1);

			Shared::initializer_initialize(b + 1);
			Paras::initializer_initialize(b + 1);
			Inclusion::initializer_initialize(b + 1);
		}
	}

	fn expected_bits() -> usize {
		Paras::parachains().len() + Configuration::config().parathread_cores as usize
	}

	fn default_bitfield() -> AvailabilityBitfield {
		AvailabilityBitfield(bitvec::bitvec![BitOrderLsb0, u8; 0; expected_bits()])
	}

	fn default_availability_votes() -> BitVec<BitOrderLsb0, u8> {
		bitvec::bitvec![BitOrderLsb0, u8; 0; Shared::active_validator_keys().len()]
	}

	fn default_backing_bitfield() -> BitVec<BitOrderLsb0, u8> {
		bitvec::bitvec![BitOrderLsb0, u8; 0; Shared::active_validator_keys().len()]
	}

	fn backing_bitfield(v: &[usize]) -> BitVec<BitOrderLsb0, u8> {
		let mut b = default_backing_bitfield();
		for i in v {
			b.set(*i, true);
		}
		b
	}

	fn validator_pubkeys(val_ids: &[Sr25519Keyring]) -> Vec<ValidatorId> {
		val_ids.iter().map(|v| v.public().into()).collect()
	}

	async fn sign_bitfield(
		keystore: &SyncCryptoStorePtr,
		key: &Sr25519Keyring,
		validator_index: ValidatorIndex,
		bitfield: AvailabilityBitfield,
		signing_context: &SigningContext,
	)
		-> SignedAvailabilityBitfield
	{
		SignedAvailabilityBitfield::sign(
			&keystore,
			bitfield,
			&signing_context,
			validator_index,
			&key.public().into(),
		).await.unwrap().unwrap()
	}

	#[derive(Default)]
	struct TestCandidateBuilder {
		para_id: ParaId,
		head_data: HeadData,
		pov_hash: Hash,
		relay_parent: Hash,
		persisted_validation_data_hash: Hash,
		new_validation_code: Option<ValidationCode>,
		validation_code: ValidationCode,
		hrmp_watermark: BlockNumber,
	}

	impl TestCandidateBuilder {
		fn build(self) -> CommittedCandidateReceipt {
			CommittedCandidateReceipt {
				descriptor: CandidateDescriptor {
					para_id: self.para_id,
					pov_hash: self.pov_hash,
					relay_parent: self.relay_parent,
					persisted_validation_data_hash: self.persisted_validation_data_hash,
					validation_code_hash: self.validation_code.hash(),
					..Default::default()
				},
				commitments: CandidateCommitments {
					head_data: self.head_data,
					new_validation_code: self.new_validation_code,
					hrmp_watermark: self.hrmp_watermark,
					..Default::default()
				},
			}
		}
	}

	fn make_vdata_hash(para_id: ParaId) -> Option<Hash> {
		let relay_parent_number = <frame_system::Pallet<Test>>::block_number() - 1;
		let persisted_validation_data
			= crate::util::make_persisted_validation_data::<Test>(
				para_id,
				relay_parent_number,
				Default::default(),
			)?;
		Some(persisted_validation_data.hash())
	}

	#[test]
	fn collect_pending_cleans_up_pending() {
		let chain_a = ParaId::from(1);
		let chain_b = ParaId::from(2);
		let thread_a = ParaId::from(3);

		let paras = vec![(chain_a, true), (chain_b, true), (thread_a, false)];
		new_test_ext(genesis_config(paras)).execute_with(|| {
			let default_candidate = TestCandidateBuilder::default().build();
			<PendingAvailability<Test>>::insert(chain_a, CandidatePendingAvailability {
				core: CoreIndex::from(0),
				hash: default_candidate.hash(),
				descriptor: default_candidate.descriptor.clone(),
				availability_votes: default_availability_votes(),
				relay_parent_number: 0,
				backed_in_number: 0,
				backers: default_backing_bitfield(),
				backing_group: GroupIndex::from(0),
			});
			PendingAvailabilityCommitments::insert(chain_a, default_candidate.commitments.clone());

			<PendingAvailability<Test>>::insert(&chain_b, CandidatePendingAvailability {
				core: CoreIndex::from(1),
				hash: default_candidate.hash(),
				descriptor: default_candidate.descriptor,
				availability_votes: default_availability_votes(),
				relay_parent_number: 0,
				backed_in_number: 0,
				backers: default_backing_bitfield(),
				backing_group: GroupIndex::from(1),
			});
			PendingAvailabilityCommitments::insert(chain_b, default_candidate.commitments);

			run_to_block(5, |_| None);

			assert!(<PendingAvailability<Test>>::get(&chain_a).is_some());
			assert!(<PendingAvailability<Test>>::get(&chain_b).is_some());
			assert!(<PendingAvailabilityCommitments>::get(&chain_a).is_some());
			assert!(<PendingAvailabilityCommitments>::get(&chain_b).is_some());

			Inclusion::collect_pending(|core, _since| core == CoreIndex::from(0));

			assert!(<PendingAvailability<Test>>::get(&chain_a).is_none());
			assert!(<PendingAvailability<Test>>::get(&chain_b).is_some());
			assert!(<PendingAvailabilityCommitments>::get(&chain_a).is_none());
			assert!(<PendingAvailabilityCommitments>::get(&chain_b).is_some());
		});
	}

	#[test]
	fn bitfield_checks() {
		let chain_a = ParaId::from(1);
		let chain_b = ParaId::from(2);
		let thread_a = ParaId::from(3);

		let paras = vec![(chain_a, true), (chain_b, true), (thread_a, false)];
		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Ferdie,
		];
		let keystore: SyncCryptoStorePtr = Arc::new(LocalKeystore::in_memory());
		for validator in validators.iter() {
			SyncCryptoStore::sr25519_generate_new(&*keystore, PARACHAIN_KEY_TYPE_ID, Some(&validator.to_seed())).unwrap();
		}
		let validator_public = validator_pubkeys(&validators);

		new_test_ext(genesis_config(paras)).execute_with(|| {
			shared::Module::<Test>::set_active_validators_ascending(validator_public.clone());
			shared::Module::<Test>::set_session_index(5);

			let signing_context = SigningContext {
				parent_hash: System::parent_hash(),
				session_index: 5,
			};

			let core_lookup = |core| match core {
				core if core == CoreIndex::from(0) => Some(chain_a),
				core if core == CoreIndex::from(1) => Some(chain_b),
				core if core == CoreIndex::from(2) => Some(thread_a),
				core if core == CoreIndex::from(3) => None, // for the expected_cores() + 1 test below.
				_ => panic!("out of bounds for testing"),
			};

			// wrong number of bits.
			{
				let mut bare_bitfield = default_bitfield();
				bare_bitfield.0.push(false);
				let signed = block_on(sign_bitfield(
					&keystore,
					&validators[0],
					ValidatorIndex(0),
					bare_bitfield,
					&signing_context,
				));

				assert!(Inclusion::process_bitfields(
					expected_bits(),
					vec![signed.into()],
					&core_lookup,
				).is_err());
			}

			// wrong number of bits: other way around.
			{
				let bare_bitfield = default_bitfield();
				let signed = block_on(sign_bitfield(
					&keystore,
					&validators[0],
					ValidatorIndex(0),
					bare_bitfield,
					&signing_context,
				));

				assert!(Inclusion::process_bitfields(
					expected_bits() + 1,
					vec![signed.into()],
					&core_lookup,
				).is_err());
			}

			// duplicate.
			{
				let bare_bitfield = default_bitfield();
				let signed: UncheckedSignedAvailabilityBitfield =
					block_on(sign_bitfield(
						&keystore,
						&validators[0],
						ValidatorIndex(0),
						bare_bitfield,
						&signing_context,
				)).into();

				assert!(Inclusion::process_bitfields(
					expected_bits(),
					vec![signed.clone(), signed],
					&core_lookup,
				).is_err());
			}

			// out of order.
			{
				let bare_bitfield = default_bitfield();
				let signed_0 = block_on(sign_bitfield(
					&keystore,
					&validators[0],
					ValidatorIndex(0),
					bare_bitfield.clone(),
					&signing_context,
				)).into();

				let signed_1 = block_on(sign_bitfield(
					&keystore,
					&validators[1],
					ValidatorIndex(1),
					bare_bitfield,
					&signing_context,
				)).into();

				assert!(Inclusion::process_bitfields(
					expected_bits(),
					vec![signed_1, signed_0],
					&core_lookup,
				).is_err());
			}

			// non-pending bit set.
			{
				let mut bare_bitfield = default_bitfield();
				*bare_bitfield.0.get_mut(0).unwrap() = true;
				let signed = block_on(sign_bitfield(
					&keystore,
					&validators[0],
					ValidatorIndex(0),
					bare_bitfield,
					&signing_context,
				));

				assert!(Inclusion::process_bitfields(
					expected_bits(),
					vec![signed.into()],
					&core_lookup,
				).is_err());
			}

			// empty bitfield signed: always OK, but kind of useless.
			{
				let bare_bitfield = default_bitfield();
				let signed = block_on(sign_bitfield(
					&keystore,
					&validators[0],
					ValidatorIndex(0),
					bare_bitfield,
					&signing_context,
				));

				assert!(Inclusion::process_bitfields(
					expected_bits(),
					vec![signed.into()],
					&core_lookup,
				).is_ok());
			}

			// bitfield signed with pending bit signed.
			{
				let mut bare_bitfield = default_bitfield();

				assert_eq!(core_lookup(CoreIndex::from(0)), Some(chain_a));

				let default_candidate = TestCandidateBuilder::default().build();
				<PendingAvailability<Test>>::insert(chain_a, CandidatePendingAvailability {
					core: CoreIndex::from(0),
					hash: default_candidate.hash(),
					descriptor: default_candidate.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: 0,
					backed_in_number: 0,
					backers: default_backing_bitfield(),
					backing_group: GroupIndex::from(0),
				});
				PendingAvailabilityCommitments::insert(chain_a, default_candidate.commitments);

				*bare_bitfield.0.get_mut(0).unwrap() = true;
				let signed = block_on(sign_bitfield(
					&keystore,
					&validators[0],
					ValidatorIndex(0),
					bare_bitfield,
					&signing_context,
				));

				assert!(Inclusion::process_bitfields(
					expected_bits(),
					vec![signed.into()],
					&core_lookup,
				).is_ok());

				<PendingAvailability<Test>>::remove(chain_a);
				PendingAvailabilityCommitments::remove(chain_a);
			}

			// bitfield signed with pending bit signed, but no commitments.
			{
				let mut bare_bitfield = default_bitfield();

				assert_eq!(core_lookup(CoreIndex::from(0)), Some(chain_a));

				let default_candidate = TestCandidateBuilder::default().build();
				<PendingAvailability<Test>>::insert(chain_a, CandidatePendingAvailability {
					core: CoreIndex::from(0),
					hash: default_candidate.hash(),
					descriptor: default_candidate.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: 0,
					backed_in_number: 0,
					backers: default_backing_bitfield(),
					backing_group: GroupIndex::from(0),
				});

				*bare_bitfield.0.get_mut(0).unwrap() = true;
				let signed = block_on(sign_bitfield(
					&keystore,
					&validators[0],
					ValidatorIndex(0),
					bare_bitfield,
					&signing_context,
				));

				// no core is freed
				assert_eq!(
					Inclusion::process_bitfields(
						expected_bits(),
						vec![signed.into()],
						&core_lookup,
					),
					Ok(vec![]),
				);
			}
		});
	}

	#[test]
	fn supermajority_bitfields_trigger_availability() {
		let chain_a = ParaId::from(1);
		let chain_b = ParaId::from(2);
		let thread_a = ParaId::from(3);

		let paras = vec![(chain_a, true), (chain_b, true), (thread_a, false)];
		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Ferdie,
		];
		let keystore: SyncCryptoStorePtr = Arc::new(LocalKeystore::in_memory());
		for validator in validators.iter() {
			SyncCryptoStore::sr25519_generate_new(&*keystore, PARACHAIN_KEY_TYPE_ID, Some(&validator.to_seed())).unwrap();
		}
		let validator_public = validator_pubkeys(&validators);

		new_test_ext(genesis_config(paras)).execute_with(|| {
			shared::Module::<Test>::set_active_validators_ascending(validator_public.clone());
			shared::Module::<Test>::set_session_index(5);

			let signing_context = SigningContext {
				parent_hash: System::parent_hash(),
				session_index: 5,
			};

			let core_lookup = |core| match core {
				core if core == CoreIndex::from(0) => Some(chain_a),
				core if core == CoreIndex::from(1) => Some(chain_b),
				core if core == CoreIndex::from(2) => Some(thread_a),
				_ => panic!("Core out of bounds for 2 parachains and 1 parathread core."),
			};

			let candidate_a = TestCandidateBuilder {
				para_id: chain_a,
				head_data: vec![1, 2, 3, 4].into(),
				..Default::default()
			}.build();

			<PendingAvailability<Test>>::insert(chain_a, CandidatePendingAvailability {
				core: CoreIndex::from(0),
				hash: candidate_a.hash(),
				descriptor: candidate_a.descriptor,
				availability_votes: default_availability_votes(),
				relay_parent_number: 0,
				backed_in_number: 0,
				backers: backing_bitfield(&[3, 4]),
				backing_group: GroupIndex::from(0),
			});
			PendingAvailabilityCommitments::insert(chain_a, candidate_a.commitments);

			let candidate_b = TestCandidateBuilder {
				para_id: chain_b,
				head_data: vec![5, 6, 7, 8].into(),
				..Default::default()
			}.build();

			<PendingAvailability<Test>>::insert(chain_b, CandidatePendingAvailability {
				core: CoreIndex::from(1),
				hash: candidate_b.hash(),
				descriptor: candidate_b.descriptor,
				availability_votes: default_availability_votes(),
				relay_parent_number: 0,
				backed_in_number: 0,
				backers: backing_bitfield(&[0, 2]),
				backing_group: GroupIndex::from(1),
			});
			PendingAvailabilityCommitments::insert(chain_b, candidate_b.commitments);

			// this bitfield signals that a and b are available.
			let a_and_b_available = {
				let mut bare_bitfield = default_bitfield();
				*bare_bitfield.0.get_mut(0).unwrap() = true;
				*bare_bitfield.0.get_mut(1).unwrap() = true;

				bare_bitfield
			};

			// this bitfield signals that only a is available.
			let a_available = {
				let mut bare_bitfield = default_bitfield();
				*bare_bitfield.0.get_mut(0).unwrap() = true;

				bare_bitfield
			};

			let threshold = availability_threshold(validators.len());

			// 4 of 5 first value >= 2/3
			assert_eq!(threshold, 4);

			let signed_bitfields = validators.iter().enumerate().filter_map(|(i, key)| {
				let to_sign = if i < 3 {
					a_and_b_available.clone()
				} else if i < 4 {
					a_available.clone()
				} else {
					// sign nothing.
					return None
				};

				Some(block_on(sign_bitfield(
					&keystore,
					key,
					ValidatorIndex(i as _),
					to_sign,
					&signing_context,
				)).into())
			}).collect();

			assert!(Inclusion::process_bitfields(
				expected_bits(),
				signed_bitfields,
				&core_lookup,
			).is_ok());

			// chain A had 4 signing off, which is >= threshold.
			// chain B has 3 signing off, which is < threshold.
			assert!(<PendingAvailability<Test>>::get(&chain_a).is_none());
			assert!(<PendingAvailabilityCommitments>::get(&chain_a).is_none());
			assert!(<PendingAvailabilityCommitments>::get(&chain_b).is_some());
			assert_eq!(
				<PendingAvailability<Test>>::get(&chain_b).unwrap().availability_votes,
				{
					// check that votes from first 3 were tracked.

					let mut votes = default_availability_votes();
					*votes.get_mut(0).unwrap() = true;
					*votes.get_mut(1).unwrap() = true;
					*votes.get_mut(2).unwrap() = true;

					votes
				},
			);

			// and check that chain head was enacted.
			assert_eq!(Paras::para_head(&chain_a), Some(vec![1, 2, 3, 4].into()));

			// Check that rewards are applied.
			{
				let rewards = crate::mock::availability_rewards();

				assert_eq!(rewards.len(), 4);
				assert_eq!(rewards.get(&ValidatorIndex(0)).unwrap(), &1);
				assert_eq!(rewards.get(&ValidatorIndex(1)).unwrap(), &1);
				assert_eq!(rewards.get(&ValidatorIndex(2)).unwrap(), &1);
				assert_eq!(rewards.get(&ValidatorIndex(3)).unwrap(), &1);
			}

			{
				let rewards = crate::mock::backing_rewards();

				assert_eq!(rewards.len(), 2);
				assert_eq!(rewards.get(&ValidatorIndex(3)).unwrap(), &1);
				assert_eq!(rewards.get(&ValidatorIndex(4)).unwrap(), &1);
			}
		});
	}

	#[test]
	fn candidate_checks() {
		let chain_a = ParaId::from(1);
		let chain_b = ParaId::from(2);
		let thread_a = ParaId::from(3);

		// The block number of the relay-parent for testing.
		const RELAY_PARENT_NUM: BlockNumber = 4;

		let paras = vec![(chain_a, true), (chain_b, true), (thread_a, false)];
		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Ferdie,
		];
		let keystore: SyncCryptoStorePtr = Arc::new(LocalKeystore::in_memory());
		for validator in validators.iter() {
			SyncCryptoStore::sr25519_generate_new(&*keystore, PARACHAIN_KEY_TYPE_ID, Some(&validator.to_seed())).unwrap();
		}
		let validator_public = validator_pubkeys(&validators);

		new_test_ext(genesis_config(paras)).execute_with(|| {
			shared::Module::<Test>::set_active_validators_ascending(validator_public.clone());
			shared::Module::<Test>::set_session_index(5);

			run_to_block(5, |_| None);

			let signing_context = SigningContext {
				parent_hash: System::parent_hash(),
				session_index: 5,
			};

			let group_validators = |group_index: GroupIndex| match group_index {
				group_index if group_index == GroupIndex::from(0) => Some(vec![0, 1]),
				group_index if group_index == GroupIndex::from(1) => Some(vec![2, 3]),
				group_index if group_index == GroupIndex::from(2) => Some(vec![4]),
				_ => panic!("Group index out of bounds for 2 parachains and 1 parathread core"),
			}.map(|m| m.into_iter().map(ValidatorIndex).collect::<Vec<_>>());

			let thread_collator: CollatorId = Sr25519Keyring::Two.public().into();

			let chain_a_assignment = CoreAssignment {
				core: CoreIndex::from(0),
				para_id: chain_a,
				kind: AssignmentKind::Parachain,
				group_idx: GroupIndex::from(0),
			};

			let chain_b_assignment = CoreAssignment {
				core: CoreIndex::from(1),
				para_id: chain_b,
				kind: AssignmentKind::Parachain,
				group_idx: GroupIndex::from(1),
			};

			let thread_a_assignment = CoreAssignment {
				core: CoreIndex::from(2),
				para_id: thread_a,
				kind: AssignmentKind::Parathread(thread_collator.clone(), 0),
				group_idx: GroupIndex::from(2),
			};

			// unscheduled candidate.
			{
				let mut candidate = TestCandidateBuilder {
					para_id: chain_a,
					relay_parent: System::parent_hash(),
					pov_hash: Hash::repeat_byte(1),
					persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
					hrmp_watermark: RELAY_PARENT_NUM,
					..Default::default()
				}.build();
				collator_sign_candidate(
					Sr25519Keyring::One,
					&mut candidate,
				);

				let backed = block_on(back_candidate(
					candidate,
					&validators,
					group_validators(GroupIndex::from(0)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Threshold,
				));

				assert_eq!(
					Inclusion::process_candidates(
						Default::default(),
						vec![backed],
						vec![chain_b_assignment.clone()],
						&group_validators,
					),
					Err(Error::<Test>::UnscheduledCandidate.into()),
				);
			}

			// candidates out of order.
			{
				let mut candidate_a = TestCandidateBuilder {
					para_id: chain_a,
					relay_parent: System::parent_hash(),
					pov_hash: Hash::repeat_byte(1),
					persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
					hrmp_watermark: RELAY_PARENT_NUM,
					..Default::default()
				}.build();
				let mut candidate_b = TestCandidateBuilder {
					para_id: chain_b,
					relay_parent: System::parent_hash(),
					pov_hash: Hash::repeat_byte(2),
					persisted_validation_data_hash: make_vdata_hash(chain_b).unwrap(),
					hrmp_watermark: RELAY_PARENT_NUM,
					..Default::default()
				}.build();

				collator_sign_candidate(
					Sr25519Keyring::One,
					&mut candidate_a,
				);

				collator_sign_candidate(
					Sr25519Keyring::Two,
					&mut candidate_b,
				);

				let backed_a = block_on(back_candidate(
					candidate_a,
					&validators,
					group_validators(GroupIndex::from(0)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Threshold,
				));

				let backed_b = block_on(back_candidate(
					candidate_b,
					&validators,
					group_validators(GroupIndex::from(1)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Threshold,
				));

				// out-of-order manifests as unscheduled.
				assert_eq!(
					Inclusion::process_candidates(
						Default::default(),
						vec![backed_b, backed_a],
						vec![chain_a_assignment.clone(), chain_b_assignment.clone()],
						&group_validators,
					),
					Err(Error::<Test>::UnscheduledCandidate.into()),
				);
			}

			// candidate not backed.
			{
				let mut candidate = TestCandidateBuilder {
					para_id: chain_a,
					relay_parent: System::parent_hash(),
					pov_hash: Hash::repeat_byte(1),
					persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
					hrmp_watermark: RELAY_PARENT_NUM,
					..Default::default()
				}.build();
				collator_sign_candidate(
					Sr25519Keyring::One,
					&mut candidate,
				);

				let backed = block_on(back_candidate(
					candidate,
					&validators,
					group_validators(GroupIndex::from(0)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Lacking,
				));

				assert_eq!(
					Inclusion::process_candidates(
						Default::default(),
						vec![backed],
						vec![chain_a_assignment.clone()],
						&group_validators,
					),
					Err(Error::<Test>::InsufficientBacking.into()),
				);
			}

			// candidate not in parent context.
			{
				let wrong_parent_hash = Hash::repeat_byte(222);
				assert!(System::parent_hash() != wrong_parent_hash);

				let mut candidate = TestCandidateBuilder {
					para_id: chain_a,
					relay_parent: wrong_parent_hash,
					pov_hash: Hash::repeat_byte(1),
					persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
					..Default::default()
				}.build();
				collator_sign_candidate(
					Sr25519Keyring::One,
					&mut candidate,
				);

				let backed = block_on(back_candidate(
					candidate,
					&validators,
					group_validators(GroupIndex::from(0)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Threshold,
				));

				assert_eq!(
					Inclusion::process_candidates(
						Default::default(),
						vec![backed],
						vec![chain_a_assignment.clone()],
						&group_validators,
					),
					Err(Error::<Test>::CandidateNotInParentContext.into()),
				);
			}

			// candidate has wrong collator.
			{
				let mut candidate = TestCandidateBuilder {
					para_id: thread_a,
					relay_parent: System::parent_hash(),
					pov_hash: Hash::repeat_byte(1),
					persisted_validation_data_hash: make_vdata_hash(thread_a).unwrap(),
					hrmp_watermark: RELAY_PARENT_NUM,
					..Default::default()
				}.build();

				assert!(CollatorId::from(Sr25519Keyring::One.public()) != thread_collator);
				collator_sign_candidate(
					Sr25519Keyring::One,
					&mut candidate,
				);

				let backed = block_on(back_candidate(
					candidate,
					&validators,
					group_validators(GroupIndex::from(2)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Threshold,
				));

				assert_eq!(
					Inclusion::process_candidates(
						Default::default(),
						vec![backed],
						vec![
							chain_a_assignment.clone(),
							chain_b_assignment.clone(),
							thread_a_assignment.clone(),
						],
						&group_validators,
					),
					Err(Error::<Test>::WrongCollator.into()),
				);
			}

			// candidate not well-signed by collator.
			{
				let mut candidate = TestCandidateBuilder {
					para_id: thread_a,
					relay_parent: System::parent_hash(),
					pov_hash: Hash::repeat_byte(1),
					persisted_validation_data_hash: make_vdata_hash(thread_a).unwrap(),
					hrmp_watermark: RELAY_PARENT_NUM,
					..Default::default()
				}.build();

				assert_eq!(CollatorId::from(Sr25519Keyring::Two.public()), thread_collator);
				collator_sign_candidate(
					Sr25519Keyring::Two,
					&mut candidate,
				);

				// change the candidate after signing.
				candidate.descriptor.pov_hash = Hash::repeat_byte(2);

				let backed = block_on(back_candidate(
					candidate,
					&validators,
					group_validators(GroupIndex::from(2)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Threshold,
				));

				assert_eq!(
					Inclusion::process_candidates(
						Default::default(),
						vec![backed],
						vec![thread_a_assignment.clone()],
						&group_validators,
					),
					Err(Error::<Test>::NotCollatorSigned.into()),
				);
			}

			// para occupied - reject.
			{
				let mut candidate = TestCandidateBuilder {
					para_id: chain_a,
					relay_parent: System::parent_hash(),
					pov_hash: Hash::repeat_byte(1),
					persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
					hrmp_watermark: RELAY_PARENT_NUM,
					..Default::default()
				}.build();

				collator_sign_candidate(
					Sr25519Keyring::One,
					&mut candidate,
				);

				let backed = block_on(back_candidate(
					candidate,
					&validators,
					group_validators(GroupIndex::from(0)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Threshold,
				));

				let candidate = TestCandidateBuilder::default().build();
				<PendingAvailability<Test>>::insert(&chain_a, CandidatePendingAvailability {
					core: CoreIndex::from(0),
					hash: candidate.hash(),
					descriptor: candidate.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: 3,
					backed_in_number: 4,
					backers: default_backing_bitfield(),
					backing_group: GroupIndex::from(0),
				});
				<PendingAvailabilityCommitments>::insert(&chain_a, candidate.commitments);

				assert_eq!(
					Inclusion::process_candidates(
						Default::default(),
						vec![backed],
						vec![chain_a_assignment.clone()],
						&group_validators,
					),
					Err(Error::<Test>::CandidateScheduledBeforeParaFree.into()),
				);

				<PendingAvailability<Test>>::remove(&chain_a);
				<PendingAvailabilityCommitments>::remove(&chain_a);
			}

			// messed up commitments storage - do not panic - reject.
			{
				let mut candidate = TestCandidateBuilder {
					para_id: chain_a,
					relay_parent: System::parent_hash(),
					pov_hash: Hash::repeat_byte(1),
					persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
					hrmp_watermark: RELAY_PARENT_NUM,
					..Default::default()
				}.build();

				collator_sign_candidate(
					Sr25519Keyring::One,
					&mut candidate,
				);

				// this is not supposed to happen
				<PendingAvailabilityCommitments>::insert(&chain_a, candidate.commitments.clone());

				let backed = block_on(back_candidate(
					candidate,
					&validators,
					group_validators(GroupIndex::from(0)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Threshold,
				));

				assert_eq!(
					Inclusion::process_candidates(
						Default::default(),
						vec![backed],
						vec![chain_a_assignment.clone()],
						&group_validators,
					),
					Err(Error::<Test>::CandidateScheduledBeforeParaFree.into()),
				);

				<PendingAvailabilityCommitments>::remove(&chain_a);
			}

			// interfering code upgrade - reject
			{
				let mut candidate = TestCandidateBuilder {
					para_id: chain_a,
					relay_parent: System::parent_hash(),
					pov_hash: Hash::repeat_byte(1),
					new_validation_code: Some(vec![5, 6, 7, 8].into()),
					persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
					hrmp_watermark: RELAY_PARENT_NUM,
					..Default::default()
				}.build();

				collator_sign_candidate(
					Sr25519Keyring::One,
					&mut candidate,
				);

				let backed = block_on(back_candidate(
					candidate,
					&validators,
					group_validators(GroupIndex::from(0)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Threshold,
				));

				{
					let cfg = Configuration::config();
					let expected_at = 10 + cfg.validation_upgrade_delay;
					assert_eq!(expected_at, 10);
					Paras::schedule_code_upgrade(
						chain_a,
						vec![1, 2, 3, 4].into(),
						expected_at,
						&cfg,
					);

					assert_eq!(Paras::last_code_upgrade(chain_a, true), Some(expected_at));
				}

				assert_eq!(
					Inclusion::process_candidates(
						Default::default(),
						vec![backed],
						vec![chain_a_assignment.clone()],
						&group_validators,
					),
					Err(Error::<Test>::PrematureCodeUpgrade.into()),
				);
			}

			// Bad validation data hash - reject
			{
				let mut candidate = TestCandidateBuilder {
					para_id: chain_a,
					relay_parent: System::parent_hash(),
					pov_hash: Hash::repeat_byte(1),
					persisted_validation_data_hash: [42u8; 32].into(),
					hrmp_watermark: RELAY_PARENT_NUM,
					..Default::default()
				}.build();

				collator_sign_candidate(
					Sr25519Keyring::One,
					&mut candidate,
				);

				let backed = block_on(back_candidate(
					candidate,
					&validators,
					group_validators(GroupIndex::from(0)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Threshold,
				));

				assert_eq!(
					Inclusion::process_candidates(
						Default::default(),
						vec![backed],
						vec![chain_a_assignment.clone()],
						&group_validators,
					),
					Err(Error::<Test>::ValidationDataHashMismatch.into()),
				);
			}

			// bad validation code hash
			{
				let mut candidate = TestCandidateBuilder {
					para_id: chain_a,
					relay_parent: System::parent_hash(),
					pov_hash: Hash::repeat_byte(1),
					persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
					hrmp_watermark: RELAY_PARENT_NUM,
					validation_code: ValidationCode(vec![1]),
					..Default::default()
				}.build();

				collator_sign_candidate(
					Sr25519Keyring::One,
					&mut candidate,
				);

				let backed = block_on(back_candidate(
					candidate,
					&validators,
					group_validators(GroupIndex::from(0)).unwrap().as_ref(),
					&keystore,
					&signing_context,
					BackingKind::Threshold,
				));

				assert_eq!(
					Inclusion::process_candidates(
						Default::default(),
						vec![backed],
						vec![chain_a_assignment.clone()],
						&group_validators,
					),
					Err(Error::<Test>::InvalidValidationCodeHash.into()),
				);
			}
		});
	}

	#[test]
	fn backing_works() {
		let chain_a = ParaId::from(1);
		let chain_b = ParaId::from(2);
		let thread_a = ParaId::from(3);

		// The block number of the relay-parent for testing.
		const RELAY_PARENT_NUM: BlockNumber = 4;

		let paras = vec![(chain_a, true), (chain_b, true), (thread_a, false)];
		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Ferdie,
		];
		let keystore: SyncCryptoStorePtr = Arc::new(LocalKeystore::in_memory());
		for validator in validators.iter() {
			SyncCryptoStore::sr25519_generate_new(&*keystore, PARACHAIN_KEY_TYPE_ID, Some(&validator.to_seed())).unwrap();
		}
		let validator_public = validator_pubkeys(&validators);

		new_test_ext(genesis_config(paras)).execute_with(|| {
			shared::Module::<Test>::set_active_validators_ascending(validator_public.clone());
			shared::Module::<Test>::set_session_index(5);

			run_to_block(5, |_| None);

			let signing_context = SigningContext {
				parent_hash: System::parent_hash(),
				session_index: 5,
			};

			let group_validators = |group_index: GroupIndex| match group_index {
				group_index if group_index == GroupIndex::from(0) => Some(vec![0, 1]),
				group_index if group_index == GroupIndex::from(1) => Some(vec![2, 3]),
				group_index if group_index == GroupIndex::from(2) => Some(vec![4]),
				_ => panic!("Group index out of bounds for 2 parachains and 1 parathread core"),
			}.map(|vs| vs.into_iter().map(ValidatorIndex).collect::<Vec<_>>());

			let thread_collator: CollatorId = Sr25519Keyring::Two.public().into();

			let chain_a_assignment = CoreAssignment {
				core: CoreIndex::from(0),
				para_id: chain_a,
				kind: AssignmentKind::Parachain,
				group_idx: GroupIndex::from(0),
			};

			let chain_b_assignment = CoreAssignment {
				core: CoreIndex::from(1),
				para_id: chain_b,
				kind: AssignmentKind::Parachain,
				group_idx: GroupIndex::from(1),
			};

			let thread_a_assignment = CoreAssignment {
				core: CoreIndex::from(2),
				para_id: thread_a,
				kind: AssignmentKind::Parathread(thread_collator.clone(), 0),
				group_idx: GroupIndex::from(2),
			};

			let mut candidate_a = TestCandidateBuilder {
				para_id: chain_a,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(1),
				persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
				hrmp_watermark: RELAY_PARENT_NUM,
				..Default::default()
			}.build();
			collator_sign_candidate(
				Sr25519Keyring::One,
				&mut candidate_a,
			);

			let mut candidate_b = TestCandidateBuilder {
				para_id: chain_b,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(2),
				persisted_validation_data_hash: make_vdata_hash(chain_b).unwrap(),
				hrmp_watermark: RELAY_PARENT_NUM,
				..Default::default()
			}.build();
			collator_sign_candidate(
				Sr25519Keyring::One,
				&mut candidate_b,
			);

			let mut candidate_c = TestCandidateBuilder {
				para_id: thread_a,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(3),
				persisted_validation_data_hash: make_vdata_hash(thread_a).unwrap(),
				hrmp_watermark: RELAY_PARENT_NUM,
				..Default::default()
			}.build();
			collator_sign_candidate(
				Sr25519Keyring::Two,
				&mut candidate_c,
			);

			let backed_a = block_on(back_candidate(
				candidate_a.clone(),
				&validators,
				group_validators(GroupIndex::from(0)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
			));

			let backed_b = block_on(back_candidate(
				candidate_b.clone(),
				&validators,
				group_validators(GroupIndex::from(1)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
			));

			let backed_c = block_on(back_candidate(
				candidate_c.clone(),
				&validators,
				group_validators(GroupIndex::from(2)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
			));

			let occupied_cores = Inclusion::process_candidates(
				Default::default(),
				vec![backed_a, backed_b, backed_c],
				vec![
					chain_a_assignment.clone(),
					chain_b_assignment.clone(),
					thread_a_assignment.clone(),
				],
				&group_validators,
			).expect("candidates scheduled, in order, and backed");

			assert_eq!(occupied_cores, vec![CoreIndex::from(0), CoreIndex::from(1), CoreIndex::from(2)]);

			assert_eq!(
				<PendingAvailability<Test>>::get(&chain_a),
				Some(CandidatePendingAvailability {
					core: CoreIndex::from(0),
					hash: candidate_a.hash(),
					descriptor: candidate_a.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: System::block_number() - 1,
					backed_in_number: System::block_number(),
					backers: backing_bitfield(&[0, 1]),
					backing_group: GroupIndex::from(0),
				})
			);
			assert_eq!(
				<PendingAvailabilityCommitments>::get(&chain_a),
				Some(candidate_a.commitments),
			);

			assert_eq!(
				<PendingAvailability<Test>>::get(&chain_b),
				Some(CandidatePendingAvailability {
					core: CoreIndex::from(1),
					hash: candidate_b.hash(),
					descriptor: candidate_b.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: System::block_number() - 1,
					backed_in_number: System::block_number(),
					backers: backing_bitfield(&[2, 3]),
					backing_group: GroupIndex::from(1),
				})
			);
			assert_eq!(
				<PendingAvailabilityCommitments>::get(&chain_b),
				Some(candidate_b.commitments),
			);

			assert_eq!(
				<PendingAvailability<Test>>::get(&thread_a),
				Some(CandidatePendingAvailability {
					core: CoreIndex::from(2),
					hash: candidate_c.hash(),
					descriptor: candidate_c.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: System::block_number() - 1,
					backed_in_number: System::block_number(),
					backers: backing_bitfield(&[4]),
					backing_group: GroupIndex::from(2),
				})
			);
			assert_eq!(
				<PendingAvailabilityCommitments>::get(&thread_a),
				Some(candidate_c.commitments),
			);
		});
	}

	#[test]
	fn can_include_candidate_with_ok_code_upgrade() {
		let chain_a = ParaId::from(1);

		// The block number of the relay-parent for testing.
		const RELAY_PARENT_NUM: BlockNumber = 4;

		let paras = vec![(chain_a, true)];
		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Ferdie,
		];
		let keystore: SyncCryptoStorePtr = Arc::new(LocalKeystore::in_memory());
		for validator in validators.iter() {
			SyncCryptoStore::sr25519_generate_new(&*keystore, PARACHAIN_KEY_TYPE_ID, Some(&validator.to_seed())).unwrap();
		}
		let validator_public = validator_pubkeys(&validators);

		new_test_ext(genesis_config(paras)).execute_with(|| {
			shared::Module::<Test>::set_active_validators_ascending(validator_public.clone());
			shared::Module::<Test>::set_session_index(5);

			run_to_block(5, |_| None);

			let signing_context = SigningContext {
				parent_hash: System::parent_hash(),
				session_index: 5,
			};

			let group_validators = |group_index: GroupIndex| match group_index {
				group_index if group_index == GroupIndex::from(0) => Some(vec![0, 1, 2, 3, 4]),
				_ => panic!("Group index out of bounds for 1 parachain"),
			}.map(|vs| vs.into_iter().map(ValidatorIndex).collect::<Vec<_>>());

			let chain_a_assignment = CoreAssignment {
				core: CoreIndex::from(0),
				para_id: chain_a,
				kind: AssignmentKind::Parachain,
				group_idx: GroupIndex::from(0),
			};

			let mut candidate_a = TestCandidateBuilder {
				para_id: chain_a,
				relay_parent: System::parent_hash(),
				pov_hash: Hash::repeat_byte(1),
				persisted_validation_data_hash: make_vdata_hash(chain_a).unwrap(),
				new_validation_code: Some(vec![1, 2, 3].into()),
				hrmp_watermark: RELAY_PARENT_NUM,
				..Default::default()
			}.build();
			collator_sign_candidate(
				Sr25519Keyring::One,
				&mut candidate_a,
			);

			let backed_a = block_on(back_candidate(
				candidate_a.clone(),
				&validators,
				group_validators(GroupIndex::from(0)).unwrap().as_ref(),
				&keystore,
				&signing_context,
				BackingKind::Threshold,
			));

			let occupied_cores = Inclusion::process_candidates(
				Default::default(),
				vec![backed_a],
				vec![
					chain_a_assignment.clone(),
				],
				&group_validators,
			).expect("candidates scheduled, in order, and backed");

			assert_eq!(occupied_cores, vec![CoreIndex::from(0)]);

			assert_eq!(
				<PendingAvailability<Test>>::get(&chain_a),
				Some(CandidatePendingAvailability {
					core: CoreIndex::from(0),
					hash: candidate_a.hash(),
					descriptor: candidate_a.descriptor,
					availability_votes: default_availability_votes(),
					relay_parent_number: System::block_number() - 1,
					backed_in_number: System::block_number(),
					backers: backing_bitfield(&[0, 1, 2]),
					backing_group: GroupIndex::from(0),
				})
			);
			assert_eq!(
				<PendingAvailabilityCommitments>::get(&chain_a),
				Some(candidate_a.commitments),
			);
		});
	}

	#[test]
	fn session_change_wipes() {
		let chain_a = ParaId::from(1);
		let chain_b = ParaId::from(2);
		let thread_a = ParaId::from(3);

		let paras = vec![(chain_a, true), (chain_b, true), (thread_a, false)];
		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Ferdie,
		];
		let keystore: SyncCryptoStorePtr = Arc::new(LocalKeystore::in_memory());
		for validator in validators.iter() {
			SyncCryptoStore::sr25519_generate_new(&*keystore, PARACHAIN_KEY_TYPE_ID, Some(&validator.to_seed())).unwrap();
		}
		let validator_public = validator_pubkeys(&validators);

		new_test_ext(genesis_config(paras)).execute_with(|| {
			shared::Module::<Test>::set_active_validators_ascending(validator_public.clone());
			shared::Module::<Test>::set_session_index(5);

			let validators_new = vec![
				Sr25519Keyring::Alice,
				Sr25519Keyring::Bob,
				Sr25519Keyring::Charlie,
			];

			let validator_public_new = validator_pubkeys(&validators_new);

			run_to_block(10, |_| None);

			<AvailabilityBitfields<Test>>::insert(
				&ValidatorIndex(0),
				AvailabilityBitfieldRecord {
					bitfield: default_bitfield(),
					submitted_at: 9,
				},
			);

			<AvailabilityBitfields<Test>>::insert(
				&ValidatorIndex(1),
				AvailabilityBitfieldRecord {
					bitfield: default_bitfield(),
					submitted_at: 9,
				},
			);

			<AvailabilityBitfields<Test>>::insert(
				&ValidatorIndex(4),
				AvailabilityBitfieldRecord {
					bitfield: default_bitfield(),
					submitted_at: 9,
				},
			);

			let candidate = TestCandidateBuilder::default().build();
			<PendingAvailability<Test>>::insert(&chain_a, CandidatePendingAvailability {
				core: CoreIndex::from(0),
				hash: candidate.hash(),
				descriptor: candidate.descriptor.clone(),
				availability_votes: default_availability_votes(),
				relay_parent_number: 5,
				backed_in_number: 6,
				backers: default_backing_bitfield(),
				backing_group: GroupIndex::from(0),
			});
			<PendingAvailabilityCommitments>::insert(&chain_a, candidate.commitments.clone());

			<PendingAvailability<Test>>::insert(&chain_b, CandidatePendingAvailability {
				core: CoreIndex::from(1),
				hash: candidate.hash(),
				descriptor: candidate.descriptor,
				availability_votes: default_availability_votes(),
				relay_parent_number: 6,
				backed_in_number: 7,
				backers: default_backing_bitfield(),
				backing_group: GroupIndex::from(1),
			});
			<PendingAvailabilityCommitments>::insert(&chain_b, candidate.commitments);

			run_to_block(11, |_| None);

			assert_eq!(shared::Module::<Test>::session_index(), 5);

			assert!(<AvailabilityBitfields<Test>>::get(&ValidatorIndex(0)).is_some());
			assert!(<AvailabilityBitfields<Test>>::get(&ValidatorIndex(1)).is_some());
			assert!(<AvailabilityBitfields<Test>>::get(&ValidatorIndex(4)).is_some());

			assert!(<PendingAvailability<Test>>::get(&chain_a).is_some());
			assert!(<PendingAvailability<Test>>::get(&chain_b).is_some());
			assert!(<PendingAvailabilityCommitments>::get(&chain_a).is_some());
			assert!(<PendingAvailabilityCommitments>::get(&chain_b).is_some());

			run_to_block(12, |n| match n {
				12 => Some(SessionChangeNotification {
					validators: validator_public_new.clone(),
					queued: Vec::new(),
					prev_config: default_config(),
					new_config: default_config(),
					random_seed: Default::default(),
					session_index: 6,
				}),
				_ => None,
			});

			assert_eq!(shared::Module::<Test>::session_index(), 6);

			assert!(<AvailabilityBitfields<Test>>::get(&ValidatorIndex(0)).is_none());
			assert!(<AvailabilityBitfields<Test>>::get(&ValidatorIndex(1)).is_none());
			assert!(<AvailabilityBitfields<Test>>::get(&ValidatorIndex(4)).is_none());

			assert!(<PendingAvailability<Test>>::get(&chain_a).is_none());
			assert!(<PendingAvailability<Test>>::get(&chain_b).is_none());
			assert!(<PendingAvailabilityCommitments>::get(&chain_a).is_none());
			assert!(<PendingAvailabilityCommitments>::get(&chain_b).is_none());

			assert!(<AvailabilityBitfields<Test>>::iter().collect::<Vec<_>>().is_empty());
			assert!(<PendingAvailability<Test>>::iter().collect::<Vec<_>>().is_empty());
			assert!(<PendingAvailabilityCommitments>::iter().collect::<Vec<_>>().is_empty());
		});
	}
}
