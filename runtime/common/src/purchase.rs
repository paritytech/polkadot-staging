// Copyright 2017-2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Pallet to process purchase of DOTs.

use parity_scale_codec::{Encode, Decode};
use sp_runtime::{Permill, RuntimeDebug, DispatchResult, DispatchError, AnySignature};
use sp_runtime::traits::{Zero, CheckedAdd, Verify, Saturating};
use frame_support::pallet_prelude::*;
use frame_support::traits::{
	EnsureOrigin, Currency, ExistenceRequirement, VestingSchedule, Get
};
use frame_system::pallet_prelude::*;
use sp_core::sr25519;
use sp_std::prelude::*;
pub use pallet::*;

type BalanceOf<T> = <<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

/// The kind of a statement an account needs to make for a claim to be valid.
#[derive(Encode, Decode, Clone, Copy, Eq, PartialEq, RuntimeDebug)]
pub enum AccountValidity {
	/// Account is not valid.
	Invalid,
	/// Account has initiated the account creation process.
	Initiated,
	/// Account is pending validation.
	Pending,
	/// Account is valid with a low contribution amount.
	ValidLow,
	/// Account is valid with a high contribution amount.
	ValidHigh,
	/// Account has completed the purchase process.
	Completed,
}

impl Default for AccountValidity {
	fn default() -> Self {
		AccountValidity::Invalid
	}
}

impl AccountValidity {
	fn is_valid(&self) -> bool {
		match self {
			Self::Invalid => false,
			Self::Initiated => false,
			Self::Pending => false,
			Self::ValidLow => true,
			Self::ValidHigh => true,
			Self::Completed => false,
		}
	}
}

/// All information about an account regarding the purchase of DOTs.
#[derive(Encode, Decode, Default, Clone, Eq, PartialEq, RuntimeDebug)]
pub struct AccountStatus<Balance> {
	/// The current validity status of the user. Will denote if the user has passed KYC,
	/// how much they are able to purchase, and when their purchase process has completed.
	validity: AccountValidity,
	/// The amount of free DOTs they have purchased.
	free_balance: Balance,
	/// The amount of locked DOTs they have purchased.
	locked_balance: Balance,
	/// Their sr25519/ed25519 signature verifying they have signed our required statement.
	signature: Vec<u8>,
	/// The percentage of VAT the purchaser is responsible for. This is already factored into account balance.
	vat: Permill,
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// Balances Pallet
		type Currency: Currency<Self::AccountId>;

		/// Vesting Pallet
		type VestingSchedule: VestingSchedule<Self::AccountId, Moment=Self::BlockNumber, Currency=Self::Currency>;

		/// The origin allowed to set account status.
		type ValidityOrigin: EnsureOrigin<Self::Origin>;

		/// The origin allowed to make configurations to the pallet.
		type ConfigurationOrigin: EnsureOrigin<Self::Origin>;

		/// The maximum statement length for the statement users to sign when creating an account.
		#[pallet::constant]
		type MaxStatementLength: Get<u32>;

		/// The amount of purchased locked DOTs that we will unlock for basic actions on the chain.
		#[pallet::constant]
		type UnlockedProportion: Get<Permill>;

		/// The maximum amount of locked DOTs that we will unlock.
		#[pallet::constant]
		type MaxUnlocked: Get<BalanceOf<Self>>;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	#[pallet::metadata(
		T::AccountId = "AccountId",
		T::BlockNumber = "BlockNumber",
		BalanceOf<T> = "Balance",
	)]
	pub enum Event<T: Config> {
		/// A [new] account was created.
		AccountCreated(T::AccountId),
		/// Someone's account validity was updated. [who, validity]
		ValidityUpdated(T::AccountId, AccountValidity),
		/// Someone's purchase balance was updated. [who, free, locked]
		BalanceUpdated(T::AccountId, BalanceOf<T>, BalanceOf<T>),
		/// A payout was made to a purchaser. [who, free, locked]
		PaymentComplete(T::AccountId, BalanceOf<T>, BalanceOf<T>),
		/// A new payment account was set. [who]
		PaymentAccountSet(T::AccountId),
		/// A new statement was set.
		StatementUpdated,
		/// A new statement was set. [block_number]
		UnlockBlockUpdated(T::BlockNumber),
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Account is not currently valid to use.
		InvalidAccount,
		/// Account used in the purchase already exists.
		ExistingAccount,
		/// Provided signature is invalid
		InvalidSignature,
		/// Account has already completed the purchase process.
		AlreadyCompleted,
		/// An overflow occurred when doing calculations.
		Overflow,
		/// The statement is too long to be stored on chain.
		InvalidStatement,
		/// The unlock block is in the past!
		InvalidUnlockBlock,
		/// Vesting schedule already exists for this account.
		VestingScheduleExists,
	}

	// A map of all participants in the DOT purchase process.
	#[pallet::storage]
	pub(super) type Accounts<T: Config> = StorageMap<
		_,
		Blake2_128Concat, T::AccountId,
		AccountStatus<BalanceOf<T>>,
		ValueQuery,
	>;

	// The account that will be used to payout participants of the DOT purchase process.
	#[pallet::storage]
	pub(super) type PaymentAccount<T: Config> = StorageValue<_, T::AccountId, ValueQuery>;

	// The statement purchasers will need to sign to participate.
	#[pallet::storage]
	pub(super) type Statement<T> = StorageValue<_, Vec<u8>, ValueQuery>;

	// The block where all locked dots will unlock.
	#[pallet::storage]
	pub(super) type UnlockBlock<T: Config> = StorageValue<_, T::BlockNumber, ValueQuery>;

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Create a new account. Proof of existence through a valid signed message.
		///
		/// We check that the account does not exist at this stage.
		///
		/// Origin must match the `ValidityOrigin`.
		#[pallet::weight(200_000_000 + T::DbWeight::get().reads_writes(4, 1))]
		pub(super) fn create_account(
			origin: OriginFor<T>,
			who: T::AccountId,
			signature: Vec<u8>
		) -> DispatchResult {
			T::ValidityOrigin::ensure_origin(origin)?;
			// Account is already being tracked by the pallet.
			ensure!(!Accounts::<T>::contains_key(&who), Error::<T>::ExistingAccount);
			// Account should not have a vesting schedule.
			ensure!(T::VestingSchedule::vesting_balance(&who).is_none(), Error::<T>::VestingScheduleExists);

			// Verify the signature provided is valid for the statement.
			Self::verify_signature(&who, &signature)?;

			// Create a new pending account.
			let status = AccountStatus {
				validity: AccountValidity::Initiated,
				signature,
				free_balance: Zero::zero(),
				locked_balance: Zero::zero(),
				vat: Permill::zero(),
			};
			Accounts::<T>::insert(&who, status);
			Self::deposit_event(Event::<T>::AccountCreated(who));
			Ok(())
		}

		/// Update the validity status of an existing account. If set to completed, the account
		/// will no longer be able to continue through the crowdfund process.
		///
		/// We check tht the account exists at this stage, but has not completed the process.
		///
		/// Origin must match the `ValidityOrigin`.
		#[pallet::weight(T::DbWeight::get().reads_writes(1, 1))]
		pub(super) fn update_validity_status(
			origin: OriginFor<T>,
			who: T::AccountId,
			validity: AccountValidity
		) -> DispatchResult {
			T::ValidityOrigin::ensure_origin(origin)?;
			ensure!(Accounts::<T>::contains_key(&who), Error::<T>::InvalidAccount);
			Accounts::<T>::try_mutate(&who, |status: &mut AccountStatus<BalanceOf<T>>| -> DispatchResult {
				ensure!(status.validity != AccountValidity::Completed, Error::<T>::AlreadyCompleted);
				status.validity = validity;
				Ok(())
			})?;
			Self::deposit_event(Event::<T>::ValidityUpdated(who, validity));
			Ok(())
		}

		/// Update the balance of a valid account.
		///
		/// We check tht the account is valid for a balance transfer at this point.
		///
		/// Origin must match the `ValidityOrigin`.
		#[pallet::weight(T::DbWeight::get().reads_writes(2, 1))]
		pub(super) fn update_balance(
			origin: OriginFor<T>,
			who: T::AccountId,
			free_balance: BalanceOf<T>,
			locked_balance: BalanceOf<T>,
			vat: Permill,
		) -> DispatchResult {
			T::ValidityOrigin::ensure_origin(origin)?;

			Accounts::<T>::try_mutate(&who, |status: &mut AccountStatus<BalanceOf<T>>| -> DispatchResult {
				// Account has a valid status (not Invalid, Pending, or Completed)...
				ensure!(status.validity.is_valid(), Error::<T>::InvalidAccount);

				free_balance.checked_add(&locked_balance).ok_or(Error::<T>::Overflow)?;
				status.free_balance = free_balance;
				status.locked_balance = locked_balance;
				status.vat = vat;
				Ok(())
			})?;
			Self::deposit_event(Event::<T>::BalanceUpdated(who, free_balance, locked_balance));
			Ok(())
		}

		/// Pay the user and complete the purchase process.
		///
		/// We reverify all assumptions about the state of an account, and complete the process.
		///
		/// Origin must match the configured `PaymentAccount`.
		#[pallet::weight(T::DbWeight::get().reads_writes(4, 2))]
		pub(super) fn payout(origin: OriginFor<T>, who: T::AccountId) -> DispatchResult {
			// Payments must be made directly by the `PaymentAccount`.
			let payment_account = ensure_signed(origin)?;
			ensure!(payment_account == PaymentAccount::<T>::get(), DispatchError::BadOrigin);

			// Account should not have a vesting schedule.
			ensure!(T::VestingSchedule::vesting_balance(&who).is_none(), Error::<T>::VestingScheduleExists);

			Accounts::<T>::try_mutate(&who, |status: &mut AccountStatus<BalanceOf<T>>| -> DispatchResult {
				// Account has a valid status (not Invalid, Pending, or Completed)...
				ensure!(status.validity.is_valid(), Error::<T>::InvalidAccount);

				// Transfer funds from the payment account into the purchasing user.
				let total_balance = status.free_balance
					.checked_add(&status.locked_balance)
					.ok_or(Error::<T>::Overflow)?;
				T::Currency::transfer(&payment_account, &who, total_balance, ExistenceRequirement::AllowDeath)?;

				if !status.locked_balance.is_zero() {
					let unlock_block = UnlockBlock::<T>::get();
					// We allow some configurable portion of the purchased locked DOTs to be unlocked for basic usage.
					let unlocked = (T::UnlockedProportion::get() * status.locked_balance).min(T::MaxUnlocked::get());
					let locked = status.locked_balance.saturating_sub(unlocked);
					// We checked that this account has no existing vesting schedule. So this function should
					// never fail, however if it does, not much we can do about it at this point.
					let _ = T::VestingSchedule::add_vesting_schedule(
						// Apply vesting schedule to this user
						&who,
						// For this much amount
						locked,
						// Unlocking the full amount after one block
						locked,
						// When everything unlocks
						unlock_block
					);
				}

				// Setting the user account to `Completed` ends the purchase process for this user.
				status.validity = AccountValidity::Completed;
				Self::deposit_event(
					Event::<T>::PaymentComplete(who.clone(), status.free_balance, status.locked_balance)
				);
				Ok(())
			})?;
			Ok(())
		}

		/* Configuration Operations */

		/// Set the account that will be used to payout users in the DOT purchase process.
		///
		/// Origin must match the `ConfigurationOrigin`
		#[pallet::weight(T::DbWeight::get().writes(1))]
		pub(super) fn set_payment_account(origin: OriginFor<T>, who: T::AccountId) -> DispatchResult {
			T::ConfigurationOrigin::ensure_origin(origin)?;
			// Possibly this is worse than having the caller account be the payment account?
			PaymentAccount::<T>::set(who.clone());
			Self::deposit_event(Event::<T>::PaymentAccountSet(who));
			Ok(())
		}

		/// Set the statement that must be signed for a user to participate on the DOT sale.
		///
		/// Origin must match the `ConfigurationOrigin`
		#[pallet::weight(T::DbWeight::get().writes(1))]
		pub(super) fn set_statement(origin: OriginFor<T>, statement: Vec<u8>) -> DispatchResult {
			T::ConfigurationOrigin::ensure_origin(origin)?;
			ensure!((statement.len() as u32) < T::MaxStatementLength::get(), Error::<T>::InvalidStatement);
			// Possibly this is worse than having the caller account be the payment account?
			Statement::<T>::set(statement);
			Self::deposit_event(Event::<T>::StatementUpdated);
			Ok(())
		}

		/// Set the block where locked DOTs will become unlocked.
		///
		/// Origin must match the `ConfigurationOrigin`
		#[pallet::weight(T::DbWeight::get().writes(1))]
		pub(super) fn set_unlock_block(origin: OriginFor<T>, unlock_block: T::BlockNumber) -> DispatchResult {
			T::ConfigurationOrigin::ensure_origin(origin)?;
			ensure!(unlock_block > frame_system::Pallet::<T>::block_number(), Error::<T>::InvalidUnlockBlock);
			// Possibly this is worse than having the caller account be the payment account?
			UnlockBlock::<T>::set(unlock_block);
			Self::deposit_event(Event::<T>::UnlockBlockUpdated(unlock_block));
			Ok(())
		}
	}
}

impl<T: Config> Pallet<T> {
	fn verify_signature(who: &T::AccountId, signature: &[u8]) -> Result<(), DispatchError> {
		// sr25519 always expects a 64 byte signature.
		ensure!(signature.len() == 64, Error::<T>::InvalidSignature);
		let signature: AnySignature = sr25519::Signature::from_slice(signature).into();

		// In Polkadot, the AccountId is always the same as the 32 byte public key.
		let account_bytes: [u8; 32] = account_to_bytes(who)?;
		let public_key = sr25519::Public::from_raw(account_bytes);

		let message = Statement::<T>::get();

		// Check if everything is good or not.
		match signature.verify(message.as_slice(), &public_key) {
			true => Ok(()),
			false => Err(Error::<T>::InvalidSignature)?,
		}
	}
}

// This function converts a 32 byte AccountId to its byte-array equivalent form.
fn account_to_bytes<AccountId>(account: &AccountId) -> Result<[u8; 32], DispatchError>
	where AccountId: Encode,
{
	let account_vec = account.encode();
	ensure!(account_vec.len() == 32, "AccountId must be 32 bytes.");
	let mut bytes = [0u8; 32];
	bytes.copy_from_slice(&account_vec);
	Ok(bytes)
}

/// WARNING: Executing this function will clear all storage used by this pallet.
/// Be sure this is what you want...
pub fn remove_pallet<T>() -> frame_support::weights::Weight
	where T: frame_system::Config
{
	use frame_support::migration::remove_storage_prefix;
	remove_storage_prefix(b"Purchase", b"Accounts", b"");
	remove_storage_prefix(b"Purchase", b"PaymentAccount", b"");
	remove_storage_prefix(b"Purchase", b"Statement", b"");
	remove_storage_prefix(b"Purchase", b"UnlockBlock", b"");

	<T as frame_system::Config>::BlockWeights::get().max_block
}

#[cfg(test)]
mod tests {
	use super::*;

	use sp_core::{H256, Pair, Public, crypto::AccountId32, ed25519};
	// The testing primitives are very useful for avoiding having to work with signatures
	// or public keys. `u64` is used as the `AccountId` and no `Signature`s are required.
	use sp_runtime::{
		MultiSignature,
		traits::{BlakeTwo256, IdentityLookup, Identity, Verify, IdentifyAccount, Dispatchable},
		testing::Header
	};
	use frame_support::{
		assert_ok, assert_noop, parameter_types,
		ord_parameter_types, dispatch::DispatchError::BadOrigin,
	};
	use frame_support::traits::Currency;
	use pallet_balances::Error as BalancesError;
	use crate::purchase;

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
			Vesting: pallet_vesting::{Pallet, Call, Storage, Config<T>, Event<T>},
			Purchase: purchase::{Pallet, Call, Storage, Event<T>},
		}
	);

	type AccountId = AccountId32;

	parameter_types! {
		pub const BlockHashCount: u32 = 250;
	}
	impl frame_system::Config for Test {
		type BaseCallFilter = ();
		type BlockWeights = ();
		type BlockLength = ();
		type DbWeight = ();
		type Origin = Origin;
		type Call = Call;
		type Index = u64;
		type BlockNumber = u64;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type AccountId = AccountId;
		type Lookup = IdentityLookup<AccountId>;
		type Header = Header;
		type Event = Event;
		type BlockHashCount = BlockHashCount;
		type Version = ();
		type PalletInfo = PalletInfo;
		type AccountData = pallet_balances::AccountData<u64>;
		type OnNewAccount = ();
		type OnKilledAccount = ();
		type SystemWeightInfo = ();
		type SS58Prefix = ();
		type OnSetCode = ();
	}

	parameter_types! {
		pub const ExistentialDeposit: u64 = 1;
		pub const MaxLocks: u32 = 10;
	}

	impl pallet_balances::Config for Test {
		type Balance = u64;
		type Event = Event;
		type DustRemoval = ();
		type ExistentialDeposit = ExistentialDeposit;
		type AccountStore = System;
		type MaxLocks = MaxLocks;
		type WeightInfo = ();
	}

	parameter_types! {
		pub const MinVestedTransfer: u64 = 0;
	}

	impl pallet_vesting::Config for Test {
		type Event = Event;
		type Currency = Balances;
		type BlockNumberToBalance = Identity;
		type MinVestedTransfer = MinVestedTransfer;
		type WeightInfo = ();
	}

	parameter_types! {
		pub const MaxStatementLength: u32 =  1_000;
		pub const UnlockedProportion: Permill = Permill::from_percent(10);
		pub const MaxUnlocked: u64 = 10;
	}

	ord_parameter_types! {
		pub const ValidityOrigin: AccountId = AccountId32::from([0u8; 32]);
		pub const PaymentOrigin: AccountId = AccountId32::from([1u8; 32]);
		pub const ConfigurationOrigin: AccountId = AccountId32::from([2u8; 32]);
	}

	impl Config for Test {
		type Event = Event;
		type Currency = Balances;
		type VestingSchedule = Vesting;
		type ValidityOrigin = frame_system::EnsureSignedBy<ValidityOrigin, AccountId>;
		type ConfigurationOrigin = frame_system::EnsureSignedBy<ConfigurationOrigin, AccountId>;
		type MaxStatementLength = MaxStatementLength;
		type UnlockedProportion = UnlockedProportion;
		type MaxUnlocked = MaxUnlocked;
	}

	// This function basically just builds a genesis storage key/value store according to
	// our desired mockup. It also executes our `setup` function which sets up this pallet for use.
	pub fn new_test_ext() -> sp_io::TestExternalities {
		let t = frame_system::GenesisConfig::default().build_storage::<Test>().unwrap();
		let mut ext = sp_io::TestExternalities::new(t);
		ext.execute_with(|| setup());
		ext
	}

	fn setup() {
		let statement = b"Hello, World".to_vec();
		let unlock_block = 100;
		Purchase::set_statement(Origin::signed(configuration_origin()), statement).unwrap();
		Purchase::set_unlock_block(Origin::signed(configuration_origin()), unlock_block).unwrap();
		Purchase::set_payment_account(Origin::signed(configuration_origin()), payment_account()).unwrap();
		Balances::make_free_balance_be(&payment_account(), 100_000);
	}

	type AccountPublic = <MultiSignature as Verify>::Signer;

	/// Helper function to generate a crypto pair from seed
	fn get_from_seed<TPublic: Public>(seed: &str) -> <TPublic::Pair as Pair>::Public {
		TPublic::Pair::from_string(&format!("//{}", seed), None)
			.expect("static values are valid; qed")
			.public()
	}

	/// Helper function to generate an account ID from seed
	fn get_account_id_from_seed<TPublic: Public>(seed: &str) -> AccountId where
		AccountPublic: From<<TPublic::Pair as Pair>::Public>
	{
		AccountPublic::from(get_from_seed::<TPublic>(seed)).into_account()
	}

	fn alice() -> AccountId {
		get_account_id_from_seed::<sr25519::Public>("Alice")
	}

	fn alice_ed25519() -> AccountId {
		get_account_id_from_seed::<ed25519::Public>("Alice")
	}

	fn bob() -> AccountId {
		get_account_id_from_seed::<sr25519::Public>("Bob")
	}

	fn alice_signature() -> [u8; 64] {
		// echo -n "Hello, World" | subkey -s sign "bottom drive obey lake curtain smoke basket hold race lonely fit walk//Alice"
		hex_literal::hex!("20e0faffdf4dfe939f2faa560f73b1d01cde8472e2b690b7b40606a374244c3a2e9eb9c8107c10b605138374003af8819bd4387d7c24a66ee9253c2e688ab881")
	}

	fn bob_signature() -> [u8; 64] {
		// echo -n "Hello, World" | subkey -s sign "bottom drive obey lake curtain smoke basket hold race lonely fit walk//Bob"
		hex_literal::hex!("d6d460187ecf530f3ec2d6e3ac91b9d083c8fbd8f1112d92a82e4d84df552d18d338e6da8944eba6e84afaacf8a9850f54e7b53a84530d649be2e0119c7ce889")
	}

	fn alice_signature_ed25519() -> [u8; 64] {
		// echo -n "Hello, World" | subkey -e sign "bottom drive obey lake curtain smoke basket hold race lonely fit walk//Alice"
		hex_literal::hex!("ee3f5a6cbfc12a8f00c18b811dc921b550ddf272354cda4b9a57b1d06213fcd8509f5af18425d39a279d13622f14806c3e978e2163981f2ec1c06e9628460b0e")
	}

	fn validity_origin() -> AccountId {
		ValidityOrigin::get()
	}

	fn configuration_origin() -> AccountId {
		ConfigurationOrigin::get()
	}

	fn payment_account() -> AccountId {
		[42u8; 32].into()
	}

	#[test]
	fn set_statement_works_and_handles_basic_errors() {
		new_test_ext().execute_with(|| {
			let statement = b"Test Set Statement".to_vec();
			// Invalid origin
			assert_noop!(
				Purchase::set_statement(Origin::signed(alice()), statement.clone()),
				BadOrigin,
			);
			// Too Long
			let long_statement = [0u8; 10_000].to_vec();
			assert_noop!(
				Purchase::set_statement(Origin::signed(configuration_origin()), long_statement),
				Error::<Test>::InvalidStatement,
			);
			// Just right...
			assert_ok!(Purchase::set_statement(Origin::signed(configuration_origin()), statement.clone()));
			assert_eq!(Statement::<Test>::get(), statement);
		});
	}

	#[test]
	fn set_unlock_block_works_and_handles_basic_errors() {
		new_test_ext().execute_with(|| {
			let unlock_block = 69;
			// Invalid origin
			assert_noop!(
				Purchase::set_unlock_block(Origin::signed(alice()), unlock_block),
				BadOrigin,
			);
			// Block Number in Past
			let bad_unlock_block = 50;
			System::set_block_number(bad_unlock_block);
			assert_noop!(
				Purchase::set_unlock_block(Origin::signed(configuration_origin()), bad_unlock_block),
				Error::<Test>::InvalidUnlockBlock,
			);
			// Just right...
			assert_ok!(Purchase::set_unlock_block(Origin::signed(configuration_origin()), unlock_block));
			assert_eq!(UnlockBlock::<Test>::get(), unlock_block);
		});
	}

	#[test]
	fn set_payment_account_works_and_handles_basic_errors() {
		new_test_ext().execute_with(|| {
			let payment_account: AccountId = [69u8; 32].into();
			// Invalid Origin
			assert_noop!(
				Purchase::set_payment_account(Origin::signed(alice()), payment_account.clone()),
				BadOrigin,
			);
			// Just right...
			assert_ok!(Purchase::set_payment_account(Origin::signed(configuration_origin()), payment_account.clone()));
			assert_eq!(PaymentAccount::<Test>::get(), payment_account);
		});
	}

	#[test]
	fn signature_verification_works() {
		new_test_ext().execute_with(|| {
			assert_ok!(Purchase::verify_signature(&alice(), &alice_signature()));
			assert_ok!(Purchase::verify_signature(&alice_ed25519(), &alice_signature_ed25519()));
			assert_ok!(Purchase::verify_signature(&bob(), &bob_signature()));

			// Mixing and matching fails
			assert_noop!(Purchase::verify_signature(&alice(), &bob_signature()), Error::<Test>::InvalidSignature);
			assert_noop!(Purchase::verify_signature(&bob(), &alice_signature()), Error::<Test>::InvalidSignature);
		});
	}

	#[test]
	fn account_creation_works() {
		new_test_ext().execute_with(|| {
			assert!(!Accounts::<Test>::contains_key(alice()));
			assert_ok!(Purchase::create_account(
				Origin::signed(validity_origin()),
				alice(),
				alice_signature().to_vec(),
			));
			assert_eq!(
				Accounts::<Test>::get(alice()),
				AccountStatus {
					validity: AccountValidity::Initiated,
					free_balance: Zero::zero(),
					locked_balance: Zero::zero(),
					signature: alice_signature().to_vec(),
					vat: Permill::zero(),
				}
			);
		});
	}

	#[test]
	fn account_creation_handles_basic_errors() {
		new_test_ext().execute_with(|| {
			// Wrong Origin
			assert_noop!(
				Purchase::create_account(Origin::signed(alice()), alice(), alice_signature().to_vec()),
				BadOrigin,
			);

			// Wrong Account/Signature
			assert_noop!(
				Purchase::create_account(Origin::signed(validity_origin()), alice(), bob_signature().to_vec()),
				Error::<Test>::InvalidSignature,
			);

			// Account with vesting
			assert_ok!(<Test as Config>::VestingSchedule::add_vesting_schedule(
				&alice(),
				100,
				1,
				50
			));
			assert_noop!(
				Purchase::create_account(Origin::signed(validity_origin()), alice(), alice_signature().to_vec()),
				Error::<Test>::VestingScheduleExists,
			);

			// Duplicate Purchasing Account
			assert_ok!(
				Purchase::create_account(Origin::signed(validity_origin()), bob(), bob_signature().to_vec())
			);
			assert_noop!(
				Purchase::create_account(Origin::signed(validity_origin()), bob(), bob_signature().to_vec()),
				Error::<Test>::ExistingAccount,
			);
		});
	}

	#[test]
	fn update_validity_status_works() {
		new_test_ext().execute_with(|| {
			// Alice account is created.
			assert_ok!(Purchase::create_account(
				Origin::signed(validity_origin()),
				alice(),
				alice_signature().to_vec(),
			));
			// She submits KYC, and we update the status to `Pending`.
			assert_ok!(Purchase::update_validity_status(
				Origin::signed(validity_origin()),
				alice(),
				AccountValidity::Pending,
			));
			// KYC comes back negative, so we mark the account invalid.
			assert_ok!(Purchase::update_validity_status(
				Origin::signed(validity_origin()),
				alice(),
				AccountValidity::Invalid,
			));
			assert_eq!(
				Accounts::<Test>::get(alice()),
				AccountStatus {
					validity: AccountValidity::Invalid,
					free_balance: Zero::zero(),
					locked_balance: Zero::zero(),
					signature: alice_signature().to_vec(),
					vat: Permill::zero(),
				}
			);
			// She fixes it, we mark her account valid.
			assert_ok!(Purchase::update_validity_status(
				Origin::signed(validity_origin()),
				alice(),
				AccountValidity::ValidLow,
			));
			assert_eq!(
				Accounts::<Test>::get(alice()),
				AccountStatus {
					validity: AccountValidity::ValidLow,
					free_balance: Zero::zero(),
					locked_balance: Zero::zero(),
					signature: alice_signature().to_vec(),
					vat: Permill::zero(),
				}
			);
		});
	}

	#[test]
	fn update_validity_status_handles_basic_errors() {
		new_test_ext().execute_with(|| {
			// Wrong Origin
			assert_noop!(Purchase::update_validity_status(
				Origin::signed(alice()),
				alice(),
				AccountValidity::Pending,
			), BadOrigin);
			// Inactive Account
			assert_noop!(Purchase::update_validity_status(
				Origin::signed(validity_origin()),
				alice(),
				AccountValidity::Pending,
			), Error::<Test>::InvalidAccount);
			// Already Completed
			assert_ok!(Purchase::create_account(
				Origin::signed(validity_origin()),
				alice(),
				alice_signature().to_vec(),
			));
			assert_ok!(Purchase::update_validity_status(
				Origin::signed(validity_origin()),
				alice(),
				AccountValidity::Completed,
			));
			assert_noop!(Purchase::update_validity_status(
				Origin::signed(validity_origin()),
				alice(),
				AccountValidity::Pending,
			), Error::<Test>::AlreadyCompleted);
		});
	}

	#[test]
	fn update_balance_works() {
		new_test_ext().execute_with(|| {
			// Alice account is created
			assert_ok!(Purchase::create_account(
				Origin::signed(validity_origin()),
				alice(),
				alice_signature().to_vec())
			);
			// And approved for basic contribution
			assert_ok!(Purchase::update_validity_status(
				Origin::signed(validity_origin()),
				alice(),
				AccountValidity::ValidLow,
			));
			// We set a balance on the user based on the payment they made. 50 locked, 50 free.
			assert_ok!(Purchase::update_balance(
				Origin::signed(validity_origin()),
				alice(),
				50,
				50,
				Permill::from_rational(77u32, 1000u32),
			));
			assert_eq!(
				Accounts::<Test>::get(alice()),
				AccountStatus {
					validity: AccountValidity::ValidLow,
					free_balance: 50,
					locked_balance: 50,
					signature: alice_signature().to_vec(),
					vat: Permill::from_parts(77000),
				}
			);
			// We can update the balance based on new information.
			assert_ok!(Purchase::update_balance(
				Origin::signed(validity_origin()),
				alice(),
				25,
				50,
				Permill::zero(),
			));
			assert_eq!(
				Accounts::<Test>::get(alice()),
				AccountStatus {
					validity: AccountValidity::ValidLow,
					free_balance: 25,
					locked_balance: 50,
					signature: alice_signature().to_vec(),
					vat: Permill::zero(),
				}
			);
		});
	}

	#[test]
	fn update_balance_handles_basic_errors() {
		new_test_ext().execute_with(|| {
			// Wrong Origin
			assert_noop!(Purchase::update_balance(
				Origin::signed(alice()),
				alice(),
				50,
				50,
				Permill::zero(),
			), BadOrigin);
			// Inactive Account
			assert_noop!(Purchase::update_balance(
				Origin::signed(validity_origin()),
				alice(),
				50,
				50,
				Permill::zero(),
			), Error::<Test>::InvalidAccount);
			// Overflow
			assert_noop!(Purchase::update_balance(
				Origin::signed(validity_origin()),
				alice(),
				u64::max_value(),
				u64::max_value(),
				Permill::zero(),
			), Error::<Test>::InvalidAccount);
		});
	}

	#[test]
	fn payout_works() {
		new_test_ext().execute_with(|| {
			// Alice and Bob accounts are created
			assert_ok!(Purchase::create_account(
				Origin::signed(validity_origin()),
				alice(),
				alice_signature().to_vec())
			);
			assert_ok!(Purchase::create_account(
				Origin::signed(validity_origin()),
				bob(),
				bob_signature().to_vec())
			);
			// Alice is approved for basic contribution
			assert_ok!(Purchase::update_validity_status(
				Origin::signed(validity_origin()),
				alice(),
				AccountValidity::ValidLow,
			));
			// Bob is approved for high contribution
			assert_ok!(Purchase::update_validity_status(
				Origin::signed(validity_origin()),
				bob(),
				AccountValidity::ValidHigh,
			));
			// We set a balance on the users based on the payment they made. 50 locked, 50 free.
			assert_ok!(Purchase::update_balance(
				Origin::signed(validity_origin()),
				alice(),
				50,
				50,
				Permill::zero(),
			));
			assert_ok!(Purchase::update_balance(
				Origin::signed(validity_origin()),
				bob(),
				100,
				150,
				Permill::zero(),
			));
			// Now we call payout for Alice and Bob.
			assert_ok!(Purchase::payout(
				Origin::signed(payment_account()),
				alice(),
			));
			assert_ok!(Purchase::payout(
				Origin::signed(payment_account()),
				bob(),
			));
			// Payment is made.
			assert_eq!(<Test as Config>::Currency::free_balance(&payment_account()), 99_650);
			assert_eq!(<Test as Config>::Currency::free_balance(&alice()), 100);
			// 10% of the 50 units is unlocked automatically for Alice
			assert_eq!(<Test as Config>::VestingSchedule::vesting_balance(&alice()), Some(45));
			assert_eq!(<Test as Config>::Currency::free_balance(&bob()), 250);
			// A max of 10 units is unlocked automatically for Bob
			assert_eq!(<Test as Config>::VestingSchedule::vesting_balance(&bob()), Some(140));
			// Status is completed.
			assert_eq!(
				Accounts::<Test>::get(alice()),
				AccountStatus {
					validity: AccountValidity::Completed,
					free_balance: 50,
					locked_balance: 50,
					signature: alice_signature().to_vec(),
					vat: Permill::zero(),
				}
			);
			assert_eq!(
				Accounts::<Test>::get(bob()),
				AccountStatus {
					validity: AccountValidity::Completed,
					free_balance: 100,
					locked_balance: 150,
					signature: bob_signature().to_vec(),
					vat: Permill::zero(),
				}
			);
			// Vesting lock is removed in whole on block 101 (100 blocks after block 1)
			System::set_block_number(100);
			let vest_call = Call::Vesting(pallet_vesting::Call::<Test>::vest());
			assert_ok!(vest_call.clone().dispatch(Origin::signed(alice())));
			assert_ok!(vest_call.clone().dispatch(Origin::signed(bob())));
			assert_eq!(<Test as Config>::VestingSchedule::vesting_balance(&alice()), Some(45));
			assert_eq!(<Test as Config>::VestingSchedule::vesting_balance(&bob()), Some(140));
			System::set_block_number(101);
			assert_ok!(vest_call.clone().dispatch(Origin::signed(alice())));
			assert_ok!(vest_call.clone().dispatch(Origin::signed(bob())));
			assert_eq!(<Test as Config>::VestingSchedule::vesting_balance(&alice()), None);
			assert_eq!(<Test as Config>::VestingSchedule::vesting_balance(&bob()), None);
		});
	}

	#[test]
	fn payout_handles_basic_errors() {
		new_test_ext().execute_with(|| {
			// Wrong Origin
			assert_noop!(Purchase::payout(
				Origin::signed(alice()),
				alice(),
			), BadOrigin);
			// Account with Existing Vesting Schedule
			assert_ok!(<Test as Config>::VestingSchedule::add_vesting_schedule(
				&bob(), 100, 1, 50,
			));
			assert_noop!(Purchase::payout(
				Origin::signed(payment_account()),
				bob(),
			), Error::<Test>::VestingScheduleExists);
			// Invalid Account (never created)
			assert_noop!(Purchase::payout(
				Origin::signed(payment_account()),
				alice(),
			), Error::<Test>::InvalidAccount);
			// Invalid Account (created, but not valid)
			assert_ok!(Purchase::create_account(
				Origin::signed(validity_origin()),
				alice(),
				alice_signature().to_vec())
			);
			assert_noop!(Purchase::payout(
				Origin::signed(payment_account()),
				alice(),
			), Error::<Test>::InvalidAccount);
			// Not enough funds in payment account
			assert_ok!(Purchase::update_validity_status(
				Origin::signed(validity_origin()),
				alice(),
				AccountValidity::ValidHigh,
			));
			assert_ok!(Purchase::update_balance(
				Origin::signed(validity_origin()),
				alice(),
				100_000,
				100_000,
				Permill::zero(),
			));
			assert_noop!(Purchase::payout(
				Origin::signed(payment_account()),
				alice(),
			), BalancesError::<Test, _>::InsufficientBalance);
		});
	}

	#[test]
	fn remove_pallet_works() {
		new_test_ext().execute_with(|| {
			let account_status = AccountStatus {
				validity: AccountValidity::Completed,
				free_balance: 1234,
				locked_balance: 4321,
				signature: b"my signature".to_vec(),
				vat: Permill::from_percent(50),
			};

			// Add some storage.
			Accounts::<Test>::insert(alice(), account_status.clone());
			Accounts::<Test>::insert(bob(), account_status);
			PaymentAccount::<Test>::put(alice());
			Statement::<Test>::put(b"hello, world!".to_vec());
			UnlockBlock::<Test>::put(4);

			// Verify storage exists.
			assert_eq!(Accounts::<Test>::iter().count(), 2);
			assert!(PaymentAccount::<Test>::exists());
			assert!(Statement::<Test>::exists());
			assert!(UnlockBlock::<Test>::exists());

			// Remove storage.
			remove_pallet::<Test>();

			// Verify storage is gone.
			assert_eq!(Accounts::<Test>::iter().count(), 0);
			assert!(!PaymentAccount::<Test>::exists());
			assert!(!Statement::<Test>::exists());
			assert!(!UnlockBlock::<Test>::exists());
		});
	}
}
