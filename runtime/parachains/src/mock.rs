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

//! Mocks for all the traits.

use sp_io::TestExternalities;
use sp_core::H256;
use sp_runtime::traits::{
	BlakeTwo256, IdentityLookup,
};
use primitives::v1::{
	AuthorityDiscoveryId, Balance, BlockNumber, Header, ValidatorIndex, SessionIndex,
};
use frame_support::parameter_types;
use frame_support::traits::GenesisBuild;
use frame_support_test::TestRandomness;
use std::cell::RefCell;
use std::collections::HashMap;
use crate::{
	inclusion, scheduler, dmp, ump, hrmp, session_info, paras, configuration,
	initializer, shared, disputes,
};

type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Test>;
type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test where
		Block = Block,
		NodeBlock = Block,
		UncheckedExtrinsic = UncheckedExtrinsic,
	{
		System: frame_system::{Pallet, Call, Config, Storage, Event<T>},
		Balances: pallet_balances::{Pallet, Call, Storage, Config<T>, Event<T>},
		Paras: paras::{Pallet, Origin, Call, Storage, Event, Config},
		Configuration: configuration::{Pallet, Call, Storage, Config<T>},
		Shared: shared::{Pallet, Call, Storage},
		ParaInclusion: inclusion::{Pallet, Call, Storage, Event<T>},
		Scheduler: scheduler::{Pallet, Call, Storage},
		Initializer: initializer::{Pallet, Call, Storage},
		Dmp: dmp::{Pallet, Call, Storage},
		Ump: ump::{Pallet, Call, Storage, Event},
		Hrmp: hrmp::{Pallet, Call, Storage, Event},
		SessionInfo: session_info::{Pallet, Call, Storage},
		Disputes: disputes::{Pallet, Storage, Event<T>},
	}
);

parameter_types! {
	pub const BlockHashCount: u32 = 250;
	pub BlockWeights: frame_system::limits::BlockWeights =
		frame_system::limits::BlockWeights::simple_max(4 * 1024 * 1024);
}

pub type AccountId = u64;

impl frame_system::Config for Test {
	type BaseCallFilter = frame_support::traits::AllowAll;
	type BlockWeights = BlockWeights;
	type BlockLength = ();
	type DbWeight = ();
	type Origin = Origin;
	type Call = Call;
	type Index = u64;
	type BlockNumber = BlockNumber;
	type Hash = H256;
	type Hashing = BlakeTwo256;
	type AccountId = u64;
	type Lookup = IdentityLookup<u64>;
	type Header = Header;
	type Event = Event;
	type BlockHashCount = BlockHashCount;
	type Version = ();
	type PalletInfo = PalletInfo;
	type AccountData = pallet_balances::AccountData<u128>;
	type OnNewAccount = ();
	type OnKilledAccount = ();
	type SystemWeightInfo = ();
	type SS58Prefix = ();
	type OnSetCode = ();
}

parameter_types! {
	pub static ExistentialDeposit: u64 = 0;
}

impl pallet_balances::Config for Test {
	type MaxLocks = ();
	type MaxReserves = ();
	type ReserveIdentifier = [u8; 8];
	type Balance = Balance;
	type Event = Event;
	type DustRemoval = ();
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type WeightInfo = ();
}

impl crate::initializer::Config for Test {
	type Randomness = TestRandomness<Self>;
	type ForceOrigin = frame_system::EnsureRoot<u64>;
}

impl crate::configuration::Config for Test { }

impl crate::shared::Config for Test { }

impl crate::paras::Config for Test {
	type Origin = Origin;
	type Event = Event;
}

impl crate::dmp::Config for Test { }

parameter_types! {
	pub const FirstMessageFactorPercent: u64 = 100;
}

impl crate::ump::Config for Test {
	type Event = Event;
	type UmpSink = crate::ump::mock_sink::MockUmpSink;
	type FirstMessageFactorPercent = FirstMessageFactorPercent;
}

impl crate::hrmp::Config for Test {
	type Event = Event;
	type Origin = Origin;
	type Currency = pallet_balances::Pallet<Test>;
}

impl crate::disputes::Config for Test {
	type Event = Event;
	type RewardValidators = Self;
	type PunishValidators = Self;
}

thread_local! {
	pub static REWARD_VALIDATORS: RefCell<Vec<(SessionIndex, Vec<ValidatorIndex>)>> = RefCell::new(Vec::new());
	pub static PUNISH_VALIDATORS_FOR: RefCell<Vec<(SessionIndex, Vec<ValidatorIndex>)>> = RefCell::new(Vec::new());
	pub static PUNISH_VALIDATORS_AGAINST: RefCell<Vec<(SessionIndex, Vec<ValidatorIndex>)>> = RefCell::new(Vec::new());
	pub static PUNISH_VALIDATORS_INCONCLUSIVE: RefCell<Vec<(SessionIndex, Vec<ValidatorIndex>)>> = RefCell::new(Vec::new());
}

impl crate::disputes::RewardValidators for Test {
	fn reward_dispute_statement(
		session: SessionIndex,
		validators: impl IntoIterator<Item=ValidatorIndex>
	) {
		REWARD_VALIDATORS.with(|r| r.borrow_mut().push((session, validators.into_iter().collect())))
	}
}

impl crate::disputes::PunishValidators for Test {
	fn punish_for_invalid(
		session: SessionIndex,
		validators: impl IntoIterator<Item=ValidatorIndex>,
	) {
		PUNISH_VALIDATORS_FOR
			.with(|r| r.borrow_mut().push((session, validators.into_iter().collect())))
	}

	fn punish_against_valid(
		session: SessionIndex,
		validators: impl IntoIterator<Item=ValidatorIndex>,
	) {
		PUNISH_VALIDATORS_AGAINST
			.with(|r| r.borrow_mut().push((session, validators.into_iter().collect())))
	}

	fn punish_inconclusive(
		session: SessionIndex,
		validators: impl IntoIterator<Item=ValidatorIndex>,
	) {
		PUNISH_VALIDATORS_INCONCLUSIVE
			.with(|r| r.borrow_mut().push((session, validators.into_iter().collect())))
	}
}

impl crate::scheduler::Config for Test { }

impl crate::inclusion::Config for Test {
	type Event = Event;
	type DisputesHandler = Disputes;
	type RewardValidators = TestRewardValidators;
}

impl crate::paras_inherent::Config for Test { }

impl crate::session_info::Config for Test { }

thread_local! {
	pub static DISCOVERY_AUTHORITIES: RefCell<Vec<AuthorityDiscoveryId>> = RefCell::new(Vec::new());
}

pub fn discovery_authorities() -> Vec<AuthorityDiscoveryId> {
	DISCOVERY_AUTHORITIES.with(|r| r.borrow().clone())
}

pub fn set_discovery_authorities(new: Vec<AuthorityDiscoveryId>) {
	DISCOVERY_AUTHORITIES.with(|r| *r.borrow_mut() = new);
}

impl crate::session_info::AuthorityDiscoveryConfig for Test {
	fn authorities() -> Vec<AuthorityDiscoveryId> {
		discovery_authorities()
	}
}

thread_local! {
	pub static BACKING_REWARDS: RefCell<HashMap<ValidatorIndex, usize>>
		= RefCell::new(HashMap::new());

	pub static AVAILABILITY_REWARDS: RefCell<HashMap<ValidatorIndex, usize>>
		= RefCell::new(HashMap::new());
}

pub fn backing_rewards() -> HashMap<ValidatorIndex, usize> {
	BACKING_REWARDS.with(|r| r.borrow().clone())
}

pub fn availability_rewards() -> HashMap<ValidatorIndex, usize> {
	AVAILABILITY_REWARDS.with(|r| r.borrow().clone())
}

pub struct TestRewardValidators;

impl inclusion::RewardValidators for TestRewardValidators {
	fn reward_backing(v: impl IntoIterator<Item = ValidatorIndex>) {
		BACKING_REWARDS.with(|r| {
			let mut r = r.borrow_mut();
			for i in v {
				*r.entry(i).or_insert(0) += 1;
			}
		})
	}
	fn reward_bitfields(v: impl IntoIterator<Item = ValidatorIndex>) {
		AVAILABILITY_REWARDS.with(|r| {
			let mut r = r.borrow_mut();
			for i in v {
				*r.entry(i).or_insert(0) += 1;
			}
		})
	}
}

/// Create a new set of test externalities.
pub fn new_test_ext(state: MockGenesisConfig) -> TestExternalities {
	BACKING_REWARDS.with(|r| r.borrow_mut().clear());
	AVAILABILITY_REWARDS.with(|r| r.borrow_mut().clear());

	let mut t = state.system.build_storage::<Test>().unwrap();
	state.configuration.assimilate_storage(&mut t).unwrap();
	GenesisBuild::<Test>::assimilate_storage(&state.paras, &mut t).unwrap();

	t.into()
}

#[derive(Default)]
pub struct MockGenesisConfig {
	pub system: frame_system::GenesisConfig,
	pub configuration: crate::configuration::GenesisConfig<Test>,
	pub paras: crate::paras::GenesisConfig,
}
