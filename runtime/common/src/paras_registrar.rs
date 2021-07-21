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

//! Pallet to handle parathread/parachain registration and related fund management.
//! In essence this is a simple wrapper around `paras`.

use sp_std::{prelude::*, result};
use frame_support::{
	ensure,
	dispatch::DispatchResult,
	traits::{Get, Currency, ReservableCurrency},
	pallet_prelude::Weight,
};
use frame_system::{self, ensure_root, ensure_signed};
use primitives::v1::{
	Id as ParaId, ValidationCode, HeadData, LOWEST_PUBLIC_ID,
};
use runtime_parachains::{
	paras::{
		self,
		ParaGenesisArgs,
	},
	configuration,
	ensure_parachain,
	Origin, ParaLifecycle,
};

use crate::traits::{Registrar, OnSwap};
use parity_scale_codec::{Encode, Decode};
use sp_runtime::{RuntimeDebug, traits::{Saturating, CheckedSub}};
pub use pallet::*;

#[derive(Encode, Decode, Clone, PartialEq, Eq, Default, RuntimeDebug)]
pub struct ParaInfo<Account, Balance> {
	/// The account that has placed a deposit for registering this para.
	pub(crate) manager: Account,
	/// The amount reserved by the `manager` account for the registration.
	deposit: Balance,
	/// Whether the para registration should be locked from being controlled by the manager.
	locked: bool,
}

type BalanceOf<T> =
	<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

pub trait WeightInfo {
	fn reserve() -> Weight;
	fn register() -> Weight;
	fn force_register() -> Weight;
	fn deregister() -> Weight;
	fn swap() -> Weight;
}

pub struct TestWeightInfo;
impl WeightInfo for TestWeightInfo {
	fn reserve() -> Weight { 0 }
	fn register() -> Weight { 0 }
	fn force_register() -> Weight { 0 }
	fn deregister() -> Weight { 0 }
	fn swap() -> Weight { 0 }
}

#[frame_support::pallet]
pub mod pallet {
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use super::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	#[pallet::disable_frame_system_supertrait_check]
	pub trait Config: configuration::Config + paras::Config {
		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// The aggregated origin type must support the `parachains` origin. We require that we can
		/// infallibly convert between this origin and the system origin, but in reality, they're the
		/// same type, we just can't express that to the Rust type system without writing a `where`
		/// clause everywhere.
		type Origin: From<<Self as frame_system::Config>::Origin>
			+ Into<result::Result<Origin, <Self as Config>::Origin>>;

		/// The system's currency for parathread payment.
		type Currency: ReservableCurrency<Self::AccountId>;

		/// Runtime hook for when a parachain and parathread swap.
		type OnSwap: crate::traits::OnSwap;

		/// The deposit to be paid to run a parathread.
		/// This should include the cost for storing the genesis head and validation code.
		#[pallet::constant]
		type ParaDeposit: Get<BalanceOf<Self>>;

		/// The deposit to be paid per byte stored on chain.
		#[pallet::constant]
		type DataDepositPerByte: Get<BalanceOf<Self>>;

		/// Weight Information for the Extrinsics in the Pallet
		type WeightInfo: WeightInfo;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	#[pallet::metadata(T::AccountId = "AccountId")]
	pub enum Event<T: Config> {
		Registered(ParaId, T::AccountId),
		Deregistered(ParaId),
		Reserved(ParaId, T::AccountId),
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The ID is not registered.
		NotRegistered,
		/// The ID is already registered.
		AlreadyRegistered,
		/// The caller is not the owner of this Id.
		NotOwner,
		/// Invalid para code size.
		CodeTooLarge,
		/// Invalid para head data size.
		HeadDataTooLarge,
		/// Para is not a Parachain.
		NotParachain,
		/// Para is not a Parathread.
		NotParathread,
		/// Cannot deregister para
		CannotDeregister,
		/// Cannot schedule downgrade of parachain to parathread
		CannotDowngrade,
		/// Cannot schedule upgrade of parathread to parachain
		CannotUpgrade,
		/// Para is locked from manipulation by the manager. Must use parachain or relay chain governance.
		ParaLocked,
		/// The ID given for registration has not been reserved.
		NotReserved,
	}

	/// Pending swap operations.
	#[pallet::storage]
	pub(super) type PendingSwap<T> = StorageMap<_, Twox64Concat, ParaId, ParaId>;

	/// Amount held on deposit for each para and the original depositor.
	///
	/// The given account ID is responsible for registering the code and initial head data, but may only do
	/// so if it isn't yet registered. (After that, it's up to governance to do so.)
	#[pallet::storage]
	pub type Paras<T: Config> = StorageMap<_, Twox64Concat, ParaId, ParaInfo<T::AccountId, BalanceOf<T>>>;

	/// The next free `ParaId`.
	#[pallet::storage]
	pub type NextFreeParaId<T> = StorageValue<_, ParaId, ValueQuery>;

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Register head data and validation code for a reserved Para Id.
		///
		/// ## Arguments
		/// - `origin`: Must be called by a `Signed` origin.
		/// - `id`: The para ID. Must be owned/managed by the `origin` signing account.
		/// - `genesis_head`: The genesis head data of the parachain/thread.
		/// - `validation_code`: The initial validation code of the parachain/thread.
		///
		/// ## Deposits/Fees
		/// The origin signed account must reserve a corresponding deposit for the registration. Anything already
		/// reserved previously for this para ID is accounted for.
		///
		/// ## Events
		/// The `Registered` event is emitted in case of success.
		#[pallet::weight(T::WeightInfo::register())]
		pub fn register(
			origin: OriginFor<T>,
			id: ParaId,
			genesis_head: HeadData,
			validation_code: ValidationCode,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_register(who, None, id, genesis_head, validation_code, true)?;
			Ok(())
		}

		/// Force the registration of a Para Id on the relay chain.
		///
		/// This function must be called by a Root origin.
		///
		/// The deposit taken can be specified for this registration. Any `ParaId`
		/// can be registered, including sub-1000 IDs which are System Parachains.
		#[pallet::weight(T::WeightInfo::force_register())]
		pub fn force_register(
			origin: OriginFor<T>,
			who: T::AccountId,
			deposit: BalanceOf<T>,
			id: ParaId,
			genesis_head: HeadData,
			validation_code: ValidationCode,
		) -> DispatchResult {
			ensure_root(origin)?;
			Self::do_register(who, Some(deposit), id, genesis_head, validation_code, false)
		}

		/// Deregister a Para Id, freeing all data and returning any deposit.
		///
		/// The caller must be Root, the `para` owner, or the `para` itself. The para must be a parathread.
		#[pallet::weight(T::WeightInfo::deregister())]
		pub fn deregister(origin: OriginFor<T>, id: ParaId) -> DispatchResult {
			Self::ensure_root_para_or_owner(origin, id)?;
			Self::do_deregister(id)
		}

		/// Swap a parachain with another parachain or parathread.
		///
		/// The origin must be Root, the `para` owner, or the `para` itself.
		///
		/// The swap will happen only if there is already an opposite swap pending. If there is not,
		/// the swap will be stored in the pending swaps map, ready for a later confirmatory swap.
		///
		/// The `ParaId`s remain mapped to the same head data and code so external code can rely on
		/// `ParaId` to be a long-term identifier of a notional "parachain". However, their
		/// scheduling info (i.e. whether they're a parathread or parachain), auction information
		/// and the auction deposit are switched.
		#[pallet::weight(T::WeightInfo::swap())]
		pub fn swap(origin: OriginFor<T>, id: ParaId, other: ParaId) -> DispatchResult {
			Self::ensure_root_para_or_owner(origin, id)?;

			if PendingSwap::<T>::get(other) == Some(id) {
				if let Some(other_lifecycle) = paras::Pallet::<T>::lifecycle(other) {
					if let Some(id_lifecycle) = paras::Pallet::<T>::lifecycle(id) {
						// identify which is a parachain and which is a parathread
						if id_lifecycle.is_parachain() && other_lifecycle.is_parathread() {
							// We check that both paras are in an appropriate lifecycle for a swap,
							// so these should never fail.
							let res1 = runtime_parachains::schedule_parachain_downgrade::<T>(id);
							debug_assert!(res1.is_ok());
							let res2 = runtime_parachains::schedule_parathread_upgrade::<T>(other);
							debug_assert!(res2.is_ok());
							T::OnSwap::on_swap(id, other);
						} else if id_lifecycle.is_parathread() && other_lifecycle.is_parachain() {
							// We check that both paras are in an appropriate lifecycle for a swap,
							// so these should never fail.
							let res1 = runtime_parachains::schedule_parachain_downgrade::<T>(other);
							debug_assert!(res1.is_ok());
							let res2 = runtime_parachains::schedule_parathread_upgrade::<T>(id);
							debug_assert!(res2.is_ok());
							T::OnSwap::on_swap(id, other);
						}

						PendingSwap::<T>::remove(other);
					}
				}
			} else {
				PendingSwap::<T>::insert(id, other);
			}

			Ok(())
		}

		/// Remove a manager lock from a para. This will allow the manager of a
		/// previously locked para to deregister or swap a para without using governance.
		///
		/// Can only be called by the Root origin.
		#[pallet::weight(T::DbWeight::get().reads_writes(1, 1))]
		pub fn force_remove_lock(origin: OriginFor<T>, para: ParaId) -> DispatchResult {
			ensure_root(origin)?;
			Self::remove_lock(para);
			Ok(())
		}

		/// Reserve a Para Id on the relay chain.
		///
		/// This function will reserve a new Para Id to be owned/managed by the origin account.
		/// The origin account is able to register head data and validation code using `register` to create
		/// a parathread. Using the Slots pallet, a parathread can then be upgraded to get a parachain slot.
		///
		/// ## Arguments
		/// - `origin`: Must be called by a `Signed` origin. Becomes the manager/owner of the new para ID.
		///
		/// ## Deposits/Fees
		/// The origin must reserve a deposit of `ParaDeposit` for the registration.
		///
		/// ## Events
		/// The `Reserved` event is emitted in case of success, which provides the ID reserved for use.
		#[pallet::weight(T::WeightInfo::reserve())]
		pub fn reserve(origin: OriginFor<T>) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let id = NextFreeParaId::<T>::get().max(LOWEST_PUBLIC_ID);
			Self::do_reserve(who, None, id)?;
			NextFreeParaId::<T>::set(id + 1);
			Ok(())
		}
	}
}

impl<T: Config> Registrar for Pallet<T> {
	type AccountId = T::AccountId;

	/// Return the manager `AccountId` of a para if one exists.
	fn manager_of(id: ParaId) -> Option<T::AccountId> {
		Some(Paras::<T>::get(id)?.manager)
	}

	// All parachains. Ordered ascending by ParaId. Parathreads are not included.
	fn parachains() -> Vec<ParaId> {
		paras::Pallet::<T>::parachains()
	}

	// Return if a para is a parathread
	fn is_parathread(id: ParaId) -> bool {
		paras::Pallet::<T>::is_parathread(id)
	}

	// Return if a para is a parachain
	fn is_parachain(id: ParaId) -> bool {
		paras::Pallet::<T>::is_parachain(id)
	}

	// Apply a lock to the parachain.
	fn apply_lock(id: ParaId) {
		Paras::<T>::mutate(id, |x| x.as_mut().map(|mut info| info.locked = true));
	}

	// Apply a lock to the parachain.
	fn remove_lock(id: ParaId) {
		Paras::<T>::mutate(id, |x| x.as_mut().map(|mut info| info.locked = false));
	}

	// Register a Para ID under control of `manager`.
	//
	// Note this is a backend registration API, so verification of ParaId
	// is not done here to prevent.
	fn register(
		manager: T::AccountId,
		id: ParaId,
		genesis_head: HeadData,
		validation_code: ValidationCode,
	) -> DispatchResult {
		Self::do_register(manager, None, id, genesis_head, validation_code, false)
	}

	// Deregister a Para ID, free any data, and return any deposits.
	fn deregister(id: ParaId) -> DispatchResult {
		Self::do_deregister(id)
	}

	// Upgrade a registered parathread into a parachain.
	fn make_parachain(id: ParaId) -> DispatchResult {
		// Para backend should think this is a parathread...
		ensure!(paras::Pallet::<T>::lifecycle(id) == Some(ParaLifecycle::Parathread), Error::<T>::NotParathread);
		runtime_parachains::schedule_parathread_upgrade::<T>(id).map_err(|_| Error::<T>::CannotUpgrade)?;
		// Once a para has upgraded to a parachain, it can no longer be managed by the owner.
		// Intentionally, the flag stays with the para even after downgrade.
		Self::apply_lock(id);
		Ok(())
	}

	// Downgrade a registered para into a parathread.
	fn make_parathread(id: ParaId) -> DispatchResult {
		// Para backend should think this is a parachain...
		ensure!(paras::Pallet::<T>::lifecycle(id) == Some(ParaLifecycle::Parachain), Error::<T>::NotParachain);
		runtime_parachains::schedule_parachain_downgrade::<T>(id).map_err(|_| Error::<T>::CannotDowngrade)?;
		Ok(())
	}

	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn worst_head_data() -> HeadData {
		let max_head_size = configuration::Pallet::<T>::config().max_head_data_size;
		assert!(max_head_size > 0, "max_head_data can't be zero for generating worst head data.");
		vec![0u8; max_head_size as usize].into()
	}

	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn worst_validation_code() -> ValidationCode {
		let max_code_size = configuration::Pallet::<T>::config().max_code_size;
		assert!(max_code_size > 0, "max_code_size can't be zero for generating worst code data.");
		let validation_code = vec![0u8; max_code_size as usize];
		validation_code.into()
	}

	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn execute_pending_transitions() {
		use runtime_parachains::shared;
		shared::Pallet::<T>::set_session_index(
			shared::Pallet::<T>::scheduled_session()
		);
		paras::Pallet::<T>::test_on_new_session();
	}
}

impl<T: Config> Pallet<T> {
	/// Ensure the origin is one of Root, the `para` owner, or the `para` itself.
	/// If the origin is the `para` owner, the `para` must be unlocked.
	fn ensure_root_para_or_owner(origin: <T as frame_system::Config>::Origin, id: ParaId) -> DispatchResult {
		ensure_signed(origin.clone()).map_err(|e| e.into())
		.and_then(|who| -> DispatchResult {
			let para_info = Paras::<T>::get(id).ok_or(Error::<T>::NotRegistered)?;
			ensure!(!para_info.locked, Error::<T>::ParaLocked);
			ensure!(para_info.manager == who, Error::<T>::NotOwner);
			Ok(())
		})
		.or_else(|_| -> DispatchResult {
			// Else check if para origin...
			let caller_id = ensure_parachain(<T as Config>::Origin::from(origin.clone()))?;
			ensure!(caller_id == id, Error::<T>::NotOwner);
			Ok(())
		}).or_else(|_| -> DispatchResult {
			// Check if root...
			ensure_root(origin.clone()).map_err(|e| e.into())
		})
	}

	fn do_reserve(
		who: T::AccountId,
		deposit_override: Option<BalanceOf<T>>,
		id: ParaId,
	) -> DispatchResult {
		ensure!(!Paras::<T>::contains_key(id), Error::<T>::AlreadyRegistered);
		ensure!(paras::Pallet::<T>::lifecycle(id).is_none(), Error::<T>::AlreadyRegistered);

		let deposit = deposit_override.unwrap_or_else(T::ParaDeposit::get);
		<T as Config>::Currency::reserve(&who, deposit)?;
		let info = ParaInfo {
			manager: who.clone(),
			deposit,
			locked: false,
		};

		Paras::<T>::insert(id, info);
		Self::deposit_event(Event::<T>::Reserved(id, who));
		Ok(())
	}

	/// Attempt to register a new Para Id under management of `who` in the
	/// system with the given information.
	fn do_register(
		who: T::AccountId,
		deposit_override: Option<BalanceOf<T>>,
		id: ParaId,
		genesis_head: HeadData,
		validation_code: ValidationCode,
		ensure_reserved: bool,
	) -> DispatchResult {
		let deposited = if let Some(para_data) = Paras::<T>::get(id) {
			ensure!(para_data.manager == who, Error::<T>::NotOwner);
			ensure!(!para_data.locked, Error::<T>::ParaLocked);
			para_data.deposit
		} else {
			ensure!(!ensure_reserved, Error::<T>::NotReserved);
			Default::default()
		};
		ensure!(paras::Pallet::<T>::lifecycle(id).is_none(), Error::<T>::AlreadyRegistered);
		let (genesis, deposit) = Self::validate_onboarding_data(
			genesis_head,
			validation_code,
			false
		)?;
		let deposit = deposit_override.unwrap_or(deposit);

		if let Some(additional) = deposit.checked_sub(&deposited) {
			<T as Config>::Currency::reserve(&who, additional)?;
		} else if let Some(rebate) = deposited.checked_sub(&deposit) {
			<T as Config>::Currency::unreserve(&who, rebate);
		};
		let info = ParaInfo {
			manager: who.clone(),
			deposit,
			locked: false,
		};

		Paras::<T>::insert(id, info);
		// We check above that para has no lifecycle, so this should not fail.
		let res = runtime_parachains::schedule_para_initialize::<T>(id, genesis);
		debug_assert!(res.is_ok());
		Self::deposit_event(Event::<T>::Registered(id, who));
		Ok(())
	}

	/// Deregister a Para Id, freeing all data returning any deposit.
	fn do_deregister(id: ParaId) -> DispatchResult {
		match paras::Pallet::<T>::lifecycle(id) {
			// Para must be a parathread, or not exist at all.
			Some(ParaLifecycle::Parathread) | None => {},
			_ => return Err(Error::<T>::NotParathread.into())
		}
		runtime_parachains::schedule_para_cleanup::<T>(id).map_err(|_| Error::<T>::CannotDeregister)?;

		if let Some(info) = Paras::<T>::take(&id) {
			<T as Config>::Currency::unreserve(&info.manager, info.deposit);
		}

		PendingSwap::<T>::remove(id);
		Self::deposit_event(Event::<T>::Deregistered(id));
		Ok(())
	}

	/// Verifies the onboarding data is valid for a para.
	///
	/// Returns `ParaGenesisArgs` and the deposit needed for the data.
	fn validate_onboarding_data(
		genesis_head: HeadData,
		validation_code: ValidationCode,
		parachain: bool,
	) -> Result<(ParaGenesisArgs, BalanceOf<T>), sp_runtime::DispatchError> {
		let config = configuration::Pallet::<T>::config();
		ensure!(validation_code.0.len() <= config.max_code_size as usize, Error::<T>::CodeTooLarge);
		ensure!(genesis_head.0.len() <= config.max_head_data_size as usize, Error::<T>::HeadDataTooLarge);

		let per_byte_fee = T::DataDepositPerByte::get();
		let deposit = T::ParaDeposit::get()
			.saturating_add(
				per_byte_fee.saturating_mul((genesis_head.0.len() as u32).into())
			).saturating_add(
				per_byte_fee.saturating_mul((validation_code.0.len() as u32).into())
			);

		Ok((ParaGenesisArgs {
			genesis_head,
			validation_code,
			parachain,
		}, deposit))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_io::TestExternalities;
	use sp_core::H256;
	use sp_runtime::{
		traits::{
			BlakeTwo256, IdentityLookup,
		}, Perbill,
	};
	use primitives::v1::{Balance, BlockNumber, Header};
	use frame_system::limits;
	use frame_support::{
		traits::{OnInitialize, OnFinalize},
		assert_ok, assert_noop, parameter_types,
		error::BadOrigin,
	};
	use runtime_parachains::{configuration, shared};
	use pallet_balances::Error as BalancesError;
	use crate::traits::Registrar as RegistrarTrait;
	use crate::paras_registrar;

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
			ParachainsConfiguration: configuration::{Pallet, Call, Storage, Config<T>},
			Parachains: paras::{Pallet, Origin, Call, Storage, Config, Event},
			ParasShared: shared::{Pallet, Call, Storage},
			Registrar: paras_registrar::{Pallet, Call, Storage, Event<T>},
		}
	);

	const NORMAL_RATIO: Perbill = Perbill::from_percent(75);
	parameter_types! {
		pub const BlockHashCount: u32 = 250;
		pub BlockWeights: limits::BlockWeights =
			frame_system::limits::BlockWeights::simple_max(1024);
		pub BlockLength: limits::BlockLength =
			limits::BlockLength::max_with_normal_ratio(4 * 1024 * 1024, NORMAL_RATIO);
	}

	impl frame_system::Config for Test {
		type BaseCallFilter = frame_support::traits::AllowAll;
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
		type OnSetCode = ();
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
		type MaxReserves = ();
		type ReserveIdentifier = [u8; 8];
		type WeightInfo = ();
	}

	impl shared::Config for Test {}

	impl paras::Config for Test {
		type Origin = Origin;
		type Event = Event;
	}

	impl configuration::Config for Test { }

	parameter_types! {
		pub const ParaDeposit: Balance = 10;
		pub const DataDepositPerByte: Balance = 1;
		pub const QueueSize: usize = 2;
		pub const MaxRetries: u32 = 3;
	}

	impl Config for Test {
		type Event = Event;
		type Origin = Origin;
		type Currency = Balances;
		type OnSwap = ();
		type ParaDeposit = ParaDeposit;
		type DataDepositPerByte = DataDepositPerByte;
		type WeightInfo = TestWeightInfo;
	}

	pub fn new_test_ext() -> TestExternalities {
		let mut t = frame_system::GenesisConfig::default().build_storage::<Test>().unwrap();

		configuration::GenesisConfig::<Test> {
			config: configuration::HostConfiguration {
				max_code_size: 2 * 1024 * 1024, // 2 MB
				max_head_data_size: 1 * 1024 * 1024, // 1 MB
				..Default::default()
			},
		}.assimilate_storage(&mut t).unwrap();

		pallet_balances::GenesisConfig::<Test> {
			balances: vec![(1, 10_000_000), (2, 10_000_000)],
		}.assimilate_storage(&mut t).unwrap();

		t.into()
	}

	const BLOCKS_PER_SESSION: u32 = 3;

	fn run_to_block(n: BlockNumber) {
		// NOTE that this function only simulates modules of interest. Depending on new pallet may
		// require adding it here.
		assert!(System::block_number() < n);
		while System::block_number() < n {
			let b = System::block_number();

			if System::block_number() > 1 {
				System::on_finalize(System::block_number());
			}
			// Session change every 3 blocks.
			if (b + 1) % BLOCKS_PER_SESSION == 0 {
				shared::Pallet::<Test>::set_session_index(
					shared::Pallet::<Test>::session_index() + 1
				);
				Parachains::test_on_new_session();
			}
			System::set_block_number(b + 1);
			System::on_initialize(System::block_number());
		}
	}

	fn run_to_session(n: BlockNumber) {
		let block_number = n * BLOCKS_PER_SESSION;
		run_to_block(block_number);
	}

	fn test_genesis_head(size: usize) -> HeadData {
		HeadData(vec![0u8; size])
	}

	fn test_validation_code(size: usize) -> ValidationCode {
		let validation_code = vec![0u8; size as usize];
		ValidationCode(validation_code)
	}

	fn para_origin(id: ParaId) -> Origin {
		runtime_parachains::Origin::Parachain(id).into()
	}

	fn max_code_size() -> u32 {
		ParachainsConfiguration::config().max_code_size
	}

	fn max_head_size() -> u32 {
		ParachainsConfiguration::config().max_head_data_size
	}

	#[test]
	fn basic_setup_works() {
		new_test_ext().execute_with(|| {
			assert_eq!(PendingSwap::<Test>::get(&ParaId::from(0u32)), None);
			assert_eq!(Paras::<Test>::get(&ParaId::from(0u32)), None);
		});
	}

	#[test]
	fn end_to_end_scenario_works() {
		new_test_ext().execute_with(|| {
			let para_id = LOWEST_PUBLIC_ID;
			run_to_block(1);
			// first para is not yet registered
			assert!(!Parachains::is_parathread(para_id));
			// We register the Para ID
			assert_ok!(Registrar::reserve(Origin::signed(1)));
			assert_ok!(Registrar::register(
				Origin::signed(1),
				para_id,
				test_genesis_head(32),
				test_validation_code(32),
			));
			run_to_session(2);
			// It is now a parathread.
			assert!(Parachains::is_parathread(para_id));
			assert!(!Parachains::is_parachain(para_id));
			// Some other external process will elevate parathread to parachain
			assert_ok!(Registrar::make_parachain(para_id));
			run_to_session(4);
			// It is now a parachain.
			assert!(!Parachains::is_parathread(para_id));
			assert!(Parachains::is_parachain(para_id));
			// Turn it back into a parathread
			assert_ok!(Registrar::make_parathread(para_id));
			run_to_session(6);
			assert!(Parachains::is_parathread(para_id));
			assert!(!Parachains::is_parachain(para_id));
			// Deregister it
			assert_ok!(Registrar::deregister(
				Origin::root(),
				para_id,
			));
			run_to_session(8);
			// It is nothing
			assert!(!Parachains::is_parathread(para_id));
			assert!(!Parachains::is_parachain(para_id));
		});
	}

	#[test]
	fn register_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			let para_id = LOWEST_PUBLIC_ID;
			assert!(!Parachains::is_parathread(para_id));
			assert_ok!(Registrar::reserve(Origin::signed(1)));
			assert_eq!(Balances::reserved_balance(&1), <Test as Config>::ParaDeposit::get());
			assert_ok!(Registrar::register(
				Origin::signed(1),
				para_id,
				test_genesis_head(32),
				test_validation_code(32),
			));
			run_to_session(2);
			assert!(Parachains::is_parathread(para_id));
			assert_eq!(
				Balances::reserved_balance(&1),
				<Test as Config>::ParaDeposit::get() + 64 * <Test as Config>::DataDepositPerByte::get()
			);
		});
	}

	#[test]
	fn register_handles_basic_errors() {
		new_test_ext().execute_with(|| {
			let para_id = LOWEST_PUBLIC_ID;

			assert_noop!(Registrar::register(
				Origin::signed(1),
				para_id,
				test_genesis_head(max_head_size() as usize),
				test_validation_code(max_code_size() as usize),
			), Error::<Test>::NotReserved);

			// Successfully register para
			assert_ok!(Registrar::reserve(Origin::signed(1)));

			assert_noop!(Registrar::register(
				Origin::signed(2),
				para_id,
				test_genesis_head(max_head_size() as usize),
				test_validation_code(max_code_size() as usize),
			), Error::<Test>::NotOwner);

			assert_ok!(Registrar::register(
				Origin::signed(1),
				para_id,
				test_genesis_head(max_head_size() as usize),
				test_validation_code(max_code_size() as usize),
			));

			run_to_session(2);

			assert_ok!(Registrar::deregister(Origin::root(), para_id));

			// Can't do it again
			assert_noop!(Registrar::register(
				Origin::signed(1),
				para_id,
				test_genesis_head(max_head_size() as usize),
				test_validation_code(max_code_size() as usize),
			), Error::<Test>::NotReserved);

			// Head Size Check
			assert_ok!(Registrar::reserve(Origin::signed(2)));
			assert_noop!(Registrar::register(
				Origin::signed(2),
				para_id + 1,
				test_genesis_head((max_head_size() + 1) as usize),
				test_validation_code(max_code_size() as usize),
			), Error::<Test>::HeadDataTooLarge);

			// Code Size Check
			assert_noop!(Registrar::register(
				Origin::signed(2),
				para_id + 1,
				test_genesis_head(max_head_size() as usize),
				test_validation_code((max_code_size() + 1) as usize),
			), Error::<Test>::CodeTooLarge);

			// Needs enough funds for deposit
			assert_noop!(Registrar::reserve(Origin::signed(1337)), BalancesError::<Test, _>::InsufficientBalance);
		});
	}

	#[test]
	fn deregister_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			let para_id = LOWEST_PUBLIC_ID;
			assert!(!Parachains::is_parathread(para_id));
			assert_ok!(Registrar::reserve(Origin::signed(1)));
			assert_ok!(Registrar::register(
				Origin::signed(1),
				para_id,
				test_genesis_head(32),
				test_validation_code(32),
			));
			run_to_session(2);
			assert!(Parachains::is_parathread(para_id));
			assert_ok!(Registrar::deregister(
				Origin::root(),
				para_id,
			));
			run_to_session(4);
			assert!(paras::Pallet::<Test>::lifecycle(para_id).is_none());
			assert_eq!(Balances::reserved_balance(&1), 0);
		});
	}

	#[test]
	fn deregister_handles_basic_errors() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			let para_id = LOWEST_PUBLIC_ID;
			assert!(!Parachains::is_parathread(para_id));
			assert_ok!(Registrar::reserve(Origin::signed(1)));
			assert_ok!(Registrar::register(
				Origin::signed(1),
				para_id,
				test_genesis_head(32),
				test_validation_code(32),
			));
			run_to_session(2);
			assert!(Parachains::is_parathread(para_id));
			// Owner check
			assert_noop!(Registrar::deregister(
				Origin::signed(2),
				para_id,
			), BadOrigin);
			assert_ok!(Registrar::make_parachain(para_id));
			run_to_session(4);
			// Cant directly deregister parachain
			assert_noop!(Registrar::deregister(
				Origin::root(),
				para_id,
			), Error::<Test>::NotParathread);
		});
	}

	#[test]
	fn swap_works() {
		new_test_ext().execute_with(|| {
			// Successfully register first two parachains
			let para_1 = LOWEST_PUBLIC_ID;
			let para_2 = LOWEST_PUBLIC_ID + 1;
			assert_ok!(Registrar::reserve(Origin::signed(1)));
			assert_ok!(Registrar::register(
				Origin::signed(1),
				para_1,
				test_genesis_head(max_head_size() as usize),
				test_validation_code(max_code_size() as usize),
			));
			assert_ok!(Registrar::reserve(Origin::signed(2)));
			assert_ok!(Registrar::register(
				Origin::signed(2),
				para_2,
				test_genesis_head(max_head_size() as usize),
				test_validation_code(max_code_size() as usize),
			));
			run_to_session(2);

			// Upgrade 1023 into a parachain
			assert_ok!(Registrar::make_parachain(para_1));

			run_to_session(4);

			// Roles are as we expect
			assert!(Parachains::is_parachain(para_1));
			assert!(!Parachains::is_parathread(para_1));
			assert!(!Parachains::is_parachain(para_2));
			assert!(Parachains::is_parathread(para_2));

			// Both paras initiate a swap
			assert_ok!(Registrar::swap(
				para_origin(para_1),
				para_1,
				para_2,
			));
			assert_ok!(Registrar::swap(
				para_origin(para_2),
				para_2,
				para_1,
			));

			run_to_session(6);

			// Deregister a parathread that was originally a parachain
			assert_eq!(Parachains::lifecycle(para_1), Some(ParaLifecycle::Parathread));
			assert_ok!(Registrar::deregister(runtime_parachains::Origin::Parachain(para_1).into(), para_1));

			run_to_block(21);

			// Roles are swapped
			assert!(!Parachains::is_parachain(para_1));
			assert!(Parachains::is_parathread(para_1));
			assert!(Parachains::is_parachain(para_2));
			assert!(!Parachains::is_parathread(para_2));
		});
	}

	#[test]
	fn para_lock_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Registrar::reserve(Origin::signed(1)));
			let para_id = LOWEST_PUBLIC_ID;
			assert_ok!(Registrar::register(
				Origin::signed(1),
				para_id,
				vec![1; 3].into(),
				vec![1, 2, 3].into(),
			));

			// Owner can call swap
			assert_ok!(Registrar::swap(Origin::signed(1), para_id, para_id + 1));

			// 2 session changes to fully onboard.
			run_to_session(2);
			assert_eq!(Parachains::lifecycle(para_id), Some(ParaLifecycle::Parathread));

			// Once they begin onboarding, we lock them in.
			assert_ok!(Registrar::make_parachain(para_id));

			// Owner cannot call swap anymore
			assert_noop!(Registrar::swap(Origin::signed(1), para_id, para_id + 2), BadOrigin);
		});
	}
}

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking {
	use super::{*, Pallet as Registrar};
	use frame_system::RawOrigin;
	use frame_support::assert_ok;
	use sp_runtime::traits::Bounded;
	use crate::traits::{Registrar as RegistrarT};
	use runtime_parachains::{paras, shared, Origin as ParaOrigin};

	use frame_benchmarking::{account, benchmarks, whitelisted_caller, impl_benchmark_test_suite};

	fn assert_last_event<T: Config>(generic_event: <T as Config>::Event) {
		let events = frame_system::Pallet::<T>::events();
		let system_event: <T as frame_system::Config>::Event = generic_event.into();
		// compare to the last event record
		let frame_system::EventRecord { event, .. } = &events[events.len() - 1];
		assert_eq!(event, &system_event);
	}

	fn register_para<T: Config>(id: u32) -> ParaId {
		let para = ParaId::from(id);
		let genesis_head = Registrar::<T>::worst_head_data();
		let validation_code = Registrar::<T>::worst_validation_code();
		let caller: T::AccountId = whitelisted_caller();
		T::Currency::make_free_balance_be(&caller, BalanceOf::<T>::max_value());
		assert_ok!(Registrar::<T>::reserve(RawOrigin::Signed(caller.clone()).into()));
		assert_ok!(Registrar::<T>::register(RawOrigin::Signed(caller).into(), para, genesis_head, validation_code));
		return para;
	}

	fn para_origin(id: u32) -> ParaOrigin {
		ParaOrigin::Parachain(id.into())
	}

	// This function moves forward to the next scheduled session for parachain lifecycle upgrades.
	fn next_scheduled_session<T: Config>() {
		shared::Pallet::<T>::set_session_index(
			shared::Pallet::<T>::scheduled_session()
		);
		paras::Pallet::<T>::test_on_new_session();
	}

	benchmarks! {
		where_clause { where ParaOrigin: Into<<T as frame_system::Config>::Origin> }

		reserve {
			let caller: T::AccountId = whitelisted_caller();
			T::Currency::make_free_balance_be(&caller, BalanceOf::<T>::max_value());
		}: _(RawOrigin::Signed(caller.clone()))
		verify {
			assert_last_event::<T>(Event::<T>::Reserved(LOWEST_PUBLIC_ID, caller).into());
			assert!(Paras::<T>::get(LOWEST_PUBLIC_ID).is_some());
			assert_eq!(paras::Pallet::<T>::lifecycle(LOWEST_PUBLIC_ID), None);
		}

		register {
			let para = LOWEST_PUBLIC_ID;
			let genesis_head = Registrar::<T>::worst_head_data();
			let validation_code = Registrar::<T>::worst_validation_code();
			let caller: T::AccountId = whitelisted_caller();
			T::Currency::make_free_balance_be(&caller, BalanceOf::<T>::max_value());
			assert_ok!(Registrar::<T>::reserve(RawOrigin::Signed(caller.clone()).into()));
		}: _(RawOrigin::Signed(caller.clone()), para, genesis_head, validation_code)
		verify {
			assert_last_event::<T>(Event::<T>::Registered(para, caller).into());
			assert_eq!(paras::Pallet::<T>::lifecycle(para), Some(ParaLifecycle::Onboarding));
			next_scheduled_session::<T>();
			assert_eq!(paras::Pallet::<T>::lifecycle(para), Some(ParaLifecycle::Parathread));
		}

		force_register {
			let manager: T::AccountId = account("manager", 0, 0);
			let deposit = 0u32.into();
			let para = ParaId::from(69);
			let genesis_head = Registrar::<T>::worst_head_data();
			let validation_code = Registrar::<T>::worst_validation_code();
		}: _(RawOrigin::Root, manager.clone(), deposit, para, genesis_head, validation_code)
		verify {
			assert_last_event::<T>(Event::<T>::Registered(para, manager).into());
			assert_eq!(paras::Pallet::<T>::lifecycle(para), Some(ParaLifecycle::Onboarding));
			next_scheduled_session::<T>();
			assert_eq!(paras::Pallet::<T>::lifecycle(para), Some(ParaLifecycle::Parathread));
		}

		deregister {
			let para = register_para::<T>(LOWEST_PUBLIC_ID.into());
			next_scheduled_session::<T>();
			let caller: T::AccountId = whitelisted_caller();
		}: _(RawOrigin::Signed(caller), para)
		verify {
			assert_last_event::<T>(Event::<T>::Deregistered(para).into());
		}

		swap {
			let parathread = register_para::<T>(LOWEST_PUBLIC_ID.into());
			let parachain = register_para::<T>((LOWEST_PUBLIC_ID + 1).into());

			let parachain_origin = para_origin(parachain.into());

			// Actually finish registration process
			next_scheduled_session::<T>();

			// Upgrade the parachain
			Registrar::<T>::make_parachain(parachain)?;
			next_scheduled_session::<T>();

			assert_eq!(paras::Pallet::<T>::lifecycle(parachain), Some(ParaLifecycle::Parachain));
			assert_eq!(paras::Pallet::<T>::lifecycle(parathread), Some(ParaLifecycle::Parathread));

			let caller: T::AccountId = whitelisted_caller();
			Registrar::<T>::swap(parachain_origin.into(), parachain, parathread)?;
		}: _(RawOrigin::Signed(caller.clone()), parathread, parachain)
		verify {
			next_scheduled_session::<T>();
			// Swapped!
			assert_eq!(paras::Pallet::<T>::lifecycle(parachain), Some(ParaLifecycle::Parathread));
			assert_eq!(paras::Pallet::<T>::lifecycle(parathread), Some(ParaLifecycle::Parachain));
		}
	}

	impl_benchmark_test_suite!(
		Registrar,
		crate::integration_tests::new_test_ext(),
		crate::integration_tests::Test,
	);
}
