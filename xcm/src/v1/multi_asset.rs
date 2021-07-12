// Copyright 2021 Parity Technologies (UK) Ltd.
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

//! Cross-Consensus Message format data structures.

use core::{result, convert::TryFrom};
use alloc::vec::Vec;

use parity_scale_codec::{self, Encode, Decode};
use super::{MultiLocation, VersionedMultiAsset};

/// A general identifier for an instance of a non-fungible asset class.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, Debug)]
pub enum AssetInstance {
	/// Undefined - used if the NFA class has only one instance.
	Undefined,

	/// A compact index. Technically this could be greater than u128, but this implementation supports only
	/// values up to `2**128 - 1`.
	Index { #[codec(compact)] id: u128 },

	/// A 4-byte fixed-length datum.
	Array4([u8; 4]),

	/// An 8-byte fixed-length datum.
	Array8([u8; 8]),

	/// A 16-byte fixed-length datum.
	Array16([u8; 16]),

	/// A 32-byte fixed-length datum.
	Array32([u8; 32]),

	/// An arbitrary piece of data. Use only when necessary.
	Blob(Vec<u8>),
}

/// A single general identifier for an asset.
///
/// Represents both fungible and non-fungible assets. May only be used to represent a single asset class.
///
/// Wildcards may or may not be allowed by the interpreting context.
///
/// Assets classes may be identified in one of two ways: either an abstract identifier or a concrete identifier.
/// Implementations may support only one of these. A single asset may be referenced from multiple asset identifiers,
/// though will tend to have only a single *preferred* identifier.
///
/// ### Abstract identifiers
///
/// Abstract identifiers are absolute identifiers that represent a notional asset which can exist within multiple
/// consensus systems. These tend to be simpler to deal with since their broad meaning is unchanged regardless stay of
/// the consensus system in which it is interpreted.
///
/// However, in the attempt to provide uniformity across consensus systems, they may conflate different instantiations
/// of some notional asset (e.g. the reserve asset and a local reserve-backed derivative of it) under the same name,
/// leading to confusion. It also implies that one notional asset is accounted for locally in only one way. This may not
/// be the case, e.g. where there are multiple bridge instances each providing a bridged "BTC" token yet none being
/// fungible between the others.
///
/// Since they are meant to be absolute and universal, a global registry is needed to ensure that name collisions do not
/// occur.
///
/// An abstract identifier is represented as a simple variable-size byte string. As of writing, no global registry
/// exists and no proposals have been put forth for asset labeling.
///
/// ### Concrete identifiers
///
/// Concrete identifiers are *relative identifiers* that specifically identify a single asset through its location in a
/// consensus system relative to the context interpreting. Use of a `MultiLocation` ensures that similar but non
/// fungible variants of the same underlying asset can be properly distinguished, and obviates the need for any kind of
/// central registry.
///
/// The limitation is that the asset identifier cannot be trivially copied between consensus systems and must instead be
/// "re-anchored" whenever being moved to a new consensus system, using the two systems' relative paths.
///
/// Throughout XCM, messages are authored such that *when interpreted from the receiver's point of view* they will have
/// the desired meaning/effect. This means that relative paths should always by constructed to be read from the point of
/// view of the receiving system, *which may be have a completely different meaning in the authoring system*.
///
/// Concrete identifiers are the preferred way of identifying an asset since they are entirely unambiguous.
///
/// A concrete identifier is represented by a `MultiLocation`. If a system has an unambiguous primary asset (such as
/// Bitcoin with BTC or Ethereum with ETH), then it will conventionally be identified as the chain itself. Alternative
/// and more specific ways of referring to an asset within a system include:
///
/// - `<chain>/PalletInstance(<id>)` for a Frame chain with a single-asset pallet instance (such as an instance of the
///   Balances pallet).
/// - `<chain>/PalletInstance(<id>)/GeneralIndex(<index>)` for a Frame chain with an indexed multi-asset pallet instance
///   (such as an instance of the Assets pallet).
/// - `<chain>/AccountId32` for an ERC-20-style single-asset smart-contract on a Frame-based contracts chain.
/// - `<chain>/AccountKey20` for an ERC-20-style single-asset smart-contract on an Ethereum-like chain.
///
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, Debug)]
pub enum MultiAsset {
	/// No assets. Rarely used.
	None,

	/// All assets. Typically used for the subset of assets to be used for an `Order`, and in that context means
	/// "all assets currently in holding".
	All,

	/// All fungible assets. Typically used for the subset of assets to be used for an `Order`, and in that context
	/// means "all fungible assets currently in holding".
	AllFungible,

	/// All non-fungible assets. Typically used for the subset of assets to be used for an `Order`, and in that
	/// context means "all non-fungible assets currently in holding".
	AllNonFungible,

	/// All fungible assets of a given abstract asset `id`entifier.
	AllAbstractFungible { id: Vec<u8> },

	/// All non-fungible assets of a given abstract asset `class`.
	AllAbstractNonFungible { class: Vec<u8> },

	/// All fungible assets of a given concrete asset `id`entifier.
	AllConcreteFungible { id: MultiLocation },

	/// All non-fungible assets of a given concrete asset `class`.
	AllConcreteNonFungible { class: MultiLocation },

	/// Some specific `amount` of the fungible asset identified by an abstract `id`.
	AbstractFungible { id: Vec<u8>, #[codec(compact)] amount: u128 },

	/// Some specific `instance` of the non-fungible asset whose `class` is identified abstractly.
	AbstractNonFungible { class: Vec<u8>, instance: AssetInstance },

	/// Some specific `amount` of the fungible asset identified by an concrete `id`.
	ConcreteFungible { id: MultiLocation, #[codec(compact)] amount: u128 },

	/// Some specific `instance` of the non-fungible asset whose `class` is identified concretely.
	ConcreteNonFungible { class: MultiLocation, instance: AssetInstance },
}

impl MultiAsset {
	/// Returns `true` if the `MultiAsset` is a wildcard and can refer to classes of assets, instead of just one.
	///
	/// Typically can also be inferred by the name starting with `All`.
	pub fn is_wildcard(&self) -> bool {
		match self {
			MultiAsset::None
			| MultiAsset::AbstractFungible {..}
			| MultiAsset::AbstractNonFungible {..}
			| MultiAsset::ConcreteFungible {..}
			| MultiAsset::ConcreteNonFungible {..}
			=> false,

			MultiAsset::All
			| MultiAsset::AllFungible
			| MultiAsset::AllNonFungible
			| MultiAsset::AllAbstractFungible {..}
			| MultiAsset::AllConcreteFungible {..}
			| MultiAsset::AllAbstractNonFungible {..}
			| MultiAsset::AllConcreteNonFungible {..}
			=> true,
		}
	}

	fn is_none(&self) -> bool {
		match self {
			MultiAsset::None
			| MultiAsset::AbstractFungible { amount: 0, .. }
			| MultiAsset::ConcreteFungible { amount: 0, .. }
			=> true,

			_ => false,
		}
	}

	fn is_fungible(&self) -> bool {
		match self {
			MultiAsset::All
			| MultiAsset::AllFungible
			| MultiAsset::AllAbstractFungible {..}
			| MultiAsset::AllConcreteFungible {..}
			| MultiAsset::AbstractFungible {..}
			| MultiAsset::ConcreteFungible {..}
			=> true,

			_ => false,
		}
	}

	fn is_non_fungible(&self) -> bool {
		match self {
			MultiAsset::All
			| MultiAsset::AllNonFungible
			| MultiAsset::AllAbstractNonFungible {..}
			| MultiAsset::AllConcreteNonFungible {..}
			| MultiAsset::AbstractNonFungible {..}
			| MultiAsset::ConcreteNonFungible {..}
			=> true,

			_ => false,
		}
	}

	fn is_concrete_fungible(&self, id: &MultiLocation) -> bool {
		match self {
			MultiAsset::AllFungible => true,
			MultiAsset::AllConcreteFungible { id: i }
			| MultiAsset::ConcreteFungible { id: i, .. }
			=> i == id,

			_ => false,
		}
	}

	fn is_abstract_fungible(&self, id: &[u8]) -> bool {
		match self {
			MultiAsset::AllFungible => true,
			MultiAsset::AllAbstractFungible { id: i }
			| MultiAsset::AbstractFungible { id: i, .. }
			=> i == id,
			_ => false,
		}
	}

	fn is_concrete_non_fungible(&self, class: &MultiLocation) -> bool {
		match self {
			MultiAsset::AllNonFungible => true,
			MultiAsset::AllConcreteNonFungible { class: i }
			| MultiAsset::ConcreteNonFungible { class: i, .. }
			=> i == class,
			_ => false,
		}
	}

	fn is_abstract_non_fungible(&self, class: &[u8]) -> bool {
		match self {
			MultiAsset::AllNonFungible => true,
			MultiAsset::AllAbstractNonFungible { class: i }
			| MultiAsset::AbstractNonFungible { class: i, .. }
			=> i == class,
			_ => false,
		}
	}

	fn is_all(&self) -> bool { matches!(self, MultiAsset::All) }

	/// Returns true if `self` is a super-set of the given `inner`.
	///
	/// Typically, any wildcard is never contained in anything else, and a wildcard can contain any other non-wildcard.
	/// For more details, see the implementation and tests.
	pub fn contains(&self, inner: &MultiAsset) -> bool {
		use MultiAsset::*;

		// Inner cannot be wild
		if inner.is_wildcard() { return false }
		// Everything contains nothing.
		if inner.is_none() { return true }

		// Everything contains anything.
		if self.is_all() { return true }
		// Nothing contains nothing.
		if self.is_none() { return false }

		match self {
			// Anything fungible contains "all fungibles"
			AllFungible => inner.is_fungible(),
			// Anything non-fungible contains "all non-fungibles"
			AllNonFungible => inner.is_non_fungible(),

			AllConcreteFungible { id } => inner.is_concrete_fungible(id),
			AllAbstractFungible { id } => inner.is_abstract_fungible(id),
			AllConcreteNonFungible { class } => inner.is_concrete_non_fungible(class),
			AllAbstractNonFungible { class } => inner.is_abstract_non_fungible(class),

			ConcreteFungible { id, amount } => matches!(
				inner,
				ConcreteFungible { id: inner_id , amount: inner_amount } if inner_id == id && amount >= inner_amount
			),
			AbstractFungible { id, amount } => matches!(
				inner,
				AbstractFungible { id: inner_id , amount: inner_amount } if inner_id == id && amount >= inner_amount
			),
			ConcreteNonFungible { .. } => self == inner,
			AbstractNonFungible { .. } => self == inner,
			_ => false,
		}
	}

	pub fn reanchor(&mut self, prepend: &MultiLocation) -> Result<(), ()> {
		use MultiAsset::*;
		match self {
			AllConcreteFungible { ref mut id }
			| AllConcreteNonFungible { class: ref mut id }
			| ConcreteFungible { ref mut id, .. }
			| ConcreteNonFungible { class: ref mut id, .. }
			=> id.prepend_with(prepend.clone()).map_err(|_| ()),
			_ => Ok(()),
		}
	}
}

impl From<MultiAsset> for VersionedMultiAsset {
	fn from(x: MultiAsset) -> Self {
		VersionedMultiAsset::V1(x)
	}
}

impl TryFrom<VersionedMultiAsset> for MultiAsset {
	type Error = ();
	fn try_from(x: VersionedMultiAsset) -> result::Result<Self, ()> {
		match x {
			VersionedMultiAsset::V1(x) => Ok(x),
			_ => Err(()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn contains_works() {
		use alloc::vec;
		use MultiAsset::*;
		// trivial case: all contains any non-wildcard.
		assert!(All.contains(&None));
		assert!(All.contains(&AbstractFungible { id: alloc::vec![99u8], amount: 1 }));

		// trivial case: none contains nothing, except itself.
		assert!(None.contains(&None));
		assert!(!None.contains(&AllFungible));
		assert!(!None.contains(&All));

		// A bit more sneaky: Nothing can contain wildcard, even All ir the thing itself.
		assert!(!All.contains(&All));
		assert!(!All.contains(&AllFungible));
		assert!(!AllFungible.contains(&AllFungible));
		assert!(!AllNonFungible.contains(&AllNonFungible));

		// For fungibles, containing is basically equality, or equal id with higher amount.
		assert!(
			!AbstractFungible { id: vec![99u8], amount: 99 }
			.contains(&AbstractFungible { id: vec![1u8], amount: 99 })
		);
		assert!(
			AbstractFungible { id: vec![99u8], amount: 99 }
			.contains(&AbstractFungible { id: vec![99u8], amount: 99 })
		);
		assert!(
			AbstractFungible { id: vec![99u8], amount: 99 }
			.contains(&AbstractFungible { id: vec![99u8], amount: 9 })
		);
		assert!(
			!AbstractFungible { id: vec![99u8], amount: 99 }
			.contains(&AbstractFungible { id: vec![99u8], amount: 100 })
		);

		// For non-fungibles, containing is equality.
		assert!(
			!AbstractNonFungible {class: vec![99u8], instance: AssetInstance::Index { id: 9 } }
			.contains(&AbstractNonFungible { class: vec![98u8], instance: AssetInstance::Index { id: 9 } })
		);
		assert!(
			!AbstractNonFungible { class: vec![99u8], instance: AssetInstance::Index { id: 8 } }
			.contains(&AbstractNonFungible { class: vec![99u8], instance: AssetInstance::Index { id: 9 } })
		);
		assert!(
			AbstractNonFungible { class: vec![99u8], instance: AssetInstance::Index { id: 9 } }
			.contains(&AbstractNonFungible { class: vec![99u8], instance: AssetInstance::Index { id: 9 } })
		);
	}
}
