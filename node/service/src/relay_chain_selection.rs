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

//! A [`SelectChain`] implementation designed for relay chains.
//!
//! This uses information about parachains to inform GRANDPA and BABE
//! about blocks which are safe to build on and blocks which are safe to
//! finalize.
//!
//! To learn more about chain-selection rules for Relay Chains, please see the
//! documentation on [chain-selection][chain-selection-guide]
//! in the implementers' guide.
//!
//! This is mostly a wrapper around a subsystem which implements the
//! chain-selection rule, which leaves the code to be very simple.
//!
//! However, this does apply the further finality constraints to the best
//! leaf returned from the chain selection subsystem by calling into other
//! subsystems which yield information about approvals and disputes.
//!
//! [chain-selection-guide]: https://w3f.github.io/parachain-implementers-guide/protocol-chain-selection.html

#![cfg(feature = "full-node")]

use {
	polkadot_primitives::v1::{
		Hash, BlockNumber, Block as PolkadotBlock, Header as PolkadotHeader,
	},
	polkadot_subsystem::messages::{ApprovalVotingMessage, ChainSelectionMessage},
	prometheus_endpoint::{self, Registry},
	polkadot_overseer::OverseerHandler,
	futures::channel::oneshot,
	consensus_common::{Error as ConsensusError, SelectChain},
	std::sync::Arc,
};

/// The maximum amount of unfinalized blocks we are willing to allow due to approval checking
/// or disputes.
///
/// This is a safety net that should be removed at some point in the future.
const MAX_FINALITY_LAG: polkadot_primitives::v1::BlockNumber = 50;

/// A chain-selection implementation which provides safety for relay chains.
pub struct SelectRelayChain<B> {
	backend: Arc<B>,
	overseer: OverseerHandler,
}

impl<B> SelectRelayChain<B> {
	/// Create a new [`SelectRelayChain`] wrapping the given chain backend
	/// and a handle to the overseer.
	pub fn new(backend: Arc<B>, overseer: OverseerHandler) -> Self {
		SelectRelayChain {
			backend,
			overseer,
		}
	}
}

impl<B> Clone for SelectRelayChain<B> {
	fn clone(&self) -> SelectRelayChain<B> {
		SelectRelayChain {
			backend: self.backend.clone(),
			overseer: self.overseer.clone(),
		}
	}
}

#[async_trait::async_trait]
impl<B> SelectChain<PolkadotBlock> for SelectRelayChain<B>
	where B: sp_blockchain::HeaderBackend<PolkadotBlock> + 'static
{
	/// Get all leaves of the chain, i.e. block hashes that are suitable to
	/// build upon and have no suitable children.
	async fn leaves(&self) -> Result<Vec<Hash>, ConsensusError> {
		unimplemented!()
	}

	/// Among all leaves, pick the one which is the best chain to build upon.
	async fn best_chain(&self) -> Result<PolkadotHeader, ConsensusError> {
		unimplemented!()
	}

	/// Get the best descendent of `target_hash` that we should attempt to
	/// finalize next, if any. It is valid to return the `target_hash` if
	/// no better block exists.
	///
	/// This will search all leaves to find the best one containing the
	/// given target hash, and then constrain to the given block number.
	///
	/// It will also constrain the chain to only chains which are fully
	/// approved, and chains which contain no disputes.
	async fn finality_target(
		&self,
		target_hash: Hash,
		maybe_max_number: Option<BlockNumber>,
	) -> Result<Option<Hash>, ConsensusError> {
		unimplemented!()
	}
}
