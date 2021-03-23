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

use sp_std::marker::PhantomData;
use sp_io::hashing::blake2_256;
use sp_runtime::traits::AccountIdConversion;
use frame_support::traits::Get;
use parity_scale_codec::Encode;
use xcm::v0::{MultiLocation, NetworkId, Junction, ChainRelativeLocation};
use xcm_executor::traits::LocationConversion;

pub struct Account32Hash<Network, AccountId>(PhantomData<(Network, AccountId)>);

impl<
	Network: Get<NetworkId>,
	AccountId: From<[u8; 32]> + Into<[u8; 32]>,
> LocationConversion<AccountId> for Account32Hash<Network, AccountId> {
	fn from_location(location: &MultiLocation) -> Option<AccountId> {
		Some(("multiloc", location).using_encoded(blake2_256).into())
	}

	fn try_into_location(who: AccountId) -> Result<MultiLocation, AccountId> {
		Err(who)
	}
}

pub struct ParentIsDefault<AccountId>(PhantomData<AccountId>);

impl<
	AccountId: Default + Eq,
> LocationConversion<AccountId> for ParentIsDefault<AccountId> {
	fn from_location(location: &MultiLocation) -> Option<AccountId> {
		if location.is_parent() {
			Some(AccountId::default())
		} else {
			None
		}
	}

	fn try_into_location(who: AccountId) -> Result<MultiLocation, AccountId> {
		if who == AccountId::default() {
			Ok(MultiLocation::parent())
		} else {
			Err(who)
		}
	}
}

pub struct ChildParachainConvertsVia<ParaId, AccountId>(PhantomData<(ParaId, AccountId)>);

impl<
	ParaId: From<u32> + Into<u32> + AccountIdConversion<AccountId>,
	AccountId,
> LocationConversion<AccountId> for ChildParachainConvertsVia<ParaId, AccountId> {
	fn from_location(location: &MultiLocation) -> Option<AccountId> {
		if let Some(id) = location.match_child_parachain() {
			Some(ParaId::from(id).into_account())
		} else {
			None
		}
	}

	fn try_into_location(who: AccountId) -> Result<MultiLocation, AccountId> {
		if let Some(id) = ParaId::try_from_account(&who) {
			Ok(MultiLocation::child_parachain(id.into()))
		} else {
			Err(who)
		}
	}
}

pub struct SiblingParachainConvertsVia<ParaId, AccountId>(PhantomData<(ParaId, AccountId)>);

impl<
	ParaId: From<u32> + Into<u32> + AccountIdConversion<AccountId>,
	AccountId,
> LocationConversion<AccountId> for SiblingParachainConvertsVia<ParaId, AccountId> {
	fn from_location(location: &MultiLocation) -> Option<AccountId> {
		if let Some(id) = location.match_sibling_parachain() {
			Some(ParaId::from(id).into_account())
		} else {
			None
		}
	}

	fn try_into_location(who: AccountId) -> Result<MultiLocation, AccountId> {
		if let Some(id) = ParaId::try_from_account(&who) {
			Ok(MultiLocation::sibling_parachain(id.into()))
		} else {
			Err(who)
		}
	}
}

pub struct AccountId32Aliases<Network, AccountId>(PhantomData<(Network, AccountId)>);

impl<
	Network: Get<NetworkId>,
	AccountId: From<[u8; 32]> + Into<[u8; 32]>,
> LocationConversion<AccountId> for AccountId32Aliases<Network, AccountId> {
	fn from_location(location: &MultiLocation) -> Option<AccountId> {
		if let MultiLocation::X1(Junction::AccountId32 { id, network }) = location {
			if matches!(network, NetworkId::Any) || network == &Network::get() {
				return Some((*id).into())
			}
		}
		None
	}

	fn try_into_location(who: AccountId) -> Result<MultiLocation, AccountId> {
		Ok(Junction::AccountId32 { id: who.into(), network: Network::get() }.into())
	}
}

pub struct AccountKey20Aliases<Network, AccountId>(PhantomData<(Network, AccountId)>);

impl<
	Network: Get<NetworkId>,
	AccountId: From<[u8; 20]> + Into<[u8; 20]>
> LocationConversion<AccountId> for AccountKey20Aliases<Network, AccountId> {
	fn from_location(location: &MultiLocation) -> Option<AccountId> {
		if let MultiLocation::X1(Junction::AccountKey20 { key, network }) = location {
			if matches!(network, NetworkId::Any) || network == &Network::get() {
				return Some((*key).into());
			}
		}
		None
	}

	fn try_into_location(who: AccountId) -> Result<MultiLocation, AccountId> {
		Ok(Junction::AccountKey20 {
			key: who.into(),
			network: Network::get(),
		}
		.into())
	}
}
