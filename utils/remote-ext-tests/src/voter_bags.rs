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

use remote_externalities::{Builder, Mode, OnlineConfig};
use sp_storage::well_known_keys;
use pallet_staking::{
	Nominators, Validators, CounterForValidators, CounterForNominators,
	voter_bags::{Bag, VoterList},
};
use frame_support::{assert_ok, traits::Get};
use sp_runtime::traits::Block as BlockT;

const LOG_TARGET: &'static str = "remote-ext-tests";

fn init_logger() {
	sp_tracing::try_init_simple();
}

/// Test voter bags migration. `currency_unit` is the number of planks per the
/// the runtimes `UNITS` (i.e. number of decimal places per DOT, KSM etc)
pub (crate) async fn test_voter_bags_migration<
		Runtime: pallet_staking::Config,
		Block: BlockT
	>(currency_unit: u64) {
	use std::env;

	init_logger();

	let ws_url = match env::var("WS_RPC") {
		Ok(ws_url) => ws_url,
		Err(_) => panic!("Must set env var `WS_RPC=<ws-url>`"),
	};

	let mut ext = Builder::<Block>::new()
		.mode(Mode::Online(OnlineConfig {
			transport: ws_url.to_string().into(),
			modules: vec!["Staking".to_string()],
			at: None,
			state_snapshot: None,
		}))
		.inject_hashed_key(well_known_keys::CODE)
		.build()
		.await
		.unwrap();

	ext.execute_with(|| {
		// Get the nominator & validator count prior to migrating; these should be invariant.
		let pre_migrate_nominator_count = <Nominators<Runtime>>::iter().collect::<Vec<_>>().len();
		let pre_migrate_validator_count = <Validators<Runtime>>::iter().collect::<Vec<_>>().len();
		log::info!(target: LOG_TARGET, "Nominator count: {}", pre_migrate_nominator_count);
		log::info!(target: LOG_TARGET, "Validator count: {}", pre_migrate_validator_count);

		assert_ok!(pallet_staking::migrations::v8::pre_migrate::<Runtime>());

		// Run the actual migration.
		let migration_weight = pallet_staking::migrations::v8::migrate::<Runtime>();
		log::info!(target: LOG_TARGET, "Migration weight: {}", migration_weight);

		// `CountFor*` storage items are created during the migration, check them.
		assert_eq!(CounterForNominators::<Runtime>::get(), pre_migrate_nominator_count as u32);
		assert_eq!(CounterForValidators::<Runtime>::get(), pre_migrate_validator_count as u32);

		// Check that the count of validators and nominators did not change during the migration.
		let post_migrate_nominator_count = <Nominators<Runtime>>::iter().collect::<Vec<_>>().len();
		let post_migrate_validator_count = <Validators<Runtime>>::iter().collect::<Vec<_>>().len();
		assert_eq!(post_migrate_nominator_count, pre_migrate_nominator_count);
		assert_eq!(post_migrate_validator_count, pre_migrate_validator_count);

		// We can't access VoterCount from here, so we create it,
		let voter_count = post_migrate_nominator_count + post_migrate_validator_count;
		let voter_list_len = VoterList::<Runtime>::iter().collect::<Vec<_>>().len();
		// and confirm it is equal to the length of the `VoterList`.
		assert_eq!(voter_count, voter_list_len);

		// Go through every bag to track the total number of voters within bags
		// and get some info about how voters are distributed within the bags.
		let mut seen_in_bags = 0;
		for vote_weight_thresh in <Runtime as pallet_staking::Config>::VoterBagThresholds::get() {
			// Threshold in terms of UNITS (e.g. KSM, DOT etc)
			let vote_weight_thresh_as_unit = *vote_weight_thresh as f64 / currency_unit as f64;
			let pretty_thresh = format!("Threshold: {}.", vote_weight_thresh_as_unit);

			let bag = match Bag::<Runtime>::get(*vote_weight_thresh) {
				Some(bag) => bag,
				None => {
					log::info!(target: LOG_TARGET, "{} NO VOTERS.", pretty_thresh);
					continue;
				},
			};

			let voters_in_bag_count = bag.iter().collect::<Vec<_>>().len();
			seen_in_bags += voters_in_bag_count;
			let percentage_of_voters =
				(voters_in_bag_count as f64 / voter_count as f64) * 100f64;
			log::info!(
				target: LOG_TARGET,
				"{} Voters: {} [%{:.3}]",
				pretty_thresh, voters_in_bag_count, percentage_of_voters
			);
		}

		assert_eq!(seen_in_bags, voter_list_len);
	});
}
