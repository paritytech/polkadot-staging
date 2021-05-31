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

use sp_std::{prelude::*, result, marker::PhantomData, borrow::Borrow};
use xcm::v0::{Error as XcmError, Result, MultiAsset, MultiLocation, Junction};
use frame_support::traits::{Get, tokens::{fungibles, WhenDust}, Contains};
use xcm_executor::traits::{TransactAsset, Convert};

/// Asset transaction errors.
pub enum Error {
	/// Asset not found.
	AssetNotFound,
	/// `MultiLocation` to `AccountId` conversion failed.
	AccountIdConversionFailed,
	/// `u128` amount to currency `Balance` conversion failed.
	AmountToBalanceConversionFailed,
	/// `MultiLocation` to `AssetId` conversion failed.
	AssetIdConversionFailed,
}

impl From<Error> for XcmError {
	fn from(e: Error) -> Self {
		match e {
			Error::AssetNotFound => XcmError::FailedToTransactAsset("AssetNotFound"),
			Error::AccountIdConversionFailed =>
				XcmError::FailedToTransactAsset("AccountIdConversionFailed"),
			Error::AmountToBalanceConversionFailed =>
				XcmError::FailedToTransactAsset("AmountToBalanceConversionFailed"),
			Error::AssetIdConversionFailed =>
				XcmError::FailedToTransactAsset("AssetIdConversionFailed"),
		}
	}
}

/// Converter struct implementing `AssetIdConversion` converting a numeric asset ID (must be TryFrom/TryInto<u128>)
/// into a `GeneralIndex` junction, prefixed by some `MultiLocation` value. The `MultiLocation` value will
/// typically be a `PalletInstance` junction.
pub struct AsPrefixedGeneralIndex<Prefix, AssetId, ConvertAssetId>(PhantomData<(Prefix, AssetId, ConvertAssetId)>);
impl<
	Prefix: Get<MultiLocation>,
	AssetId: Clone,
	ConvertAssetId: Convert<u128, AssetId>,
> Convert<MultiLocation, AssetId> for AsPrefixedGeneralIndex<Prefix, AssetId, ConvertAssetId> {
	fn convert_ref(id: impl Borrow<MultiLocation>) -> result::Result<AssetId, ()> {
		let prefix = Prefix::get();
		let id = id.borrow();
		if !prefix.iter().enumerate().all(|(index, item)| id.at(index) == Some(item)) {
			return Err(())
		}
		match id.at(prefix.len()) {
			Some(Junction::GeneralIndex { id }) => ConvertAssetId::convert_ref(id),
			_ => Err(()),
		}
	}
	fn reverse_ref(what: impl Borrow<AssetId>) -> result::Result<MultiLocation, ()> {
		let mut location = Prefix::get();
		let id = ConvertAssetId::reverse_ref(what)?;
		location.push(Junction::GeneralIndex { id }).map_err(|_| ())?;
		Ok(location)
	}
}

pub trait MatchesFungibles<AssetId, Balance> {
	fn matches_fungibles(a: &MultiAsset) -> result::Result<(AssetId, Balance), Error>;
}

#[impl_trait_for_tuples::impl_for_tuples(30)]
impl<
	AssetId: Clone,
	Balance: Clone,
> MatchesFungibles<AssetId, Balance> for Tuple {
	fn matches_fungibles(a: &MultiAsset) -> result::Result<(AssetId, Balance), Error> {
		for_tuples!( #(
			match Tuple::matches_fungibles(a) { o @ Ok(_) => return o, _ => () }
		)* );
		Err(Error::AssetNotFound)
	}
}

pub struct ConvertedConcreteAssetId<AssetId, Balance, ConvertAssetId, ConvertBalance>(
	PhantomData<(AssetId, Balance, ConvertAssetId, ConvertBalance)>
);
impl<
	AssetId: Clone,
	Balance: Clone,
	ConvertAssetId: Convert<MultiLocation, AssetId>,
	ConvertBalance: Convert<u128, Balance>,
> MatchesFungibles<AssetId, Balance> for
	ConvertedConcreteAssetId<AssetId, Balance, ConvertAssetId, ConvertBalance>
{
	fn matches_fungibles(a: &MultiAsset) -> result::Result<(AssetId, Balance), Error> {
		let (id, amount) = match a {
			MultiAsset::ConcreteFungible { id, amount } => (id, amount),
			_ => return Err(Error::AssetNotFound),
		};
		let what = ConvertAssetId::convert_ref(id).map_err(|_| Error::AssetIdConversionFailed)?;
		let amount = ConvertBalance::convert_ref(amount).map_err(|_| Error::AmountToBalanceConversionFailed)?;
		Ok((what, amount))
	}
}

pub struct ConvertedAbstractAssetId<AssetId, Balance, ConvertAssetId, ConvertBalance>(
	PhantomData<(AssetId, Balance, ConvertAssetId, ConvertBalance)>
);
impl<
	AssetId: Clone,
	Balance: Clone,
	ConvertAssetId: Convert<Vec<u8>, AssetId>,
	ConvertBalance: Convert<u128, Balance>,
> MatchesFungibles<AssetId, Balance> for
	ConvertedAbstractAssetId<AssetId, Balance, ConvertAssetId, ConvertBalance>
{
	fn matches_fungibles(a: &MultiAsset) -> result::Result<(AssetId, Balance), Error> {
		let (id, amount) = match a {
			MultiAsset::AbstractFungible { id, amount } => (id, amount),
			_ => return Err(Error::AssetNotFound),
		};
		let what = ConvertAssetId::convert_ref(id).map_err(|_| Error::AssetIdConversionFailed)?;
		let amount = ConvertBalance::convert_ref(amount).map_err(|_| Error::AmountToBalanceConversionFailed)?;
		Ok((what, amount))
	}
}

pub struct FungiblesTransferAdapter<Assets, Matcher, AccountIdConverter, AccountId>(
	PhantomData<(Assets, Matcher, AccountIdConverter, AccountId)>
);
impl<
	Assets: fungibles::Transfer<AccountId>,
	Matcher: MatchesFungibles<Assets::AssetId, Assets::Balance>,
	AccountIdConverter: Convert<MultiLocation, AccountId>,
	AccountId: Clone,	// can't get away without it since Currency is generic over it.
> TransactAsset for FungiblesTransferAdapter<Assets, Matcher, AccountIdConverter, AccountId> {
	fn transfer_asset(
		what: &MultiAsset,
		from: &MultiLocation,
		to: &MultiLocation,
	) -> result::Result<xcm_executor::Assets, XcmError> {
		// Check we handle this asset.
		let (asset_id, amount) = Matcher::matches_fungibles(what)?;
		let source = AccountIdConverter::convert_ref(from)
			.map_err(|()| Error::AccountIdConversionFailed)?;
		let dest = AccountIdConverter::convert_ref(to)
			.map_err(|()| Error::AccountIdConversionFailed)?;
		Assets::transfer(asset_id, &source, &dest, amount, WhenDust::KeepAlive)
			.map_err(|e| XcmError::FailedToTransactAsset(e.into()))?;
		Ok(what.clone().into())
	}
}

pub struct FungiblesMutateAdapter<Assets, Matcher, AccountIdConverter, AccountId, CheckAsset, CheckingAccount>(
	PhantomData<(Assets, Matcher, AccountIdConverter, AccountId, CheckAsset, CheckingAccount)>
);
impl<
	Assets: fungibles::Mutate<AccountId>,
	Matcher: MatchesFungibles<Assets::AssetId, Assets::Balance>,
	AccountIdConverter: Convert<MultiLocation, AccountId>,
	AccountId: Clone,	// can't get away without it since Currency is generic over it.
	CheckAsset: Contains<Assets::AssetId>,
	CheckingAccount: Get<AccountId>,
> TransactAsset for FungiblesMutateAdapter<Assets, Matcher, AccountIdConverter, AccountId, CheckAsset, CheckingAccount> {
	fn can_check_in(_origin: &MultiLocation, what: &MultiAsset) -> Result {
		// Check we handle this asset.
		let (asset_id, amount) = Matcher::matches_fungibles(what)?;
		if CheckAsset::contains(&asset_id) {
			// This is an asset whose teleports we track.
			let checking_account = CheckingAccount::get();
			Assets::can_withdraw(asset_id, &checking_account, amount)
				.into_result(false)
				.map_err(|_| XcmError::NotWithdrawable)?;
		}
		Ok(())
	}

	fn check_in(_origin: &MultiLocation, what: &MultiAsset) {
		if let Ok((asset_id, amount)) = Matcher::matches_fungibles(what) {
			if CheckAsset::contains(&asset_id) {
				let checking_account = CheckingAccount::get();
				let ok = Assets::burn_from(asset_id, &checking_account, amount).is_ok();
				debug_assert!(ok, "`can_check_in` must have returned `true` immediately prior; qed");
			}
		}
	}

	fn check_out(_dest: &MultiLocation, what: &MultiAsset) {
		if let Ok((asset_id, amount)) = Matcher::matches_fungibles(what) {
			if CheckAsset::contains(&asset_id) {
				let checking_account = CheckingAccount::get();
				let ok = Assets::mint_into(asset_id, &checking_account, amount).is_ok();
				debug_assert!(ok, "`mint_into` cannot generally fail; qed");
			}
		}
	}

	fn deposit_asset(what: &MultiAsset, who: &MultiLocation) -> Result {
		// Check we handle this asset.
		let (asset_id, amount) = Matcher::matches_fungibles(what)?;
		let who = AccountIdConverter::convert_ref(who)
			.map_err(|()| Error::AccountIdConversionFailed)?;
		Assets::mint_into(asset_id, &who, amount)
			.map_err(|e| XcmError::FailedToTransactAsset(e.into()))
	}

	fn withdraw_asset(
		what: &MultiAsset,
		who: &MultiLocation
	) -> result::Result<xcm_executor::Assets, XcmError> {
		// Check we handle this asset.
		let (asset_id, amount) = Matcher::matches_fungibles(what)?;
		let who = AccountIdConverter::convert_ref(who)
			.map_err(|()| Error::AccountIdConversionFailed)?;
		Assets::burn_from(asset_id, &who, amount)
			.map_err(|e| XcmError::FailedToTransactAsset(e.into()))?;
		Ok(what.clone().into())
	}
}

pub struct FungiblesAdapter<Assets, Matcher, AccountIdConverter, AccountId, CheckAsset, CheckingAccount>(
	PhantomData<(Assets, Matcher, AccountIdConverter, AccountId, CheckAsset, CheckingAccount)>
);
impl<
	Assets: fungibles::Mutate<AccountId> + fungibles::Transfer<AccountId>,
	Matcher: MatchesFungibles<Assets::AssetId, Assets::Balance>,
	AccountIdConverter: Convert<MultiLocation, AccountId>,
	AccountId: Clone,	// can't get away without it since Currency is generic over it.
	CheckAsset: Contains<Assets::AssetId>,
	CheckingAccount: Get<AccountId>,
> TransactAsset for FungiblesAdapter<Assets, Matcher, AccountIdConverter, AccountId, CheckAsset, CheckingAccount> {
	fn can_check_in(origin: &MultiLocation, what: &MultiAsset) -> Result {
		FungiblesMutateAdapter::<Assets, Matcher, AccountIdConverter, AccountId, CheckAsset, CheckingAccount>
			::can_check_in(origin, what)
	}

	fn check_in(origin: &MultiLocation, what: &MultiAsset) {
		FungiblesMutateAdapter::<Assets, Matcher, AccountIdConverter, AccountId, CheckAsset, CheckingAccount>
			::check_in(origin, what)
	}

	fn check_out(dest: &MultiLocation, what: &MultiAsset) {
		FungiblesMutateAdapter::<Assets, Matcher, AccountIdConverter, AccountId, CheckAsset, CheckingAccount>
			::check_out(dest, what)
	}

	fn deposit_asset(what: &MultiAsset, who: &MultiLocation) -> Result {
		FungiblesMutateAdapter::<Assets, Matcher, AccountIdConverter, AccountId, CheckAsset, CheckingAccount>
			::deposit_asset(what, who)
	}

	fn withdraw_asset(
		what: &MultiAsset,
		who: &MultiLocation
	) -> result::Result<xcm_executor::Assets, XcmError> {
		FungiblesMutateAdapter::<Assets, Matcher, AccountIdConverter, AccountId, CheckAsset, CheckingAccount>
			::withdraw_asset(what, who)
	}

	fn transfer_asset(
		what: &MultiAsset,
		from: &MultiLocation,
		to: &MultiLocation,
	) -> result::Result<xcm_executor::Assets, XcmError> {
		FungiblesTransferAdapter::<Assets, Matcher, AccountIdConverter, AccountId>::transfer_asset(what, from, to)
	}
}
