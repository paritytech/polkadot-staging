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

//! Primitive types which are strictly necessary from a parachain-execution point
//! of view.

use sp_std::vec::Vec;

use parity_scale_codec::{Encode, Decode, CompactAs};
use sp_core::{RuntimeDebug, TypeId};
use sp_runtime::traits::Hash as _;

#[cfg(feature = "std")]
use serde::{Serialize, Deserialize};

#[cfg(feature = "std")]
use sp_core::bytes;

#[cfg(feature = "std")]
use parity_util_mem::MallocSizeOf;

use polkadot_core_primitives::{Hash, OutboundHrmpMessage};

/// Block number type used by the relay chain.
pub use polkadot_core_primitives::BlockNumber as RelayChainBlockNumber;

/// Parachain head data included in the chain.
#[derive(PartialEq, Eq, Clone, PartialOrd, Ord, Encode, Decode, RuntimeDebug, derive_more::From)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize, Default, Hash, MallocSizeOf))]
pub struct HeadData(#[cfg_attr(feature = "std", serde(with="bytes"))] pub Vec<u8>);

#[cfg(feature = "std")]
impl HeadData {
	/// Returns the hash of this head data.
	pub fn hash(&self) -> Hash {
		sp_runtime::traits::BlakeTwo256::hash(&self.0)
	}
}

/// Parachain validation code.
#[derive(Default, PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug, derive_more::From)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize, Hash, MallocSizeOf))]
pub struct ValidationCode(#[cfg_attr(feature = "std", serde(with = "bytes"))] pub Vec<u8>);

impl ValidationCode {
	/// Get the blake2-256 hash of the validation code bytes.
	pub fn hash(&self) -> ValidationCodeHash {
		ValidationCodeHash(sp_runtime::traits::BlakeTwo256::hash(&self.0[..]))
	}
}

/// Unit type wrapper around [`Hash`] that represents a validation code hash.
///
/// This type is produced by [`ValidationCode::hash`].
///
/// This type makes it easy to enforce that a hash is a validation code hash on the type level.
#[derive(Clone, Copy, Encode, Decode, Default, Hash, Eq, PartialEq, PartialOrd, Ord)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize, MallocSizeOf))]
pub struct ValidationCodeHash(Hash);

impl sp_std::fmt::Display for ValidationCodeHash {
	fn fmt(&self, f: &mut sp_std::fmt::Formatter<'_>) -> sp_std::fmt::Result {
		self.0.fmt(f)
	}
}

impl sp_std::fmt::Debug for ValidationCodeHash {
	fn fmt(&self, f: &mut sp_std::fmt::Formatter<'_>) -> sp_std::fmt::Result {
		write!(f, "{:?}", self.0)
	}
}

impl AsRef<[u8]> for ValidationCodeHash {
	fn as_ref(&self) -> &[u8] {
		self.0.as_ref()
	}
}

impl From<Hash> for ValidationCodeHash {
	fn from(hash: Hash) -> ValidationCodeHash {
		ValidationCodeHash(hash)
	}
}

impl From<[u8; 32]> for ValidationCodeHash {
	fn from(hash: [u8; 32]) -> ValidationCodeHash {
		ValidationCodeHash(hash.into())
	}
}

impl sp_std::fmt::LowerHex for ValidationCodeHash {
	fn fmt(&self, f: &mut sp_std::fmt::Formatter<'_>) -> sp_std::fmt::Result {
		sp_std::fmt::LowerHex::fmt(&self.0, f)
	}
}

/// Parachain validation code and its hash.
///
/// The hash is as resulting from [`ValidationCode::hash`].
#[derive(PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize, Hash, MallocSizeOf))]
pub struct ValidationCodeAndHash {
	code: ValidationCode,
	hash: ValidationCodeHash,
}

impl ValidationCodeAndHash {
	/// Create from the code and hash.
	pub fn new(code: ValidationCode, hash: ValidationCodeHash) -> Self {
		Self {
			hash,
			code,
		}
	}

	/// Create from code and compute the hash.
	///
	/// Depending on the size of the code the hash computation can be expensive.
	pub fn compute_from_code(code: ValidationCode) -> Self {
		Self {
			hash: code.hash(),
			code,
		}
	}

	/// Get a reference to the validation code.
	pub fn code(&self) -> &ValidationCode {
		&self.code
	}

	/// Get a reference to the validation code hash.
	pub fn hash(&self) -> &ValidationCodeHash {
		&self.hash
	}

	/// Convert into validation code and the hash
	pub fn into_parts(self) -> (ValidationCode, ValidationCodeHash) {
		(self.code, self.hash)
	}

	/// Convert into validation code
	pub fn into_code(self) -> ValidationCode {
		self.code
	}
}

impl From<ValidationCodeAndHash> for ValidationCode {
	fn from(x: ValidationCodeAndHash) -> Self {
		x.into_code()
	}
}

impl Default for ValidationCodeAndHash {
	fn default() -> Self {
		Self::compute_from_code(Default::default())
	}
}

/// Parachain block data.
///
/// Contains everything required to validate para-block, may contain block and witness data.
#[derive(PartialEq, Eq, Clone, Encode, Decode, derive_more::From)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize, Debug, MallocSizeOf))]
pub struct BlockData(#[cfg_attr(feature = "std", serde(with = "bytes"))] pub Vec<u8>);

/// Unique identifier of a parachain.
#[derive(
	Clone, CompactAs, Copy, Decode, Default, Encode, Eq,
	Hash, Ord, PartialEq, PartialOrd, RuntimeDebug,
)]
#[cfg_attr(feature = "std", derive(
	serde::Serialize, serde::Deserialize, derive_more::Display, MallocSizeOf)
)]
pub struct Id(u32);

impl TypeId for Id {
	const TYPE_ID: [u8; 4] = *b"para";
}

impl From<Id> for u32 {
	fn from(x: Id) -> Self { x.0 }
}

impl From<u32> for Id {
	fn from(x: u32) -> Self { Id(x) }
}

impl From<usize> for Id {
	fn from(x: usize) -> Self {
		use sp_std::convert::TryInto;
		// can't panic, so need to truncate
		let x = x.try_into().unwrap_or(u32::MAX);
		Id(x)
	}
}

// When we added a second From impl for Id, type inference could no longer
// determine which impl should apply for things like `5.into()`. It therefore
// raised a bunch of errors in our test code, scattered throughout the
// various modules' tests, that there is no impl of `From<i32>` (`i32` being
// the default numeric type).
//
// We can't use `cfg(test)` here, because that configuration directive does not
// propagate between crates, which would fail to fix tests in crates other than
// this one.
//
// Instead, let's take advantage of the observation that what really matters for a
// ParaId within a test context is that it is unique and constant. I believe that
// there is no case where someone does `(-1).into()` anyway, but if they do, it
// never matters whether the actual contained ID is `-1` or `4294967295`. Nobody
// does arithmetic on a `ParaId`; doing so would be a bug.
impl From<i32> for Id {
	fn from(x: i32) -> Self {
		Id(x as u32)
	}
}

const USER_INDEX_START: u32 = 1000;
const PUBLIC_INDEX_START: u32 = 2000;

/// The ID of the first user (non-system) parachain.
pub const LOWEST_USER_ID: Id = Id(USER_INDEX_START);

/// The ID of the first publicly registerable parachain.
pub const LOWEST_PUBLIC_ID: Id = Id(PUBLIC_INDEX_START);

impl Id {
	/// Create an `Id`.
	pub const fn new(id: u32) -> Self {
		Self(id)
	}
}

/// Determine if a parachain is a system parachain or not.
pub trait IsSystem {
	/// Returns `true` if a parachain is a system parachain, `false` otherwise.
	fn is_system(&self) -> bool;
}

impl IsSystem for Id {
	fn is_system(&self) -> bool {
		self.0 < USER_INDEX_START
	}
}

impl sp_std::ops::Add<u32> for Id {
	type Output = Self;

	fn add(self, other: u32) -> Self {
		Self(self.0 + other)
	}
}

impl sp_std::ops::Sub<u32> for Id {
	type Output = Self;

	fn sub(self, other: u32) -> Self {
		Self(self.0 - other)
	}
}

#[derive(Clone, Copy, Default, Encode, Decode, Eq, PartialEq, Ord, PartialOrd, RuntimeDebug)]
pub struct Sibling(pub Id);

impl From<Id> for Sibling {
	fn from(i: Id) -> Self {
		Self(i)
	}
}

impl From<Sibling> for Id {
	fn from(i: Sibling) -> Self {
		i.0
	}
}

impl AsRef<Id> for Sibling {
	fn as_ref(&self) -> &Id {
		&self.0
	}
}

impl TypeId for Sibling {
	const TYPE_ID: [u8; 4] = *b"sibl";
}

impl From<Sibling> for u32 {
	fn from(x: Sibling) -> Self { x.0.into() }
}

impl From<u32> for Sibling {
	fn from(x: u32) -> Self { Sibling(x.into()) }
}

impl IsSystem for Sibling {
	fn is_system(&self) -> bool {
		IsSystem::is_system(&self.0)
	}
}

/// This type can be converted into and possibly from an AccountId (which itself is generic).
pub trait AccountIdConversion<AccountId>: Sized {
	/// Convert into an account ID. This is infallible.
	fn into_account(&self) -> AccountId;

	/// Try to convert an account ID into this type. Might not succeed.
	fn try_from_account(a: &AccountId) -> Option<Self>;
}

// TODO: Remove all of this, move sp-runtime::AccountIdConversion to own crate and and use that.
// #360
struct TrailingZeroInput<'a>(&'a [u8]);
impl<'a> parity_scale_codec::Input for TrailingZeroInput<'a> {
	fn remaining_len(&mut self) -> Result<Option<usize>, parity_scale_codec::Error> {
		Ok(None)
	}

	fn read(&mut self, into: &mut [u8]) -> Result<(), parity_scale_codec::Error> {
		let len = into.len().min(self.0.len());
		into[..len].copy_from_slice(&self.0[..len]);
		for i in &mut into[len..] {
			*i = 0;
		}
		self.0 = &self.0[len..];
		Ok(())
	}
}

/// Format is b"para" ++ encode(parachain ID) ++ 00.... where 00... is indefinite trailing
/// zeroes to fill AccountId.
impl<T: Encode + Decode + Default> AccountIdConversion<T> for Id {
	fn into_account(&self) -> T {
		(b"para", self).using_encoded(|b|
			T::decode(&mut TrailingZeroInput(b))
		).unwrap_or_default()
	}

	fn try_from_account(x: &T) -> Option<Self> {
		x.using_encoded(|d| {
			if &d[0..4] != b"para" { return None }
			let mut cursor = &d[4..];
			let result = Decode::decode(&mut cursor).ok()?;
			if cursor.iter().all(|x| *x == 0) {
				Some(result)
			} else {
				None
			}
		})
	}
}

/// A type that uniquely identifies an HRMP channel. An HRMP channel is established between two paras.
/// In text, we use the notation `(A, B)` to specify a channel between A and B. The channels are
/// unidirectional, meaning that `(A, B)` and `(B, A)` refer to different channels. The convention is
/// that we use the first item tuple for the sender and the second for the recipient. Only one channel
/// is allowed between two participants in one direction, i.e. there cannot be 2 different channels
/// identified by `(A, B)`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, RuntimeDebug)]
#[cfg_attr(feature = "std", derive(Hash))]
pub struct HrmpChannelId {
	/// The para that acts as the sender in this channel.
	pub sender: Id,
	/// The para that acts as the recipient in this channel.
	pub recipient: Id,
}

/// A message from a parachain to its Relay Chain.
pub type UpwardMessage = Vec<u8>;

/// Validation parameters for evaluating the parachain validity function.
// TODO: balance downloads (https://github.com/paritytech/polkadot/issues/220)
#[derive(PartialEq, Eq, Decode, Clone)]
#[cfg_attr(feature = "std", derive(Debug, Encode))]
pub struct ValidationParams {
	/// Previous head-data.
	pub parent_head: HeadData,
	/// The collation body.
	pub block_data: BlockData,
	/// The current relay-chain block number.
	pub relay_parent_number: RelayChainBlockNumber,
	/// The relay-chain block's storage root.
	pub relay_parent_storage_root: Hash,
}

/// The result of parachain validation.
// TODO: balance uploads (https://github.com/paritytech/polkadot/issues/220)
#[derive(PartialEq, Eq, Clone, Encode)]
#[cfg_attr(feature = "std", derive(Debug, Decode))]
pub struct ValidationResult {
	/// New head data that should be included in the relay chain state.
	pub head_data: HeadData,
	/// An update to the validation code that should be scheduled in the relay chain.
	pub new_validation_code: Option<ValidationCode>,
	/// Upward messages send by the Parachain.
	pub upward_messages: Vec<UpwardMessage>,
	/// Outbound horizontal messages sent by the parachain.
	pub horizontal_messages: Vec<OutboundHrmpMessage<Id>>,
	/// Number of downward messages that were processed by the Parachain.
	///
	/// It is expected that the Parachain processes them from first to last.
	pub processed_downward_messages: u32,
	/// The mark which specifies the block number up to which all inbound HRMP messages are processed.
	pub hrmp_watermark: RelayChainBlockNumber,
}
