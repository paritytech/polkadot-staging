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

//! # Parachain Crowdloaning pallet
//!
//! The point of this pallet is to allow parachain projects to offer the ability to help fund a
//! deposit for the parachain. When the crowdloan has ended, the funds are returned.
//!
//! Each fund has a child-trie which stores all contributors account IDs together with the amount
//! they contributed; the root of this can then be used by the parachain to allow contributors to
//! prove that they made some particular contribution to the project (e.g. to be rewarded through
//! some token or badge). The trie is retained for later (efficient) redistribution back to the
//! contributors.
//!
//! Contributions must be of at least `MinContribution` (to account for the resources taken in
//! tracking contributions), and may never tally greater than the fund's `cap`, set and fixed at the
//! time of creation. The `create` call may be used to create a new fund. In order to do this, then
//! a deposit must be paid of the amount `SubmissionDeposit`. Substantial resources are taken on
//! the main trie in tracking a fund and this accounts for that.
//!
//! Funds may be set up during an auction period; their closing time is fixed at creation (as a
//! block number) and if the fund is not successful by the closing time, then it can be dissolved.
//! Funds may span multiple auctions, and even auctions that sell differing periods. However, for a
//! fund to be active in bidding for an auction, it *must* have had *at least one bid* since the end
//! of the last auction. Until a fund takes a further bid following the end of an auction, then it
//! will be inactive.
//!
//! Contributors will get a refund of their contributions from completed funds before the crowdloan
//! can be dissolved.
//!
//! Funds may accept contributions at any point before their success or end. When a parachain
//! slot auction enters its ending period, then parachains will each place a bid; the bid will be
//! raised once per block if the parachain had additional funds contributed since the last bid.
//!
//! Successful funds remain tracked (in the `Funds` storage item and the associated child trie) as long as
//! the parachain remains active. Users can withdraw their funds once the slot is completed and funds are
//! returned to the crowdloan account.

use frame_support::{
	ensure, Identity, PalletId,
	storage::{child, ChildTriePrefixIterator},
	traits::{
		Currency, ReservableCurrency, Get, ExistenceRequirement::AllowDeath
	},
	pallet_prelude::Weight,
};
use sp_runtime::{
	RuntimeDebug, MultiSignature, MultiSigner,
	traits::{
		AccountIdConversion, Hash, Saturating, Zero, One, CheckedAdd, Verify, IdentifyAccount,
	},
};
use crate::traits::{Registrar, Auctioneer};
use crate::slot_range::SlotRange;
use parity_scale_codec::{Encode, Decode};
use scale_info::TypeInfo;
use sp_std::vec::Vec;
use primitives::v1::Id as ParaId;
pub use pallet::*;

type CurrencyOf<T> = <<T as Config>::Auctioneer as Auctioneer>::Currency;
type LeasePeriodOf<T> = <<T as Config>::Auctioneer as Auctioneer>::LeasePeriod;
type BalanceOf<T> = <CurrencyOf<T> as Currency<<T as frame_system::Config>::AccountId>>::Balance;

#[allow(dead_code)]
type NegativeImbalanceOf<T> = <CurrencyOf<T> as Currency<<T as frame_system::Config>::AccountId>>::NegativeImbalance;

type TrieIndex = u32;

pub trait WeightInfo {
	fn create() -> Weight;
	fn contribute() -> Weight;
	fn withdraw() -> Weight;
	fn refund(k: u32, ) -> Weight;
	fn dissolve() -> Weight;
	fn edit() -> Weight;
	fn add_memo() -> Weight;
	fn on_initialize(n: u32, ) -> Weight;
	fn poke() -> Weight;
}

pub struct TestWeightInfo;
impl WeightInfo for TestWeightInfo {
	fn create() -> Weight { 0 }
	fn contribute() -> Weight { 0 }
	fn withdraw() -> Weight { 0 }
	fn refund(_k: u32, ) -> Weight { 0 }
	fn dissolve() -> Weight { 0 }
	fn edit() -> Weight { 0 }
	fn add_memo() -> Weight { 0 }
	fn on_initialize(_n: u32, ) -> Weight { 0 }
	fn poke() -> Weight { 0 }
}

#[derive(Encode, Decode, Copy, Clone, PartialEq, Eq, RuntimeDebug, TypeInfo)]
pub enum LastContribution<BlockNumber> {
	Never,
	PreEnding(u32),
	Ending(BlockNumber),
}

/// Information on a funding effort for a pre-existing parachain. We assume that the parachain ID
/// is known as it's used for the key of the storage item for which this is the value (`Funds`).
#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug, TypeInfo)]
#[codec(dumb_trait_bound)]
pub struct FundInfo<AccountId, Balance, BlockNumber, LeasePeriod> {
	/// The owning account who placed the deposit.
	depositor: AccountId,
	/// An optional verifier. If exists, contributions must be signed by verifier.
	verifier: Option<MultiSigner>,
	/// The amount of deposit placed.
	deposit: Balance,
	/// The total amount raised.
	raised: Balance,
	/// Block number after which the funding must have succeeded. If not successful at this number
	/// then everyone may withdraw their funds.
	end: BlockNumber,
	/// A hard-cap on the amount that may be contributed.
	cap: Balance,
	/// The most recent block that this had a contribution. Determines if we make a bid or not.
	/// If this is `Never`, this fund has never received a contribution.
	/// If this is `PreEnding(n)`, this fund received a contribution sometime in auction
	/// number `n` before the ending period.
	/// If this is `Ending(n)`, this fund received a contribution during the current ending period,
	/// where `n` is how far into the ending period the contribution was made.
	last_contribution: LastContribution<BlockNumber>,
	/// First lease period in range to bid on; it's actually a LeasePeriod, but that's the same type
	/// as BlockNumber.
	first_period: LeasePeriod,
	/// Last lease period in range to bid on; it's actually a LeasePeriod, but that's the same type
	/// as BlockNumber.
	last_period: LeasePeriod,
	/// Index used for the child trie of this fund
	trie_index: TrieIndex,
}

#[frame_support::pallet]
pub mod pallet {
	use frame_support::pallet_prelude::*;
	use frame_system::{pallet_prelude::*, ensure_signed, ensure_root};
	use super::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// PalletId for the crowdloan pallet. An appropriate value could be ```PalletId(*b"py/cfund")```
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// The amount to be held on deposit by the depositor of a crowdloan.
		type SubmissionDeposit: Get<BalanceOf<Self>>;

		/// The minimum amount that may be contributed into a crowdloan. Should almost certainly be at
		/// least ExistentialDeposit.
		#[pallet::constant]
		type MinContribution: Get<BalanceOf<Self>>;

		/// Max number of storage keys to remove per extrinsic call.
		#[pallet::constant]
		type RemoveKeysLimit: Get<u32>;

		/// The parachain registrar type. We jus use this to ensure that only the manager of a para is able to
		/// start a crowdloan for its slot.
		type Registrar: Registrar<AccountId=Self::AccountId>;

		/// The type representing the auctioning system.
		type Auctioneer: Auctioneer<
			AccountId=Self::AccountId,
			BlockNumber=Self::BlockNumber,
			LeasePeriod=Self::BlockNumber,
		>;

		/// The maximum length for the memo attached to a crowdloan contribution.
		type MaxMemoLength: Get<u8>;

		/// Weight Information for the Extrinsics in the Pallet
		type WeightInfo: WeightInfo;
	}

	/// Info on all of the funds.
	#[pallet::storage]
	#[pallet::getter(fn funds)]
	pub(super) type Funds<T: Config> = StorageMap<
		_,
		Twox64Concat, ParaId,
		FundInfo<T::AccountId, BalanceOf<T>, T::BlockNumber, LeasePeriodOf<T>>,
	>;

	/// The funds that have had additional contributions during the last block. This is used
	/// in order to determine which funds should submit new or updated bids.
	#[pallet::storage]
	#[pallet::getter(fn new_raise)]
	pub(super) type NewRaise<T> = StorageValue<_, Vec<ParaId>, ValueQuery>;

	/// The number of auctions that have entered into their ending period so far.
	#[pallet::storage]
	#[pallet::getter(fn endings_count)]
	pub(super) type EndingsCount<T> = StorageValue<_, u32, ValueQuery>;

	/// Tracker for the next available trie index
	#[pallet::storage]
	#[pallet::getter(fn next_trie_index)]
	pub(super) type NextTrieIndex<T> = StorageValue<_, u32, ValueQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Create a new crowdloaning campaign. [fund_index]
		Created(ParaId),
		/// Contributed to a crowd sale. [who, fund_index, amount]
		Contributed(T::AccountId, ParaId, BalanceOf<T>),
		/// Withdrew full balance of a contributor. [who, fund_index, amount]
		Withdrew(T::AccountId, ParaId, BalanceOf<T>),
		/// The loans in a fund have been partially dissolved, i.e. there are some left
		/// over child keys that still need to be killed. [fund_index]
		PartiallyRefunded(ParaId),
		/// All loans in a fund have been refunded. [fund_index]
		AllRefunded(ParaId),
		/// Fund is dissolved. [fund_index]
		Dissolved(ParaId),
		/// The result of trying to submit a new bid to the Slots pallet.
		HandleBidResult(ParaId, DispatchResult),
		/// The configuration to a crowdloan has been edited. [fund_index]
		Edited(ParaId),
		/// A memo has been updated. [who, fund_index, memo]
		MemoUpdated(T::AccountId, ParaId, Vec<u8>),
		/// A parachain has been moved to NewRaise
		AddedToNewRaise(ParaId),
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The current lease period is more than the first lease period.
		FirstPeriodInPast,
		/// The first lease period needs to at least be less than 3 `max_value`.
		FirstPeriodTooFarInFuture,
		/// Last lease period must be greater than first lease period.
		LastPeriodBeforeFirstPeriod,
		/// The last lease period cannot be more then 3 periods after the first period.
		LastPeriodTooFarInFuture,
		/// The campaign ends before the current block number. The end must be in the future.
		CannotEndInPast,
		/// The end date for this crowdloan is not sensible.
		EndTooFarInFuture,
		/// There was an overflow.
		Overflow,
		/// The contribution was below the minimum, `MinContribution`.
		ContributionTooSmall,
		/// Invalid fund index.
		InvalidParaId,
		/// Contributions exceed maximum amount.
		CapExceeded,
		/// The contribution period has already ended.
		ContributionPeriodOver,
		/// The origin of this call is invalid.
		InvalidOrigin,
		/// This crowdloan does not correspond to a parachain.
		NotParachain,
		/// This parachain lease is still active and retirement cannot yet begin.
		LeaseActive,
		/// This parachain's bid or lease is still active and withdraw cannot yet begin.
		BidOrLeaseActive,
		/// The crowdloan has not yet ended.
		FundNotEnded,
		/// There are no contributions stored in this crowdloan.
		NoContributions,
		/// The crowdloan is not ready to dissolve. Potentially still has a slot or in retirement period.
		NotReadyToDissolve,
		/// Invalid signature.
		InvalidSignature,
		/// The provided memo is too large.
		MemoTooLarge,
		/// The fund is already in NewRaise
		AlreadyInNewRaise,
		/// No contributions allowed during the VRF delay
		VrfDelayInProgress,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(num: T::BlockNumber) -> frame_support::weights::Weight {
			if let Some((sample, sub_sample)) = T::Auctioneer::auction_status(num).is_ending() {
				// This is the very first block in the ending period
				if sample.is_zero() && sub_sample.is_zero() {
					// first block of ending period.
					EndingsCount::<T>::mutate(|c| *c += 1);
				}
				let new_raise = NewRaise::<T>::take();
				let new_raise_len = new_raise.len() as u32;
				for (fund, para_id) in new_raise.into_iter().filter_map(|i| Self::funds(i).map(|f| (f, i))) {
					// Care needs to be taken by the crowdloan creator that this function will succeed given
					// the crowdloaning configuration. We do some checks ahead of time in crowdloan `create`.
					let result = T::Auctioneer::place_bid(
						Self::fund_account_id(para_id),
						para_id,
						fund.first_period,
						fund.last_period,
						fund.raised,
					);

					Self::deposit_event(Event::<T>::HandleBidResult(para_id, result));
				}
				T::WeightInfo::on_initialize(new_raise_len)
			} else {
				T::DbWeight::get().reads(1)
			}
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Create a new crowdloaning campaign for a parachain slot with the given lease period range.
		///
		/// This applies a lock to your parachain configuration, ensuring that it cannot be changed
		/// by the parachain manager.
		#[pallet::weight(T::WeightInfo::create())]
		pub fn create(
			origin: OriginFor<T>,
			#[pallet::compact] index: ParaId,
			#[pallet::compact] cap: BalanceOf<T>,
			#[pallet::compact] first_period: LeasePeriodOf<T>,
			#[pallet::compact] last_period: LeasePeriodOf<T>,
			#[pallet::compact] end: T::BlockNumber,
			verifier: Option<MultiSigner>,
		) -> DispatchResult {
			let depositor = ensure_signed(origin)?;

			ensure!(first_period <= last_period, Error::<T>::LastPeriodBeforeFirstPeriod);
			let last_period_limit = first_period
				.checked_add(&((SlotRange::LEASE_PERIODS_PER_SLOT as u32) - 1).into())
				.ok_or(Error::<T>::FirstPeriodTooFarInFuture)?;
			ensure!(last_period <= last_period_limit, Error::<T>::LastPeriodTooFarInFuture);
			ensure!(end > <frame_system::Pallet<T>>::block_number(), Error::<T>::CannotEndInPast);
			let last_possible_win_date = (first_period.saturating_add(One::one())).saturating_mul(T::Auctioneer::lease_period());
			ensure!(end <= last_possible_win_date, Error::<T>::EndTooFarInFuture);
			ensure!(first_period >= T::Auctioneer::lease_period_index(), Error::<T>::FirstPeriodInPast);

			// There should not be an existing fund.
			ensure!(!Funds::<T>::contains_key(index), Error::<T>::FundNotEnded);

			let manager = T::Registrar::manager_of(index).ok_or(Error::<T>::InvalidParaId)?;
			ensure!(depositor == manager, Error::<T>::InvalidOrigin);
			ensure!(T::Registrar::is_registered(index), Error::<T>::InvalidParaId);

			let trie_index = Self::next_trie_index();
			let new_trie_index = trie_index.checked_add(1).ok_or(Error::<T>::Overflow)?;

			let deposit = T::SubmissionDeposit::get();

			CurrencyOf::<T>::reserve(&depositor, deposit)?;

			Funds::<T>::insert(index, FundInfo {
				depositor,
				verifier,
				deposit,
				raised: Zero::zero(),
				end,
				cap,
				last_contribution: LastContribution::Never,
				first_period,
				last_period,
				trie_index,
			});

			NextTrieIndex::<T>::put(new_trie_index);
			// Add a lock to the para so that the configuration cannot be changed.
			T::Registrar::apply_lock(index);

			Self::deposit_event(Event::<T>::Created(index));
			Ok(())
		}

		/// Contribute to a crowd sale. This will transfer some balance over to fund a parachain
		/// slot. It will be withdrawable when the crowdloan has ended and the funds are unused.
		#[pallet::weight(T::WeightInfo::contribute())]
		pub fn contribute(
			origin: OriginFor<T>,
			#[pallet::compact] index: ParaId,
			#[pallet::compact] value: BalanceOf<T>,
			signature: Option<MultiSignature>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;

			ensure!(value >= T::MinContribution::get(), Error::<T>::ContributionTooSmall);
			let mut fund = Self::funds(index).ok_or(Error::<T>::InvalidParaId)?;
			fund.raised  = fund.raised.checked_add(&value).ok_or(Error::<T>::Overflow)?;
			ensure!(fund.raised <= fund.cap, Error::<T>::CapExceeded);

			// Make sure crowdloan has not ended
			let now = <frame_system::Pallet<T>>::block_number();
			ensure!(now < fund.end, Error::<T>::ContributionPeriodOver);

			// Make sure crowdloan is in a valid lease period
			let current_lease_period = T::Auctioneer::lease_period_index();
			ensure!(current_lease_period <= fund.first_period, Error::<T>::ContributionPeriodOver);

			// Make sure crowdloan has not already won.
			let fund_account = Self::fund_account_id(index);
			ensure!(!T::Auctioneer::has_won_an_auction(index, &fund_account), Error::<T>::BidOrLeaseActive);

			// We disallow any crowdloan contributions during the VRF Period, so that people do not sneak their
			// contributions into the auction when it would not impact the outcome.
			ensure!(!T::Auctioneer::auction_status(now).is_vrf(), Error::<T>::VrfDelayInProgress);

			let (old_balance, memo) = Self::contribution_get(fund.trie_index, &who);

			if let Some(ref verifier) = fund.verifier {
				let signature = signature.ok_or(Error::<T>::InvalidSignature)?;
				let payload = (index, &who, old_balance, value);
				let valid = payload.using_encoded(|encoded| signature.verify(encoded, &verifier.clone().into_account()));
				ensure!(valid, Error::<T>::InvalidSignature);
			}

			CurrencyOf::<T>::transfer(&who, &fund_account, value, AllowDeath)?;

			let balance = old_balance.saturating_add(value);
			Self::contribution_put(fund.trie_index, &who, &balance, &memo);

			if T::Auctioneer::auction_status(now).is_ending().is_some() {
				match fund.last_contribution {
					// In ending period; must ensure that we are in NewRaise.
					LastContribution::Ending(n) if n == now => {
						// do nothing - already in NewRaise
					}
					_ => {
						NewRaise::<T>::append(index);
						fund.last_contribution = LastContribution::Ending(now);
					}
				}
			} else {
				let endings_count = Self::endings_count();
				match fund.last_contribution {
					LastContribution::PreEnding(a) if a == endings_count => {
						// Not in ending period and no auctions have ended ending since our
						// previous bid which was also not in an ending period.
						// `NewRaise` will contain our ID still: Do nothing.
					}
					_ => {
						// Not in ending period; but an auction has been ending since our previous
						// bid, or we never had one to begin with. Add bid.
						NewRaise::<T>::append(index);
						fund.last_contribution = LastContribution::PreEnding(endings_count);
					}
				}
			}

			Funds::<T>::insert(index, &fund);

			Self::deposit_event(Event::<T>::Contributed(who, index, value));
			Ok(())
		}

		/// Withdraw full balance of a specific contributor.
		///
		/// Origin must be signed, but can come from anyone.
		///
		/// The fund must be either in, or ready for, retirement. For a fund to be *in* retirement, then the retirement
		/// flag must be set. For a fund to be ready for retirement, then:
		/// - it must not already be in retirement;
		/// - the amount of raised funds must be bigger than the _free_ balance of the account;
		/// - and either:
		///   - the block number must be at least `end`; or
		///   - the current lease period must be greater than the fund's `last_period`.
		///
		/// In this case, the fund's retirement flag is set and its `end` is reset to the current block
		/// number.
		///
		/// - `who`: The account whose contribution should be withdrawn.
		/// - `index`: The parachain to whose crowdloan the contribution was made.
		#[pallet::weight(T::WeightInfo::withdraw())]
		pub fn withdraw(
			origin: OriginFor<T>,
			who: T::AccountId,
			#[pallet::compact] index: ParaId,
		) -> DispatchResult {
			ensure_signed(origin)?;

			let mut fund = Self::funds(index).ok_or(Error::<T>::InvalidParaId)?;
			let now = frame_system::Pallet::<T>::block_number();
			let fund_account = Self::fund_account_id(index);
			Self::ensure_crowdloan_ended(now, &fund_account, &fund)?;

			let (balance, _) = Self::contribution_get(fund.trie_index, &who);
			ensure!(balance > Zero::zero(), Error::<T>::NoContributions);

			CurrencyOf::<T>::transfer(&fund_account, &who, balance, AllowDeath)?;

			Self::contribution_kill(fund.trie_index, &who);
			fund.raised = fund.raised.saturating_sub(balance);

			Funds::<T>::insert(index, &fund);

			Self::deposit_event(Event::<T>::Withdrew(who, index, balance));
			Ok(())
		}

		/// Automatically refund contributors of an ended crowdloan.
		/// Due to weight restrictions, this function may need to be called multiple
		/// times to fully refund all users. We will refund `RemoveKeysLimit` users at a time.
		///
		/// Origin must be signed, but can come from anyone.
		#[pallet::weight(T::WeightInfo::refund(T::RemoveKeysLimit::get()))]
		pub fn refund(
			origin: OriginFor<T>,
			#[pallet::compact] index: ParaId,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;

			let mut fund = Self::funds(index).ok_or(Error::<T>::InvalidParaId)?;
			let now = frame_system::Pallet::<T>::block_number();
			let fund_account = Self::fund_account_id(index);
			Self::ensure_crowdloan_ended(now, &fund_account, &fund)?;

			let mut refund_count = 0u32;
			// Try killing the crowdloan child trie
			let contributions = Self::contribution_iterator(fund.trie_index);
			// Assume everyone will be refunded.
			let mut all_refunded = true;
			for (who, (balance, _)) in contributions {
				if refund_count >= T::RemoveKeysLimit::get() {
					// Not everyone was able to be refunded this time around.
					all_refunded = false;
					break;
				}
				CurrencyOf::<T>::transfer(&fund_account, &who, balance, AllowDeath)?;
				Self::contribution_kill(fund.trie_index, &who);
				fund.raised = fund.raised.saturating_sub(balance);
				refund_count += 1;
			}

			// Save the changes.
			Funds::<T>::insert(index, &fund);

			if all_refunded {
				Self::deposit_event(Event::<T>::AllRefunded(index));
				// Refund for unused refund count.
				Ok(Some(T::WeightInfo::refund(refund_count)).into())
			} else {
				Self::deposit_event(Event::<T>::PartiallyRefunded(index));
				// No weight to refund since we did not finish the loop.
				Ok(().into())
			}
		}

		/// Remove a fund after the retirement period has ended and all funds have been returned.
		#[pallet::weight(T::WeightInfo::dissolve())]
		pub fn dissolve(origin: OriginFor<T>, #[pallet::compact] index: ParaId) -> DispatchResult {
			let who = ensure_signed(origin)?;

			let fund = Self::funds(index).ok_or(Error::<T>::InvalidParaId)?;
			let now = frame_system::Pallet::<T>::block_number();

			// Only allow dissolution when the raised funds goes to zero,
			// and the caller is the fund creator or we are past the end date.
			let permitted = who == fund.depositor || now >= fund.end;
			let can_dissolve = permitted && fund.raised.is_zero();
			ensure!(can_dissolve, Error::<T>::NotReadyToDissolve);

			// Assuming state is not corrupted, the child trie should already be cleaned up
			// and all funds in the crowdloan account have been returned. If not, governance
			// can take care of that.
			debug_assert!(Self::contribution_iterator(fund.trie_index).count().is_zero());

			CurrencyOf::<T>::unreserve(&fund.depositor, fund.deposit);
			Funds::<T>::remove(index);
			Self::deposit_event(Event::<T>::Dissolved(index));
			Ok(())
		}

		/// Edit the configuration for an in-progress crowdloan.
		///
		/// Can only be called by Root origin.
		#[pallet::weight(T::WeightInfo::edit())]
		pub fn edit(
			origin: OriginFor<T>,
			#[pallet::compact] index: ParaId,
			#[pallet::compact] cap: BalanceOf<T>,
			#[pallet::compact] first_period: LeasePeriodOf<T>,
			#[pallet::compact] last_period: LeasePeriodOf<T>,
			#[pallet::compact] end: T::BlockNumber,
			verifier: Option<MultiSigner>,
		) -> DispatchResult {
			ensure_root(origin)?;

			let fund = Self::funds(index).ok_or(Error::<T>::InvalidParaId)?;

			Funds::<T>::insert(index, FundInfo {
				depositor: fund.depositor,
				verifier,
				deposit: fund.deposit,
				raised: fund.raised,
				end,
				cap,
				last_contribution: fund.last_contribution,
				first_period,
				last_period,
				trie_index: fund.trie_index,
			});

			Self::deposit_event(Event::<T>::Edited(index));
			Ok(())
		}

		/// Add an optional memo to an existing crowdloan contribution.
		///
		/// Origin must be Signed, and the user must have contributed to the crowdloan.
		#[pallet::weight(T::WeightInfo::add_memo())]
		pub fn add_memo(origin: OriginFor<T>, index: ParaId, memo: Vec<u8>) -> DispatchResult {
			let who = ensure_signed(origin)?;

			ensure!(memo.len() <= T::MaxMemoLength::get().into(), Error::<T>::MemoTooLarge);
			let fund = Self::funds(index).ok_or(Error::<T>::InvalidParaId)?;

			let (balance, _) = Self::contribution_get(fund.trie_index, &who);
			ensure!(balance > Zero::zero(), Error::<T>::NoContributions);

			Self::contribution_put(fund.trie_index, &who, &balance, &memo);
			Self::deposit_event(Event::<T>::MemoUpdated(who, index, memo));
			Ok(())
		}

		/// Poke the fund into NewRaise
		///
		/// Origin must be Signed, and the fund has non-zero raise.
		#[pallet::weight(T::WeightInfo::poke())]
		pub fn poke(origin: OriginFor<T>, index: ParaId) -> DispatchResult {
			ensure_signed(origin)?;
			let fund = Self::funds(index).ok_or(Error::<T>::InvalidParaId)?;
			ensure!(!fund.raised.is_zero(), Error::<T>::NoContributions);
			ensure!(!NewRaise::<T>::get().contains(&index), Error::<T>::AlreadyInNewRaise);
			NewRaise::<T>::append(index);
			Self::deposit_event(Event::<T>::AddedToNewRaise(index));
			Ok(())
		}
	}
}

impl<T: Config> Pallet<T> {
	/// The account ID of the fund pot.
	///
	/// This actually does computation. If you need to keep using it, then make sure you cache the
	/// value and only call this once.
	pub fn fund_account_id(index: ParaId) -> T::AccountId {
		T::PalletId::get().into_sub_account(index)
	}

	pub fn id_from_index(index: TrieIndex) -> child::ChildInfo {
		let mut buf = Vec::new();
		buf.extend_from_slice(b"crowdloan");
		buf.extend_from_slice(&index.encode()[..]);
		child::ChildInfo::new_default(T::Hashing::hash(&buf[..]).as_ref())
	}

	pub fn contribution_put(index: TrieIndex, who: &T::AccountId, balance: &BalanceOf<T>, memo: &[u8]) {
		who.using_encoded(|b| child::put(&Self::id_from_index(index), b, &(balance, memo)));
	}

	pub fn contribution_get(index: TrieIndex, who: &T::AccountId) -> (BalanceOf<T>, Vec<u8>) {
		who.using_encoded(|b| child::get_or_default::<(BalanceOf<T>, Vec<u8>)>(
			&Self::id_from_index(index),
			b,
		))
	}

	pub fn contribution_kill(index: TrieIndex, who: &T::AccountId) {
		who.using_encoded(|b| child::kill(&Self::id_from_index(index), b));
	}

	pub fn crowdloan_kill(index: TrieIndex) -> child::KillStorageResult {
		child::kill_storage(&Self::id_from_index(index), Some(T::RemoveKeysLimit::get()))
	}

	pub fn contribution_iterator(
		index: TrieIndex
	) -> ChildTriePrefixIterator<(T::AccountId, (BalanceOf<T>, Vec<u8>))> {
		ChildTriePrefixIterator::<_>::with_prefix_over_key::<Identity>(&Self::id_from_index(index), &[])
	}

	/// This function checks all conditions which would qualify a crowdloan has ended.
	/// * If we have reached the `fund.end` block OR the first lease period the fund is
	///   trying to bid for has started already.
	/// * And, if the fund has enough free funds to refund full raised amount.
	fn ensure_crowdloan_ended(
		now: T::BlockNumber,
		fund_account: &T::AccountId,
		fund: &FundInfo<T::AccountId, BalanceOf<T>, T::BlockNumber, LeasePeriodOf<T>>
	) -> sp_runtime::DispatchResult {
			// `fund.end` can represent the end of a failed crowdloan or the beginning of retirement
			// If the current lease period is past the first period they are trying to bid for, then
			// it is already too late to win the bid.
			let current_lease_period = T::Auctioneer::lease_period_index();
			ensure!(now >= fund.end || current_lease_period > fund.first_period, Error::<T>::FundNotEnded);
			// free balance must greater than or equal amount raised, otherwise funds are being used
			// and a bid or lease must be active.
			ensure!(CurrencyOf::<T>::free_balance(&fund_account) >= fund.raised, Error::<T>::BidOrLeaseActive);

			Ok(())
	}
}

impl<T: Config> crate::traits::OnSwap for Pallet<T> {
	fn on_swap(one: ParaId, other: ParaId) {
		Funds::<T>::mutate(one, |x|
			Funds::<T>::mutate(other, |y|
				sp_std::mem::swap(x, y)
			)
		)
	}
}

#[cfg(any(feature = "runtime-benchmarks", test))]
mod crypto {
	use sp_core::ed25519;
	use sp_io::crypto::{ed25519_sign, ed25519_generate};
	use sp_std::{
		vec::Vec,
		convert::TryFrom,
	};
	use sp_runtime::{MultiSigner, MultiSignature};

	pub fn create_ed25519_pubkey(seed: Vec<u8>) -> MultiSigner {
		ed25519_generate(0.into(), Some(seed)).into()
	}

	pub fn create_ed25519_signature(payload: &[u8], pubkey: MultiSigner) -> MultiSignature {
		let edpubkey = ed25519::Public::try_from(pubkey).unwrap();
		let edsig = ed25519_sign(0.into(), &edpubkey, payload).unwrap();
		edsig.into()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use std::{cell::RefCell, sync::Arc, collections::BTreeMap};
	use frame_support::{
		assert_ok, assert_noop, parameter_types,
		traits::{OnInitialize, OnFinalize},
	};
	use sp_core::H256;
	use primitives::v1::Id as ParaId;
	// The testing primitives are very useful for avoiding having to work with signatures
	// or public keys. `u64` is used as the `AccountId` and no `Signature`s are requried.
	use sp_runtime::{
		testing::Header, traits::{BlakeTwo256, IdentityLookup}, DispatchResult,
	};
	use crate::{
		mock::TestRegistrar,
		traits::{OnSwap, AuctionStatus},
		crowdloan,
	};
	use sp_keystore::{KeystoreExt, testing::KeyStore};

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
			Crowdloan: crowdloan::{Pallet, Call, Storage, Event<T>},
		}
	);

	parameter_types! {
		pub const BlockHashCount: u32 = 250;
	}

	type BlockNumber = u64;

	impl frame_system::Config for Test {
		type BaseCallFilter = frame_support::traits::AllowAll;
		type BlockWeights = ();
		type BlockLength = ();
		type DbWeight = ();
		type Origin = Origin;
		type Call = Call;
		type Index = u64;
		type BlockNumber = BlockNumber;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type AccountId = u64;
		type Lookup = IdentityLookup<Self::AccountId>;
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
	}

	impl pallet_balances::Config for Test {
		type Balance = u64;
		type Event = Event;
		type DustRemoval = ();
		type ExistentialDeposit = ExistentialDeposit;
		type AccountStore = System;
		type MaxLocks = ();
		type MaxReserves = ();
		type ReserveIdentifier = [u8; 8];
		type WeightInfo = ();
	}

	#[derive(Copy, Clone, Eq, PartialEq, Debug)]
	struct BidPlaced {
		height: u64,
		bidder: u64,
		para: ParaId,
		first_period: u64,
		last_period: u64,
		amount: u64
	}
	thread_local! {
		static AUCTION: RefCell<Option<(u64, u64)>> = RefCell::new(None);
		static VRF_DELAY: RefCell<u64> = RefCell::new(0);
		static ENDING_PERIOD: RefCell<u64> = RefCell::new(5);
		static BIDS_PLACED: RefCell<Vec<BidPlaced>> = RefCell::new(Vec::new());
		static HAS_WON: RefCell<BTreeMap<(ParaId, u64), bool>> = RefCell::new(BTreeMap::new());
	}

	#[allow(unused)]
	fn set_ending_period(ending_period: u64) {
		ENDING_PERIOD.with(|p| *p.borrow_mut() = ending_period);
	}
	fn auction() -> Option<(u64, u64)> {
		AUCTION.with(|p| p.borrow().clone())
	}
	fn ending_period() -> u64 {
		ENDING_PERIOD.with(|p| p.borrow().clone())
	}
	fn bids() -> Vec<BidPlaced> {
		BIDS_PLACED.with(|p| p.borrow().clone())
	}
	fn vrf_delay() -> u64 {
		VRF_DELAY.with(|p| p.borrow().clone())
	}
	fn set_vrf_delay(delay: u64) {
		VRF_DELAY.with(|p| *p.borrow_mut() = delay);
	}
	// Emulate what would happen if we won an auction:
	// balance is reserved and a deposit_held is recorded
	fn set_winner(para: ParaId, who: u64, winner: bool) {
		let account_id = Crowdloan::fund_account_id(para);
		if winner {
			let free_balance = Balances::free_balance(&account_id);
			Balances::reserve(&account_id, free_balance).expect("should be able to reserve free balance");
		} else {
			let reserved_balance = Balances::reserved_balance(&account_id);
			Balances::unreserve(&account_id, reserved_balance);
		}
		HAS_WON.with(|p| p.borrow_mut().insert((para, who), winner));
	}

	pub struct TestAuctioneer;
	impl Auctioneer for TestAuctioneer {
		type AccountId = u64;
		type BlockNumber = BlockNumber;
		type LeasePeriod = u64;
		type Currency = Balances;

		fn new_auction(duration: u64, lease_period_index: u64) -> DispatchResult {
			assert!(lease_period_index >= Self::lease_period_index());

			let ending = System::block_number().saturating_add(duration);
			AUCTION.with(|p| *p.borrow_mut() = Some((lease_period_index, ending)));
			Ok(())
		}

		fn auction_status(now: u64) -> AuctionStatus<u64> {
			let early_end = match auction() {
				Some((_, early_end)) => early_end,
				None => return AuctionStatus::NotStarted,
			};
			let after_early_end = match now.checked_sub(early_end) {
				Some(after_early_end) => after_early_end,
				None => return AuctionStatus::StartingPeriod,
			};

			let ending_period = ending_period();
			if after_early_end < ending_period {
				return AuctionStatus::EndingPeriod(after_early_end, 0)
			} else {
				let after_end = after_early_end - ending_period;
				// Optional VRF delay
				if after_end < vrf_delay() {
					return AuctionStatus::VrfDelay(after_end);
				} else {
					// VRF delay is done, so we just end the auction
					return AuctionStatus::NotStarted;
				}
			}
		}

		fn place_bid(
			bidder: u64,
			para: ParaId,
			first_period: u64,
			last_period: u64,
			amount: u64
		) -> DispatchResult {
			let height = System::block_number();
			BIDS_PLACED.with(|p| p.borrow_mut().push(BidPlaced { height, bidder, para, first_period, last_period, amount }));
			Ok(())
		}

		fn lease_period_index() -> u64 {
			System::block_number() / Self::lease_period()
		}

		fn lease_period() -> u64 {
			20
		}

		fn has_won_an_auction(para: ParaId, bidder: &u64) -> bool {
			HAS_WON.with(|p| *p.borrow().get(&(para, *bidder)).unwrap_or(&false))
		}
	}

	parameter_types! {
		pub const SubmissionDeposit: u64 = 1;
		pub const MinContribution: u64 = 10;
		pub const CrowdloanPalletId: PalletId = PalletId(*b"py/cfund");
		pub const RemoveKeysLimit: u32 = 10;
		pub const MaxMemoLength: u8 = 32;
	}

	impl Config for Test {
		type Event = Event;
		type SubmissionDeposit = SubmissionDeposit;
		type MinContribution = MinContribution;
		type PalletId = CrowdloanPalletId;
		type RemoveKeysLimit = RemoveKeysLimit;
		type Registrar = TestRegistrar<Test>;
		type Auctioneer = TestAuctioneer;
		type MaxMemoLength = MaxMemoLength;
		type WeightInfo = crate::crowdloan::TestWeightInfo;
	}

	use pallet_balances::Error as BalancesError;

	// This function basically just builds a genesis storage key/value store according to
	// our desired mockup.
	pub fn new_test_ext() -> sp_io::TestExternalities {
		let mut t = frame_system::GenesisConfig::default().build_storage::<Test>().unwrap();
		pallet_balances::GenesisConfig::<Test>{
			balances: vec![(1, 1000), (2, 2000), (3, 3000), (4, 4000)],
		}.assimilate_storage(&mut t).unwrap();
		let keystore = KeyStore::new();
		let mut t: sp_io::TestExternalities = t.into();
		t.register_extension(KeystoreExt(Arc::new(keystore)));
		t
	}

	fn new_para() -> ParaId {
		for i in 0.. {
			let para: ParaId = i.into();
			if TestRegistrar::<Test>::is_registered(para) { continue }
			assert_ok!(TestRegistrar::<Test>::register(1, para, Default::default(), Default::default()));
			return para;
		}
		unreachable!()
	}

	fn run_to_block(n: u64) {
		while System::block_number() < n {
			Crowdloan::on_finalize(System::block_number());
			Balances::on_finalize(System::block_number());
			System::on_finalize(System::block_number());
			System::set_block_number(System::block_number() + 1);
			System::on_initialize(System::block_number());
			Balances::on_initialize(System::block_number());
			Crowdloan::on_initialize(System::block_number());
		}
	}

	fn last_event() -> Event {
		System::events().pop().expect("Event expected").event
	}

	#[test]
	fn basic_setup_works() {
		new_test_ext().execute_with(|| {
			assert_eq!(System::block_number(), 0);
			assert_eq!(Crowdloan::funds(ParaId::from(0)), None);
			let empty: Vec<ParaId> = Vec::new();
			assert_eq!(Crowdloan::new_raise(), empty);
			assert_eq!(Crowdloan::contribution_get(0u32, &1).0, 0);
			assert_eq!(Crowdloan::endings_count(), 0);

			assert_ok!(TestAuctioneer::new_auction(5, 0));

			assert_eq!(bids(), vec![]);
			assert_ok!(TestAuctioneer::place_bid(1, 2.into(), 0, 3, 6));
			let b = BidPlaced { height: 0, bidder: 1, para: 2.into(), first_period: 0, last_period: 3, amount: 6 };
			assert_eq!(bids(), vec![b]);
			assert_eq!(TestAuctioneer::auction_status(4), AuctionStatus::<u64>::StartingPeriod);
			assert_eq!(TestAuctioneer::auction_status(5), AuctionStatus::<u64>::EndingPeriod(0, 0));
			assert_eq!(TestAuctioneer::auction_status(9), AuctionStatus::<u64>::EndingPeriod(4, 0));
			assert_eq!(TestAuctioneer::auction_status(11), AuctionStatus::<u64>::NotStarted);
		});
	}

	#[test]
	fn create_works() {
		new_test_ext().execute_with(|| {
			let para = new_para();
			// Now try to create a crowdloan campaign
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 4, 9, None));
			// This is what the initial `fund_info` should look like
			let fund_info = FundInfo {
				depositor: 1,
				verifier: None,
				deposit: 1,
				raised: 0,
				// 5 blocks length + 3 block ending period + 1 starting block
				end: 9,
				cap: 1000,
				last_contribution: LastContribution::Never,
				first_period: 1,
				last_period: 4,
				trie_index: 0,
			};
			assert_eq!(Crowdloan::funds(para), Some(fund_info));
			// User has deposit removed from their free balance
			assert_eq!(Balances::free_balance(1), 999);
			// Deposit is placed in reserved
			assert_eq!(Balances::reserved_balance(1), 1);
			// No new raise until first contribution
			let empty: Vec<ParaId> = Vec::new();
			assert_eq!(Crowdloan::new_raise(), empty);
		});
	}

	#[test]
	fn create_with_verifier_works() {
		new_test_ext().execute_with(|| {
			let pubkey = crypto::create_ed25519_pubkey(b"//verifier".to_vec());
			let para = new_para();
			// Now try to create a crowdloan campaign
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 4, 9, Some(pubkey.clone())));
			// This is what the initial `fund_info` should look like
			let fund_info = FundInfo {
				depositor: 1,
				verifier: Some(pubkey),
				deposit: 1,
				raised: 0,
				// 5 blocks length + 3 block ending period + 1 starting block
				end: 9,
				cap: 1000,
				last_contribution: LastContribution::Never,
				first_period: 1,
				last_period: 4,
				trie_index: 0,
			};
			assert_eq!(Crowdloan::funds(ParaId::from(0)), Some(fund_info));
			// User has deposit removed from their free balance
			assert_eq!(Balances::free_balance(1), 999);
			// Deposit is placed in reserved
			assert_eq!(Balances::reserved_balance(1), 1);
			// No new raise until first contribution
			let empty: Vec<ParaId> = Vec::new();
			assert_eq!(Crowdloan::new_raise(), empty);
		});
	}

	#[test]
	fn create_handles_basic_errors() {
		new_test_ext().execute_with(|| {
			// Now try to create a crowdloan campaign
			let para = new_para();

			let e = Error::<Test>::InvalidParaId;
			assert_noop!(Crowdloan::create(Origin::signed(1), 1.into(), 1000, 1, 4, 9, None), e);
			// Cannot create a crowdloan with bad lease periods
			let e = Error::<Test>::LastPeriodBeforeFirstPeriod;
			assert_noop!(Crowdloan::create(Origin::signed(1), para, 1000, 4, 1, 9, None), e);
			let e = Error::<Test>::LastPeriodTooFarInFuture;
			assert_noop!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 9, 9, None), e);

			// Cannot create a crowdloan without some deposit funds
			assert_ok!(TestRegistrar::<Test>::register(1337, ParaId::from(1234), Default::default(), Default::default()));
			let e = BalancesError::<Test, _>::InsufficientBalance;
			assert_noop!(Crowdloan::create(Origin::signed(1337), ParaId::from(1234), 1000, 1, 3, 9, None), e);

			// Cannot create a crowdloan with nonsense end date
			// This crowdloan would end in lease period 2, but is bidding for some slot that starts in lease period 1.
			assert_noop!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 4, 41, None), Error::<Test>::EndTooFarInFuture);
		});
	}

	#[test]
	fn contribute_works() {
		new_test_ext().execute_with(|| {
			let para = new_para();

			// Set up a crowdloan
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 4, 9, None));

			// No contributions yet
			assert_eq!(Crowdloan::contribution_get(u32::from(para), &1).0, 0);

			// User 1 contributes to their own crowdloan
			assert_ok!(Crowdloan::contribute(Origin::signed(1), para, 49, None));
			// User 1 has spent some funds to do this, transfer fees **are** taken
			assert_eq!(Balances::free_balance(1), 950);
			// Contributions are stored in the trie
			assert_eq!(Crowdloan::contribution_get(u32::from(para), &1).0, 49);
			// Contributions appear in free balance of crowdloan
			assert_eq!(Balances::free_balance(Crowdloan::fund_account_id(para)), 49);
			// Crowdloan is added to NewRaise
			assert_eq!(Crowdloan::new_raise(), vec![para]);

			let fund = Crowdloan::funds(para).unwrap();

			// Last contribution time recorded
			assert_eq!(fund.last_contribution, LastContribution::PreEnding(0));
			assert_eq!(fund.raised, 49);
		});
	}

	#[test]
	fn contribute_with_verifier_works() {
		new_test_ext().execute_with(|| {
			let para = new_para();
			let pubkey = crypto::create_ed25519_pubkey(b"//verifier".to_vec());
			// Set up a crowdloan
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 4, 9, Some(pubkey.clone())));

			// No contributions yet
			assert_eq!(Crowdloan::contribution_get(u32::from(para), &1).0, 0);

			// Missing signature
			assert_noop!(Crowdloan::contribute(Origin::signed(1), para, 49, None), Error::<Test>::InvalidSignature);

			let payload = (0u32, 1u64, 0u64, 49u64);
			let valid_signature = crypto::create_ed25519_signature(&payload.encode(), pubkey.clone());
			let invalid_signature = MultiSignature::default();

			// Invalid signature
			assert_noop!(Crowdloan::contribute(Origin::signed(1), para, 49, Some(invalid_signature)), Error::<Test>::InvalidSignature);

			// Valid signature wrong parameter
			assert_noop!(Crowdloan::contribute(Origin::signed(1), para, 50, Some(valid_signature.clone())), Error::<Test>::InvalidSignature);
			assert_noop!(Crowdloan::contribute(Origin::signed(2), para, 49, Some(valid_signature.clone())), Error::<Test>::InvalidSignature);

			// Valid signature
			assert_ok!(Crowdloan::contribute(Origin::signed(1), para, 49, Some(valid_signature.clone())));

			// Reuse valid signature
			assert_noop!(Crowdloan::contribute(Origin::signed(1), para, 49, Some(valid_signature)), Error::<Test>::InvalidSignature);

			let payload_2 = (0u32, 1u64, 49u64, 10u64);
			let valid_signature_2 = crypto::create_ed25519_signature(&payload_2.encode(), pubkey);

			// New valid signature
			assert_ok!(Crowdloan::contribute(Origin::signed(1), para, 10, Some(valid_signature_2)));

			// Contributions appear in free balance of crowdloan
			assert_eq!(Balances::free_balance(Crowdloan::fund_account_id(para)), 59);

			// Contribution amount is correct
			let fund = Crowdloan::funds(para).unwrap();
			assert_eq!(fund.raised, 59);
		});
	}

	#[test]
	fn contribute_handles_basic_errors() {
		new_test_ext().execute_with(|| {
			let para = new_para();

			// Cannot contribute to non-existing fund
			assert_noop!(Crowdloan::contribute(Origin::signed(1), para, 49, None), Error::<Test>::InvalidParaId);
			// Cannot contribute below minimum contribution
			assert_noop!(Crowdloan::contribute(Origin::signed(1), para, 9, None), Error::<Test>::ContributionTooSmall);

			// Set up a crowdloan
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 4, 9, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(1), para, 101, None));

			// Cannot contribute past the limit
			assert_noop!(Crowdloan::contribute(Origin::signed(2), para, 900, None), Error::<Test>::CapExceeded);

			// Move past end date
			run_to_block(10);

			// Cannot contribute to ended fund
			assert_noop!(Crowdloan::contribute(Origin::signed(1), para, 49, None), Error::<Test>::ContributionPeriodOver);

			// If a crowdloan has already won, it should not allow contributions.
			let para_2 = new_para();
			assert_ok!(Crowdloan::create(Origin::signed(1), para_2, 1000, 1, 4, 40, None));
			// Emulate a win by leasing out and putting a deposit. Slots pallet would normally do this.
			let crowdloan_account = Crowdloan::fund_account_id(para_2);
			set_winner(para_2, crowdloan_account, true);
			assert_noop!(Crowdloan::contribute(Origin::signed(1), para_2, 49, None), Error::<Test>::BidOrLeaseActive);

			// Move past lease period 1, should not be allowed to have further contributions with a crowdloan
			// that has starting period 1.
			let para_3 = new_para();
			assert_ok!(Crowdloan::create(Origin::signed(1), para_3, 1000, 1, 4, 40, None));
			run_to_block(40);
			assert_eq!(TestAuctioneer::lease_period_index(), 2);
			assert_noop!(Crowdloan::contribute(Origin::signed(1), para_3, 49, None), Error::<Test>::ContributionPeriodOver);
		});
	}

	#[test]
	fn cannot_contribute_during_vrf() {
		new_test_ext().execute_with(|| {
			set_vrf_delay(5);

			let para = new_para();
			let first_period = 1;
			let last_period = 4;

			assert_ok!(TestAuctioneer::new_auction(5, 0));

			// Set up a crowdloan
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, first_period, last_period, 20, None));

			run_to_block(8);
			// Can def contribute when auction is running.
			assert!(TestAuctioneer::auction_status(System::block_number()).is_ending().is_some());
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para, 250, None));

			run_to_block(10);
			// Can't contribute when auction is in the VRF delay period.
			assert!(TestAuctioneer::auction_status(System::block_number()).is_vrf());
			assert_noop!(Crowdloan::contribute(Origin::signed(2), para, 250, None), Error::<Test>::VrfDelayInProgress);

			run_to_block(15);
			// Its fine to contribute when no auction is running.
			assert!(!TestAuctioneer::auction_status(System::block_number()).is_in_progress());
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para, 250, None));
		})
	}

	#[test]
	fn bidding_works() {
		new_test_ext().execute_with(|| {
			let para = new_para();
			let first_period = 1;
			let last_period = 4;

			assert_ok!(TestAuctioneer::new_auction(5, 0));

			// Set up a crowdloan
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, first_period, last_period, 9, None));
			let bidder = Crowdloan::fund_account_id(para);

			// Fund crowdloan
			run_to_block(1);
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para, 100, None));
			run_to_block(3);
			assert_ok!(Crowdloan::contribute(Origin::signed(3), para, 150, None));
			run_to_block(5);
			assert_ok!(Crowdloan::contribute(Origin::signed(4), para, 200, None));
			run_to_block(8);
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para, 250, None));
			run_to_block(10);

			assert_eq!(bids(), vec![
				BidPlaced { height: 5, amount: 250, bidder, para, first_period, last_period },
				BidPlaced { height: 6, amount: 450, bidder, para, first_period, last_period },
				BidPlaced { height: 9, amount: 700, bidder, para, first_period, last_period },
			]);

			// Endings count incremented
			assert_eq!(Crowdloan::endings_count(), 1);
		});
	}

	#[test]
	fn withdraw_from_failed_works() {
		new_test_ext().execute_with(|| {
			let para = new_para();

			// Set up a crowdloan
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 1, 9, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para, 100, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(3), para, 50, None));

			run_to_block(10);
			let account_id = Crowdloan::fund_account_id(para);
			// para has no reserved funds, indicating it did not win the auction.
			assert_eq!(Balances::reserved_balance(&account_id), 0);
			// but there's still the funds in its balance.
			assert_eq!(Balances::free_balance(&account_id), 150);
			assert_eq!(Balances::free_balance(2), 1900);
			assert_eq!(Balances::free_balance(3), 2950);

			assert_ok!(Crowdloan::withdraw(Origin::signed(2), 2, para));
			assert_eq!(Balances::free_balance(&account_id), 50);
			assert_eq!(Balances::free_balance(2), 2000);

			assert_ok!(Crowdloan::withdraw(Origin::signed(2), 3, para));
			assert_eq!(Balances::free_balance(&account_id), 0);
			assert_eq!(Balances::free_balance(3), 3000);
		});
	}

	#[test]
	fn withdraw_cannot_be_griefed() {
		new_test_ext().execute_with(|| {
			let para = new_para();

			// Set up a crowdloan
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 1, 9, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para, 100, None));

			run_to_block(10);
			let account_id = Crowdloan::fund_account_id(para);

			// user sends the crowdloan funds trying to make an accounting error
			assert_ok!(Balances::transfer(Origin::signed(1), account_id, 10));

			// overfunded now
			assert_eq!(Balances::free_balance(&account_id), 110);
			assert_eq!(Balances::free_balance(2), 1900);

			assert_ok!(Crowdloan::withdraw(Origin::signed(2), 2, para));
			assert_eq!(Balances::free_balance(2), 2000);

			// Some funds are left over
			assert_eq!(Balances::free_balance(&account_id), 10);
			// They wil be left in the account at the end
			assert_ok!(Crowdloan::dissolve(Origin::signed(1), para));
			assert_eq!(Balances::free_balance(&account_id), 10);
		});
	}

	#[test]
	fn refund_works() {
		new_test_ext().execute_with(|| {
			let para = new_para();
			let account_id = Crowdloan::fund_account_id(para);

			// Set up a crowdloan ending on 9
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 1, 9, None));
			// Make some contributions
			assert_ok!(Crowdloan::contribute(Origin::signed(1), para, 100, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para, 200, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(3), para, 300, None));

			assert_eq!(Balances::free_balance(account_id), 600);

			// Can't refund before the crowdloan it has ended
			assert_noop!(
				Crowdloan::refund(Origin::signed(1337), para),
				Error::<Test>::FundNotEnded,
			);

			// Move to the end of the crowdloan
			run_to_block(10);
			assert_ok!(Crowdloan::refund(Origin::signed(1337), para));

			// Funds are returned
			assert_eq!(Balances::free_balance(account_id), 0);
			// 1 deposit for the crowdloan which hasn't dissolved yet.
			assert_eq!(Balances::free_balance(1), 1000 - 1);
			assert_eq!(Balances::free_balance(2), 2000);
			assert_eq!(Balances::free_balance(3), 3000);
		});
	}

	#[test]
	fn multiple_refund_works() {
		new_test_ext().execute_with(|| {
			let para = new_para();
			let account_id = Crowdloan::fund_account_id(para);

			// Set up a crowdloan ending on 9
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 100000, 1, 1, 9, None));
			// Make more contributions than our limit
			for i in 1 ..= RemoveKeysLimit::get() * 2 {
				Balances::make_free_balance_be(&i.into(), (1000 * i).into());
				assert_ok!(Crowdloan::contribute(Origin::signed(i.into()), para, (i * 100).into(), None));
			}

			assert_eq!(Balances::free_balance(account_id), 21000);

			// Move to the end of the crowdloan
			run_to_block(10);
			assert_ok!(Crowdloan::refund(Origin::signed(1337), para));
			assert_eq!(last_event(), super::Event::<Test>::PartiallyRefunded(para).into());

			// Funds still left over
			assert!(!Balances::free_balance(account_id).is_zero());

			// Call again
			assert_ok!(Crowdloan::refund(Origin::signed(1337), para));
			assert_eq!(last_event(), super::Event::<Test>::AllRefunded(para).into());

			// Funds are returned
			assert_eq!(Balances::free_balance(account_id), 0);
			// 1 deposit for the crowdloan which hasn't dissolved yet.
			for i in 1 ..= RemoveKeysLimit::get() * 2 {
				assert_eq!(Balances::free_balance(&i.into()), i as u64 * 1000);
			}
		});
	}

	#[test]
	fn refund_and_dissolve_works() {
		new_test_ext().execute_with(|| {
			let para = new_para();
			let issuance = Balances::total_issuance();

			// Set up a crowdloan
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 1, 9, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para, 100, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(3), para, 50, None));

			run_to_block(10);
			// All funds are refunded
			assert_ok!(Crowdloan::refund(Origin::signed(2), para));

			// Now that `fund.raised` is zero, it can be dissolved.
			assert_ok!(Crowdloan::dissolve(Origin::signed(1), para));
			assert_eq!(Balances::free_balance(1), 1000);
			assert_eq!(Balances::free_balance(2), 2000);
			assert_eq!(Balances::free_balance(3), 3000);
			assert_eq!(Balances::total_issuance(), issuance);
		});
	}

	#[test]
	fn dissolve_works() {
		new_test_ext().execute_with(|| {
			let para = new_para();
			let issuance = Balances::total_issuance();

			// Set up a crowdloan
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 1, 9, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para, 100, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(3), para, 50, None));

			// Can't dissolve before it ends
			assert_noop!(Crowdloan::dissolve(Origin::signed(1), para), Error::<Test>::NotReadyToDissolve);

			run_to_block(10);
			set_winner(para, 1, true);
			// Can't dissolve when it won.
			assert_noop!(Crowdloan::dissolve(Origin::signed(1), para), Error::<Test>::NotReadyToDissolve);
			set_winner(para, 1, false);

			// Can't dissolve while it still has user funds
			assert_noop!(Crowdloan::dissolve(Origin::signed(1), para), Error::<Test>::NotReadyToDissolve);

			// All funds are refunded
			assert_ok!(Crowdloan::refund(Origin::signed(2), para));

			// Now that `fund.raised` is zero, it can be dissolved.
			assert_ok!(Crowdloan::dissolve(Origin::signed(1), para));
			assert_eq!(Balances::free_balance(1), 1000);
			assert_eq!(Balances::free_balance(2), 2000);
			assert_eq!(Balances::free_balance(3), 3000);
			assert_eq!(Balances::total_issuance(), issuance);
		});
	}

	#[test]
	fn withdraw_from_finished_works() {
		new_test_ext().execute_with(|| {
			let para = new_para();
			let account_id = Crowdloan::fund_account_id(para);

			// Set up a crowdloan
			assert_ok!(Crowdloan::create(Origin::signed(1), para, 1000, 1, 1, 9, None));

			// Fund crowdloans.
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para, 100, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(3), para, 50, None));
			// simulate the reserving of para's funds. this actually happens in the Slots pallet.
			assert_ok!(Balances::reserve(&account_id, 150));

			run_to_block(19);
			assert_noop!(Crowdloan::withdraw(Origin::signed(2), 2, para), Error::<Test>::BidOrLeaseActive);

			run_to_block(20);
			// simulate the unreserving of para's funds, now that the lease expired. this actually
			// happens in the Slots pallet.
			Balances::unreserve(&account_id, 150);

			// para has no reserved funds, indicating it did ot win the auction.
			assert_eq!(Balances::reserved_balance(&account_id), 0);
			// but there's still the funds in its balance.
			assert_eq!(Balances::free_balance(&account_id), 150);
			assert_eq!(Balances::free_balance(2), 1900);
			assert_eq!(Balances::free_balance(3), 2950);

			assert_ok!(Crowdloan::withdraw(Origin::signed(2), 2, para));
			assert_eq!(Balances::free_balance(&account_id), 50);
			assert_eq!(Balances::free_balance(2), 2000);

			assert_ok!(Crowdloan::withdraw(Origin::signed(2), 3, para));
			assert_eq!(Balances::free_balance(&account_id), 0);
			assert_eq!(Balances::free_balance(3), 3000);
		});
	}

	#[test]
	fn on_swap_works() {
		new_test_ext().execute_with(|| {
			let para_1 = new_para();
			let para_2 = new_para();

			// Set up crowdloans
			assert_ok!(Crowdloan::create(Origin::signed(1), para_1, 1000, 1, 1, 9, None));
			assert_ok!(Crowdloan::create(Origin::signed(1), para_2, 1000, 1, 1, 9, None));
			// Different contributions
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para_1, 100, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(3), para_2, 50, None));
			// Original state
			assert_eq!(Funds::<Test>::get(para_1).unwrap().raised, 100);
			assert_eq!(Funds::<Test>::get(para_2).unwrap().raised, 50);
			// Swap
			Crowdloan::on_swap(para_1, para_2);
			// Final state
			assert_eq!(Funds::<Test>::get(para_2).unwrap().raised, 100);
			assert_eq!(Funds::<Test>::get(para_1).unwrap().raised, 50);
		});
	}

	#[test]
	fn cannot_create_fund_when_already_active() {
		new_test_ext().execute_with(|| {
			let para_1 = new_para();

			assert_ok!(Crowdloan::create(Origin::signed(1), para_1, 1000, 1, 1, 9, None));
			// Cannot create a fund again
			assert_noop!(
				Crowdloan::create(Origin::signed(1), para_1, 1000, 1, 1, 9, None),
				Error::<Test>::FundNotEnded,
			);

		});
	}

	#[test]
	fn edit_works() {
		new_test_ext().execute_with(|| {
			let para_1 = new_para();

			assert_ok!(Crowdloan::create(Origin::signed(1), para_1, 1000, 1, 1, 9, None));
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para_1, 100, None));
			let old_crowdloan = Crowdloan::funds(para_1).unwrap();

			assert_ok!(Crowdloan::edit(Origin::root(), para_1, 1234, 2, 3, 4, None));
			let new_crowdloan = Crowdloan::funds(para_1).unwrap();

			// Some things stay the same
			assert_eq!(old_crowdloan.depositor, new_crowdloan.depositor);
			assert_eq!(old_crowdloan.deposit, new_crowdloan.deposit);
			assert_eq!(old_crowdloan.raised, new_crowdloan.raised);

			// Some things change
			assert!(old_crowdloan.cap != new_crowdloan.cap);
			assert!(old_crowdloan.first_period != new_crowdloan.first_period);
			assert!(old_crowdloan.last_period != new_crowdloan.last_period);
		});
	}

	#[test]
	fn add_memo_works() {
		new_test_ext().execute_with(|| {
			let para_1 = new_para();

			assert_ok!(Crowdloan::create(Origin::signed(1), para_1, 1000, 1, 1, 9, None));
			// Cant add a memo before you have contributed.
			assert_noop!(
				Crowdloan::add_memo(Origin::signed(1), para_1, b"hello, world".to_vec()),
				Error::<Test>::NoContributions,
			);
			// Make a contribution. Initially no memo.
			assert_ok!(Crowdloan::contribute(Origin::signed(1), para_1, 100, None));
			assert_eq!(Crowdloan::contribution_get(0u32, &1), (100, vec![]));
			// Can't place a memo that is too large.
			assert_noop!(
				Crowdloan::add_memo(Origin::signed(1), para_1, vec![123; 123]),
				Error::<Test>::MemoTooLarge,
			);
			// Adding a memo to an existing contribution works
			assert_ok!(Crowdloan::add_memo(Origin::signed(1), para_1, b"hello, world".to_vec()));
			assert_eq!(Crowdloan::contribution_get(0u32, &1), (100, b"hello, world".to_vec()));
			// Can contribute again and data persists
			assert_ok!(Crowdloan::contribute(Origin::signed(1), para_1, 100, None));
			assert_eq!(Crowdloan::contribution_get(0u32, &1), (200, b"hello, world".to_vec()));
		});
	}

	#[test]
	fn poke_works() {
		new_test_ext().execute_with(|| {
			let para_1 = new_para();

			assert_ok!(TestAuctioneer::new_auction(5, 0));
			assert_ok!(Crowdloan::create(Origin::signed(1), para_1, 1000, 1, 1, 9, None));
			// Should fail when no contributions.
			assert_noop!(
				Crowdloan::poke(Origin::signed(1), para_1),
				Error::<Test>::NoContributions
			);
			assert_ok!(Crowdloan::contribute(Origin::signed(2), para_1, 100, None));
			run_to_block(6);
			assert_ok!(Crowdloan::poke(Origin::signed(1), para_1));
			assert_eq!(Crowdloan::new_raise(), vec![para_1]);
			assert_noop!(
				Crowdloan::poke(Origin::signed(1), para_1),
				Error::<Test>::AlreadyInNewRaise
			);
		});
	}
}

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking {
	use super::{*, Pallet as Crowdloan};
	use frame_system::RawOrigin;
	use frame_support::{
		assert_ok,
		traits::OnInitialize,
	};
	use sp_runtime::traits::{Bounded, CheckedSub};
	use sp_std::prelude::*;

	use frame_benchmarking::{benchmarks, whitelisted_caller, account, impl_benchmark_test_suite};

	fn assert_last_event<T: Config>(generic_event: <T as Config>::Event) {
		let events = frame_system::Pallet::<T>::events();
		let system_event: <T as frame_system::Config>::Event = generic_event.into();
		// compare to the last event record
		let frame_system::EventRecord { event, .. } = &events[events.len() - 1];
		assert_eq!(event, &system_event);
	}

	fn create_fund<T: Config>(id: u32, end: T::BlockNumber) -> ParaId {
		let cap = BalanceOf::<T>::max_value();
		let lease_period_index = T::Auctioneer::lease_period_index();
		let first_period = lease_period_index;
		let last_period = lease_period_index + ((SlotRange::LEASE_PERIODS_PER_SLOT as u32) - 1).into();
		let para_id = id.into();

		let caller = account("fund_creator", id, 0);
		CurrencyOf::<T>::make_free_balance_be(&caller, BalanceOf::<T>::max_value());

		// Assume ed25519 is most complex signature format
		let pubkey = crypto::create_ed25519_pubkey(b"//verifier".to_vec());

		let head_data = T::Registrar::worst_head_data();
		let validation_code = T::Registrar::worst_validation_code();
		assert_ok!(T::Registrar::register(caller.clone(), para_id, head_data, validation_code));
		T::Registrar::execute_pending_transitions();

		assert_ok!(Crowdloan::<T>::create(
			RawOrigin::Signed(caller).into(),
			para_id,
			cap,
			first_period,
			last_period,
			end,
			Some(pubkey)
		));

		para_id
	}

	fn contribute_fund<T: Config>(who: &T::AccountId, index: ParaId) {
		CurrencyOf::<T>::make_free_balance_be(&who, BalanceOf::<T>::max_value());
		let value = T::MinContribution::get();

		let pubkey = crypto::create_ed25519_pubkey(b"//verifier".to_vec());
		let payload = (index, &who, BalanceOf::<T>::default(), value);
		let sig = crypto::create_ed25519_signature(&payload.encode(), pubkey);

		assert_ok!(Crowdloan::<T>::contribute(RawOrigin::Signed(who.clone()).into(), index, value, Some(sig)));
	}

	benchmarks! {
		create {
			let para_id = ParaId::from(1);
			let cap = BalanceOf::<T>::max_value();
			let first_period = 0u32.into();
			let last_period = 3u32.into();
			let end = T::Auctioneer::lease_period();

			let caller: T::AccountId = whitelisted_caller();
			let head_data = T::Registrar::worst_head_data();
			let validation_code = T::Registrar::worst_validation_code();

			let verifier = account("verifier", 0, 0);

			CurrencyOf::<T>::make_free_balance_be(&caller, BalanceOf::<T>::max_value());
			T::Registrar::register(caller.clone(), para_id, head_data, validation_code)?;
			T::Registrar::execute_pending_transitions();

		}: _(RawOrigin::Signed(caller), para_id, cap, first_period, last_period, end, Some(verifier))
		verify {
			assert_last_event::<T>(Event::<T>::Created(para_id).into())
		}

		// Contribute has two arms: PreEnding and Ending, but both are equal complexity.
		contribute {
			let fund_index = create_fund::<T>(1, 100u32.into());
			let caller: T::AccountId = whitelisted_caller();
			let contribution = T::MinContribution::get();
			CurrencyOf::<T>::make_free_balance_be(&caller, BalanceOf::<T>::max_value());
			assert!(NewRaise::<T>::get().is_empty());

			let pubkey = crypto::create_ed25519_pubkey(b"//verifier".to_vec());
			let payload = (fund_index, &caller, BalanceOf::<T>::default(), contribution);
			let sig = crypto::create_ed25519_signature(&payload.encode(), pubkey);

		}: _(RawOrigin::Signed(caller.clone()), fund_index, contribution, Some(sig))
		verify {
			// NewRaise is appended to, so we don't need to fill it up for worst case scenario.
			assert!(!NewRaise::<T>::get().is_empty());
			assert_last_event::<T>(Event::<T>::Contributed(caller, fund_index, contribution).into());
		}

		withdraw {
			let fund_index = create_fund::<T>(1337, 100u32.into());
			let caller: T::AccountId = whitelisted_caller();
			let contributor = account("contributor", 0, 0);
			contribute_fund::<T>(&contributor, fund_index);
			frame_system::Pallet::<T>::set_block_number(200u32.into());
		}: _(RawOrigin::Signed(caller), contributor.clone(), fund_index)
		verify {
			assert_last_event::<T>(Event::<T>::Withdrew(contributor, fund_index, T::MinContribution::get()).into());
		}

		// Worst case: Refund removes `RemoveKeysLimit` keys, and is fully refunded.
		refund {
			let k in 0 .. T::RemoveKeysLimit::get();
			let fund_index = create_fund::<T>(1337, 100u32.into());

			// Dissolve will remove at most `RemoveKeysLimit` at once.
			for i in 0 .. k {
				contribute_fund::<T>(&account("contributor", i, 0), fund_index);
			}

			let caller: T::AccountId = whitelisted_caller();
			frame_system::Pallet::<T>::set_block_number(200u32.into());
		}: _(RawOrigin::Signed(caller), fund_index)
		verify {
			assert_last_event::<T>(Event::<T>::AllRefunded(fund_index).into());
		}

		dissolve {
			let fund_index = create_fund::<T>(1337, 100u32.into());
			let caller: T::AccountId = whitelisted_caller();
			frame_system::Pallet::<T>::set_block_number(T::BlockNumber::max_value());
		}: _(RawOrigin::Signed(caller.clone()), fund_index)
		verify {
			assert_last_event::<T>(Event::<T>::Dissolved(fund_index).into());
		}

		edit {
			let para_id = ParaId::from(1);
			let cap = BalanceOf::<T>::max_value();
			let first_period = 0u32.into();
			let last_period = 3u32.into();
			let end = T::Auctioneer::lease_period();

			let caller: T::AccountId = whitelisted_caller();
			let head_data = T::Registrar::worst_head_data();
			let validation_code = T::Registrar::worst_validation_code();

			let verifier: MultiSigner = account("verifier", 0, 0);

			CurrencyOf::<T>::make_free_balance_be(&caller, BalanceOf::<T>::max_value());
			T::Registrar::register(caller.clone(), para_id, head_data, validation_code)?;
			T::Registrar::execute_pending_transitions();

			Crowdloan::<T>::create(
				RawOrigin::Signed(caller).into(),
				para_id, cap, first_period, last_period, end, Some(verifier.clone()),
			)?;

			// Doesn't matter what we edit to, so use the same values.
		}: _(RawOrigin::Root, para_id, cap, first_period, last_period, end, Some(verifier))
		verify {
			assert_last_event::<T>(Event::<T>::Edited(para_id).into())
		}

		add_memo {
			let fund_index = create_fund::<T>(1, 100u32.into());
			let caller: T::AccountId = whitelisted_caller();
			contribute_fund::<T>(&caller, fund_index);
			let worst_memo = vec![42; T::MaxMemoLength::get().into()];
		}: _(RawOrigin::Signed(caller.clone()), fund_index, worst_memo.clone())
		verify {
			let fund = Funds::<T>::get(fund_index).expect("fund was created...");
			assert_eq!(
				Crowdloan::<T>::contribution_get(fund.trie_index, &caller),
				(T::MinContribution::get(), worst_memo),
			);
		}

		poke {
			let fund_index = create_fund::<T>(1, 100u32.into());
			let caller: T::AccountId = whitelisted_caller();
			contribute_fund::<T>(&caller, fund_index);
			NewRaise::<T>::kill();
			assert!(NewRaise::<T>::get().is_empty());
		}: _(RawOrigin::Signed(caller), fund_index)
		verify {
			assert!(!NewRaise::<T>::get().is_empty());
			assert_last_event::<T>(Event::<T>::AddedToNewRaise(fund_index).into())
		}

		// Worst case scenario: N funds are all in the `NewRaise` list, we are
		// in the beginning of the ending period, and each fund outbids the next
		// over the same periods.
		on_initialize {
			// We test the complexity over different number of new raise
			let n in 2 .. 100;
			let end_block: T::BlockNumber = 100u32.into();

			let pubkey = crypto::create_ed25519_pubkey(b"//verifier".to_vec());

			for i in 0 .. n {
				let fund_index = create_fund::<T>(i, end_block);
				let contributor: T::AccountId = account("contributor", i, 0);
				let contribution = T::MinContribution::get() * (i + 1).into();
				let payload = (fund_index, &contributor, BalanceOf::<T>::default(), contribution);
				let sig = crypto::create_ed25519_signature(&payload.encode(), pubkey.clone());

				CurrencyOf::<T>::make_free_balance_be(&contributor, BalanceOf::<T>::max_value());
				Crowdloan::<T>::contribute(RawOrigin::Signed(contributor).into(), fund_index, contribution, Some(sig))?;
			}

			let lease_period_index = T::Auctioneer::lease_period_index();
			let duration = end_block
				.checked_sub(&frame_system::Pallet::<T>::block_number())
				.ok_or("duration of auction less than zero")?;
			T::Auctioneer::new_auction(duration, lease_period_index)?;

			assert_eq!(T::Auctioneer::auction_status(end_block).is_ending(), Some((0u32.into(), 0u32.into())));
			assert_eq!(NewRaise::<T>::get().len(), n as usize);
			let old_endings_count = EndingsCount::<T>::get();
		}: {
			Crowdloan::<T>::on_initialize(end_block);
		} verify {
			assert_eq!(EndingsCount::<T>::get(), old_endings_count + 1);
			assert_last_event::<T>(Event::<T>::HandleBidResult((n - 1).into(), Ok(())).into());
		}
	}

	impl_benchmark_test_suite!(
		Crowdloan,
		crate::integration_tests::new_test_ext(),
		crate::integration_tests::Test,
	);
}
