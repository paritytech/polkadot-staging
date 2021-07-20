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
// along with Polkadot. If not, see <http://www.gnu.org/licenses/>.

//! Tests for the Westend Runtime Configuration

use crate::*;

#[test]
fn remove_keys_weight_is_sensible() {
	use runtime_common::crowdloan::WeightInfo;
	let max_weight = <Runtime as crowdloan::Config>::WeightInfo::refund(RemoveKeysLimit::get());
	// Max remove keys limit should be no more than half the total block weight.
	assert!(max_weight * 2 < BlockWeights::get().max_block);
}

#[test]
fn sample_size_is_sensible() {
	use runtime_common::auctions::WeightInfo;
	// Need to clean up all samples at the end of an auction.
	let samples: BlockNumber = EndingPeriod::get() / SampleLength::get();
	let max_weight: Weight = RocksDbWeight::get().reads_writes(samples.into(), samples.into());
	// Max sample cleanup should be no more than half the total block weight.
	assert!(max_weight * 2 < BlockWeights::get().max_block);
	assert!(<Runtime as auctions::Config>::WeightInfo::on_initialize() * 2 < BlockWeights::get().max_block);
}

#[test]
fn election_provider_signed_reward_base_is_as_expected() {
	sp_io::TestExternalities::new_empty().execute_with(|| {
		assert_eq!(
			<Runtime as pallet_election_provider_multi_phase::Config>::SignedRewardBase::get(),
			2_463_776_601_600u64.into()
		);
	});
}
