// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

//! Cross-Consensus Message format asset data structure.

use alloc::vec::Vec;
use parity_scale_codec::{self as codec, Encode, Decode};
use super::{MultiLocation, multi_asset::{AssetInstance, MultiAsset as OldMultiAsset}};

/// Classification of an asset being concrete or abstract.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub enum AssetId {
	Concrete(MultiLocation),
	Abstract(Vec<u8>),
}

impl AssetId {
	/// Prepend a MultiLocation to a concrete asset, giving it a new root location.
	pub fn reanchor(&mut self, prepend: &MultiLocation) -> Result<(), ()> {
		if let AssetId::Concrete(ref mut l) = self {
			l.prepend_with(prepend.clone()).map_err(|_| ())?;
		}
		Ok(())
	}
}

/// Classification of whether an asset is fungible or not, along with an optional amount or instance.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub enum Fungibility {
	Fungible(Option<u128>),
	NonFungible(Option<AssetInstance>),
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub enum MultiAsset {
	None,
	Asset(Fungibility, Option<AssetId>),
	All,
}

impl From<(AssetId, Fungibility)> for MultiAsset {
	fn from((asset_id, fungibility): (AssetId, Fungibility)) -> MultiAsset {
		MultiAsset::Asset(fungibility, Some(asset_id))
	}
}

impl From<Fungibility> for MultiAsset {
	fn from(fungibility: Fungibility) -> MultiAsset {
		MultiAsset::Asset(fungibility, None)
	}
}

impl From<()> for MultiAsset {
	fn from(_: ()) -> MultiAsset {
		MultiAsset::None
	}
}

impl Encode for MultiAsset {
	fn encode(&self) -> Vec<u8> {
		OldMultiAsset::from(self.clone()).encode()
	}
}

impl Decode for MultiAsset {
	fn decode<I: codec::Input>(input: &mut I) -> Result<Self, parity_scale_codec::Error> {
		OldMultiAsset::decode(input).map(Into::into)
	}
}

impl From<MultiAsset> for OldMultiAsset {
	fn from(a: MultiAsset) -> Self {
		use {AssetId::*, Fungibility::*, OldMultiAsset::*, MultiAsset::Asset};
		match a {
			MultiAsset::None => OldMultiAsset::None,
			MultiAsset::All => All,

			Asset(Fungible(_), Option::None) => AllFungible,
			Asset(NonFungible(_), Option::None) => AllNonFungible,

			Asset(Fungible(Option::None), Some(Concrete(id))) => AllConcreteFungible { id },
			Asset(Fungible(Option::None), Some(Abstract(id))) => AllAbstractFungible { id },
			Asset(NonFungible(Option::None), Some(Concrete(class))) => AllConcreteNonFungible { class },
			Asset(NonFungible(Option::None), Some(Abstract(class))) => AllAbstractNonFungible { class },

			Asset(Fungible(Some(amount)), Some(Concrete(id))) => ConcreteFungible { id, amount },
			Asset(Fungible(Some(amount)), Some(Abstract(id))) => AbstractFungible { id, amount },
			Asset(NonFungible(Some(instance)), Some(Concrete(class))) => ConcreteNonFungible { class, instance },
			Asset(NonFungible(Some(instance)), Some(Abstract(class))) => AbstractNonFungible { class, instance },
		}
	}
}

impl From<OldMultiAsset> for MultiAsset {
	fn from(a: OldMultiAsset) -> Self {
		use {AssetId::*, Fungibility::*, OldMultiAsset::*, MultiAsset::Asset};
		match a {
			None => MultiAsset::None,
			All => MultiAsset::All,

			AllFungible => Asset(Fungible(Option::None), Option::None),
			AllNonFungible => Asset(NonFungible(Option::None), Option::None),

			AllConcreteFungible { id } => Asset(Fungible(Option::None), Some(Concrete(id))),
			AllAbstractFungible { id } => Asset(Fungible(Option::None), Some(Abstract(id))),
			AllConcreteNonFungible { class } => Asset(NonFungible(Option::None), Some(Concrete(class))),
			AllAbstractNonFungible { class } => Asset(NonFungible(Option::None), Some(Abstract(class))),

			ConcreteFungible { id, amount } => Asset(Fungible(Some(amount)), Some(Concrete(id))),
			AbstractFungible { id, amount } => Asset(Fungible(Some(amount)), Some(Abstract(id))),
			ConcreteNonFungible { class, instance } => Asset(NonFungible(Some(instance)), Some(Concrete(class))),
			AbstractNonFungible { class, instance } => Asset(NonFungible(Some(instance)), Some(Abstract(class))),
		}
	}
}
