// Copyright 2017-2020 Parity Technologies (UK) Ltd.
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

//! Module to handle parathread/parachain registration and related fund management.
//! In essence this is a simple wrapper around `paras`.

use crate::WASM_MAGIC;
use sp_std::{prelude::*, result};
use frame_support::{
	decl_storage, decl_module, decl_error, ensure,
	dispatch::DispatchResult,
	traits::{Get, Currency, ReservableCurrency},
};
use frame_system::{self, ensure_root, ensure_signed};
use primitives::v1::{
	Id as ParaId, ValidationCode, HeadData,
};
use runtime_parachains::{
	paras::{
		self,
		ParaGenesisArgs,
	},
	dmp, ump, hrmp,
	ensure_parachain,
	Origin,
};

type BalanceOf<T> =
	<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

pub trait Config: paras::Config + dmp::Config + ump::Config + hrmp::Config {
	/// The aggregated origin type must support the `parachains` origin. We require that we can
	/// infallibly convert between this origin and the system origin, but in reality, they're the
	/// same type, we just can't express that to the Rust type system without writing a `where`
	/// clause everywhere.
	type Origin: From<<Self as frame_system::Config>::Origin>
		+ Into<result::Result<Origin, <Self as Config>::Origin>>;

	/// The system's currency for parathread payment.
	type Currency: ReservableCurrency<Self::AccountId>;

	/// The deposit to be paid to run a parathread.
	type ParathreadDeposit: Get<BalanceOf<Self>>;
}

decl_storage! {
	trait Store for Module<T: Config> as Registrar {
		/// Whether parathreads are enabled or not.
		ParathreadsRegistrationEnabled: bool;

		/// Pending swap operations.
		PendingSwap: map hasher(twox_64_concat) ParaId => Option<ParaId>;

		/// Map of all registered parathreads/chains.
		Paras get(fn paras): map hasher(twox_64_concat) ParaId => Option<bool>;

		/// Users who have paid a parathread's deposit.
		Debtors: map hasher(twox_64_concat) ParaId => T::AccountId;
	}
}

decl_error! {
	pub enum Error for Module<T: Config> {
		/// Parachain already exists.
		ParaAlreadyExists,
		/// Invalid parachain ID.
		InvalidChainId,
		/// Invalid parathread ID.
		InvalidThreadId,
		/// Invalid para code size.
		CodeTooLarge,
		/// Invalid para head data size.
		HeadDataTooLarge,
		/// Parathreads registration is disabled.
		ParathreadsRegistrationDisabled,
		/// The validation code provided doesn't start with the Wasm file magic string.
		DefinitelyNotWasm,
		/// Cannot deregister para
		CannotDeregister,
	}
}

decl_module! {
	pub struct Module<T: Config> for enum Call where origin: <T as frame_system::Config>::Origin {
		type Error = Error<T>;

		/// Register a parathread with given code for immediate use.
		///
		/// Must be sent from a Signed origin that is able to have `ParathreadDeposit` reserved.
		/// `genesis_head` and `validation_code` are used to initalize the parathread's state.
		#[weight = 0]
		fn register_parathread(
			origin,
			id: ParaId,
			genesis_head: HeadData,
			validation_code: ValidationCode,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;

			ensure!(ParathreadsRegistrationEnabled::get(), Error::<T>::ParathreadsRegistrationDisabled);
			ensure!(validation_code.0.starts_with(WASM_MAGIC), Error::<T>::DefinitelyNotWasm);

			ensure!(!Paras::contains_key(id), Error::<T>::ParaAlreadyExists);

			let genesis = ParaGenesisArgs {
				genesis_head,
				validation_code,
				parachain: false,
			};
			ensure!(paras::Module::<T>::can_schedule_para_initialize(&id, &genesis), Error::<T>::ParaAlreadyExists);
			<T as Config>::Currency::reserve(&who, T::ParathreadDeposit::get())?;

			<Debtors<T>>::insert(id, who);
			Paras::insert(id, false);
			// Checked this shouldn't fail above.
			let _ = runtime_parachains::schedule_para_initialize::<T>(id, genesis);

			Ok(())
		}

		/// Deregister a parathread and retreive the deposit.
		///
		/// Must be sent from a `Parachain` origin which is currently a parathread.
		///
		/// Ensure that before calling this that any funds you want emptied from the parathread's
		/// account is moved out; after this it will be impossible to retreive them (without
		/// governance intervention).
		#[weight = 0]
		fn deregister_parathread(origin) -> DispatchResult {
			let id = ensure_parachain(<T as Config>::Origin::from(origin))?;

			ensure!(ParathreadsRegistrationEnabled::get(), Error::<T>::ParathreadsRegistrationDisabled);

			let is_parachain = Paras::get(id).ok_or(Error::<T>::InvalidChainId)?;

			ensure!(!is_parachain, Error::<T>::InvalidThreadId);

			runtime_parachains::schedule_para_cleanup::<T>(id).map_err(|_| Error::<T>::CannotDeregister)?;

			let debtor = <Debtors<T>>::take(id);
			let _ = <T as Config>::Currency::unreserve(&debtor, T::ParathreadDeposit::get());
			Paras::remove(&id);
			PendingSwap::remove(&id);

			Ok(())
		}

		#[weight = 0]
		fn enable_parathread_registration(origin) -> DispatchResult {
			ensure_root(origin)?;

			ParathreadsRegistrationEnabled::put(true);

			Ok(())
		}

		#[weight = 0]
		fn disable_parathread_registration(origin) -> DispatchResult {
			ensure_root(origin)?;

			ParathreadsRegistrationEnabled::put(false);

			Ok(())
		}

		/// Swap a parachain with another parachain or parathread. The origin must be a `Parachain`.
		/// The swap will happen only if there is already an opposite swap pending. If there is not,
		/// the swap will be stored in the pending swaps map, ready for a later confirmatory swap.
		///
		/// The `ParaId`s remain mapped to the same head data and code so external code can rely on
		/// `ParaId` to be a long-term identifier of a notional "parachain". However, their
		/// scheduling info (i.e. whether they're a parathread or parachain), auction information
		/// and the auction deposit are switched.
		#[weight = 0]
		fn swap(origin, other: ParaId) {
			let id = ensure_parachain(<T as Config>::Origin::from(origin))?;

			if PendingSwap::get(other) == Some(id) {
				// Remove intention to swap.
				PendingSwap::remove(other);

				Paras::mutate(id, |i|
					Paras::mutate(other, |j|
						sp_std::mem::swap(i, j)
					)
				);

				<Debtors<T>>::mutate(id, |i|
					<Debtors<T>>::mutate(other, |j|
						sp_std::mem::swap(i, j)
					)
				);
			} else {
				PendingSwap::insert(id, other);
			}
		}
	}
}

impl<T: Config> Module<T> {
	/// Register a parachain with given code. Must be called by root.
	/// Fails if given ID is already used.
	pub fn register_parachain(
		id: ParaId,
		genesis_head: HeadData,
		validation_code: ValidationCode,
	) -> DispatchResult {
		ensure!(!Paras::contains_key(id), Error::<T>::ParaAlreadyExists);
		ensure!(validation_code.0.starts_with(WASM_MAGIC), Error::<T>::DefinitelyNotWasm);

		let genesis = ParaGenesisArgs {
			genesis_head,
			validation_code,
			parachain: true,
		};

		runtime_parachains::schedule_para_initialize::<T>(id, genesis).map_err(|_| Error::<T>::ParaAlreadyExists)?;

		Paras::insert(id, true);
		Ok(())
	}

	/// Deregister a parachain with the given ID. Must be called by root.
	pub fn deregister_parachain(id: ParaId) -> DispatchResult {
		let is_parachain = Paras::get(id).ok_or(Error::<T>::InvalidChainId)?;

		ensure!(is_parachain, Error::<T>::InvalidChainId);

		runtime_parachains::schedule_para_cleanup::<T>(id).map_err(|_| Error::<T>::CannotDeregister)?;
		Paras::remove(&id);
		PendingSwap::remove(&id);

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_io::TestExternalities;
	use sp_core::H256;
	use sp_runtime::{
		traits::{
			BlakeTwo256, IdentityLookup, Extrinsic as ExtrinsicT,
		}, testing::{UintAuthorityId, TestXt}, Perbill, curve::PiecewiseLinear,
	};
	use primitives::v1::{
		Balance, BlockNumber, Header, Signature, AuthorityDiscoveryId, ValidatorIndex,
	};
	use frame_system::limits;
	use frame_support::{
		traits::{OnInitialize, OnFinalize},
		assert_ok, assert_noop, parameter_types,
	};
	use frame_support_test::TestRandomness;
	use keyring::Sr25519Keyring;
	use runtime_parachains::{
		initializer, configuration, inclusion, session_info, scheduler, dmp, ump, hrmp, shared,
		ParaLifecycle,
	};
	use frame_support::traits::OneSessionHandler;
	use crate::paras_registrar;

	type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Test>;
	type Block = frame_system::mocking::MockBlock<Test>;

	frame_support::construct_runtime!(
		pub enum Test where
			Block = Block,
			NodeBlock = Block,
			UncheckedExtrinsic = UncheckedExtrinsic,
		{
			System: frame_system::{Module, Call, Config, Storage, Event<T>},
			Balances: pallet_balances::{Module, Call, Storage, Config<T>, Event<T>},
			Parachains: paras::{Module, Origin, Call, Storage, Config<T>},
			Shared: shared::{Module, Call, Storage},
			Inclusion: inclusion::{Module, Call, Storage, Event<T>},
			Registrar: paras_registrar::{Module, Call, Storage},
	 		Staking: pallet_staking::{Module, Call, Config<T>, Storage, Event<T>, ValidateUnsigned},
			Session: pallet_session::{Module, Call, Storage, Event, Config<T>},
			Initializer: initializer::{Module, Call, Storage},
			Hrmp: hrmp::{Module, Call, Storage, Event},
		}
	);

	pallet_staking_reward_curve::build! {
		const REWARD_CURVE: PiecewiseLinear<'static> = curve!(
			min_inflation: 0_025_000,
			max_inflation: 0_100_000,
			ideal_stake: 0_500_000,
			falloff: 0_050_000,
			max_piece_count: 40,
			test_precision: 0_005_000,
		);
	}

	const NORMAL_RATIO: Perbill = Perbill::from_percent(75);
	parameter_types! {
		pub const BlockHashCount: u32 = 250;
		pub BlockWeights: limits::BlockWeights =
			frame_system::limits::BlockWeights::simple_max(1024);
		pub BlockLength: limits::BlockLength =
			limits::BlockLength::max_with_normal_ratio(4 * 1024 * 1024, NORMAL_RATIO);
	}

	impl frame_system::Config for Test {
		type BaseCallFilter = ();
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
		type DbWeight = ();
		type BlockWeights = BlockWeights;
		type BlockLength = BlockLength;
		type Version = ();
		type PalletInfo = PalletInfo;
		type AccountData = pallet_balances::AccountData<u128>;
		type OnNewAccount = ();
		type OnKilledAccount = ();
		type SystemWeightInfo = ();
		type SS58Prefix = ();
	}

	impl<C> frame_system::offchain::SendTransactionTypes<C> for Test where
		Call: From<C>,
	{
		type OverarchingCall = Call;
		type Extrinsic = TestXt<Call, ()>;
	}

	parameter_types! {
		pub const ExistentialDeposit: Balance = 1;
	}

	impl pallet_balances::Config for Test {
		type Balance = u128;
		type DustRemoval = ();
		type Event = Event;
		type ExistentialDeposit = ExistentialDeposit;
		type AccountStore = System;
		type MaxLocks = ();
		type WeightInfo = ();
	}

	parameter_types!{
		pub const SlashDeferDuration: pallet_staking::EraIndex = 7;
		pub const AttestationPeriod: BlockNumber = 100;
		pub const MinimumPeriod: u64 = 3;
		pub const SessionsPerEra: sp_staking::SessionIndex = 6;
		pub const BondingDuration: pallet_staking::EraIndex = 28;
		pub const MaxNominatorRewardedPerValidator: u32 = 64;
	}

	parameter_types! {
		pub const Period: BlockNumber = 3;
		pub const Offset: BlockNumber = 0;
		pub const DisabledValidatorsThreshold: Perbill = Perbill::from_percent(17);
		pub const RewardCurve: &'static PiecewiseLinear<'static> = &REWARD_CURVE;
	}

	impl pallet_session::Config for Test {
		type SessionManager = ();
		type Keys = UintAuthorityId;
		type ShouldEndSession = pallet_session::PeriodicSessions<Period, Offset>;
		type NextSessionRotation = pallet_session::PeriodicSessions<Period, Offset>;
		type SessionHandler = pallet_session::TestSessionHandler;
		type Event = Event;
		type ValidatorId = u64;
		type ValidatorIdOf = ();
		type DisabledValidatorsThreshold = DisabledValidatorsThreshold;
		type ReferencedAccount = Accounts;
		type WeightInfo = ();
	}

	parameter_types! {
		pub const MaxHeadDataSize: u32 = 100;
		pub const MaxCodeSize: u32 = 100;

		pub const ValidationUpgradeFrequency: BlockNumber = 10;
		pub const ValidationUpgradeDelay: BlockNumber = 2;
		pub const SlashPeriod: BlockNumber = 50;
		pub const ElectionLookahead: BlockNumber = 0;
		pub const StakingUnsignedPriority: u64 = u64::max_value() / 2;
	}

	impl sp_election_providers::onchain::Config for Test {
		type AccountId = <Self as frame_system::Config>::AccountId;
		type BlockNumber = <Self as frame_system::Config>::BlockNumber;
		type Accuracy = sp_runtime::Perbill;
		type DataProvider = pallet_staking::Module<Test>;
	}

	impl pallet_staking::Config for Test {
		type RewardRemainder = ();
		type CurrencyToVote = frame_support::traits::SaturatingCurrencyToVote;
		type Event = Event;
		type Currency = pallet_balances::Module<Test>;
		type Slash = ();
		type Reward = ();
		type SessionsPerEra = SessionsPerEra;
		type BondingDuration = BondingDuration;
		type SlashDeferDuration = SlashDeferDuration;
		type SlashCancelOrigin = frame_system::EnsureRoot<Self::AccountId>;
		type SessionInterface = Self;
		type UnixTime = pallet_timestamp::Module<Test>;
		type RewardCurve = RewardCurve;
		type MaxNominatorRewardedPerValidator = MaxNominatorRewardedPerValidator;
		type NextNewSession = Session;
		type ElectionLookahead = ElectionLookahead;
		type Call = Call;
		type UnsignedPriority = StakingUnsignedPriority;
		type MaxIterations = ();
		type MinSolutionScoreBump = ();
		type OffchainSolutionWeightLimit = ();
		type ElectionProvider = sp_election_providers::onchain::OnChainSequentialPhragmen<Self>;
		type WeightInfo = ();
	}

	impl pallet_timestamp::Config for Test {
		type Moment = u64;
		type OnTimestampSet = ();
		type MinimumPeriod = MinimumPeriod;
		type WeightInfo = ();
	}

	impl shared::Config for Test {}

	impl dmp::Config for Test {}

	impl ump::Config for Test {
		type UmpSink = ();
	}

	impl hrmp::Config for Test {
		type Event = Event;
		type Origin = Origin;
		type Currency = pallet_balances::Module<Test>;
	}

	impl pallet_session::historical::Config for Test {
		type FullIdentification = pallet_staking::Exposure<u64, Balance>;
		type FullIdentificationOf = pallet_staking::ExposureOf<Self>;
	}

	// This is needed for a custom `AccountId` type which is `u64` in testing here.
	pub mod test_keys {
		use sp_core::crypto::KeyTypeId;

		pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"test");

		mod app {
			use super::super::Shared;
			use sp_application_crypto::{app_crypto, sr25519};

			app_crypto!(sr25519, super::KEY_TYPE);

			impl sp_runtime::traits::IdentifyAccount for Public {
				type AccountId = u64;

				fn into_account(self) -> Self::AccountId {
					let id = self.0.clone().into();
					Shared::active_validator_keys().iter().position(|b| *b == id).unwrap() as u64
				}
			}
		}

		pub type ReporterId = app::Public;
	}

	impl paras::Config for Test {
		type Origin = Origin;
	}

	impl configuration::Config for Test { }

	pub struct TestRewardValidators;

	impl inclusion::RewardValidators for TestRewardValidators {
		fn reward_backing(_: impl IntoIterator<Item = ValidatorIndex>) { }
		fn reward_bitfields(_: impl IntoIterator<Item = ValidatorIndex>) { }
	}

	impl inclusion::Config for Test {
		type Event = Event;
		type RewardValidators = TestRewardValidators;
	}

	impl session_info::AuthorityDiscoveryConfig for Test {
		fn authorities() -> Vec<AuthorityDiscoveryId> {
			Vec::new()
		}
	}

	impl session_info::Config for Test { }

	impl initializer::Config for Test {
		type Randomness = TestRandomness<Self>;
	}

	impl scheduler::Config for Test { }

	type Extrinsic = TestXt<Call, ()>;

	impl<LocalCall> frame_system::offchain::CreateSignedTransaction<LocalCall> for Test where
		Call: From<LocalCall>,
	{
		fn create_transaction<C: frame_system::offchain::AppCrypto<Self::Public, Self::Signature>>(
			call: Call,
			_public: test_keys::ReporterId,
			_account: <Test as frame_system::Config>::AccountId,
			nonce: <Test as frame_system::Config>::Index,
		) -> Option<(Call, <Extrinsic as ExtrinsicT>::SignaturePayload)> {
			Some((call, (nonce, ())))
		}
	}

	impl frame_system::offchain::SigningTypes for Test {
		type Public = test_keys::ReporterId;
		type Signature = Signature;
	}

	parameter_types! {
		pub const ParathreadDeposit: Balance = 10;
		pub const QueueSize: usize = 2;
		pub const MaxRetries: u32 = 3;
	}

	impl Config for Test {
		type Origin = Origin;
		type Currency = pallet_balances::Module<Test>;
		type ParathreadDeposit = ParathreadDeposit;
	}

	fn new_test_ext() -> TestExternalities {
		let mut t = frame_system::GenesisConfig::default().build_storage::<Test>().unwrap();

		let authority_keys = [
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
			Sr25519Keyring::Ferdie,
			Sr25519Keyring::One,
			Sr25519Keyring::Two,
		];

		let balances: Vec<_> = (0..authority_keys.len()).map(|i| (i as u64, 10_000_000)).collect();

		pallet_balances::GenesisConfig::<Test> {
			balances,
		}.assimilate_storage(&mut t).unwrap();

		// stashes are the index.
		let session_keys: Vec<_> = authority_keys.iter().enumerate()
			.map(|(i, _k)| (i as u64, i as u64, UintAuthorityId(i as u64)))
			.collect();

		pallet_session::GenesisConfig::<Test> {
			keys: session_keys,
		}.assimilate_storage(&mut t).unwrap();

		t.into()
	}

	fn run_to_block(n: BlockNumber) {
		// NOTE that this function only simulates modules of interest. Depending on new module may
		// require adding it here.
		println!("Running until block {}", n);
		while System::block_number() < n {
			let b = System::block_number();

			if System::block_number() > 1 {
				println!("Finalizing {}", System::block_number());
				System::on_finalize(System::block_number());
				Initializer::on_finalize(System::block_number());
			}
			// Session change every 3 blocks.
			if (b + 1) % Period::get() == 0 {
				println!("New session at {}", System::block_number());
				Initializer::on_new_session(
					false,
					Vec::new().into_iter(),
					Vec::new().into_iter(),
				);
			}
			System::set_block_number(b + 1);
			println!("Initializing {}", System::block_number());
			System::on_initialize(System::block_number());
			Session::on_initialize(System::block_number());
			Initializer::on_initialize(System::block_number());
		}
	}

	#[test]
	fn basic_setup_works() {
		new_test_ext().execute_with(|| {
			assert_eq!(PendingSwap::get(&ParaId::from(0u32)), None);
			assert_eq!(Paras::get(&ParaId::from(0u32)), None);
		});
	}

	#[test]
	fn register_deregister_chain_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Registrar::enable_parathread_registration(
				Origin::root(),
			));
			run_to_block(2);

			assert_ok!(Registrar::register_parachain(
				2u32.into(),
				vec![3; 3].into(),
				WASM_MAGIC.to_vec().into(),
			));

			let orig_bal = Balances::free_balance(&3u64);

			// Register a new parathread
			assert_ok!(Registrar::register_parathread(
				Origin::signed(3u64),
				8u32.into(),
				vec![3; 3].into(),
				WASM_MAGIC.to_vec().into(),
			));

			// deposit should be taken (reserved)
			assert_eq!(Balances::free_balance(3u64) + ParathreadDeposit::get(), orig_bal);
			assert_eq!(Balances::reserved_balance(3u64), ParathreadDeposit::get());

			run_to_block(10);

			assert_ok!(Registrar::deregister_parachain(2u32.into()));

			assert_ok!(Registrar::deregister_parathread(
				runtime_parachains::Origin::Parachain(8u32.into()).into()
			));

			// reserved balance should be returned.
			assert_eq!(Balances::free_balance(3u64), orig_bal);
			assert_eq!(Balances::reserved_balance(3u64), 0);
		});
	}

	#[test]
	fn swap_handles_funds_correctly() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Registrar::enable_parathread_registration(
				Origin::root(),
			));
			run_to_block(2);

			let initial_1_balance = Balances::free_balance(1);
			let initial_2_balance = Balances::free_balance(2);

			// User 1 register a new parathread
			assert_ok!(Registrar::register_parathread(
				Origin::signed(1),
				8u32.into(),
				vec![1; 3].into(),
				WASM_MAGIC.to_vec().into(),
			));

			assert_ok!(Registrar::register_parachain(
				2u32.into(),
				vec![1; 3].into(),
				WASM_MAGIC.to_vec().into(),
			));

			run_to_block(9);

			// Swap the parachain and parathread
			assert_ok!(Registrar::swap(runtime_parachains::Origin::Parachain(2u32.into()).into(), 8u32.into()));
			assert_ok!(Registrar::swap(runtime_parachains::Origin::Parachain(8u32.into()).into(), 2u32.into()));

			run_to_block(15);

			// Deregister a parathread that was originally a parachain
			assert_ok!(Registrar::deregister_parathread(runtime_parachains::Origin::Parachain(2u32.into()).into()));

			run_to_block(21);

			// Funds are correctly returned
			assert_eq!(Balances::free_balance(1), initial_1_balance);
			assert_eq!(Balances::free_balance(2), initial_2_balance);
		});
	}

	#[test]
	fn cannot_register_until_para_is_cleaned_up() {
		new_test_ext().execute_with(|| {
			run_to_block(2);

			assert_ok!(Registrar::register_parachain(
				1u32.into(),
				vec![1; 3].into(),
				WASM_MAGIC.to_vec().into(),
			));

			// 2 session changes to fully onboard.
			run_to_block(12);

			assert_eq!(Parachains::lifecycle(1u32.into()), Some(ParaLifecycle::Parachain));

			assert_ok!(Registrar::deregister_parachain(1u32.into()));
			run_to_block(13);

			assert_eq!(Parachains::lifecycle(1u32.into()), Some(ParaLifecycle::OffboardingParachain));

			assert_noop!(Registrar::register_parachain(
				1u32.into(),
				vec![1; 3].into(),
				WASM_MAGIC.to_vec().into(),
			), Error::<Test>::ParaAlreadyExists);

			// Need 2 session changes to see the effect, which takes place by block 13.
			run_to_block(18);

			assert!(Parachains::lifecycle(1u32.into()).is_none());
			assert_ok!(Registrar::register_parachain(
				1u32.into(),
				vec![1; 3].into(),
				WASM_MAGIC.to_vec().into(),
			));
		});
	}
}
