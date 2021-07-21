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

//! Configuration manager for the Polkadot runtime parachains logic.
//!
//! Configuration can change only at session boundaries and is buffered until then.

use sp_std::prelude::*;
use primitives::v1::{Balance, SessionIndex, MAX_CODE_SIZE, MAX_POV_SIZE};
use frame_support::{
	decl_storage, decl_module, decl_error,
	ensure,
	dispatch::DispatchResult,
	weights::{DispatchClass, Weight},
};
use parity_scale_codec::{Encode, Decode};
use frame_system::ensure_root;
use sp_runtime::traits::Zero;
use crate::shared;

/// All configuration of the runtime with respect to parachains and parathreads.
#[derive(Clone, Encode, Decode, PartialEq, sp_core::RuntimeDebug)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
pub struct HostConfiguration<BlockNumber> {
	// NOTE: This structure is used by parachains via merkle proofs. Therefore, this struct requires
	// special treatment.
	//
	// A parachain requested this struct can only depend on the subset of this struct. Specifically,
	// only a first few fields can be depended upon. These fields cannot be changed without
	// corresponding migration of the parachains.

	/**
	 * The parameters that are required for the parachains.
	 */

	/// The maximum validation code size, in bytes.
	pub max_code_size: u32,
	/// The maximum head-data size, in bytes.
	pub max_head_data_size: u32,
	/// Total number of individual messages allowed in the parachain -> relay-chain message queue.
	pub max_upward_queue_count: u32,
	/// Total size of messages allowed in the parachain -> relay-chain message queue before which
	/// no further messages may be added to it. If it exceeds this then the queue may contain only
	/// a single message.
	pub max_upward_queue_size: u32,
	/// The maximum size of an upward message that can be sent by a candidate.
	///
	/// This parameter affects the size upper bound of the `CandidateCommitments`.
	pub max_upward_message_size: u32,
	/// The maximum number of messages that a candidate can contain.
	///
	/// This parameter affects the size upper bound of the `CandidateCommitments`.
	pub max_upward_message_num_per_candidate: u32,
	/// The maximum number of outbound HRMP messages can be sent by a candidate.
	///
	/// This parameter affects the upper bound of size of `CandidateCommitments`.
	pub hrmp_max_message_num_per_candidate: u32,
	/// The minimum frequency at which parachains can update their validation code.
	pub validation_upgrade_frequency: BlockNumber,
	/// The delay, in blocks, before a validation upgrade is applied.
	pub validation_upgrade_delay: BlockNumber,

	/**
	 * The parameters that are not essential, but still may be of interest for parachains.
	 */

	/// The maximum POV block size, in bytes.
	pub max_pov_size: u32,
	/// The maximum size of a message that can be put in a downward message queue.
	///
	/// Since we require receiving at least one DMP message the obvious upper bound of the size is
	/// the PoV size. Of course, there is a lot of other different things that a parachain may
	/// decide to do with its PoV so this value in practice will be picked as a fraction of the PoV
	/// size.
	pub max_downward_message_size: u32,
	/// The amount of weight we wish to devote to the processing the dispatchable upward messages
	/// stage.
	///
	/// NOTE that this is a soft limit and could be exceeded.
	pub ump_service_total_weight: Weight,
	/// The maximum number of outbound HRMP channels a parachain is allowed to open.
	pub hrmp_max_parachain_outbound_channels: u32,
	/// The maximum number of outbound HRMP channels a parathread is allowed to open.
	pub hrmp_max_parathread_outbound_channels: u32,
	/// Number of sessions after which an HRMP open channel request expires.
	pub hrmp_open_request_ttl: u32,
	/// The deposit that the sender should provide for opening an HRMP channel.
	pub hrmp_sender_deposit: Balance,
	/// The deposit that the recipient should provide for accepting opening an HRMP channel.
	pub hrmp_recipient_deposit: Balance,
	/// The maximum number of messages allowed in an HRMP channel at once.
	pub hrmp_channel_max_capacity: u32,
	/// The maximum total size of messages in bytes allowed in an HRMP channel at once.
	pub hrmp_channel_max_total_size: u32,
	/// The maximum number of inbound HRMP channels a parachain is allowed to accept.
	pub hrmp_max_parachain_inbound_channels: u32,
	/// The maximum number of inbound HRMP channels a parathread is allowed to accept.
	pub hrmp_max_parathread_inbound_channels: u32,
	/// The maximum size of a message that could ever be put into an HRMP channel.
	///
	/// This parameter affects the upper bound of size of `CandidateCommitments`.
	pub hrmp_channel_max_message_size: u32,

	/**
	 * Parameters that will unlikely be needed by parachains.
	 */

	/// How long to keep code on-chain, in blocks. This should be sufficiently long that disputes
	/// have concluded.
	pub code_retention_period: BlockNumber,
	/// The amount of execution cores to dedicate to parathread execution.
	pub parathread_cores: u32,
	/// The number of retries that a parathread author has to submit their block.
	pub parathread_retries: u32,
	/// How often parachain groups should be rotated across parachains.
	///
	/// Must be non-zero.
	pub group_rotation_frequency: BlockNumber,
	/// The availability period, in blocks, for parachains. This is the amount of blocks
	/// after inclusion that validators have to make the block available and signal its availability to
	/// the chain.
	///
	/// Must be at least 1.
	pub chain_availability_period: BlockNumber,
	/// The availability period, in blocks, for parathreads. Same as the `chain_availability_period`,
	/// but a differing timeout due to differing requirements.
	///
	/// Must be at least 1.
	pub thread_availability_period: BlockNumber,
	/// The amount of blocks ahead to schedule parachains and parathreads.
	pub scheduling_lookahead: u32,
	/// The maximum number of validators to have per core.
	///
	/// `None` means no maximum.
	pub max_validators_per_core: Option<u32>,
	/// The maximum number of validators to use for parachain consensus, period.
	///
	/// `None` means no maximum.
	pub max_validators: Option<u32>,
	/// The amount of sessions to keep for disputes.
	pub dispute_period: SessionIndex,
	/// How long after dispute conclusion to accept statements.
	pub dispute_post_conclusion_acceptance_period: BlockNumber,
	/// The maximum number of dispute spam slots
	pub dispute_max_spam_slots: u32,
	/// How long it takes for a dispute to conclude by time-out, if no supermajority is reached.
	pub dispute_conclusion_by_time_out_period: BlockNumber,
	/// The amount of consensus slots that must pass between submitting an assignment and
	/// submitting an approval vote before a validator is considered a no-show.
	///
	/// Must be at least 1.
	pub no_show_slots: u32,
	/// The number of delay tranches in total.
	pub n_delay_tranches: u32,
	/// The width of the zeroth delay tranche for approval assignments. This many delay tranches
	/// beyond 0 are all consolidated to form a wide 0 tranche.
	pub zeroth_delay_tranche_width: u32,
	/// The number of validators needed to approve a block.
	pub needed_approvals: u32,
	/// The number of samples to do of the `RelayVRFModulo` approval assignment criterion.
	pub relay_vrf_modulo_samples: u32,
}

impl<BlockNumber: Default + From<u32>> Default for HostConfiguration<BlockNumber> {
	fn default() -> Self {
		Self {
			group_rotation_frequency: 1u32.into(),
			chain_availability_period: 1u32.into(),
			thread_availability_period: 1u32.into(),
			no_show_slots: 1u32.into(),
			validation_upgrade_frequency: Default::default(),
			validation_upgrade_delay: Default::default(),
			code_retention_period: Default::default(),
			max_code_size: Default::default(),
			max_pov_size: Default::default(),
			max_head_data_size: Default::default(),
			parathread_cores: Default::default(),
			parathread_retries: Default::default(),
			scheduling_lookahead: Default::default(),
			max_validators_per_core: Default::default(),
			max_validators: None,
			dispute_period: 6,
			dispute_post_conclusion_acceptance_period: 100.into(),
			dispute_max_spam_slots: 2,
			dispute_conclusion_by_time_out_period: 200.into(),
			n_delay_tranches: Default::default(),
			zeroth_delay_tranche_width: Default::default(),
			needed_approvals: Default::default(),
			relay_vrf_modulo_samples: Default::default(),
			max_upward_queue_count: Default::default(),
			max_upward_queue_size: Default::default(),
			max_downward_message_size: Default::default(),
			ump_service_total_weight: Default::default(),
			max_upward_message_size: Default::default(),
			max_upward_message_num_per_candidate: Default::default(),
			hrmp_open_request_ttl: Default::default(),
			hrmp_sender_deposit: Default::default(),
			hrmp_recipient_deposit: Default::default(),
			hrmp_channel_max_capacity: Default::default(),
			hrmp_channel_max_total_size: Default::default(),
			hrmp_max_parachain_inbound_channels: Default::default(),
			hrmp_max_parathread_inbound_channels: Default::default(),
			hrmp_channel_max_message_size: Default::default(),
			hrmp_max_parachain_outbound_channels: Default::default(),
			hrmp_max_parathread_outbound_channels: Default::default(),
			hrmp_max_message_num_per_candidate: Default::default(),
		}
	}
}

impl<BlockNumber: Zero> HostConfiguration<BlockNumber> {
	/// Checks that this instance is consistent with the requirements on each individual member.
	///
	/// # Panic
	///
	/// This function panics if any member is not set properly.
	pub fn check_consistency(&self) {
		if self.group_rotation_frequency.is_zero() {
			panic!("`group_rotation_frequency` must be non-zero!")
		}

		if self.chain_availability_period.is_zero() {
			panic!("`chain_availability_period` must be at least 1!")
		}

		if self.thread_availability_period.is_zero() {
			panic!("`thread_availability_period` must be at least 1!")
		}

		if self.no_show_slots.is_zero() {
			panic!("`no_show_slots` must be at least 1!")
		}

		if self.max_code_size > MAX_CODE_SIZE {
			panic!(
				"`max_code_size` ({}) is bigger than allowed by the client ({})",
				self.max_code_size,
				MAX_CODE_SIZE,
			)
		}

		if self.max_pov_size > MAX_POV_SIZE {
			panic!("`max_pov_size` is bigger than allowed by the client")
		}
	}
}

pub trait Config: frame_system::Config + shared::Config { }

decl_storage! {
	trait Store for Module<T: Config> as Configuration {
		/// The active configuration for the current session.
		ActiveConfig get(fn config) config(): HostConfiguration<T::BlockNumber>;
		/// Pending configuration (if any) for the next session.
		PendingConfig: map hasher(twox_64_concat) SessionIndex => Option<HostConfiguration<T::BlockNumber>>;
	}
	add_extra_genesis {
		build(|config: &Self| {
			config.config.check_consistency();
		})
	}
}

decl_error! {
	pub enum Error for Module<T: Config> {
		/// The new value for a configuration parameter is invalid.
		InvalidNewValue,
	}
}

decl_module! {
	/// The parachains configuration module.
	pub struct Module<T: Config> for enum Call where origin: <T as frame_system::Config>::Origin {
		type Error = Error<T>;

		/// Set the validation upgrade frequency.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_validation_upgrade_frequency(origin, new: T::BlockNumber) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.validation_upgrade_frequency, new) != new
			});
			Ok(())
		}

		/// Set the validation upgrade delay.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_validation_upgrade_delay(origin, new: T::BlockNumber) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.validation_upgrade_delay, new) != new
			});
			Ok(())
		}

		/// Set the acceptance period for an included candidate.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_code_retention_period(origin, new: T::BlockNumber) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.code_retention_period, new) != new
			});
			Ok(())
		}

		/// Set the max validation code size for incoming upgrades.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_max_code_size(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			ensure!(new <= MAX_CODE_SIZE, Error::<T>::InvalidNewValue);
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.max_code_size, new) != new
			});
			Ok(())
		}

		/// Set the max POV block size for incoming upgrades.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_max_pov_size(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			ensure!(new <= MAX_POV_SIZE, Error::<T>::InvalidNewValue);
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.max_pov_size, new) != new
			});
			Ok(())
		}

		/// Set the max head data size for paras.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_max_head_data_size(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.max_head_data_size, new) != new
			});
			Ok(())
		}

		/// Set the number of parathread execution cores.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_parathread_cores(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.parathread_cores, new) != new
			});
			Ok(())
		}

		/// Set the number of retries for a particular parathread.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_parathread_retries(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.parathread_retries, new) != new
			});
			Ok(())
		}


		/// Set the parachain validator-group rotation frequency
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_group_rotation_frequency(origin, new: T::BlockNumber) -> DispatchResult {
			ensure_root(origin)?;

			ensure!(!new.is_zero(), Error::<T>::InvalidNewValue);

			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.group_rotation_frequency, new) != new
			});
			Ok(())
		}

		/// Set the availability period for parachains.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_chain_availability_period(origin, new: T::BlockNumber) -> DispatchResult {
			ensure_root(origin)?;

			ensure!(!new.is_zero(), Error::<T>::InvalidNewValue);

			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.chain_availability_period, new) != new
			});
			Ok(())
		}

		/// Set the availability period for parathreads.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_thread_availability_period(origin, new: T::BlockNumber) -> DispatchResult {
			ensure_root(origin)?;

			ensure!(!new.is_zero(), Error::<T>::InvalidNewValue);

			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.thread_availability_period, new) != new
			});
			Ok(())
		}

		/// Set the scheduling lookahead, in expected number of blocks at peak throughput.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_scheduling_lookahead(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.scheduling_lookahead, new) != new
			});
			Ok(())
		}

		/// Set the maximum number of validators to assign to any core.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_max_validators_per_core(origin, new: Option<u32>) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.max_validators_per_core, new) != new
			});
			Ok(())
		}

		/// Set the maximum number of validators to use in parachain consensus.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_max_validators(origin, new: Option<u32>) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.max_validators, new) != new
			});
			Ok(())
		}

		/// Set the dispute period, in number of sessions to keep for disputes.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_dispute_period(origin, new: SessionIndex) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.dispute_period, new) != new
			});
			Ok(())
		}

		/// Set the dispute post conclusion acceptance period.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_dispute_post_conclusion_acceptance_period(
			origin,
			new: T::BlockNumber,
		) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.dispute_post_conclusion_acceptance_period, new) != new
			});
			Ok(())
		}

		/// Set the maximum number of dispute spam slots.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_dispute_max_spam_slots(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.dispute_max_spam_slots, new) != new
			});
			Ok(())
		}

		/// Set the dispute conclusion by time out period.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_dispute_conclusion_by_time_out_period(origin, new: T::BlockNumber)
			-> DispatchResult
		{
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.dispute_conclusion_by_time_out_period, new) != new
			});
			Ok(())
		}

		/// Set the no show slots, in number of number of consensus slots.
		/// Must be at least 1.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_no_show_slots(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;

			ensure!(!new.is_zero(), Error::<T>::InvalidNewValue);

			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.no_show_slots, new) != new
			});
			Ok(())
		}

		/// Set the total number of delay tranches.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_n_delay_tranches(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.n_delay_tranches, new) != new
			});
			Ok(())
		}

		/// Set the zeroth delay tranche width.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_zeroth_delay_tranche_width(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.zeroth_delay_tranche_width, new) != new
			});
			Ok(())
		}

		/// Set the number of validators needed to approve a block.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_needed_approvals(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.needed_approvals, new) != new
			});
			Ok(())
		}

		/// Set the number of samples to do of the `RelayVRFModulo` approval assignment criterion.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_relay_vrf_modulo_samples(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.relay_vrf_modulo_samples, new) != new
			});
			Ok(())
		}

		/// Sets the maximum items that can present in a upward dispatch queue at once.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_max_upward_queue_count(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.max_upward_queue_count, new) != new
			});
			Ok(())
		}

		/// Sets the maximum total size of items that can present in a upward dispatch queue at once.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_max_upward_queue_size(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.max_upward_queue_size, new) != new
			});
			Ok(())
		}

		/// Set the critical downward message size.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_max_downward_message_size(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.max_downward_message_size, new) != new
			});
			Ok(())
		}

		/// Sets the soft limit for the phase of dispatching dispatchable upward messages.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_ump_service_total_weight(origin, new: Weight) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.ump_service_total_weight, new) != new
			});
			Ok(())
		}

		/// Sets the maximum size of an upward message that can be sent by a candidate.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_max_upward_message_size(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.max_upward_message_size, new) != new
			});
			Ok(())
		}

		/// Sets the maximum number of messages that a candidate can contain.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_max_upward_message_num_per_candidate(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.max_upward_message_num_per_candidate, new) != new
			});
			Ok(())
		}

		/// Sets the number of sessions after which an HRMP open channel request expires.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_hrmp_open_request_ttl(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.hrmp_open_request_ttl, new) != new
			});
			Ok(())
		}

		/// Sets the amount of funds that the sender should provide for opening an HRMP channel.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_hrmp_sender_deposit(origin, new: Balance) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.hrmp_sender_deposit, new) != new
			});
			Ok(())
		}

		/// Sets the amount of funds that the recipient should provide for accepting opening an HRMP
		/// channel.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_hrmp_recipient_deposit(origin, new: Balance) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.hrmp_recipient_deposit, new) != new
			});
			Ok(())
		}

		/// Sets the maximum number of messages allowed in an HRMP channel at once.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_hrmp_channel_max_capacity(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.hrmp_channel_max_capacity, new) != new
			});
			Ok(())
		}

		/// Sets the maximum total size of messages in bytes allowed in an HRMP channel at once.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_hrmp_channel_max_total_size(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.hrmp_channel_max_total_size, new) != new
			});
			Ok(())
		}

		/// Sets the maximum number of inbound HRMP channels a parachain is allowed to accept.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_hrmp_max_parachain_inbound_channels(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.hrmp_max_parachain_inbound_channels, new) != new
			});
			Ok(())
		}

		/// Sets the maximum number of inbound HRMP channels a parathread is allowed to accept.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_hrmp_max_parathread_inbound_channels(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.hrmp_max_parathread_inbound_channels, new) != new
			});
			Ok(())
		}

		/// Sets the maximum size of a message that could ever be put into an HRMP channel.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_hrmp_channel_max_message_size(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.hrmp_channel_max_message_size, new) != new
			});
			Ok(())
		}

		/// Sets the maximum number of outbound HRMP channels a parachain is allowed to open.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_hrmp_max_parachain_outbound_channels(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.hrmp_max_parachain_outbound_channels, new) != new
			});
			Ok(())
		}

		/// Sets the maximum number of outbound HRMP channels a parathread is allowed to open.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_hrmp_max_parathread_outbound_channels(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.hrmp_max_parathread_outbound_channels, new) != new
			});
			Ok(())
		}

		/// Sets the maximum number of outbound HRMP messages can be sent by a candidate.
		#[weight = (1_000, DispatchClass::Operational)]
		pub fn set_hrmp_max_message_num_per_candidate(origin, new: u32) -> DispatchResult {
			ensure_root(origin)?;
			Self::update_config_member(|config| {
				sp_std::mem::replace(&mut config.hrmp_max_message_num_per_candidate, new) != new
			});
			Ok(())
		}
	}
}

impl<T: Config> Module<T> {
	/// Called by the initializer to initialize the configuration module.
	pub(crate) fn initializer_initialize(_now: T::BlockNumber) -> Weight {
		0
	}

	/// Called by the initializer to finalize the configuration module.
	pub(crate) fn initializer_finalize() { }

	/// Called by the initializer to note that a new session has started.
	pub(crate) fn initializer_on_new_session(
		session_index: &SessionIndex,
	) {
		if let Some(pending) = <Self as Store>::PendingConfig::take(session_index) {
			<Self as Store>::ActiveConfig::set(pending);
		}
	}

	/// Return the session index that should be used for any future scheduled changes.
	fn scheduled_session() -> SessionIndex {
		shared::Pallet::<T>::scheduled_session()
	}

	/// Forcibly set the active config. This should be used with extreme care, and typically
	/// only when enabling parachains runtime modules for the first time on a chain which has
	/// been running without them.
	pub fn force_set_active_config(config: HostConfiguration<T::BlockNumber>) {
		<Self as Store>::ActiveConfig::set(config);
	}

	// NOTE: Explicitly tell rustc not to inline this because otherwise heuristics note the incoming
	// closure making it's attractive to inline. However, in this case, we will end up with lots of
	// duplicated code (making this function to show up in the top of heaviest functions) only for
	// the sake of essentially avoiding an indirect call. Doesn't worth it.
	#[inline(never)]
	fn update_config_member(
		updater: impl FnOnce(&mut HostConfiguration<T::BlockNumber>) -> bool,
	) {
		let scheduled_session = Self::scheduled_session();
		let pending = <Self as Store>::PendingConfig::get(scheduled_session);
		let mut prev = pending.unwrap_or_else(Self::config);

		if updater(&mut prev) {
			<Self as Store>::PendingConfig::insert(scheduled_session, prev);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::mock::{new_test_ext, Configuration, Origin};

	use frame_support::assert_ok;

	#[test]
	fn config_changes_after_2_session_boundary() {
		new_test_ext(Default::default()).execute_with(|| {
			let old_config = Configuration::config();
			let mut config = old_config.clone();
			config.validation_upgrade_delay = 100;
			assert!(old_config != config);

			assert_ok!(Configuration::set_validation_upgrade_delay(Origin::root(), 100));

			assert_eq!(Configuration::config(), old_config);
			assert_eq!(<Configuration as Store>::PendingConfig::get(1), None);

			Configuration::initializer_on_new_session(&1);

			assert_eq!(Configuration::config(), old_config);
			assert_eq!(<Configuration as Store>::PendingConfig::get(2), Some(config.clone()));

			Configuration::initializer_on_new_session(&2);

			assert_eq!(Configuration::config(), config);
			assert_eq!(<Configuration as Store>::PendingConfig::get(3), None);
		})
	}

	#[test]
	fn setting_pending_config_members() {
		new_test_ext(Default::default()).execute_with(|| {
			let new_config = HostConfiguration {
				validation_upgrade_frequency: 100,
				validation_upgrade_delay: 10,
				code_retention_period: 5,
				max_code_size: 100_000,
				max_pov_size: 1024,
				max_head_data_size: 1_000,
				parathread_cores: 2,
				parathread_retries: 5,
				group_rotation_frequency: 20,
				chain_availability_period: 10,
				thread_availability_period: 8,
				scheduling_lookahead: 3,
				max_validators_per_core: None,
				max_validators: None,
				dispute_period: 239,
				dispute_post_conclusion_acceptance_period: 10,
				dispute_max_spam_slots: 2,
				dispute_conclusion_by_time_out_period: 512,
				no_show_slots: 240,
				n_delay_tranches: 241,
				zeroth_delay_tranche_width: 242,
				needed_approvals: 242,
				relay_vrf_modulo_samples: 243,
				max_upward_queue_count: 1337,
				max_upward_queue_size: 228,
				max_downward_message_size: 2048,
				ump_service_total_weight: 20000,
				max_upward_message_size: 448,
				max_upward_message_num_per_candidate: 5,
				hrmp_open_request_ttl: 1312,
				hrmp_sender_deposit: 22,
				hrmp_recipient_deposit: 4905,
				hrmp_channel_max_capacity: 3921,
				hrmp_channel_max_total_size: 7687,
				hrmp_max_parachain_inbound_channels: 3722,
				hrmp_max_parathread_inbound_channels: 1967,
				hrmp_channel_max_message_size: 8192,
				hrmp_max_parachain_outbound_channels: 100,
				hrmp_max_parathread_outbound_channels: 200,
				hrmp_max_message_num_per_candidate: 20,
			};

			assert!(<Configuration as Store>::PendingConfig::get(shared::SESSION_DELAY).is_none());

			Configuration::set_validation_upgrade_frequency(
				Origin::root(), new_config.validation_upgrade_frequency,
			).unwrap();
			Configuration::set_validation_upgrade_delay(
				Origin::root(), new_config.validation_upgrade_delay,
			).unwrap();
			Configuration::set_code_retention_period(
				Origin::root(), new_config.code_retention_period,
			).unwrap();
			Configuration::set_max_code_size(
				Origin::root(), new_config.max_code_size,
			).unwrap();
			Configuration::set_max_pov_size(
				Origin::root(), new_config.max_pov_size,
			).unwrap();
			Configuration::set_max_head_data_size(
				Origin::root(), new_config.max_head_data_size,
			).unwrap();
			Configuration::set_parathread_cores(
				Origin::root(), new_config.parathread_cores,
			).unwrap();
			Configuration::set_parathread_retries(
				Origin::root(), new_config.parathread_retries,
			).unwrap();
			Configuration::set_group_rotation_frequency(
				Origin::root(), new_config.group_rotation_frequency,
			).unwrap();
			Configuration::set_chain_availability_period(
				Origin::root(), new_config.chain_availability_period,
			).unwrap();
			Configuration::set_thread_availability_period(
				Origin::root(), new_config.thread_availability_period,
			).unwrap();
			Configuration::set_scheduling_lookahead(
				Origin::root(), new_config.scheduling_lookahead,
			).unwrap();
			Configuration::set_max_validators_per_core(
				Origin::root(), new_config.max_validators_per_core,
			).unwrap();
			Configuration::set_max_validators(
				Origin::root(), new_config.max_validators,
			).unwrap();
			Configuration::set_dispute_period(
				Origin::root(), new_config.dispute_period,
			).unwrap();
			Configuration::set_dispute_post_conclusion_acceptance_period(
				Origin::root(), new_config.dispute_post_conclusion_acceptance_period,
			).unwrap();
			Configuration::set_dispute_max_spam_slots(
				Origin::root(), new_config.dispute_max_spam_slots,
			).unwrap();
			Configuration::set_dispute_conclusion_by_time_out_period(
				Origin::root(), new_config.dispute_conclusion_by_time_out_period,
			).unwrap();
			Configuration::set_no_show_slots(
				Origin::root(), new_config.no_show_slots,
			).unwrap();
			Configuration::set_n_delay_tranches(
				Origin::root(), new_config.n_delay_tranches,
			).unwrap();
			Configuration::set_zeroth_delay_tranche_width(
				Origin::root(), new_config.zeroth_delay_tranche_width,
			).unwrap();
			Configuration::set_needed_approvals(
				Origin::root(), new_config.needed_approvals,
			).unwrap();
			Configuration::set_relay_vrf_modulo_samples(
				Origin::root(), new_config.relay_vrf_modulo_samples,
			).unwrap();
			Configuration::set_max_upward_queue_count(
				Origin::root(), new_config.max_upward_queue_count,
			).unwrap();
			Configuration::set_max_upward_queue_size(
				Origin::root(), new_config.max_upward_queue_size,
			).unwrap();
			Configuration::set_max_downward_message_size(
				Origin::root(), new_config.max_downward_message_size,
			).unwrap();
			Configuration::set_ump_service_total_weight(
				Origin::root(), new_config.ump_service_total_weight,
			).unwrap();
			Configuration::set_max_upward_message_size(
				Origin::root(), new_config.max_upward_message_size,
			).unwrap();
			Configuration::set_max_upward_message_num_per_candidate(
				Origin::root(), new_config.max_upward_message_num_per_candidate,
			).unwrap();
			Configuration::set_hrmp_open_request_ttl(
				Origin::root(),
				new_config.hrmp_open_request_ttl,
			).unwrap();
			Configuration::set_hrmp_sender_deposit(
				Origin::root(),
				new_config.hrmp_sender_deposit,
			).unwrap();
			Configuration::set_hrmp_recipient_deposit(
				Origin::root(),
				new_config.hrmp_recipient_deposit,
			).unwrap();
			Configuration::set_hrmp_channel_max_capacity(
				Origin::root(),
				new_config.hrmp_channel_max_capacity,
			).unwrap();
			Configuration::set_hrmp_channel_max_total_size(
				Origin::root(),
				new_config.hrmp_channel_max_total_size,
			).unwrap();
			Configuration::set_hrmp_max_parachain_inbound_channels(
				Origin::root(),
				new_config.hrmp_max_parachain_inbound_channels,
			).unwrap();
			Configuration::set_hrmp_max_parathread_inbound_channels(
				Origin::root(),
				new_config.hrmp_max_parathread_inbound_channels,
			).unwrap();
			Configuration::set_hrmp_channel_max_message_size(
				Origin::root(),
				new_config.hrmp_channel_max_message_size,
			).unwrap();
			Configuration::set_hrmp_max_parachain_outbound_channels(
				Origin::root(),
				new_config.hrmp_max_parachain_outbound_channels,
			).unwrap();
			Configuration::set_hrmp_max_parathread_outbound_channels(
				Origin::root(),
				new_config.hrmp_max_parathread_outbound_channels,
			).unwrap();
			Configuration::set_hrmp_max_message_num_per_candidate(
				Origin::root(),
				new_config.hrmp_max_message_num_per_candidate,
			).unwrap();

			assert_eq!(<Configuration as Store>::PendingConfig::get(shared::SESSION_DELAY), Some(new_config));
		})
	}

	#[test]
	fn non_root_cannot_set_config() {
		new_test_ext(Default::default()).execute_with(|| {
			assert!(Configuration::set_validation_upgrade_delay(Origin::signed(1), 100).is_err());
		});
	}

	#[test]
	fn setting_config_to_same_as_current_is_noop() {
		new_test_ext(Default::default()).execute_with(|| {
			Configuration::set_validation_upgrade_delay(Origin::root(), Default::default()).unwrap();
			assert!(<Configuration as Store>::PendingConfig::get(shared::SESSION_DELAY).is_none())
		});
	}

	#[test]
	fn verify_externally_accessible() {
		// This test verifies that the value can be accessed through the well known keys and the
		// host configuration decodes into the abridged version.

		use primitives::v1::{well_known_keys, AbridgedHostConfiguration};

		new_test_ext(Default::default()).execute_with(|| {
			let ground_truth = HostConfiguration::default();

			// Make sure that the configuration is stored in the storage.
			<Configuration as Store>::ActiveConfig::put(ground_truth.clone());

			// Extract the active config via the well known key.
			let raw_active_config = sp_io::storage::get(well_known_keys::ACTIVE_CONFIG)
				.expect("config must be present in storage under ACTIVE_CONFIG");
			let abridged_config = AbridgedHostConfiguration::decode(&mut &raw_active_config[..])
				.expect("HostConfiguration must be decodable into AbridgedHostConfiguration");

			assert_eq!(
				abridged_config,
				AbridgedHostConfiguration {
					max_code_size: ground_truth.max_code_size,
					max_head_data_size: ground_truth.max_head_data_size,
					max_upward_queue_count: ground_truth.max_upward_queue_count,
					max_upward_queue_size: ground_truth.max_upward_queue_size,
					max_upward_message_size: ground_truth.max_upward_message_size,
					max_upward_message_num_per_candidate: ground_truth
						.max_upward_message_num_per_candidate,
					hrmp_max_message_num_per_candidate: ground_truth
						.hrmp_max_message_num_per_candidate,
					validation_upgrade_frequency: ground_truth.validation_upgrade_frequency,
					validation_upgrade_delay: ground_truth.validation_upgrade_delay,
				},
			);
		});
	}
}
