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

//! Support datastructures for `MultiLocation`, primarily the `Junction` datatype.

use alloc::vec::Vec;
use parity_scale_codec::{self, Encode, Decode};

/// A global identifier of an account-bearing consensus system.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, Debug)]
pub enum NetworkId {
	/// Unidentified/any.
	Any,
	/// Some named network.
	Named(Vec<u8>),
	/// The Polkadot Relay chain
	Polkadot,
	/// Kusama.
	Kusama,
}

/// An identifier of a pluralistic body.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, Debug)]
pub enum BodyId {
	/// The only body in its context.
	Unit,
	/// A named body.
	Named(Vec<u8>),
	/// An indexed body.
	// TODO: parity-scale-codec#262: Change to be a tuple.
	Index { #[codec(compact)] id: u32 },
	/// The unambiguous executive body (for Polkadot, this would be the Polkadot council).
	Executive,
	/// The unambiguous technical body (for Polkadot, this would be the Technical Committee).
	Technical,
	/// The unambiguous legislative body (for Polkadot, this could be considered the opinion of a majority of
	/// lock-voters).
	Legislative,
	/// The unambiguous judicial body (this doesn't exist on Polkadot, but if it were to get a "grand oracle", it
	/// may be considered as that).
	Judicial,
}

/// A part of a pluralistic body.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, Debug)]
pub enum BodyPart {
	/// The body's declaration, under whatever means it decides.
	Voice,
	/// A given number of members of the body.
	Members { #[codec(compact)] count: u32 },
	/// A given number of members of the body, out of some larger caucus.
	Fraction { #[codec(compact)] nom: u32, #[codec(compact)] denom: u32 },
	/// No less than the given proportion of members of the body.
	AtLeastProportion { #[codec(compact)] nom: u32, #[codec(compact)] denom: u32 },
	/// More than than the given proportion of members of the body.
	MoreThanProportion { #[codec(compact)] nom: u32, #[codec(compact)] denom: u32 },
}

impl BodyPart {
	/// Returns `true` if the part represents a strict majority (> 50%) of the body in question.
	pub fn is_majority(&self) -> bool {
		match self {
			BodyPart::Fraction { nom, denom } if *nom * 2 > *denom => true,
			BodyPart::AtLeastProportion { nom, denom } if *nom * 2 > *denom => true,
			BodyPart::MoreThanProportion { nom, denom } if *nom * 2 >= *denom => true,
			_ => false,
		}
	}
}

/// A single item in a path to describe the relative location of a consensus system.
///
/// Each item assumes a pre-existing location as its context and is defined in terms of it.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, Debug)]
pub enum Junction {
	/// An indexed parachain belonging to and operated by the context.
	///
	/// Generally used when the context is a Polkadot Relay-chain.
	Parachain(#[codec(compact)] u32),
	/// A 32-byte identifier for an account of a specific network that is respected as a sovereign endpoint within
	/// the context.
	///
	/// Generally used when the context is a Substrate-based chain.
	AccountId32 { network: NetworkId, id: [u8; 32] },
	/// An 8-byte index for an account of a specific network that is respected as a sovereign endpoint within
	/// the context.
	///
	/// May be used when the context is a Frame-based chain and includes e.g. an indices pallet.
	AccountIndex64 { network: NetworkId, #[codec(compact)] index: u64 },
	/// A 20-byte identifier for an account of a specific network that is respected as a sovereign endpoint within
	/// the context.
	///
	/// May be used when the context is an Ethereum or Bitcoin chain or smart-contract.
	AccountKey20 { network: NetworkId, key: [u8; 20] },
	/// An instanced, indexed pallet that forms a constituent part of the context.
	///
	/// Generally used when the context is a Frame-based chain.
	PalletInstance(u8),
	/// A non-descript index within the context location.
	///
	/// Usage will vary widely owing to its generality.
	///
	/// NOTE: Try to avoid using this and instead use a more specific item.
	GeneralIndex { #[codec(compact)] id: u128 },
	/// A nondescript datum acting as a key within the context location.
	///
	/// Usage will vary widely owing to its generality.
	///
	/// NOTE: Try to avoid using this and instead use a more specific item.
	GeneralKey(Vec<u8>),
	/// The unambiguous child.
	///
	/// Not currently used except as a fallback when deriving ancestry.
	OnlyChild,
	/// A pluralistic body existing within consensus.
	///
	/// Typical to be used to represent a governance origin of a chain, but could in principle be used to represent
	/// things such as multisigs also.
	Plurality { id: BodyId, part: BodyPart },
}
