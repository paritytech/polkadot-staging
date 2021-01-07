// Copyright 2021 Parity Technologies (UK) Ltd.
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

//! All peersets and protocols used for parachains.

use sc_network::config::{NonDefaultSetConfig, SetConfig};
use std::borrow::Cow;

/// The peer-sets and thus the protocols which are used for the network.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PeerSet {
	/// The validation peer-set is responsible for all messages related to candidate validation and communication among validators.
	Validation,
	/// The collation peer-set is used for validator<>collator communication.
	Collation,
}

/// Protocol name as understood in substrate.
///
/// Ideally this would be defined in substrate as a newtype.
type ProtocolName = Cow<'static, str>;

impl PeerSet {
	/// Get `sc_network` peer set configurations for each peerset.
	///
	/// Those should be used in the network configuration to register the protocols with the
	/// network service.
	pub fn get_info(self) -> NonDefaultSetConfig {
		let protocol = self.into_protocol_name();
		match self {
			PeerSet::Validation => NonDefaultSetConfig {
				notifications_protocol: protocol,
				set_config: sc_network::config::SetConfig {
					in_peers: 25,
					out_peers: 0,
					reserved_nodes: Vec::new(),
					non_reserved_mode: sc_network::config::NonReservedPeerMode::Accept,
				},
			},
			PeerSet::Collation => NonDefaultSetConfig {
				notifications_protocol: protocol,
				set_config: SetConfig {
					in_peers: 25,
					out_peers: 0,
					reserved_nodes: Vec::new(),
					non_reserved_mode: sc_network::config::NonReservedPeerMode::Accept,
				},
			},
		}
	}

	/// Get the protocol name associated with each peer set as static str.
	pub const fn get_protocol_name_static(self) -> &'static str {
		match self {
			PeerSet::Validation => "/polkadot/validation/1",
			PeerSet::Collation => "/polkadot/collation/1",
		}
	}

	/// Convert a peer set into a protocol name as understood by Substrate.
	///
	/// With `ProtocolName` being a proper newtype we could use the `Into` trait here.
	pub fn into_protocol_name(self) -> ProtocolName {
		self.get_protocol_name_static().into()
	}

	/// Try parsing a protocol name into a peer set.
	///
	/// If ProtocolName was a newtype, this would actually be nice to implement in terms of the
	/// standard `TryFrom` trait.
	pub fn try_from_protocol_name(name: &ProtocolName) -> Option<PeerSet> {
		match name {
			n if n == &PeerSet::Validation.into_protocol_name() => Some(PeerSet::Validation),
			n if n == &PeerSet::Collation.into_protocol_name() => Some(PeerSet::Collation),
			_ => None,
		}
	}
}
