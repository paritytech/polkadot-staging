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

//! Version 0 of the Cross-Consensus Message format data structures.

use core::{result, convert::TryFrom, fmt::Debug};
use derivative::Derivative;
use alloc::vec::Vec;
use parity_scale_codec::{self, Encode, Decode};
use crate::{VersionedMultiAsset, DoubleEncoded, VersionedXcm};

mod junction;
mod multi_asset;
mod multi_location;
mod order;
mod traits;
pub mod multiasset;	// the new multiasset.

pub use junction::{Junction, NetworkId};
pub use multi_asset::{MultiAsset, AssetInstance};
pub use multi_location::MultiLocation;
pub use order::Order;
pub use traits::{Error, Result, SendXcm, ExecuteXcm, Outcome};

// TODO: #2841 #XCMENCODE Efficient encodings for Vec<MultiAsset>, Vec<Order>, using initial byte values 128+ to encode
//   the number of items in the vector.

/// Basically just the XCM (more general) version of `ParachainDispatchOrigin`.
#[derive(Copy, Clone, Eq, PartialEq, Encode, Decode, Debug)]
pub enum OriginKind {
	/// Origin should just be the native origin for the sender. For Cumulus/Frame chains this is
	/// the `Parachain` origin.
	Native,

	/// Origin should just be the standard account-based origin with the sovereign account of
	/// the sender. For Cumulus/Frame chains, this is the `Signed` origin.
	SovereignAccount,

	/// Origin should be the super-user. For Cumulus/Frame chains, this is the `Root` origin.
	/// This will not usually be an available option.
	Superuser,
}

/// Response data to a query.
#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug)]
pub enum Response {
	/// Some assets.
	Assets(Vec<MultiAsset>),
}

/// Cross-Consensus Message: A message from one consensus system to another.
///
/// Consensus systems that may send and receive messages include blockchains and smart contracts.
///
/// All messages are delivered from a known *origin*, expressed as a `MultiLocation`.
///
/// This is the inner XCM format and is version-sensitive. Messages are typically passed using the outer
/// XCM format, known as `VersionedXcm`.
#[derive(Derivative, Encode, Decode)]
#[derivative(Clone(bound = ""), Eq(bound = ""), PartialEq(bound = ""), Debug(bound = ""))]
#[codec(encode_bound())]
#[codec(decode_bound())]
pub enum Xcm<Call> {
	/// Withdraw asset(s) (`assets`) from the ownership of `origin` and place them into `holding`. Execute the
	/// orders (`effects`).
	///
	/// - `assets`: The asset(s) to be withdrawn into holding.
	/// - `effects`: The order(s) to execute on the holding account.
	///
	/// Kind: *Instruction*.
	///
	/// Errors:
	#[codec(index = 0)]
	WithdrawAsset { assets: Vec<MultiAsset>, effects: Vec<Order<Call>> },

	/// Asset(s) (`assets`) have been received into the ownership of this system on the `origin` system.
	///
	/// Some orders are given (`effects`) which should be executed once the corresponding derivative assets have
	/// been placed into `holding`.
	///
	/// - `assets`: The asset(s) that are minted into holding.
	/// - `effects`: The order(s) to execute on the holding account.
	///
	/// Safety: `origin` must be trusted to have received and be storing `assets` such that they may later be
	/// withdrawn should this system send a corresponding message.
	///
	/// Kind: *Trusted Indication*.
	///
	/// Errors:
	#[codec(index = 1)]
	ReserveAssetDeposit { assets: Vec<MultiAsset>, effects: Vec<Order<Call>> },

	/// Asset(s) (`assets`) have been destroyed on the `origin` system and equivalent assets should be
	/// created on this system.
	///
	/// Some orders are given (`effects`) which should be executed once the corresponding derivative assets have
	/// been placed into `holding`.
	///
	/// - `assets`: The asset(s) that are minted into holding.
	/// - `effects`: The order(s) to execute on the holding account.
	///
	/// Safety: `origin` must be trusted to have irrevocably destroyed the `assets` prior as a consequence of
	/// sending this message.
	///
	/// Kind: *Trusted Indication*.
	///
	/// Errors:
	#[codec(index = 2)]
	TeleportAsset { assets: Vec<MultiAsset>, effects: Vec<Order<Call>> },

	/// Indication of the contents of the holding account corresponding to the `QueryHolding` order of `query_id`.
	///
	/// - `query_id`: The identifier of the query that resulted in this message being sent.
	/// - `assets`: The message content.
	///
	/// Safety: No concerns.
	///
	/// Kind: *Information*.
	///
	/// Errors:
	#[codec(index = 3)]
	QueryResponse { #[codec(compact)] query_id: u64, response: Response },

	/// Withdraw asset(s) (`assets`) from the ownership of `origin` and place equivalent assets under the
	/// ownership of `dest` within this consensus system.
	///
	/// - `assets`: The asset(s) to be withdrawn.
	/// - `dest`: The new owner for the assets.
	///
	/// Safety: No concerns.
	///
	/// Kind: *Instruction*.
	///
	/// Errors:
	#[codec(index = 4)]
	TransferAsset { assets: Vec<MultiAsset>, dest: MultiLocation },

	/// Withdraw asset(s) (`assets`) from the ownership of `origin` and place equivalent assets under the
	/// ownership of `dest` within this consensus system.
	///
	/// Send an onward XCM message to `dest` of `ReserveAssetDeposit` with the given `effects`.
	///
	/// - `assets`: The asset(s) to be withdrawn.
	/// - `dest`: The new owner for the assets.
	/// - `effects`: The orders that should be contained in the `ReserveAssetDeposit` which is sent onwards to
	///   `dest.
	///
	/// Safety: No concerns.
	///
	/// Kind: *Instruction*.
	///
	/// Errors:
	#[codec(index = 5)]
	TransferReserveAsset { assets: Vec<MultiAsset>, dest: MultiLocation, effects: Vec<Order<()>> },

	/// Apply the encoded transaction `call`, whose dispatch-origin should be `origin` as expressed by the kind
	/// of origin `origin_type`.
	///
	/// - `origin_type`: The means of expressing the message origin as a dispatch origin.
	/// - `max_weight`: The weight of `call`; this should be at least the chain's calculated weight and will
	///   be used in the weight determination arithmetic.
	/// - `call`: The encoded transaction to be applied.
	///
	/// Safety: No concerns.
	///
	/// Kind: *Instruction*.
	///
	/// Errors:
	#[codec(index = 6)]
	Transact { origin_type: OriginKind, require_weight_at_most: u64, call: DoubleEncoded<Call> },

	/// A message to notify about a new incoming HRMP channel. This message is meant to be sent by the
	/// relay-chain to a para.
	///
	/// - `sender`: The sender in the to-be opened channel. Also, the initiator of the channel opening.
	/// - `max_message_size`: The maximum size of a message proposed by the sender.
	/// - `max_capacity`: The maximum number of messages that can be queued in the channel.
	///
	/// Safety: The message should originate directly from the relay-chain.
	///
	/// Kind: *System Notification*
	#[codec(index = 7)]
	HrmpNewChannelOpenRequest {
		#[codec(compact)] sender: u32,
		#[codec(compact)] max_message_size: u32,
		#[codec(compact)] max_capacity: u32,
	},

	/// A message to notify about that a previously sent open channel request has been accepted by
	/// the recipient. That means that the channel will be opened during the next relay-chain session
	/// change. This message is meant to be sent by the relay-chain to a para.
	///
	/// Safety: The message should originate directly from the relay-chain.
	///
	/// Kind: *System Notification*
	///
	/// Errors:
	#[codec(index = 8)]
	HrmpChannelAccepted {
		#[codec(compact)] recipient: u32,
	},

	/// A message to notify that the other party in an open channel decided to close it. In particular,
	/// `inititator` is going to close the channel opened from `sender` to the `recipient`. The close
	/// will be enacted at the next relay-chain session change. This message is meant to be sent by
	/// the relay-chain to a para.
	///
	/// Safety: The message should originate directly from the relay-chain.
	///
	/// Kind: *System Notification*
	///
	/// Errors:
	#[codec(index = 9)]
	HrmpChannelClosing {
		#[codec(compact)] initiator: u32,
		#[codec(compact)] sender: u32,
		#[codec(compact)] recipient: u32,
	},

	/// A message to indicate that the embedded XCM is actually arriving on behalf of some consensus
	/// location within the origin.
	///
	/// Safety: No concerns.
	///
	/// Kind: *Instruction*
	///
	/// Errors:
	#[codec(index = 10)]
	RelayedFrom {
		who: MultiLocation,
		message: alloc::boxed::Box<Xcm<Call>>,
	},
}

impl<Call> From<Xcm<Call>> for VersionedXcm<Call> {
	fn from(x: Xcm<Call>) -> Self {
		VersionedXcm::V0(x)
	}
}

impl<Call> TryFrom<VersionedXcm<Call>> for Xcm<Call> {
	type Error = ();
	fn try_from(x: VersionedXcm<Call>) -> result::Result<Self, ()> {
		match x {
			VersionedXcm::V0(x) => Ok(x),
		}
	}
}

impl<Call> Xcm<Call> {
	pub fn into<C>(self) -> Xcm<C> { Xcm::from(self) }
	pub fn from<C>(xcm: Xcm<C>) -> Self {
		use Xcm::*;
		match xcm {
			WithdrawAsset { assets, effects }
			=> WithdrawAsset { assets, effects: effects.into_iter().map(Order::into).collect() },
			ReserveAssetDeposit { assets, effects }
			=> ReserveAssetDeposit { assets, effects: effects.into_iter().map(Order::into).collect() },
			TeleportAsset { assets, effects }
			=> TeleportAsset { assets, effects: effects.into_iter().map(Order::into).collect() },
			QueryResponse { query_id: u64, response }
			=> QueryResponse { query_id: u64, response },
			TransferAsset { assets, dest }
			=> TransferAsset { assets, dest },
			TransferReserveAsset { assets, dest, effects }
			=> TransferReserveAsset { assets, dest, effects },
			HrmpNewChannelOpenRequest { sender, max_message_size, max_capacity}
			=> HrmpNewChannelOpenRequest { sender, max_message_size, max_capacity},
			HrmpChannelAccepted { recipient}
			=> HrmpChannelAccepted { recipient},
			HrmpChannelClosing { initiator, sender, recipient}
			=> HrmpChannelClosing { initiator, sender, recipient},
			Transact { origin_type, require_weight_at_most, call}
			=> Transact { origin_type, require_weight_at_most, call: call.into() },
			RelayedFrom { who, message }
			=> RelayedFrom { who, message: alloc::boxed::Box::new((*message).into()) },
		}
	}
}

pub mod opaque {
	/// The basic concrete type of `generic::Xcm`, which doesn't make any assumptions about the format of a
	/// call other than it is pre-encoded.
	pub type Xcm = super::Xcm<()>;

	pub use super::order::opaque::*;
}
