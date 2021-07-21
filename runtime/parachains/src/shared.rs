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

//! A pallet for any shared state that other pallets may want access to.
//!
//! To avoid cyclic dependencies, it is important that this pallet is not
//! dependent on any of the other pallets.

use primitives::v1::{SessionIndex, ValidatorId, ValidatorIndex};
use frame_support::pallet_prelude::*;
use sp_std::vec::Vec;

use rand::{SeedableRng, seq::SliceRandom};
use rand_chacha::ChaCha20Rng;

use crate::configuration::HostConfiguration;

pub use pallet::*;

// `SESSION_DELAY` is used to delay any changes to Paras registration or configurations.
// Wait until the session index is 2 larger then the current index to apply any changes,
// which guarantees that at least one full session has passed before any changes are applied.
pub(crate) const SESSION_DELAY: SessionIndex = 2;

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {}

	/// The current session index.
	#[pallet::storage]
	#[pallet::getter(fn session_index)]
	pub(super) type CurrentSessionIndex<T: Config> = StorageValue<_, SessionIndex, ValueQuery>;

	/// All the validators actively participating in parachain consensus.
	/// Indices are into the broader validator set.
	#[pallet::storage]
	#[pallet::getter(fn active_validator_indices)]
	pub(super) type ActiveValidatorIndices<T: Config> = StorageValue<_, Vec<ValidatorIndex>, ValueQuery>;

	/// The parachain attestation keys of the validators actively participating in parachain consensus.
	/// This should be the same length as `ActiveValidatorIndices`.
	#[pallet::storage]
	#[pallet::getter(fn active_validator_keys)]
	pub(super) type ActiveValidatorKeys<T: Config> = StorageValue<_, Vec<ValidatorId>, ValueQuery>;

	#[pallet::call]
	impl<T: Config> Pallet<T> {}
}

impl<T: Config> Pallet<T> {
	/// Called by the initializer to initialize the configuration pallet.
	pub(crate) fn initializer_initialize(_now: T::BlockNumber) -> Weight {
		0
	}

	/// Called by the initializer to finalize the configuration pallet.
	pub(crate) fn initializer_finalize() { }

	/// Called by the initializer to note that a new session has started.
	///
	/// Returns the list of outgoing paras from the actions queue.
	pub(crate) fn initializer_on_new_session(
		session_index: SessionIndex,
		random_seed: [u8; 32],
		new_config: &HostConfiguration<T::BlockNumber>,
		all_validators: Vec<ValidatorId>,
	) -> Vec<ValidatorId> {
		CurrentSessionIndex::<T>::set(session_index);
		let mut rng: ChaCha20Rng = SeedableRng::from_seed(random_seed);

		let mut shuffled_indices: Vec<_> = (0..all_validators.len())
			.enumerate()
			.map(|(i, _)| ValidatorIndex(i as _))
			.collect();

		shuffled_indices.shuffle(&mut rng);

		if let Some(max) = new_config.max_validators {
			shuffled_indices.truncate(max as usize);
		}

		let active_validator_keys = crate::util::take_active_subset(
			&shuffled_indices,
			&all_validators,
		);

		ActiveValidatorIndices::<T>::set(shuffled_indices);
		ActiveValidatorKeys::<T>::set(active_validator_keys.clone());

		active_validator_keys
	}

	/// Return the session index that should be used for any future scheduled changes.
	pub fn scheduled_session() -> SessionIndex {
		Self::session_index().saturating_add(SESSION_DELAY)
	}

	/// Test function for setting the current session index.
	#[cfg(any(feature = "std", feature = "runtime-benchmarks", test))]
	pub fn set_session_index(index: SessionIndex) {
		CurrentSessionIndex::<T>::set(index);
	}

	#[cfg(test)]
	pub(crate) fn set_active_validators_ascending(active: Vec<ValidatorId>) {
		ActiveValidatorIndices::<T>::set(
			(0..active.len()).map(|i| ValidatorIndex(i as _)).collect()
		);
		ActiveValidatorKeys::<T>::set(active);
	}

	#[cfg(test)]
	pub(crate) fn set_active_validators_with_indices(
		indices: Vec<ValidatorIndex>,
		keys: Vec<ValidatorId>,
	) {
		assert_eq!(indices.len(), keys.len());
		ActiveValidatorIndices::<T>::set(indices);
		ActiveValidatorKeys::<T>::set(keys);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::configuration::HostConfiguration;
	use crate::mock::{new_test_ext, MockGenesisConfig, ParasShared};
	use keyring::Sr25519Keyring;

	fn validator_pubkeys(val_ids: &[Sr25519Keyring]) -> Vec<ValidatorId> {
		val_ids.iter().map(|v| v.public().into()).collect()
	}

	#[test]
	fn sets_and_shuffles_validators() {
		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Ferdie,
		];

		let mut config = HostConfiguration::default();
		config.max_validators = None;

		let pubkeys = validator_pubkeys(&validators);

		new_test_ext(MockGenesisConfig::default()).execute_with(|| {
			let validators = ParasShared::initializer_on_new_session(
				1,
				[1; 32],
				&config,
				pubkeys,
			);

			assert_eq!(
				validators,
				validator_pubkeys(&[
					Sr25519Keyring::Ferdie,
					Sr25519Keyring::Bob,
					Sr25519Keyring::Charlie,
					Sr25519Keyring::Dave,
					Sr25519Keyring::Alice,
				])
			);

			assert_eq!(
				ParasShared::active_validator_keys(),
				validators,
			);

			assert_eq!(
				ParasShared::active_validator_indices(),
				vec![
					ValidatorIndex(4),
					ValidatorIndex(1),
					ValidatorIndex(2),
					ValidatorIndex(3),
					ValidatorIndex(0),
				]
			);
		});
	}

	#[test]
	fn sets_truncates_and_shuffles_validators() {
		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Ferdie,
		];

		let mut config = HostConfiguration::default();
		config.max_validators = Some(2);

		let pubkeys = validator_pubkeys(&validators);

		new_test_ext(MockGenesisConfig::default()).execute_with(|| {
			let validators = ParasShared::initializer_on_new_session(
				1,
				[1; 32],
				&config,
				pubkeys,
			);

			assert_eq!(
				validators,
				validator_pubkeys(&[
					Sr25519Keyring::Ferdie,
					Sr25519Keyring::Bob,
				])
			);

			assert_eq!(
				ParasShared::active_validator_keys(),
				validators,
			);

			assert_eq!(
				ParasShared::active_validator_indices(),
				vec![
					ValidatorIndex(4),
					ValidatorIndex(1),
				]
			);
		});
	}
}
