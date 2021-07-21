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

//! The session info module provides information about validator sets
//! from prior sessions needed for approvals and disputes.
//!
//! See https://w3f.github.io/parachain-implementers-guide/runtime/session_info.html.

use primitives::v1::{AssignmentId, AuthorityDiscoveryId, SessionIndex, SessionInfo};
use frame_support::{
	decl_storage, decl_module, decl_error,
	traits::OneSessionHandler, weights::Weight,
};
use crate::{configuration, paras, scheduler, shared};
use crate::util::take_active_subset;
use sp_std::vec::Vec;

pub trait Config:
	frame_system::Config
	+ configuration::Config
	+ shared::Config
	+ paras::Config
	+ scheduler::Config
	+ AuthorityDiscoveryConfig
{
}

decl_storage! {
	trait Store for Module<T: Config> as ParaSessionInfo {
		/// Assignment keys for the current session.
		/// Note that this API is private due to it being prone to 'off-by-one' at session boundaries.
		/// When in doubt, use `Sessions` API instead.
		AssignmentKeysUnsafe: Vec<AssignmentId>;
		/// The earliest session for which previous session info is stored.
		EarliestStoredSession get(fn earliest_stored_session): SessionIndex;
		/// Session information in a rolling window.
		/// Should have an entry in range `EarliestStoredSession..=CurrentSessionIndex`.
		/// Does not have any entries before the session index in the first session change notification.
		Sessions get(fn session_info): map hasher(identity) SessionIndex => Option<SessionInfo>;
	}
}

decl_error! {
	pub enum Error for Module<T: Config> { }
}

decl_module! {
	/// The session info module.
	pub struct Module<T: Config> for enum Call where origin: <T as frame_system::Config>::Origin {
		type Error = Error<T>;
	}
}

/// An abstraction for the authority discovery pallet
/// to help with mock testing.
pub trait AuthorityDiscoveryConfig {
	/// Retrieve authority identifiers of the current authority set in canonical ordering.
	fn authorities() -> Vec<AuthorityDiscoveryId>;
}

impl<T: pallet_authority_discovery::Config> AuthorityDiscoveryConfig for T {
	fn authorities() -> Vec<AuthorityDiscoveryId> {
		<pallet_authority_discovery::Pallet<T>>::current_authorities()
	}
}

impl<T: Config> Module<T> {
	/// Handle an incoming session change.
	pub(crate) fn initializer_on_new_session(
		notification: &crate::initializer::SessionChangeNotification<T::BlockNumber>
	) {
		let config = <configuration::Module<T>>::config();

		let dispute_period = config.dispute_period;

		let validators = notification.validators.clone();
		let discovery_keys = <T as AuthorityDiscoveryConfig>::authorities();
		let assignment_keys = AssignmentKeysUnsafe::get();
		let active_set = <shared::Pallet<T>>::active_validator_indices();

		let validator_groups = <scheduler::Module<T>>::validator_groups();
		let n_cores = <scheduler::Module<T>>::availability_cores().len() as u32;
		let zeroth_delay_tranche_width = config.zeroth_delay_tranche_width;
		let relay_vrf_modulo_samples = config.relay_vrf_modulo_samples;
		let n_delay_tranches = config.n_delay_tranches;
		let no_show_slots = config.no_show_slots;
		let needed_approvals = config.needed_approvals;

		let new_session_index = notification.session_index;
		let old_earliest_stored_session = EarliestStoredSession::get();
		let new_earliest_stored_session = new_session_index.saturating_sub(dispute_period);
		let new_earliest_stored_session = core::cmp::max(new_earliest_stored_session, old_earliest_stored_session);
		// remove all entries from `Sessions` from the previous value up to the new value
		// avoid a potentially heavy loop when introduced on a live chain
		if old_earliest_stored_session != 0 || Sessions::get(0).is_some() {
			for idx in old_earliest_stored_session..new_earliest_stored_session {
				Sessions::remove(&idx);
			}
			// update `EarliestStoredSession` based on `config.dispute_period`
			EarliestStoredSession::set(new_earliest_stored_session);
		} else {
			// just introduced on a live chain
			EarliestStoredSession::set(new_session_index);
		}
		// create a new entry in `Sessions` with information about the current session
		let new_session_info = SessionInfo {
			validators, // these are from the notification and are thus already correct.
			discovery_keys: take_active_subset(&active_set, &discovery_keys),
			assignment_keys: take_active_subset(&active_set, &assignment_keys),
			validator_groups,
			n_cores,
			zeroth_delay_tranche_width,
			relay_vrf_modulo_samples,
			n_delay_tranches,
			no_show_slots,
			needed_approvals,
		};
		Sessions::insert(&new_session_index, &new_session_info);
	}

	/// Called by the initializer to initialize the session info module.
	pub(crate) fn initializer_initialize(_now: T::BlockNumber) -> Weight {
		0
	}

	/// Called by the initializer to finalize the session info module.
	pub(crate) fn initializer_finalize() {}
}

impl<T: Config> sp_runtime::BoundToRuntimeAppPublic for Module<T> {
	type Public = AssignmentId;
}

impl<T: pallet_session::Config + Config> OneSessionHandler<T::AccountId> for Module<T> {
	type Key = AssignmentId;

	fn on_genesis_session<'a, I: 'a>(_validators: I)
		where I: Iterator<Item=(&'a T::AccountId, Self::Key)>
	{

	}

	fn on_new_session<'a, I: 'a>(_changed: bool, validators: I, _queued: I)
		where I: Iterator<Item=(&'a T::AccountId, Self::Key)>
	{
		let assignment_keys: Vec<_> = validators.map(|(_, v)| v).collect();
		AssignmentKeysUnsafe::set(assignment_keys);
	}

	fn on_disabled(_i: usize) { }
}


#[cfg(test)]
mod tests {
	use super::*;
	use crate::mock::{
		new_test_ext, Configuration, SessionInfo, System, MockGenesisConfig,
		Origin, ParasShared,
	};
	use crate::initializer::SessionChangeNotification;
	use crate::configuration::HostConfiguration;
	use frame_support::traits::{OnFinalize, OnInitialize};
	use primitives::v1::{BlockNumber, ValidatorId, ValidatorIndex};
	use keyring::Sr25519Keyring;

	fn run_to_block(
		to: BlockNumber,
		new_session: impl Fn(BlockNumber) -> Option<SessionChangeNotification<BlockNumber>>,
	) {
		while System::block_number() < to {
			let b = System::block_number();

			SessionInfo::initializer_finalize();
			ParasShared::initializer_finalize();
			Configuration::initializer_finalize();

			if let Some(notification) = new_session(b + 1) {
				Configuration::initializer_on_new_session(
					&notification.session_index,
				);
				ParasShared::initializer_on_new_session(
					notification.session_index,
					notification.random_seed,
					&notification.new_config,
					notification.validators.clone(),
				);
				SessionInfo::initializer_on_new_session(&notification);
			}

			System::on_finalize(b);

			System::on_initialize(b + 1);
			System::set_block_number(b + 1);

			Configuration::initializer_initialize(b + 1);
			ParasShared::initializer_initialize(b + 1);
			SessionInfo::initializer_initialize(b + 1);
		}
	}

	fn default_config() -> HostConfiguration<BlockNumber> {
		HostConfiguration {
			parathread_cores: 1,
			dispute_period: 2,
			needed_approvals: 3,
			..Default::default()
		}
	}

	fn genesis_config() -> MockGenesisConfig {
		MockGenesisConfig {
			configuration: configuration::GenesisConfig {
				config: default_config(),
				..Default::default()
			},
			..Default::default()
		}
	}

	fn session_changes(n: BlockNumber) -> Option<SessionChangeNotification<BlockNumber>> {
		if n % 10 == 0 {
			Some(SessionChangeNotification {
				session_index: n / 10,
				..Default::default()
			})
		} else {
			None
		}
	}

	fn new_session_every_block(n: BlockNumber) -> Option<SessionChangeNotification<BlockNumber>> {
		Some(SessionChangeNotification{
			session_index: n,
			..Default::default()
		})
	}

	#[test]
	fn session_pruning_is_based_on_dispute_period() {
		new_test_ext(genesis_config()).execute_with(|| {
			// Dispute period starts at 2
			let config = Configuration::config();
			assert_eq!(config.dispute_period, 2);

			// Move to session 10
			run_to_block(100, session_changes);
			// Earliest stored session is 10 - 2 = 8
			assert_eq!(EarliestStoredSession::get(), 8);
			// Pruning works as expected
			assert!(Sessions::get(7).is_none());
			assert!(Sessions::get(8).is_some());
			assert!(Sessions::get(9).is_some());

			// changing dispute_period works
			let dispute_period = 5;
			Configuration::set_dispute_period(Origin::root(), dispute_period).unwrap();

			// Dispute period does not automatically change
			let config = Configuration::config();
			assert_eq!(config.dispute_period, 2);
			// Two sessions later it will though
			run_to_block(120, session_changes);
			let config = Configuration::config();
			assert_eq!(config.dispute_period, 5);

			run_to_block(200, session_changes);
			assert_eq!(EarliestStoredSession::get(), 20 - dispute_period);

			// Increase dispute period even more
			let new_dispute_period = 16;
			Configuration::set_dispute_period(Origin::root(), new_dispute_period).unwrap();

			run_to_block(210, session_changes);
			assert_eq!(EarliestStoredSession::get(), 21 - dispute_period);

			// Two sessions later it kicks in
			run_to_block(220, session_changes);
			let config = Configuration::config();
			assert_eq!(config.dispute_period, 16);
			// Earliest session stays the same
			assert_eq!(EarliestStoredSession::get(), 21 - dispute_period);

			// We still don't have enough stored sessions to start pruning
			run_to_block(300, session_changes);
			assert_eq!(EarliestStoredSession::get(), 21 - dispute_period);

			// now we do
			run_to_block(420, session_changes);
			assert_eq!(EarliestStoredSession::get(), 42 - new_dispute_period);
		})
	}

	#[test]
	fn session_info_is_based_on_config() {
		new_test_ext(genesis_config()).execute_with(|| {
			run_to_block(1, new_session_every_block);
			let session = Sessions::get(&1).unwrap();
			assert_eq!(session.needed_approvals, 3);

			// change some param
			Configuration::set_needed_approvals(Origin::root(), 42).unwrap();
			// 2 sessions later
			run_to_block(3, new_session_every_block);
			let session = Sessions::get(&3).unwrap();
			assert_eq!(session.needed_approvals, 42);
		})
	}

	#[test]
	fn session_info_active_subsets() {
		let unscrambled = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
		];

		let active_set = vec![ValidatorIndex(4), ValidatorIndex(0), ValidatorIndex(2)];

		let unscrambled_validators: Vec<ValidatorId>
			= unscrambled.iter().map(|v| v.public().into()).collect();
		let unscrambled_discovery: Vec<AuthorityDiscoveryId>
			= unscrambled.iter().map(|v| v.public().into()).collect();
		let unscrambled_assignment: Vec<AssignmentId>
			= unscrambled.iter().map(|v| v.public().into()).collect();

		let validators = take_active_subset(&active_set, &unscrambled_validators);

		new_test_ext(genesis_config()).execute_with(|| {
			ParasShared::set_active_validators_with_indices(
				active_set.clone(),
				validators.clone(),
			);

			assert_eq!(ParasShared::active_validator_indices(), active_set);

			AssignmentKeysUnsafe::set(unscrambled_assignment.clone());
			crate::mock::set_discovery_authorities(unscrambled_discovery.clone());
			assert_eq!(<crate::mock::Test>::authorities(), unscrambled_discovery);

			// invoke directly, because `run_to_block` will invoke `Shared`	and clobber our
			// values.
			SessionInfo::initializer_on_new_session(&SessionChangeNotification {
				session_index: 1,
				validators: validators.clone(),
				..Default::default()
			});
			let session = Sessions::get(&1).unwrap();

			assert_eq!(session.validators, validators);
			assert_eq!(
				session.discovery_keys,
				take_active_subset(&active_set, &unscrambled_discovery),
			);
			assert_eq!(
				session.assignment_keys,
				take_active_subset(&active_set, &unscrambled_assignment),
			);
		})
	}
}
