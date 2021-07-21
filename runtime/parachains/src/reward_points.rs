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

//! An implementation of the `RewardValidators` trait used by `inclusion` that employs
//! `pallet-staking` to compute the rewards.
//!
//! Based on https://w3f-research.readthedocs.io/en/latest/polkadot/Token%20Economics.html
//! which doesn't currently mention availability bitfields. As such, we don't reward them
//! for the time being, although we will build schemes to do so in the future.

use primitives::v1::ValidatorIndex;
use pallet_staking::SessionInterface;
use crate::shared;

/// The amount of era points given by backing a candidate that is included.
pub const BACKING_POINTS: u32 = 20;

/// Rewards validators for participating in parachains with era points in pallet-staking.
pub struct RewardValidatorsWithEraPoints<C>(sp_std::marker::PhantomData<C>);

fn validators_to_reward<C, T, I>(validators: &'_ [T], indirect_indices: I) -> impl IntoIterator<Item=&'_ T> where
	C: shared::Config,
	I: IntoIterator<Item = ValidatorIndex>
{
	let validator_indirection = <shared::Pallet<C>>::active_validator_indices();

	indirect_indices.into_iter()
		.filter_map(move |i| validator_indirection.get(i.0 as usize).map(|v| v.clone()))
		.filter_map(move |i| validators.get(i.0 as usize))
}

impl<C> crate::inclusion::RewardValidators for RewardValidatorsWithEraPoints<C>
	where C: pallet_staking::Config + shared::Config,
{
	fn reward_backing(indirect_indices: impl IntoIterator<Item=ValidatorIndex>) {
		// Fetch the validators from the _session_ because sessions are offset from eras
		// and we are rewarding for behavior in current session.
		let validators = C::SessionInterface::validators();

		let rewards = validators_to_reward::<C, _, _>(&validators, indirect_indices)
			.into_iter()
			.map(|v| (v.clone(), BACKING_POINTS));

		<pallet_staking::Pallet<C>>::reward_by_ids(rewards);
	}

	fn reward_bitfields(_validators: impl IntoIterator<Item=ValidatorIndex>) { }
}

#[cfg(test)]
mod tests {
	use super::*;
	use primitives::v1::ValidatorId;
	use crate::configuration::HostConfiguration;
	use crate::mock::{new_test_ext, MockGenesisConfig, ParasShared, Test};
	use keyring::Sr25519Keyring;

	#[test]
	fn rewards_based_on_indirection() {
		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Ferdie,
		];

		fn validator_pubkeys(val_ids: &[Sr25519Keyring]) -> Vec<ValidatorId> {
			val_ids.iter().map(|v| v.public().into()).collect()
		}

		new_test_ext(MockGenesisConfig::default()).execute_with(|| {
			let mut config = HostConfiguration::default();
			config.max_validators = None;

			let pubkeys = validator_pubkeys(&validators);

			let shuffled_pubkeys = ParasShared::initializer_on_new_session(
				1,
				[1; 32],
				&config,
				pubkeys,
			);

			assert_eq!(
				shuffled_pubkeys,
				validator_pubkeys(&[
					Sr25519Keyring::Ferdie,
					Sr25519Keyring::Bob,
					Sr25519Keyring::Charlie,
					Sr25519Keyring::Dave,
					Sr25519Keyring::Alice,
				])
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

			assert_eq!(
				validators_to_reward::<Test, _, _>(
					&validators,
					vec![ValidatorIndex(0), ValidatorIndex(1), ValidatorIndex(2)],
				).into_iter().copied().collect::<Vec<_>>(),
				vec![Sr25519Keyring::Ferdie, Sr25519Keyring::Bob, Sr25519Keyring::Charlie],
			);
		})
	}
}
