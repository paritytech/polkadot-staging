// Copyright 2019-2020 Parity Technologies (UK) Ltd.
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

//! Auctioning system to determine the set of Parachains in operation. This includes logic for the
//! auctioning mechanism and for reserving balance as part of the "payment". Unreserving the balance
//! happens elsewhere.

use sp_std::{prelude::*, mem::swap};
use sp_runtime::traits::{CheckedSub, Zero, One, Saturating};
use frame_support::{
	ensure, dispatch::DispatchResult,
	traits::{Randomness, Currency, ReservableCurrency, Get},
	weights::{Weight},
};
use primitives::v1::Id as ParaId;
use crate::slot_range::SlotRange;
use crate::traits::{Leaser, LeaseError, Auctioneer, Registrar, AuctionStatus};
use parity_scale_codec::Decode;
pub use pallet::*;

type CurrencyOf<T> = <<T as Config>::Leaser as Leaser>::Currency;
type BalanceOf<T> =
	<<<T as Config>::Leaser as Leaser>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

pub trait WeightInfo {
	fn new_auction() -> Weight;
	fn bid() -> Weight;
	fn cancel_auction() -> Weight;
	fn on_initialize() -> Weight;
}

pub struct TestWeightInfo;
impl WeightInfo for TestWeightInfo {
	fn new_auction() -> Weight { 0 }
	fn bid() -> Weight { 0 }
	fn cancel_auction() -> Weight { 0 }
	fn on_initialize() -> Weight { 0 }
}

/// An auction index. We count auctions in this type.
pub type AuctionIndex = u32;

type LeasePeriodOf<T> = <<T as Config>::Leaser as Leaser>::LeasePeriod;
// Winning data type. This encodes the top bidders of each range together with their bid.
type WinningData<T> =
	[Option<(<T as frame_system::Config>::AccountId, ParaId, BalanceOf<T>)>; SlotRange::SLOT_RANGE_COUNT];
// Winners data type. This encodes each of the final winners of a parachain auction, the parachain
// index assigned to them, their winning bid and the range that they won.
type WinnersData<T> = Vec<(<T as frame_system::Config>::AccountId, ParaId, BalanceOf<T>, SlotRange)>;

#[frame_support::pallet]
pub mod pallet {
	use frame_support::{pallet_prelude::*, traits::EnsureOrigin, weights::DispatchClass};
	use frame_system::{pallet_prelude::*, ensure_signed, ensure_root};
	use super::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	/// The module's configuration trait.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// The type representing the leasing system.
		type Leaser: Leaser<AccountId=Self::AccountId, LeasePeriod=Self::BlockNumber>;

		/// The parachain registrar type.
		type Registrar: Registrar<AccountId=Self::AccountId>;

		/// The number of blocks over which an auction may be retroactively ended.
		#[pallet::constant]
		type EndingPeriod: Get<Self::BlockNumber>;

		/// The length of each sample to take during the ending period.
		///
		/// EndingPeriod / SampleLength = Total # of Samples
		#[pallet::constant]
		type SampleLength: Get<Self::BlockNumber>;

		/// Something that provides randomness in the runtime.
		type Randomness: Randomness<Self::Hash, Self::BlockNumber>;

		/// The origin which may initiate auctions.
		type InitiateOrigin: EnsureOrigin<Self::Origin>;

		/// Weight Information for the Extrinsics in the Pallet
		type WeightInfo: WeightInfo;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// An auction started. Provides its index and the block number where it will begin to
		/// close and the first lease period of the quadruplet that is auctioned.
		/// [auction_index, lease_period, ending]
		AuctionStarted(AuctionIndex, LeasePeriodOf<T>, T::BlockNumber),
		/// An auction ended. All funds become unreserved. [auction_index]
		AuctionClosed(AuctionIndex),
		/// Funds were reserved for a winning bid. First balance is the extra amount reserved.
		/// Second is the total. [bidder, extra_reserved, total_amount]
		Reserved(T::AccountId, BalanceOf<T>, BalanceOf<T>),
		/// Funds were unreserved since bidder is no longer active. [bidder, amount]
		Unreserved(T::AccountId, BalanceOf<T>),
		/// Someone attempted to lease the same slot twice for a parachain. The amount is held in reserve
		/// but no parachain slot has been leased.
		/// \[parachain_id, leaser, amount\]
		ReserveConfiscated(ParaId, T::AccountId, BalanceOf<T>),
		/// A new bid has been accepted as the current winner.
		/// \[who, para_id, amount, first_slot, last_slot\]
		BidAccepted(T::AccountId, ParaId, BalanceOf<T>, LeasePeriodOf<T>, LeasePeriodOf<T>),
		/// The winning offset was chosen for an auction. This will map into the `Winning` storage map.
		/// \[auction_index, block_number\]
		WinningOffset(AuctionIndex, T::BlockNumber),
	}

	#[pallet::error]
	pub enum Error<T> {
		/// This auction is already in progress.
		AuctionInProgress,
		/// The lease period is in the past.
		LeasePeriodInPast,
		/// Para is not registered
		ParaNotRegistered,
		/// Not a current auction.
		NotCurrentAuction,
		/// Not an auction.
		NotAuction,
		/// Auction has already ended.
		AuctionEnded,
		/// The para is already leased out for part of this range.
		AlreadyLeasedOut,
	}

	/// Number of auctions started so far.
	#[pallet::storage]
	#[pallet::getter(fn auction_counter)]
	pub type AuctionCounter<T> = StorageValue<_, AuctionIndex, ValueQuery>;

	/// Information relating to the current auction, if there is one.
	///
	/// The first item in the tuple is the lease period index that the first of the four
	/// contiguous lease periods on auction is for. The second is the block number when the
	/// auction will "begin to end", i.e. the first block of the Ending Period of the auction.
	#[pallet::storage]
	#[pallet::getter(fn auction_info)]
	pub type AuctionInfo<T: Config> = StorageValue<_, (LeasePeriodOf<T>, T::BlockNumber)>;

	/// Amounts currently reserved in the accounts of the bidders currently winning
	/// (sub-)ranges.
	#[pallet::storage]
	#[pallet::getter(fn reserved_amounts)]
	pub type ReservedAmounts<T: Config> = StorageMap<_, Twox64Concat, (T::AccountId, ParaId), BalanceOf<T>>;

	/// The winning bids for each of the 10 ranges at each sample in the final Ending Period of
	/// the current auction. The map's key is the 0-based index into the Sample Size. The
	/// first sample of the ending period is 0; the last is `Sample Size - 1`.
	#[pallet::storage]
	#[pallet::getter(fn winning)]
	pub type Winning<T: Config> = StorageMap<_, Twox64Concat, T::BlockNumber, WinningData<T>>;

	#[pallet::extra_constants]
	impl<T: Config> Pallet<T> {
		//TODO: rename to snake case after https://github.com/paritytech/substrate/issues/8826 fixed.
		#[allow(non_snake_case)]
		fn SlotRangeCount() -> u32 {
			SlotRange::SLOT_RANGE_COUNT as u32
		}

		//TODO: rename to snake case after https://github.com/paritytech/substrate/issues/8826 fixed.
		#[allow(non_snake_case)]
		fn LeasePeriodsPerSlot() -> u32 {
			SlotRange::LEASE_PERIODS_PER_SLOT as u32
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(n: T::BlockNumber) -> Weight {
			let mut weight = T::DbWeight::get().reads(1);

			// If the current auction was in its ending period last block, then ensure that the (sub-)range
			// winner information is duplicated from the previous block in case no bids happened in the
			// last block.
			if let AuctionStatus::EndingPeriod(offset, _sub_sample) = Self::auction_status(n) {
				weight = weight.saturating_add(T::DbWeight::get().reads(1));
				if !Winning::<T>::contains_key(&offset) {
					weight = weight.saturating_add(T::DbWeight::get().writes(1));
					let winning_data = offset.checked_sub(&One::one())
							.and_then(Winning::<T>::get)
							.unwrap_or([Self::EMPTY; SlotRange::SLOT_RANGE_COUNT]);
					Winning::<T>::insert(offset, winning_data);
				}
			}

			// Check to see if an auction just ended.
			if let Some((winning_ranges, auction_lease_period_index)) = Self::check_auction_end(n) {
				// Auction is ended now. We have the winning ranges and the lease period index which
				// acts as the offset. Handle it.
				Self::manage_auction_end(
					auction_lease_period_index,
					winning_ranges,
				);
				weight = weight.saturating_add(T::WeightInfo::on_initialize());
			}

			weight
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Create a new auction.
		///
		/// This can only happen when there isn't already an auction in progress and may only be
		/// called by the root origin. Accepts the `duration` of this auction and the
		/// `lease_period_index` of the initial lease period of the four that are to be auctioned.
		#[pallet::weight((T::WeightInfo::new_auction(), DispatchClass::Operational))]
		pub fn new_auction(
			origin: OriginFor<T>,
			#[pallet::compact] duration: T::BlockNumber,
			#[pallet::compact] lease_period_index: LeasePeriodOf<T>,
		) -> DispatchResult {
			T::InitiateOrigin::ensure_origin(origin)?;
			Self::do_new_auction(duration, lease_period_index)
		}

		/// Make a new bid from an account (including a parachain account) for deploying a new
		/// parachain.
		///
		/// Multiple simultaneous bids from the same bidder are allowed only as long as all active
		/// bids overlap each other (i.e. are mutually exclusive). Bids cannot be redacted.
		///
		/// - `sub` is the sub-bidder ID, allowing for multiple competing bids to be made by (and
		/// funded by) the same account.
		/// - `auction_index` is the index of the auction to bid on. Should just be the present
		/// value of `AuctionCounter`.
		/// - `first_slot` is the first lease period index of the range to bid on. This is the
		/// absolute lease period index value, not an auction-specific offset.
		/// - `last_slot` is the last lease period index of the range to bid on. This is the
		/// absolute lease period index value, not an auction-specific offset.
		/// - `amount` is the amount to bid to be held as deposit for the parachain should the
		/// bid win. This amount is held throughout the range.
		#[pallet::weight(T::WeightInfo::bid())]
		pub fn bid(
			origin: OriginFor<T>,
			#[pallet::compact] para: ParaId,
			#[pallet::compact] auction_index: AuctionIndex,
			#[pallet::compact] first_slot: LeasePeriodOf<T>,
			#[pallet::compact] last_slot: LeasePeriodOf<T>,
			#[pallet::compact] amount: BalanceOf<T>
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::handle_bid(who, para, auction_index, first_slot, last_slot, amount)?;
			Ok(())
		}

		/// Cancel an in-progress auction.
		///
		/// Can only be called by Root origin.
		#[pallet::weight(T::WeightInfo::cancel_auction())]
		pub fn cancel_auction(origin: OriginFor<T>) -> DispatchResult {
			ensure_root(origin)?;
			// Unreserve all bids.
			for ((bidder, _), amount) in ReservedAmounts::<T>::drain() {
				CurrencyOf::<T>::unreserve(&bidder, amount);
			}
			Winning::<T>::remove_all(None);
			AuctionInfo::<T>::kill();
			Ok(())
		}
	}
}

impl<T: Config> Auctioneer for Pallet<T> {
	type AccountId = T::AccountId;
	type BlockNumber = T::BlockNumber;
	type LeasePeriod = T::BlockNumber;
	type Currency = CurrencyOf<T>;

	fn new_auction(
		duration: T::BlockNumber,
		lease_period_index: LeasePeriodOf<T>,
	) -> DispatchResult {
		Self::do_new_auction(duration, lease_period_index)
	}

	// Returns the status of the auction given the current block number.
	fn auction_status(now: Self::BlockNumber) -> AuctionStatus<Self::BlockNumber> {
		let early_end = match AuctionInfo::<T>::get() {
			Some((_, early_end)) => early_end,
			None => return AuctionStatus::NotStarted,
		};

		let after_early_end = match now.checked_sub(&early_end) {
			Some(after_early_end) => after_early_end,
			None => return AuctionStatus::StartingPeriod,
		};

		let ending_period = T::EndingPeriod::get();
		if after_early_end < ending_period {
			let sample_length = T::SampleLength::get().max(One::one());
			let sample = after_early_end / sample_length;
			let sub_sample = after_early_end % sample_length;
			return AuctionStatus::EndingPeriod(sample, sub_sample)
		} else {
			// This is safe because of the comparison operator above
			return AuctionStatus::VrfDelay(after_early_end - ending_period)
		}
	}

	fn place_bid(
		bidder: T::AccountId,
		para: ParaId,
		first_slot: LeasePeriodOf<T>,
		last_slot: LeasePeriodOf<T>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		Self::handle_bid(bidder, para, AuctionCounter::<T>::get(), first_slot, last_slot, amount)
	}

	fn lease_period_index() -> Self::LeasePeriod {
		T::Leaser::lease_period_index()
	}

	fn lease_period() -> Self::LeasePeriod {
		T::Leaser::lease_period()
	}

	fn has_won_an_auction(para: ParaId, bidder: &T::AccountId) -> bool {
		!T::Leaser::deposit_held(para, bidder).is_zero()
	}
}

impl<T: Config> Pallet<T> {
	// A trick to allow me to initialize large arrays with nothing in them.
	const EMPTY: Option<(<T as frame_system::Config>::AccountId, ParaId, BalanceOf<T>)> = None;

	/// Create a new auction.
	///
	/// This can only happen when there isn't already an auction in progress. Accepts the `duration`
	/// of this auction and the `lease_period_index` of the initial lease period of the four that
	/// are to be auctioned.
	fn do_new_auction(
		duration: T::BlockNumber,
		lease_period_index: LeasePeriodOf<T>,
	) -> DispatchResult {
		let maybe_auction = AuctionInfo::<T>::get();
		ensure!(maybe_auction.is_none(), Error::<T>::AuctionInProgress);
		ensure!(lease_period_index >= T::Leaser::lease_period_index(), Error::<T>::LeasePeriodInPast);

		// Bump the counter.
		let n = AuctionCounter::<T>::mutate(|n| { *n += 1; *n });

		// Set the information.
		let ending = frame_system::Pallet::<T>::block_number().saturating_add(duration);
		AuctionInfo::<T>::put((lease_period_index, ending));

		Self::deposit_event(Event::<T>::AuctionStarted(n, lease_period_index, ending));
		Ok(())
	}

	/// Actually place a bid in the current auction.
	///
	/// - `bidder`: The account that will be funding this bid.
	/// - `auction_index`: The auction index of the bid. For this to succeed, must equal
	/// the current value of `AuctionCounter`.
	/// - `first_slot`: The first lease period index of the range to be bid on.
	/// - `last_slot`: The last lease period index of the range to be bid on (inclusive).
	/// - `amount`: The total amount to be the bid for deposit over the range.
	pub fn handle_bid(
		bidder: T::AccountId,
		para: ParaId,
		auction_index: u32,
		first_slot: LeasePeriodOf<T>,
		last_slot: LeasePeriodOf<T>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		// Ensure para is registered before placing a bid on it.
		ensure!(T::Registrar::is_registered(para), Error::<T>::ParaNotRegistered);
		// Bidding on latest auction.
		ensure!(auction_index == AuctionCounter::<T>::get(), Error::<T>::NotCurrentAuction);
		// Assume it's actually an auction (this should never fail because of above).
		let (first_lease_period, _) = AuctionInfo::<T>::get().ok_or(Error::<T>::NotAuction)?;

		// Get the auction status and the current sample block. For the starting period, the sample
		// block is zero.
		let auction_status = Self::auction_status(frame_system::Pallet::<T>::block_number());
		// The offset into the ending samples of the auction.
		let offset = match auction_status {
			AuctionStatus::NotStarted => return Err(Error::<T>::AuctionEnded.into()),
			AuctionStatus::StartingPeriod => Zero::zero(),
			AuctionStatus::EndingPeriod(o, _) => o,
			AuctionStatus::VrfDelay(_) => return Err(Error::<T>::AuctionEnded.into()),
		};

		// We also make sure that the bid is not for any existing leases the para already has.
		ensure!(!T::Leaser::already_leased(para, first_slot, last_slot), Error::<T>::AlreadyLeasedOut);

		// Our range.
		let range = SlotRange::new_bounded(first_lease_period, first_slot, last_slot)?;
		// Range as an array index.
		let range_index = range as u8 as usize;

		// The current winning ranges.
		let mut current_winning = Winning::<T>::get(offset)
			.or_else(|| offset.checked_sub(&One::one()).and_then(Winning::<T>::get))
			.unwrap_or([Self::EMPTY; SlotRange::SLOT_RANGE_COUNT]);

		// If this bid beat the previous winner of our range.
		if current_winning[range_index].as_ref().map_or(true, |last| amount > last.2) {
			// Ok; we are the new winner of this range - reserve the additional amount and record.

			// Get the amount already held on deposit if this is a renewal bid (i.e. there's
			// an existing lease on the same para by the same leaser).
			let existing_lease_deposit = T::Leaser::deposit_held(para, &bidder);
			let reserve_required = amount.saturating_sub(existing_lease_deposit);

			// Get the amount already reserved in any prior and still active bids by us.
			let bidder_para = (bidder.clone(), para);
			let already_reserved = ReservedAmounts::<T>::get(&bidder_para).unwrap_or_default();

			// If these don't already cover the bid...
			if let Some(additional) = reserve_required.checked_sub(&already_reserved) {
				// ...then reserve some more funds from their account, failing if there's not
				// enough funds.
				CurrencyOf::<T>::reserve(&bidder, additional)?;
				// ...and record the amount reserved.
				ReservedAmounts::<T>::insert(&bidder_para, reserve_required);

				Self::deposit_event(Event::<T>::Reserved(
					bidder.clone(),
					additional,
					reserve_required,
				));
			}

			// Return any funds reserved for the previous winner if we are not in the ending period
			// and they no longer have any active bids.
			let mut outgoing_winner = Some((bidder.clone(), para, amount));
			swap(&mut current_winning[range_index], &mut outgoing_winner);
			if let Some((who, para, _amount)) = outgoing_winner {
				if auction_status.is_starting() && current_winning.iter()
					.filter_map(Option::as_ref)
					.all(|&(ref other, other_para, _)| other != &who || other_para != para)
				{
					// Previous bidder is no longer winning any ranges: unreserve their funds.
					if let Some(amount) = ReservedAmounts::<T>::take(&(who.clone(), para)) {
						// It really should be reserved; there's not much we can do here on fail.
						let err_amt = CurrencyOf::<T>::unreserve(&who, amount);
						debug_assert!(err_amt.is_zero());
						Self::deposit_event(Event::<T>::Unreserved(who, amount));
					}
				}
			}

			// Update the range winner.
			Winning::<T>::insert(offset, &current_winning);
			Self::deposit_event(Event::<T>::BidAccepted(bidder, para, amount, first_slot, last_slot));
		}
		Ok(())
	}

	/// Some when the auction's end is known (with the end block number). None if it is unknown.
	/// If `Some` then the block number must be at most the previous block and at least the
	/// previous block minus `T::EndingPeriod::get()`.
	///
	/// This mutates the state, cleaning up `AuctionInfo` and `Winning` in the case of an auction
	/// ending. An immediately subsequent call with the same argument will always return `None`.
	fn check_auction_end(now: T::BlockNumber) -> Option<(WinningData<T>, LeasePeriodOf<T>)> {
		if let Some((lease_period_index, early_end)) = AuctionInfo::<T>::get() {
			let ending_period = T::EndingPeriod::get();
			let late_end = early_end.saturating_add(ending_period);
			let is_ended = now >= late_end;
			if is_ended {
				// auction definitely ended.
				// check to see if we can determine the actual ending point.
				let (raw_offset, known_since) = T::Randomness::random(&b"para_auction"[..]);

				if late_end <= known_since {
					// Our random seed was known only after the auction ended. Good to use.
					let raw_offset_block_number = <T::BlockNumber>::decode(&mut raw_offset.as_ref())
						.expect("secure hashes should always be bigger than the block number; qed");
					let offset = (raw_offset_block_number % ending_period) / T::SampleLength::get().max(One::one());

					let auction_counter = AuctionCounter::<T>::get();
					Self::deposit_event(Event::<T>::WinningOffset(auction_counter, offset));
					let res = Winning::<T>::get(offset).unwrap_or([Self::EMPTY; SlotRange::SLOT_RANGE_COUNT]);
					// This `remove_all` statement should remove at most `EndingPeriod` / `SampleLength` items,
					// which should be bounded and sensibly configured in the runtime.
					Winning::<T>::remove_all(None);
					AuctionInfo::<T>::kill();
					return Some((res, lease_period_index))
				}
			}
		}
		None
	}

	/// Auction just ended. We have the current lease period, the auction's lease period (which
	/// is guaranteed to be at least the current period) and the bidders that were winning each
	/// range at the time of the auction's close.
	fn manage_auction_end(
		auction_lease_period_index: LeasePeriodOf<T>,
		winning_ranges: WinningData<T>,
	) {
		// First, unreserve all amounts that were reserved for the bids. We will later re-reserve the
		// amounts from the bidders that ended up being assigned the slot so there's no need to
		// special-case them here.
		for ((bidder, _), amount) in ReservedAmounts::<T>::drain() {
			CurrencyOf::<T>::unreserve(&bidder, amount);
		}

		// Next, calculate the winning combination of slots and thus the final winners of the
		// auction.
		let winners = Self::calculate_winners(winning_ranges);

		// Go through those winners and re-reserve their bid, updating our table of deposits
		// accordingly.
		for (leaser, para, amount, range) in winners.into_iter() {
			let begin_offset = LeasePeriodOf::<T>::from(range.as_pair().0 as u32);
			let period_begin = auction_lease_period_index + begin_offset;
			let period_count = LeasePeriodOf::<T>::from(range.len() as u32);

			match T::Leaser::lease_out(para, &leaser, amount, period_begin, period_count) {
				Err(LeaseError::ReserveFailed) | Err(LeaseError::AlreadyEnded) => {
					// Should never happen since we just unreserved this amount (and our offset is from the
					// present period). But if it does, there's not much we can do.
				}
				Err(LeaseError::AlreadyLeased) => {
					// The leaser attempted to get a second lease on the same para ID, possibly griefing us. Let's
					// keep the amount reserved and let governance sort it out.
					if CurrencyOf::<T>::reserve(&leaser, amount).is_ok() {
						Self::deposit_event(Event::<T>::ReserveConfiscated(para, leaser, amount));
					}
				}
				Ok(()) => {}, // Nothing to report.
			}
		}

		Self::deposit_event(Event::<T>::AuctionClosed(AuctionCounter::<T>::get()));
	}

	/// Calculate the final winners from the winning slots.
	///
	/// This is a simple dynamic programming algorithm designed by Al, the original code is at:
	/// https://github.com/w3f/consensus/blob/master/NPoS/auctiondynamicthing.py
	fn calculate_winners(
		mut winning: WinningData<T>
	) -> WinnersData<T> {
		let winning_ranges = {
			let mut best_winners_ending_at:
				[(Vec<SlotRange>, BalanceOf<T>); SlotRange::LEASE_PERIODS_PER_SLOT] = Default::default();
			let best_bid = |range: SlotRange| {
				winning[range as u8 as usize].as_ref()
					.map(|(_, _, amount)| *amount * (range.len() as u32).into())
			};
			for i in 0..SlotRange::LEASE_PERIODS_PER_SLOT {
				let r = SlotRange::new_bounded(0, 0, i as u32).expect("`i < 4`; qed");
				if let Some(bid) = best_bid(r) {
					best_winners_ending_at[i] = (vec![r], bid);
				}
				for j in 0..i {
					let r = SlotRange::new_bounded(0, j as u32 + 1, i as u32)
						.expect("`i < LPPS`; `j < i`; `j + 1 < LPPS`; qed");
					if let Some(mut bid) = best_bid(r) {
						bid += best_winners_ending_at[j].1;
						if bid > best_winners_ending_at[i].1 {
							let mut new_winners = best_winners_ending_at[j].0.clone();
							new_winners.push(r);
							best_winners_ending_at[i] = (new_winners, bid);
						}
					} else {
						if best_winners_ending_at[j].1 > best_winners_ending_at[i].1 {
							best_winners_ending_at[i] = best_winners_ending_at[j].clone();
						}
					}
				}
			}
			best_winners_ending_at[SlotRange::LEASE_PERIODS_PER_SLOT - 1].0.clone()
		};

		winning_ranges.into_iter().map(|range| {
			let mut final_winner = Default::default();
			swap(&mut final_winner, winning[range as u8 as usize].as_mut()
				.expect("none values are filtered out in previous logic; qed"));
			let (bidder, para, amount) = final_winner;
			(bidder, para, amount, range)
		}).collect::<Vec<_>>()
	}
}

/// tests for this module
#[cfg(test)]
mod tests {
	use super::*;
	use std::{collections::BTreeMap, cell::RefCell};
	use sp_core::H256;
	use sp_runtime::traits::{BlakeTwo256, IdentityLookup};
	use frame_support::{
		parameter_types, ord_parameter_types, assert_ok, assert_noop, assert_storage_noop,
		traits::{OnInitialize, OnFinalize},
		dispatch::DispatchError::BadOrigin,
	};
	use frame_system::{EnsureSignedBy, EnsureOneOf, EnsureRoot};
	use pallet_balances;
	use crate::{auctions, mock::TestRegistrar};
	use primitives::v1::{BlockNumber, Header, Id as ParaId};

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
			Auctions: auctions::{Pallet, Call, Storage, Event<T>},
		}
	);

	parameter_types! {
		pub const BlockHashCount: u32 = 250;
	}
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
		pub const MaxReserves: u32 = 50;
	}

	impl pallet_balances::Config for Test {
		type Balance = u64;
		type DustRemoval = ();
		type Event = Event;
		type ExistentialDeposit = ExistentialDeposit;
		type AccountStore = System;
		type WeightInfo = ();
		type MaxLocks = ();
		type MaxReserves = MaxReserves;
		type ReserveIdentifier = [u8; 8];
	}

	#[derive(Eq, PartialEq, Ord, PartialOrd, Clone, Copy, Debug)]
	pub struct LeaseData {
		leaser: u64,
		amount: u64,
	}

	thread_local! {
		pub static LEASES:
			RefCell<BTreeMap<(ParaId, BlockNumber), LeaseData>> = RefCell::new(BTreeMap::new());
	}

	fn leases() -> Vec<((ParaId, BlockNumber), LeaseData)> {
		LEASES.with(|p| (&*p.borrow()).clone().into_iter().collect::<Vec<_>>())
	}

	pub struct TestLeaser;
	impl Leaser for TestLeaser {
		type AccountId = u64;
		type LeasePeriod = BlockNumber;
		type Currency = Balances;

		fn lease_out(
			para: ParaId,
			leaser: &Self::AccountId,
			amount: <Self::Currency as Currency<Self::AccountId>>::Balance,
			period_begin: Self::LeasePeriod,
			period_count: Self::LeasePeriod,
		) -> Result<(), LeaseError> {
			LEASES.with(|l| {
				let mut leases = l.borrow_mut();
				if period_begin < Self::lease_period_index() {
					return Err(LeaseError::AlreadyEnded)
				}
				for period in period_begin..(period_begin + period_count) {
					if leases.contains_key(&(para, period)) {
						return Err(LeaseError::AlreadyLeased)
					}
					leases.insert((para, period), LeaseData { leaser: leaser.clone(), amount });
				}
				Ok(())
			})
		}

		fn deposit_held(
			para: ParaId,
			leaser: &Self::AccountId
		) -> <Self::Currency as Currency<Self::AccountId>>::Balance {
			leases().iter()
				.filter_map(|((id, _period), data)|
					if id == &para && &data.leaser == leaser { Some(data.amount) } else { None }
				)
				.max()
				.unwrap_or_default()
		}

		fn lease_period() -> Self::LeasePeriod {
			10
		}

		fn lease_period_index() -> Self::LeasePeriod {
			(System::block_number() / Self::lease_period()).into()
		}

		fn already_leased(
			para_id: ParaId,
			first_period: Self::LeasePeriod,
			last_period: Self::LeasePeriod
		) -> bool {
			leases().into_iter().any(|((para, period), _data)| {
				para == para_id &&
				first_period <= period &&
				period <= last_period
			})
		}
	}

	ord_parameter_types!{
		pub const Six: u64 = 6;
	}

	type RootOrSix = EnsureOneOf<
		u64,
		EnsureRoot<u64>,
		EnsureSignedBy<Six, u64>,
	>;

	thread_local! {
		pub static LAST_RANDOM: RefCell<Option<(H256, u32)>> = RefCell::new(None);
	}
	fn set_last_random(output: H256, known_since: u32) {
		LAST_RANDOM.with(|p| *p.borrow_mut() = Some((output, known_since)))
	}
	pub struct TestPastRandomness;
	impl Randomness<H256, BlockNumber> for TestPastRandomness {
		fn random(_subject: &[u8]) -> (H256, u32) {
			LAST_RANDOM.with(|p| {
				if let Some((output, known_since)) = &*p.borrow() {
					(*output, *known_since)
				} else {
					(H256::zero(), frame_system::Pallet::<Test>::block_number())
				}
			})
		}
	}

	parameter_types!{
		pub static EndingPeriod: BlockNumber = 3;
		pub static SampleLength: BlockNumber = 1;
	}

	impl Config for Test {
		type Event = Event;
		type Leaser = TestLeaser;
		type Registrar = TestRegistrar<Self>;
		type EndingPeriod = EndingPeriod;
		type SampleLength = SampleLength;
		type Randomness = TestPastRandomness;
		type InitiateOrigin = RootOrSix;
		type WeightInfo = crate::auctions::TestWeightInfo;
	}

	// This function basically just builds a genesis storage key/value store according to
	// our desired mock up.
	pub fn new_test_ext() -> sp_io::TestExternalities {
		let mut t = frame_system::GenesisConfig::default().build_storage::<Test>().unwrap();
		pallet_balances::GenesisConfig::<Test>{
			balances: vec![(1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60)],
		}.assimilate_storage(&mut t).unwrap();
		let mut ext: sp_io::TestExternalities = t.into();
		ext.execute_with(|| {
			// Register para 0, 1, 2, and 3 for tests
			assert_ok!(TestRegistrar::<Test>::register(1, 0.into(), Default::default(), Default::default()));
			assert_ok!(TestRegistrar::<Test>::register(1, 1.into(), Default::default(), Default::default()));
			assert_ok!(TestRegistrar::<Test>::register(1, 2.into(), Default::default(), Default::default()));
			assert_ok!(TestRegistrar::<Test>::register(1, 3.into(), Default::default(), Default::default()));
		});
		ext
	}

	fn run_to_block(n: BlockNumber) {
		while System::block_number() < n {
			Auctions::on_finalize(System::block_number());
			Balances::on_finalize(System::block_number());
			System::on_finalize(System::block_number());
			System::set_block_number(System::block_number() + 1);
			System::on_initialize(System::block_number());
			Balances::on_initialize(System::block_number());
			Auctions::on_initialize(System::block_number());
		}
	}

	#[test]
	fn basic_setup_works() {
		new_test_ext().execute_with(|| {
			assert_eq!(AuctionCounter::<Test>::get(), 0);
			assert_eq!(TestLeaser::deposit_held(0u32.into(), &1), 0);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::NotStarted);

			run_to_block(10);

			assert_eq!(AuctionCounter::<Test>::get(), 0);
			assert_eq!(TestLeaser::deposit_held(0u32.into(), &1), 0);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::NotStarted);
		});
	}

	#[test]
	fn can_start_auction() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_noop!(Auctions::new_auction(Origin::signed(1), 5, 1), BadOrigin);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));

			assert_eq!(AuctionCounter::<Test>::get(), 1);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);
		});
	}

	#[test]
	fn bidding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 1, 4, 5));

			assert_eq!(Balances::reserved_balance(1), 5);
			assert_eq!(Balances::free_balance(1), 5);
			assert_eq!(
				Auctions::winning(0).unwrap()[SlotRange::ZeroThree as u8 as usize],
				Some((1, 0.into(), 5))
			);
		});
	}

	#[test]
	fn under_bidding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));

			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 1, 4, 5));

			assert_storage_noop!(
				{assert_ok!(Auctions::bid(Origin::signed(2), 0.into(), 1, 1, 4, 1));}
			);
		});
	}

	#[test]
	fn over_bidding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 1, 4, 5));
			assert_ok!(Auctions::bid(Origin::signed(2), 0.into(), 1, 1, 4, 6));

			assert_eq!(Balances::reserved_balance(1), 0);
			assert_eq!(Balances::free_balance(1), 10);
			assert_eq!(Balances::reserved_balance(2), 6);
			assert_eq!(Balances::free_balance(2), 14);
			assert_eq!(
				Auctions::winning(0).unwrap()[SlotRange::ZeroThree as u8 as usize],
				Some((2, 0.into(), 6))
			);
		});
	}

	#[test]
	fn auction_proceeds_correctly() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));

			assert_eq!(AuctionCounter::<Test>::get(), 1);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);

			run_to_block(2);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);

			run_to_block(3);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);

			run_to_block(4);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);

			run_to_block(5);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);

			run_to_block(6);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(0, 0));

			run_to_block(7);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(1, 0));

			run_to_block(8);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(2, 0));

			run_to_block(9);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::NotStarted);
		});
	}

	#[test]
	fn can_win_auction() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 1, 4, 1));
			assert_eq!(Balances::reserved_balance(1), 1);
			assert_eq!(Balances::free_balance(1), 9);
			run_to_block(9);

			assert_eq!(leases(), vec![
				((0.into(), 1), LeaseData { leaser: 1, amount: 1 }),
				((0.into(), 2), LeaseData { leaser: 1, amount: 1 }),
				((0.into(), 3), LeaseData { leaser: 1, amount: 1 }),
				((0.into(), 4), LeaseData { leaser: 1, amount: 1 }),
			]);
			assert_eq!(TestLeaser::deposit_held(0.into(), &1), 1);
		});
	}

	#[test]
	fn can_win_auction_with_late_randomness() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 1, 4, 1));
			assert_eq!(Balances::reserved_balance(1), 1);
			assert_eq!(Balances::free_balance(1), 9);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);
			run_to_block(8);
			// Auction has not yet ended.
			assert_eq!(leases(), vec![]);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(2, 0));
			// This will prevent the auction's winner from being decided in the next block, since the random
			// seed was known before the final bids were made.
			set_last_random(H256::zero(), 8);
			// Auction definitely ended now, but we don't know exactly when in the last 3 blocks yet since
			// no randomness available yet.
			run_to_block(9);
			// Auction has now ended... But auction winner still not yet decided, so no leases yet.
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::VrfDelay(0));
			assert_eq!(leases(), vec![]);

			// Random seed now updated to a value known at block 9, when the auction ended. This means
			// that the winner can now be chosen.
			set_last_random(H256::zero(), 9);
			run_to_block(10);
			// Auction ended and winner selected
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::NotStarted);
			assert_eq!(leases(), vec![
				((0.into(), 1), LeaseData { leaser: 1, amount: 1 }),
				((0.into(), 2), LeaseData { leaser: 1, amount: 1 }),
				((0.into(), 3), LeaseData { leaser: 1, amount: 1 }),
				((0.into(), 4), LeaseData { leaser: 1, amount: 1 }),
			]);
			assert_eq!(TestLeaser::deposit_held(0.into(), &1), 1);
		});
	}

	#[test]
	fn can_win_incomplete_auction() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 4, 4, 5));
			run_to_block(9);

			assert_eq!(leases(), vec![
				((0.into(), 4), LeaseData { leaser: 1, amount: 5 }),
			]);
			assert_eq!(TestLeaser::deposit_held(0.into(), &1), 5);
		});
	}

	#[test]
	fn should_choose_best_combination() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 1, 1, 1));
			assert_ok!(Auctions::bid(Origin::signed(2), 0.into(), 1, 2, 3, 4));
			assert_ok!(Auctions::bid(Origin::signed(3), 0.into(), 1, 4, 4, 2));
			assert_ok!(Auctions::bid(Origin::signed(1), 1.into(), 1, 1, 4, 2));
			run_to_block(9);

			assert_eq!(leases(), vec![
				((0.into(), 1), LeaseData { leaser: 1, amount: 1 }),
				((0.into(), 2), LeaseData { leaser: 2, amount: 4 }),
				((0.into(), 3), LeaseData { leaser: 2, amount: 4 }),
				((0.into(), 4), LeaseData { leaser: 3, amount: 2 }),
			]);
			assert_eq!(TestLeaser::deposit_held(0.into(), &1), 1);
			assert_eq!(TestLeaser::deposit_held(1.into(), &1), 0);
			assert_eq!(TestLeaser::deposit_held(0.into(), &2), 4);
			assert_eq!(TestLeaser::deposit_held(0.into(), &3), 2);
		});
	}

	#[test]
	fn gap_bid_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));

			// User 1 will make a bid for period 1 and 4 for the same Para 0
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 1, 1, 1));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 4, 4, 4));

			// User 2 and 3 will make a bid for para 1 on period 2 and 3 respectively
			assert_ok!(Auctions::bid(Origin::signed(2), 1.into(), 1, 2, 2, 2));
			assert_ok!(Auctions::bid(Origin::signed(3), 1.into(), 1, 3, 3, 3));

			// Total reserved should be the max of the two
			assert_eq!(Balances::reserved_balance(1), 4);

			// Other people are reserved correctly too
			assert_eq!(Balances::reserved_balance(2), 2);
			assert_eq!(Balances::reserved_balance(3), 3);

			// End the auction.
			run_to_block(9);

			assert_eq!(leases(), vec![
				((0.into(), 1), LeaseData { leaser: 1, amount: 1 }),
				((0.into(), 4), LeaseData { leaser: 1, amount: 4 }),
				((1.into(), 2), LeaseData { leaser: 2, amount: 2 }),
				((1.into(), 3), LeaseData { leaser: 3, amount: 3 }),
			]);
			assert_eq!(TestLeaser::deposit_held(0.into(), &1), 4);
			assert_eq!(TestLeaser::deposit_held(1.into(), &2), 2);
			assert_eq!(TestLeaser::deposit_held(1.into(), &3), 3);
		});
	}

	#[test]
	fn deposit_credit_should_work() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 1, 1, 5));
			assert_eq!(Balances::reserved_balance(1), 5);
			run_to_block(10);

			assert_eq!(leases(), vec![
				((0.into(), 1), LeaseData { leaser: 1, amount: 5 }),
			]);
			assert_eq!(TestLeaser::deposit_held(0.into(), &1), 5);

			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 2));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 2, 2, 2, 6));
			// Only 1 reserved since we have a deposit credit of 5.
			assert_eq!(Balances::reserved_balance(1), 1);
			run_to_block(20);

			assert_eq!(leases(), vec![
				((0.into(), 1), LeaseData { leaser: 1, amount: 5 }),
				((0.into(), 2), LeaseData { leaser: 1, amount: 6 }),
			]);
			assert_eq!(TestLeaser::deposit_held(0.into(), &1), 6);
		});
	}

	#[test]
	fn deposit_credit_on_alt_para_should_not_count() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 1, 1, 5));
			assert_eq!(Balances::reserved_balance(1), 5);
			run_to_block(10);

			assert_eq!(leases(), vec![
				((0.into(), 1), LeaseData { leaser: 1, amount: 5 }),
			]);
			assert_eq!(TestLeaser::deposit_held(0.into(), &1), 5);

			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 2));
			assert_ok!(Auctions::bid(Origin::signed(1), 1.into(), 2, 2, 2, 6));
			// 6 reserved since we are bidding on a new para; only works because we don't
			assert_eq!(Balances::reserved_balance(1), 6);
			run_to_block(20);

			assert_eq!(leases(), vec![
				((0.into(), 1), LeaseData { leaser: 1, amount: 5 }),
				((1.into(), 2), LeaseData { leaser: 1, amount: 6 }),
			]);
			assert_eq!(TestLeaser::deposit_held(0.into(), &1), 5);
			assert_eq!(TestLeaser::deposit_held(1.into(), &1), 6);
		});
	}

	#[test]
	fn multiple_bids_work_pre_ending() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));

			for i in 1..6u64 {
				run_to_block(i as _);
				assert_ok!(Auctions::bid(Origin::signed(i), 0.into(), 1, 1, 4, i));
				for j in 1..6 {
					assert_eq!(Balances::reserved_balance(j), if j == i { j } else { 0 });
					assert_eq!(Balances::free_balance(j), if j == i { j * 9 } else { j * 10 });
				}
			}

			run_to_block(9);
			assert_eq!(leases(), vec![
				((0.into(), 1), LeaseData { leaser: 5, amount: 5 }),
				((0.into(), 2), LeaseData { leaser: 5, amount: 5 }),
				((0.into(), 3), LeaseData { leaser: 5, amount: 5 }),
				((0.into(), 4), LeaseData { leaser: 5, amount: 5 }),
			]);
		});
	}

	#[test]
	fn multiple_bids_work_post_ending() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Auctions::new_auction(Origin::signed(6), 0, 1));

			for i in 1..6u64 {
				run_to_block(((i - 1) / 2 + 1) as _);
				assert_ok!(Auctions::bid(Origin::signed(i), 0.into(), 1, 1, 4, i));
				for j in 1..6 {
					assert_eq!(Balances::reserved_balance(j), if j <= i { j } else { 0 });
					assert_eq!(Balances::free_balance(j), if j <= i { j * 9 } else { j * 10 });
				}
			}
			for i in 1..6u64 {
				assert_eq!(ReservedAmounts::<Test>::get((i, ParaId::from(0))).unwrap(), i);
			}

			run_to_block(5);
			assert_eq!(leases(), (1..=4).map(|i| ((0.into(), i), LeaseData { leaser: 2, amount: 2 })).collect::<Vec<_>>());
		});
	}

	#[test]
	fn incomplete_calculate_winners_works() {
		let mut winning = [None; SlotRange::SLOT_RANGE_COUNT];
		winning[SlotRange::ThreeThree as u8 as usize] = Some((1, 0.into(), 1));

		let winners = vec![
			(1, 0.into(), 1, SlotRange::ThreeThree)
		];

		assert_eq!(Auctions::calculate_winners(winning), winners);
	}

	#[test]
	fn first_incomplete_calculate_winners_works() {
		let mut winning = [None; SlotRange::SLOT_RANGE_COUNT];
		winning[0] = Some((1, 0.into(), 1));

		let winners = vec![
			(1, 0.into(), 1, SlotRange::ZeroZero)
		];

		assert_eq!(Auctions::calculate_winners(winning), winners);
	}

	#[test]
	fn calculate_winners_works() {
		let mut winning = [None; SlotRange::SLOT_RANGE_COUNT];
		winning[SlotRange::ZeroZero as u8 as usize] = Some((2, 0.into(), 2));
		winning[SlotRange::ZeroThree as u8 as usize] = Some((1, 100.into(), 1));
		winning[SlotRange::OneOne as u8 as usize] = Some((3, 1.into(), 1));
		winning[SlotRange::TwoTwo as u8 as usize] = Some((1, 2.into(), 53));
		winning[SlotRange::ThreeThree as u8 as usize] = Some((5, 3.into(), 1));

		let winners = vec![
			(2, 0.into(), 2, SlotRange::ZeroZero),
			(3, 1.into(), 1, SlotRange::OneOne),
			(1, 2.into(), 53, SlotRange::TwoTwo),
			(5, 3.into(), 1, SlotRange::ThreeThree),
		];
		assert_eq!(Auctions::calculate_winners(winning.clone()), winners);

		winning[SlotRange::ZeroOne as u8 as usize] = Some((4, 10.into(), 3));
		let winners = vec![
			(4, 10.into(), 3, SlotRange::ZeroOne),
			(1, 2.into(), 53, SlotRange::TwoTwo),
			(5, 3.into(), 1, SlotRange::ThreeThree),
		];
		assert_eq!(Auctions::calculate_winners(winning.clone()), winners);

		winning[SlotRange::ZeroThree as u8 as usize] = Some((1, 100.into(), 100));
		let winners = vec![
			(1, 100.into(), 100, SlotRange::ZeroThree),
		];
		assert_eq!(Auctions::calculate_winners(winning.clone()), winners);
	}

	#[test]
	fn lower_bids_are_correctly_refunded() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 1, 1));
			let para_1 = ParaId::from(1);
			let para_2 = ParaId::from(2);

			// Make a bid and reserve a balance
			assert_ok!(Auctions::bid(Origin::signed(1), para_1, 1, 1, 4, 10));
			assert_eq!(Balances::reserved_balance(1), 10);
			assert_eq!(ReservedAmounts::<Test>::get((1, para_1)), Some(10));
			assert_eq!(Balances::reserved_balance(2), 0);
			assert_eq!(ReservedAmounts::<Test>::get((2, para_2)), None);

			// Bigger bid, reserves new balance and returns funds
			assert_ok!(Auctions::bid(Origin::signed(2), para_2, 1, 1, 4, 20));
			assert_eq!(Balances::reserved_balance(1), 0);
			assert_eq!(ReservedAmounts::<Test>::get((1, para_1)), None);
			assert_eq!(Balances::reserved_balance(2), 20);
			assert_eq!(ReservedAmounts::<Test>::get((2, para_2)), Some(20));
		});
	}

	#[test]
	fn initialize_winners_in_ending_period_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 9, 1));
			let para_1 = ParaId::from(1);
			let para_2 = ParaId::from(2);
			let para_3 = ParaId::from(3);

			// Make bids
			assert_ok!(Auctions::bid(Origin::signed(1), para_1, 1, 1, 4, 10));
			assert_ok!(Auctions::bid(Origin::signed(2), para_2, 1, 3, 4, 20));

			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);
			let mut winning = [None; SlotRange::SLOT_RANGE_COUNT];
			winning[SlotRange::ZeroThree as u8 as usize] = Some((1, para_1, 10));
			winning[SlotRange::TwoThree as u8 as usize] = Some((2, para_2, 20));
			assert_eq!(Auctions::winning(0), Some(winning));

			run_to_block(9);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);

			run_to_block(10);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(0, 0));
			assert_eq!(Auctions::winning(0), Some(winning));

			run_to_block(11);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(1, 0));
			assert_eq!(Auctions::winning(1), Some(winning));
			assert_ok!(Auctions::bid(Origin::signed(3), para_3, 1, 3, 4, 30));

			run_to_block(12);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(2, 0));
			winning[SlotRange::TwoThree as u8 as usize] = Some((3, para_3, 30));
			assert_eq!(Auctions::winning(2), Some(winning));
		});
	}

	#[test]
	fn handle_bid_requires_registered_para() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_noop!(Auctions::bid(Origin::signed(1), 1337.into(), 1, 1, 4, 1), Error::<Test>::ParaNotRegistered);
			assert_ok!(TestRegistrar::<Test>::register(1, 1337.into(), Default::default(), Default::default()));
			assert_ok!(Auctions::bid(Origin::signed(1), 1337.into(), 1, 1, 4, 1));
		});
	}

	#[test]
	fn handle_bid_checks_existing_lease_periods() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 2, 3, 1));
			assert_eq!(Balances::reserved_balance(1), 1);
			assert_eq!(Balances::free_balance(1), 9);
			run_to_block(9);

			assert_eq!(leases(), vec![
				((0.into(), 2), LeaseData { leaser: 1, amount: 1 }),
				((0.into(), 3), LeaseData { leaser: 1, amount: 1 }),
			]);
			assert_eq!(TestLeaser::deposit_held(0.into(), &1), 1);

			// Para 1 just won an auction above and won some lease periods.
			// No bids can work which overlap these periods.
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_noop!(
				Auctions::bid(Origin::signed(1), 0.into(), 2, 1, 4, 1),
				Error::<Test>::AlreadyLeasedOut,
			);
			assert_noop!(
				Auctions::bid(Origin::signed(1), 0.into(), 2, 1, 2, 1),
				Error::<Test>::AlreadyLeasedOut,
			);
			assert_noop!(
				Auctions::bid(Origin::signed(1), 0.into(), 2, 3, 4, 1),
				Error::<Test>::AlreadyLeasedOut,
			);
			// This is okay, not an overlapping bid.
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 2, 1, 1, 1));
		});
	}

	// Here we will test that taking only 10 samples during the ending period works as expected.
	#[test]
	fn less_winning_samples_work() {
		new_test_ext().execute_with(|| {
			EndingPeriod::set(30);
			SampleLength::set(10);

			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 9, 11));
			let para_1 = ParaId::from(1);
			let para_2 = ParaId::from(2);
			let para_3 = ParaId::from(3);

			// Make bids
			assert_ok!(Auctions::bid(Origin::signed(1), para_1, 1, 11, 14, 10));
			assert_ok!(Auctions::bid(Origin::signed(2), para_2, 1, 13, 14, 20));

			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);
			let mut winning = [None; SlotRange::SLOT_RANGE_COUNT];
			winning[SlotRange::ZeroThree as u8 as usize] = Some((1, para_1, 10));
			winning[SlotRange::TwoThree as u8 as usize] = Some((2, para_2, 20));
			assert_eq!(Auctions::winning(0), Some(winning));

			run_to_block(9);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);

			run_to_block(10);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(0, 0));
			assert_eq!(Auctions::winning(0), Some(winning));

			// New bids update the current winning
			assert_ok!(Auctions::bid(Origin::signed(3), para_3, 1, 14, 14, 30));
			winning[SlotRange::ThreeThree as u8 as usize] = Some((3, para_3, 30));
			assert_eq!(Auctions::winning(0), Some(winning));

			run_to_block(20);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(1, 0));
			assert_eq!(Auctions::winning(1), Some(winning));
			run_to_block(25);
			// Overbid mid sample
			assert_ok!(Auctions::bid(Origin::signed(3), para_3, 1, 13, 14, 30));
			winning[SlotRange::TwoThree as u8 as usize] = Some((3, para_3, 30));
			assert_eq!(Auctions::winning(1), Some(winning));

			run_to_block(30);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(2, 0));
			assert_eq!(Auctions::winning(2), Some(winning));

			set_last_random(H256::from([254; 32]), 40);
			run_to_block(40);
			// Auction ended and winner selected
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::NotStarted);
			assert_eq!(leases(), vec![
				((3.into(), 13), LeaseData { leaser: 3, amount: 30 }),
				((3.into(), 14), LeaseData { leaser: 3, amount: 30 }),
			]);
		});
	}

	#[test]
	fn auction_status_works() {
		new_test_ext().execute_with(|| {
			EndingPeriod::set(30);
			SampleLength::set(10);
			set_last_random(Default::default(), 0);

			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::NotStarted);

			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 9, 11));

			run_to_block(9);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::StartingPeriod);

			run_to_block(10);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(0, 0));

			run_to_block(11);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(0, 1));

			run_to_block(19);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(0, 9));

			run_to_block(20);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(1, 0));

			run_to_block(25);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(1, 5));

			run_to_block(30);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(2, 0));

			run_to_block(39);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::EndingPeriod(2, 9));

			run_to_block(40);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::VrfDelay(0));

			run_to_block(44);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::VrfDelay(4));

			set_last_random(Default::default(), 45);
			run_to_block(45);
			assert_eq!(Auctions::auction_status(System::block_number()), AuctionStatus::<u32>::NotStarted);
		});
	}

	#[test]
	fn can_cancel_auction() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Auctions::new_auction(Origin::signed(6), 5, 1));
			assert_ok!(Auctions::bid(Origin::signed(1), 0.into(), 1, 1, 4, 1));
			assert_eq!(Balances::reserved_balance(1), 1);
			assert_eq!(Balances::free_balance(1), 9);

			assert_noop!(Auctions::cancel_auction(Origin::signed(6)), BadOrigin);
			assert_ok!(Auctions::cancel_auction(Origin::root()));

			assert!(AuctionInfo::<Test>::get().is_none());
			assert_eq!(Balances::reserved_balance(1), 0);
			assert_eq!(ReservedAmounts::<Test>::iter().count(), 0);
			assert_eq!(Winning::<Test>::iter().count(), 0);
		});
	}
}

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking {
	use super::{*, Pallet as Auctions};
	use frame_system::RawOrigin;
	use frame_support::traits::{EnsureOrigin, OnInitialize};
	use sp_runtime::{traits::Bounded, SaturatedConversion};

	use frame_benchmarking::{benchmarks, whitelisted_caller, account, impl_benchmark_test_suite};

	fn assert_last_event<T: Config>(generic_event: <T as Config>::Event) {
		let events = frame_system::Pallet::<T>::events();
		let system_event: <T as frame_system::Config>::Event = generic_event.into();
		// compare to the last event record
		let frame_system::EventRecord { event, .. } = &events[events.len() - 1];
		assert_eq!(event, &system_event);
	}

	fn fill_winners<T: Config>(lease_period_index: LeasePeriodOf<T>) {
		let auction_index = AuctionCounter::<T>::get();
		let minimum_balance = CurrencyOf::<T>::minimum_balance();

		for n in 1 ..= SlotRange::SLOT_RANGE_COUNT as u32 {
			let owner = account("owner", n, 0);
			let worst_validation_code = T::Registrar::worst_validation_code();
			let worst_head_data = T::Registrar::worst_head_data();
			CurrencyOf::<T>::make_free_balance_be(&owner, BalanceOf::<T>::max_value());

			assert!(T::Registrar::register(
				owner,
				ParaId::from(n),
				worst_head_data,
				worst_validation_code
			).is_ok());
		}

		T::Registrar::execute_pending_transitions();

		for n in 1 ..= SlotRange::SLOT_RANGE_COUNT as u32 {
			let bidder = account("bidder", n, 0);
			CurrencyOf::<T>::make_free_balance_be(&bidder, BalanceOf::<T>::max_value());

			let slot_range = SlotRange::n((n - 1) as u8).unwrap();
			let (start, end) = slot_range.as_pair();

			assert!(Auctions::<T>::bid(
				RawOrigin::Signed(bidder).into(),
				ParaId::from(n),
				auction_index,
				lease_period_index + start.into(), // First Slot
				lease_period_index + end.into(), // Last slot
				minimum_balance.saturating_mul(n.into()), // Amount
			).is_ok());
		}
	}

	benchmarks! {
		where_clause { where T: pallet_babe::Config }

		new_auction {
			let duration = T::BlockNumber::max_value();
			let lease_period_index = LeasePeriodOf::<T>::max_value();
			let origin = T::InitiateOrigin::successful_origin();
		}: _(RawOrigin::Root, duration, lease_period_index)
		verify {
			assert_last_event::<T>(Event::<T>::AuctionStarted(
				AuctionCounter::<T>::get(),
				LeasePeriodOf::<T>::max_value(),
				T::BlockNumber::max_value(),
			).into());
		}

		// Worst case scenario a new bid comes in which kicks out an existing bid for the same slot.
		bid {
			// Create a new auction
			let duration = T::BlockNumber::max_value();
			let lease_period_index = LeasePeriodOf::<T>::zero();
			Auctions::<T>::new_auction(RawOrigin::Root.into(), duration, lease_period_index)?;

			let para = ParaId::from(0);
			let new_para = ParaId::from(1);

			// Register the paras
			let owner = account("owner", 0, 0);
			CurrencyOf::<T>::make_free_balance_be(&owner, BalanceOf::<T>::max_value());
			let worst_head_data = T::Registrar::worst_head_data();
			let worst_validation_code = T::Registrar::worst_validation_code();
			T::Registrar::register(owner.clone(), para, worst_head_data.clone(), worst_validation_code.clone())?;
			T::Registrar::register(owner, new_para, worst_head_data, worst_validation_code)?;
			T::Registrar::execute_pending_transitions();

			// Make an existing bid
			let auction_index = AuctionCounter::<T>::get();
			let first_slot = AuctionInfo::<T>::get().unwrap().0;
			let last_slot = first_slot + 3u32.into();
			let first_amount = CurrencyOf::<T>::minimum_balance();
			let first_bidder: T::AccountId = account("first_bidder", 0, 0);
			CurrencyOf::<T>::make_free_balance_be(&first_bidder, BalanceOf::<T>::max_value());
			Auctions::<T>::bid(
				RawOrigin::Signed(first_bidder.clone()).into(),
				para,
				auction_index,
				first_slot,
				last_slot,
				first_amount,
			)?;

			let caller: T::AccountId = whitelisted_caller();
			CurrencyOf::<T>::make_free_balance_be(&caller, BalanceOf::<T>::max_value());
			let bigger_amount = CurrencyOf::<T>::minimum_balance().saturating_mul(10u32.into());
			assert_eq!(CurrencyOf::<T>::reserved_balance(&first_bidder), first_amount);
		}: _(RawOrigin::Signed(caller.clone()), new_para, auction_index, first_slot, last_slot, bigger_amount)
		verify {
			// Confirms that we unreserved funds from a previous bidder, which is worst case scenario.
			assert_eq!(CurrencyOf::<T>::reserved_balance(&caller), bigger_amount);
		}

		// Worst case: 10 bidders taking all wining spots, and we need to calculate the winner for auction end.
		// Entire winner map should be full and removed at the end of the benchmark.
		on_initialize {
			// Create a new auction
			let duration: T::BlockNumber = 99u32.into();
			let lease_period_index = LeasePeriodOf::<T>::zero();
			let now = frame_system::Pallet::<T>::block_number();
			Auctions::<T>::new_auction(RawOrigin::Root.into(), duration, lease_period_index)?;

			fill_winners::<T>(lease_period_index);

			for winner in Winning::<T>::get(T::BlockNumber::from(0u32)).unwrap().iter() {
				assert!(winner.is_some());
			}

			let winning_data = Winning::<T>::get(T::BlockNumber::from(0u32)).unwrap();
			// Make winning map full
			for i in 0u32 .. (T::EndingPeriod::get() / T::SampleLength::get()).saturated_into() {
				Winning::<T>::insert(T::BlockNumber::from(i), winning_data.clone());
			}

			// Move ahead to the block we want to initialize
			frame_system::Pallet::<T>::set_block_number(duration + now + T::EndingPeriod::get());

			// Trigger epoch change for new random number value:
			{
				pallet_babe::Pallet::<T>::on_initialize(duration + now + T::EndingPeriod::get());
				let authorities = pallet_babe::Pallet::<T>::authorities();
				let next_authorities = authorities.clone();
				pallet_babe::Pallet::<T>::enact_epoch_change(authorities, next_authorities);
			}

		}: {
			Auctions::<T>::on_initialize(duration + now + T::EndingPeriod::get());
		} verify {
			let auction_index = AuctionCounter::<T>::get();
			assert_last_event::<T>(Event::<T>::AuctionClosed(auction_index).into());
			assert!(Winning::<T>::iter().count().is_zero());
		}

		// Worst case: 10 bidders taking all wining spots, and winning data is full.
		cancel_auction {
			// Create a new auction
			let duration: T::BlockNumber = 99u32.into();
			let lease_period_index = LeasePeriodOf::<T>::zero();
			let now = frame_system::Pallet::<T>::block_number();
			Auctions::<T>::new_auction(RawOrigin::Root.into(), duration, lease_period_index)?;

			fill_winners::<T>(lease_period_index);

			let winning_data = Winning::<T>::get(T::BlockNumber::from(0u32)).unwrap();
			for winner in winning_data.iter() {
				assert!(winner.is_some());
			}

			// Make winning map full
			for i in 0u32 .. (T::EndingPeriod::get() / T::SampleLength::get()).saturated_into() {
				Winning::<T>::insert(T::BlockNumber::from(i), winning_data.clone());
			}
			assert!(AuctionInfo::<T>::get().is_some());
		}: _(RawOrigin::Root)
		verify {
			assert!(AuctionInfo::<T>::get().is_none());
		}
	}

	impl_benchmark_test_suite!(
		Auctions,
		crate::integration_tests::new_test_ext(),
		crate::integration_tests::Test,
	);
}
