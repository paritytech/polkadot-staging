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

//! Tests for the Kusama Runtime Configuration

use crate::*;
use frame_support::weights::WeightToFeePolynomial;
use sp_runtime::FixedPointNumber;
use frame_support::weights::GetDispatchInfo;
use parity_scale_codec::Encode;
use pallet_transaction_payment::Multiplier;
use separator::Separatable;

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
fn payout_weight_portion() {
	use pallet_staking::WeightInfo;
	let payout_weight =
		<Runtime as pallet_staking::Config>::WeightInfo::payout_stakers_alive_staked(
			MaxNominatorRewardedPerValidator::get(),
		) as f64;
	let block_weight = BlockWeights::get().max_block as f64;

	println!(
		"a full payout takes {:.2} of the block weight [{} / {}]",
		payout_weight / block_weight,
		payout_weight,
		block_weight
	);
	assert!(payout_weight * 2f64 < block_weight);
}

#[test]
#[ignore]
fn block_cost() {
	let max_block_weight = BlockWeights::get().max_block;
	let raw_fee = WeightToFee::calc(&max_block_weight);

	println!(
		"Full Block weight == {} // WeightToFee(full_block) == {} plank",
		max_block_weight,
		raw_fee.separated_string(),
	);
}

#[test]
#[ignore]
fn transfer_cost_min_multiplier() {
	let min_multiplier = runtime_common::MinimumMultiplier::get();
	let call = <pallet_balances::Call<Runtime>>::transfer_keep_alive(Default::default(), Default::default());
	let info = call.get_dispatch_info();
	// convert to outer call.
	let call = Call::Balances(call);
	let len = call.using_encoded(|e| e.len()) as u32;

	let mut ext = sp_io::TestExternalities::new_empty();
	let mut test_with_multiplier = |m| {
		ext.execute_with(|| {
			pallet_transaction_payment::NextFeeMultiplier::<Runtime>::put(m);
			let fee = TransactionPayment::compute_fee(len, &info, 0);
			println!(
				"weight = {:?} // multiplier = {:?} // full transfer fee = {:?}",
				info.weight.separated_string(),
				pallet_transaction_payment::NextFeeMultiplier::<Runtime>::get(),
				fee.separated_string(),
			);
		});
	};

	test_with_multiplier(min_multiplier);
	test_with_multiplier(Multiplier::saturating_from_rational(1, 1u128));
	test_with_multiplier(Multiplier::saturating_from_rational(1, 1_000u128));
	test_with_multiplier(Multiplier::saturating_from_rational(1, 1_000_000u128));
	test_with_multiplier(Multiplier::saturating_from_rational(1, 1_000_000_000u128));
}

#[test]
fn nominator_limit() {
	use pallet_election_provider_multi_phase::WeightInfo;
	// starting point of the nominators.
	let all_voters: u32 = 10_000;

	// assuming we want around 5k candidates and 1k active validators.
	let all_targets: u32 = 5_000;
	let desired: u32 = 1_000;
	let weight_with = |active| {
		<Runtime as pallet_election_provider_multi_phase::Config>::WeightInfo::submit_unsigned(
			all_voters.max(active),
			all_targets,
			active,
			desired,
		)
	};

	let mut active = 1;
	while weight_with(active) <= OffchainSolutionWeightLimit::get() || active == all_voters {
		active += 1;
	}

	println!("can support {} nominators to yield a weight of {}", active, weight_with(active));
}

#[test]
fn compute_inflation_should_give_sensible_results() {
	assert_eq!(pallet_staking_reward_fn::compute_inflation(
		Perquintill::from_percent(75),
		Perquintill::from_percent(75),
		Perquintill::from_percent(5),
	), Perquintill::one());
	assert_eq!(pallet_staking_reward_fn::compute_inflation(
		Perquintill::from_percent(50),
		Perquintill::from_percent(75),
		Perquintill::from_percent(5),
	), Perquintill::from_rational(2u64, 3u64));
	assert_eq!(pallet_staking_reward_fn::compute_inflation(
		Perquintill::from_percent(80),
		Perquintill::from_percent(75),
		Perquintill::from_percent(5),
	), Perquintill::from_rational(1u64, 2u64));
}

#[test]
fn era_payout_should_give_sensible_results() {
	assert_eq!(era_payout(
		75,
		100,
		Perquintill::from_percent(10),
		Perquintill::one(),
		0,
	), (10, 0));
	assert_eq!(era_payout(
		80,
		100,
		Perquintill::from_percent(10),
		Perquintill::one(),
		0,
	), (6, 4));
}

#[test]
fn election_provider_signed_reward_base_is_as_expected() {
	sp_io::TestExternalities::new_empty().execute_with(|| {
		assert_eq!(
			<Runtime as pallet_election_provider_multi_phase::Config>::SignedRewardBase::get(),
			8_205_631_934u64.into()
		);
	});
}
