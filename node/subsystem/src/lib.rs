// Copyright 2017-2020 Parity Technologies (UK) Ltd.
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

//! Subsystem accumulation.
//!
//! Node-side types and generated overseer.

#![deny(missing_docs)]
#![deny(unused_crate_dependencies)]

pub use polkadot_node_jaeger as jaeger;
pub use jaeger::*;

pub use polkadot_overseer::{OverseerSignal, ActiveLeavesUpdate, self as overseer};

pub use polkadot_node_subsystem_types::{
	errors::{self, *},
	ActivatedLeaf,
	LeafStatus,
};

/// Re-export of all messages type, including the wrapper type.
pub mod messages {
	pub use super::overseer::AllMessages;
	pub use polkadot_node_subsystem_types::messages::*;
}
