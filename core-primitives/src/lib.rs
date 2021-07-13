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

#![cfg_attr(not(feature = "std"), no_std)]

//! Core Polkadot types.
//!
//! These core Polkadot types are used by the relay chain and the Parachains.

use sp_runtime::{generic, MultiSignature, traits::{Verify, IdentifyAccount}};
use scale_info::TypeInfo;
use parity_scale_codec::{Encode, Decode};
#[cfg(feature = "std")]
use parity_util_mem::MallocSizeOf;

pub use sp_runtime::traits::{BlakeTwo256, Hash as HashT};

/// The block number type used by Polkadot.
/// 32-bits will allow for 136 years of blocks assuming 1 block per second.
pub type BlockNumber = u32;

/// An instant or duration in time.
pub type Moment = u64;

/// Alias to type for a signature for a transaction on the relay chain. This allows one of several
/// kinds of underlying crypto to be used, so isn't a fixed size when encoded.
pub type Signature = MultiSignature;

/// Alias to the public key used for this chain, actually a `MultiSigner`. Like the signature, this
/// also isn't a fixed size when encoded, as different cryptos have different size public keys.
pub type AccountPublic = <Signature as Verify>::Signer;

/// Alias to the opaque account ID type for this chain, actually a `AccountId32`. This is always
/// 32 bytes.
pub type AccountId = <AccountPublic as IdentifyAccount>::AccountId;

/// The type for looking up accounts. We don't expect more than 4 billion of them.
pub type AccountIndex = u32;

/// Identifier for a chain. 32-bit should be plenty.
pub type ChainId = u32;

/// A hash of some data used by the relay chain.
pub type Hash = sp_core::H256;

/// Unit type wrapper around [`Hash`] that represents a candidate hash.
///
/// This type is produced by [`CandidateReceipt::hash`].
///
/// This type makes it easy to enforce that a hash is a candidate hash on the type level.
#[derive(Clone, Copy, Encode, Decode, Hash, Eq, PartialEq, Default, PartialOrd, Ord, TypeInfo)]
#[cfg_attr(feature = "std", derive(MallocSizeOf))]
pub struct CandidateHash(pub Hash);

#[cfg(feature="std")]
impl std::fmt::Display for CandidateHash {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.0.fmt(f)
	}
}

impl sp_std::fmt::Debug for CandidateHash {
	fn fmt(&self, f: &mut sp_std::fmt::Formatter<'_>) -> sp_std::fmt::Result {
		write!(f, "{:?}", self.0)
	}
}

/// Index of a transaction in the relay chain. 32-bit should be plenty.
pub type Nonce = u32;

/// The balance of an account.
/// 128-bits (or 38 significant decimal figures) will allow for 10m currency (10^7) at a resolution
/// to all for one second's worth of an annualised 50% reward be paid to a unit holder (10^11 unit
/// denomination), or 10^18 total atomic units, to grow at 50%/year for 51 years (10^9 multiplier)
/// for an eventual total of 10^27 units (27 significant decimal figures).
/// We round denomination to 10^12 (12 sdf), and leave the other redundancy at the upper end so
/// that 32 bits may be multiplied with a balance in 128 bits without worrying about overflow.
pub type Balance = u128;

/// Header type.
pub type Header = generic::Header<BlockNumber, BlakeTwo256>;
/// Block type.
pub type Block = generic::Block<Header, UncheckedExtrinsic>;
/// Block ID.
pub type BlockId = generic::BlockId<Block>;

/// Opaque, encoded, unchecked extrinsic.
pub use sp_runtime::OpaqueExtrinsic as UncheckedExtrinsic;

/// The information that goes alongside a transfer_into_parachain operation. Entirely opaque, it
/// will generally be used for identifying the reason for the transfer. Typically it will hold the
/// destination account to which the transfer should be credited. If still more information is
/// needed, then this should be a hash with the pre-image presented via an off-chain mechanism on
/// the parachain.
pub type Remark = [u8; 32];

/// A message sent from the relay-chain down to a parachain.
///
/// The size of the message is limited by the `config.max_downward_message_size` parameter.
pub type DownwardMessage = sp_std::vec::Vec<u8>;

/// A wrapped version of `DownwardMessage`. The difference is that it has attached the block number when
/// the message was sent.
#[derive(Encode, Decode, Clone, sp_runtime::RuntimeDebug, PartialEq, TypeInfo)]
#[cfg_attr(feature = "std", derive(MallocSizeOf))]
pub struct InboundDownwardMessage<BlockNumber = crate::BlockNumber> {
	/// The block number at which this messages was put into the downward message queue.
	pub sent_at: BlockNumber,
	/// The actual downward message to processes.
	pub msg: DownwardMessage,
}

/// An HRMP message seen from the perspective of a recipient.
#[derive(Encode, Decode, Clone, sp_runtime::RuntimeDebug, PartialEq, TypeInfo)]
#[cfg_attr(feature = "std", derive(MallocSizeOf))]
pub struct InboundHrmpMessage<BlockNumber = crate::BlockNumber> {
	/// The block number at which this message was sent.
	/// Specifically, it is the block number at which the candidate that sends this message was
	/// enacted.
	pub sent_at: BlockNumber,
	/// The message payload.
	pub data: sp_std::vec::Vec<u8>,
}

/// An HRMP message seen from the perspective of a sender.
#[derive(Encode, Decode, Clone, sp_runtime::RuntimeDebug, PartialEq, Eq, Hash, TypeInfo)]
#[cfg_attr(feature = "std", derive(MallocSizeOf))]
pub struct OutboundHrmpMessage<Id> {
	/// The para that will get this message in its downward message queue.
	pub recipient: Id,
	/// The message payload.
	pub data: sp_std::vec::Vec<u8>,
}

/// V1 primitives.
pub mod v1 {
	pub use super::*;
}
