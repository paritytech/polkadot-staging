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

use core::{result, convert::TryFrom};
use alloc::{boxed::Box, vec::Vec};

use parity_scale_codec::{self, Encode, Decode};
use super::{VersionedXcm, VersionedMultiAsset};

mod junction;
mod multi_asset;
mod multi_location;
mod order;
mod traits;
pub use junction::{Junction, NetworkId};
pub use multi_asset::{MultiAsset, AssetInstance};
pub use multi_location::MultiLocation;
pub use order::Order;
pub use traits::{Error, Result, SendXcm, ExecuteXcm, ExecuteHrmp};

// TODO: Efficient encodings for Vec<MultiAsset>, Vec<Order>, using initial byte values 128+ to encode the number of
//   items in the vector.

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

/// Cross-Consensus Message: A message from one consensus system to another.
///
/// Consensus systems that may send and receive messages include blockchains and smart contracts.
///
/// All messages are delivered from a known *origin*, expressed as a `MultiLocation`.
///
/// This is the inner XCM format and is version-sensitive. Messages are typically passed using the outer
/// XCM format, known as `VersionedXcm`.
#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug)]
pub enum Xcm {
	/// Withdraw asset(s) (`assets`) from the ownership of `origin` and place them into `holding`. Execute the
	/// orders (`effects`).
	///
	/// - `assets`: The asset(s) to be withdrawn into holding.
	/// - `effects`: The order(s) to execute on the holding account.
	///
	/// Kind: *Instruction*.
	///
	/// Errors:
	WithdrawAsset { assets: Vec<MultiAsset>, effects: Vec<Order> },

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
	ReserveAssetDeposit { assets: Vec<MultiAsset>, effects: Vec<Order> },

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
	TeleportAsset { assets: Vec<MultiAsset>, effects: Vec<Order> },

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
	Balances { #[codec(compact)] query_id: u64, assets: Vec<MultiAsset> },

	/// Apply the encoded transaction `call`, whose dispatch-origin should be `origin` as expressed by the kind
	/// of origin `origin_type`.
	///
	/// - `origin_type`: The means of expressing the message origin as a dispatch origin.
	/// - `call`: The encoded transaction to be applied.
	///
	/// Safety: No concerns.
	///
	/// Kind: *Instruction*.
	///
	/// Errors:
	Transact { origin_type: OriginKind, call: Vec<u8> },

	/// Relay an inner message (`inner`) to a locally reachable destination ID `dest`.
	///
	/// The message sent to the destination will be wrapped into a `RelayedFrom` message, with the
	/// `superorigin` being this location.
	///
	/// - `dest: MultiLocation`: The location of the to be relayed into. This may never contain `Parent`, and
	///   it must be immediately reachable from the interpreting context.
	/// - `inner: VersionedXcm`: The message to be wrapped and relayed.
	///
	/// Safety: No concerns.
	///
	/// Kind: *Instruction*.
	///
	/// Errors:
	RelayTo { dest: MultiLocation, inner: Box<VersionedXcm> },

	/// A message (`inner`) was sent to `origin` from `superorigin` with the intention of being relayed.
	///
	/// - `superorigin`: The location of the `inner` message origin, **relative to `origin`**.
	/// - `inner`: The message sent by the super origin.
	///
	/// Safety: `superorigin` must express a sub-consensus only; it may *NEVER* contain a `Parent` junction.
	///
	/// Kind: *Trusted Indication*.
	///
	/// Errors:
	RelayedFrom { superorigin: MultiLocation, inner: Box<VersionedXcm> },

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
	HrmpChannelClosing {
		#[codec(compact)] initiator: u32,
		#[codec(compact)] sender: u32,
		#[codec(compact)] recipient: u32,
	},

	/// A message to propose opening a channel on the relay-chain between the
	/// sender para to the recipient para.
	///
	/// - `recipient`: The recipient in the to-be opened channel.
	/// - `max_message_size`: The maximum size of a message proposed by the sender.
	/// - `max_capacity`: The maximum number of messages that can be queued in the channel.
	///
	/// Safety: The message should originate directly from the sender para.
	///
	/// Kind: *Instruction*.
	///
	/// Errors:
	HrmpInitOpenChannel {
		#[codec(compact)] recipient: u32,
		#[codec(compact)] max_message_size: u32,
		#[codec(compact)] max_capacity: u32,
	},

	/// A message to accept opening a channel on the relay-chain.
	///
	/// - `sender`: The sender in the to-be opened channel.
	///
	/// Safety: The message should originate directly from the recipient para.
	///
	/// Kind: *Instruction*.
	///
	/// Errors:
	HrmpAcceptOpenChannel {
		#[codec(compact)] sender: u32,
	},

	/// A message to close a channel on the relay-chain.
	///
	/// - `sender`: The sender in the to-be closed channel.
	/// - `recipient`: The recipient in the to-be closed channel.
	///
	/// Safety: The message should originate directly from the sender or
	/// recipient para (either member of the channel).
	///
	/// Kind: *Instruction*.
	///
	/// Errors:
	HrmpCloseChannel {
		#[codec(compact)] sender: u32,
		#[codec(compact)] recipient: u32,
	},
}

impl From<Xcm> for VersionedXcm {
	fn from(x: Xcm) -> Self {
		VersionedXcm::V0(x)
	}
}

impl TryFrom<VersionedXcm> for Xcm {
	type Error = ();
	fn try_from(x: VersionedXcm) -> result::Result<Self, ()> {
		match x {
			VersionedXcm::V0(x) => Ok(x),
		}
	}
}
