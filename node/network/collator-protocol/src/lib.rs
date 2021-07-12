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

//! The Collator Protocol allows collators and validators talk to each other.
//! This subsystem implements both sides of the collator protocol.

#![deny(missing_docs, unused_crate_dependencies)]
#![recursion_limit="256"]

use std::time::Duration;

use futures::{FutureExt, TryFutureExt};

use sp_keystore::SyncCryptoStorePtr;

use polkadot_node_network_protocol::{PeerId, UnifiedReputationChange as Rep};
use polkadot_primitives::v1::CollatorPair;

use polkadot_subsystem::{
	SpawnedSubsystem,
	SubsystemContext,
	SubsystemSender,
	overseer,
	messages::{
		CollatorProtocolMessage, NetworkBridgeMessage,
	},
	errors::SubsystemError,
};

mod error;
use error::Result;

mod collator_side;
mod validator_side;

const LOG_TARGET: &'static str = "parachain::collator-protocol";

/// A collator eviction policy - how fast to evict collators which are inactive.
#[derive(Debug, Clone, Copy)]
pub struct CollatorEvictionPolicy {
	/// How fast to evict collators who are inactive.
	pub inactive_collator: Duration,
	/// How fast to evict peers which don't declare their para.
	pub undeclared: Duration,
}

impl Default for CollatorEvictionPolicy {
	fn default() -> Self {
		CollatorEvictionPolicy {
			inactive_collator: Duration::from_secs(24),
			undeclared: Duration::from_secs(1),
		}
	}
}

/// What side of the collator protocol is being engaged
pub enum ProtocolSide {
	/// Validators operate on the relay chain.
	Validator {
		/// The keystore holding validator keys.
		keystore: SyncCryptoStorePtr,
		/// An eviction policy for inactive peers or validators.
		eviction_policy: CollatorEvictionPolicy,
		/// Prometheus metrics for validators.
		metrics: validator_side::Metrics,
	},
	/// Collators operate on a parachain.
	Collator(PeerId, CollatorPair, collator_side::Metrics),
}

/// The collator protocol subsystem.
pub struct CollatorProtocolSubsystem {
	protocol_side: ProtocolSide,
}

impl CollatorProtocolSubsystem {
	/// Start the collator protocol.
	/// If `id` is `Some` this is a collator side of the protocol.
	/// If `id` is `None` this is a validator side of the protocol.
	/// Caller must provide a registry for prometheus metrics.
	pub fn new(protocol_side: ProtocolSide) -> Self {
		Self {
			protocol_side,
		}
	}

	async fn run<Context>(self, ctx: Context) -> Result<()>
	where
		Context: overseer::SubsystemContext<Message=CollatorProtocolMessage>,
		{
		match self.protocol_side {
			ProtocolSide::Validator { keystore, eviction_policy, metrics } => validator_side::run(
				ctx,
				keystore,
				eviction_policy,
				metrics,
			).await,
			ProtocolSide::Collator(local_peer_id, collator_pair, metrics) => collator_side::run(
				ctx,
				local_peer_id,
				collator_pair,
				metrics,
			).await,
		}
	}
}

impl<Context> overseer::overseer::Subsystem<Context> for CollatorProtocolSubsystem
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>,
	<Context as SubsystemContext>::Sender: SubsystemSender,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = self
			.run(ctx)
			.map_err(|e| SubsystemError::with_origin("collator-protocol", e))
			.boxed();

		SpawnedSubsystem {
			name: "collator-protocol-subsystem",
			future,
		}
	}
}

/// Modify the reputation of a peer based on its behavior.
async fn modify_reputation<Context>(ctx: &mut Context, peer: PeerId, rep: Rep)
where
	Context: SubsystemContext,
{
	tracing::trace!(
		target: LOG_TARGET,
		rep = ?rep,
		peer_id = %peer,
		"reputation change for peer",
	);

	ctx.send_message(
		NetworkBridgeMessage::ReportPeer(peer, rep),
	).await;
}
