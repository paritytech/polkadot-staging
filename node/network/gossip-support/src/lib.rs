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

//! This subsystem is responsible for keeping track of session changes
//! and issuing a connection request to the validators relevant to
//! the gossiping subsystems on every new session.

use futures::{channel::mpsc, FutureExt as _};
use polkadot_node_subsystem::{
	messages::{
		GossipSupportMessage,
	},
	ActiveLeavesUpdate, FromOverseer, OverseerSignal,
	Subsystem, SpawnedSubsystem, SubsystemContext,
};
use polkadot_node_subsystem_util::{
	validator_discovery,
	self as util,
};
use polkadot_primitives::v1::{
	Hash, SessionIndex, AuthorityDiscoveryId,
};
use polkadot_node_network_protocol::{peer_set::PeerSet, PeerId};
use sp_keystore::{CryptoStore, SyncCryptoStorePtr};
use sp_application_crypto::{Public, AppKey};

const LOG_TARGET: &str = "parachain::gossip-support";

/// The Gossip Support subsystem.
pub struct GossipSupport {
	keystore: SyncCryptoStorePtr,
}

#[derive(Default)]
struct State {
	last_session_index: Option<SessionIndex>,
	/// when we overwrite this, it automatically drops the previous request
	_last_connection_request: Option<mpsc::Receiver<(AuthorityDiscoveryId, PeerId)>>,
}

impl GossipSupport {
	/// Create a new instance of the [`GossipSupport`] subsystem.
	pub fn new(keystore: SyncCryptoStorePtr) -> Self {
		Self {
			keystore,
		}
	}

	#[tracing::instrument(skip(self, ctx), fields(subsystem = LOG_TARGET))]
	async fn run<Context>(self, mut ctx: Context)
	where
		Context: SubsystemContext<Message = GossipSupportMessage>,
	{
		let mut state = State::default();
		let Self { keystore } = self;
		loop {
			let message = match ctx.recv().await {
				Ok(message) => message,
				Err(e) => {
					tracing::debug!(
						target: LOG_TARGET,
						err = ?e,
						"Failed to receive a message from Overseer, exiting"
					);
					return;
				},
			};
			match message {
				FromOverseer::Communication { .. } => {},
				FromOverseer::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
					activated,
					..
				})) => {
					tracing::trace!(target: LOG_TARGET, "active leaves signal");

					let leaves = activated.into_iter().map(|a| a.hash);
					if let Err(e) = state.handle_active_leaves(&mut ctx, &keystore, leaves).await {
						tracing::debug!(target: LOG_TARGET, error = ?e);
					}
				}
				FromOverseer::Signal(OverseerSignal::BlockFinalized(_hash, _number)) => {},
				FromOverseer::Signal(OverseerSignal::Conclude) => {
					return;
				}
			}
		}
	}
}

async fn determine_relevant_authorities(
	ctx: &mut impl SubsystemContext,
	relay_parent: Hash,
) -> Result<Vec<AuthorityDiscoveryId>, util::Error> {
	let authorities = util::request_authorities(relay_parent, ctx.sender()).await.await??;
	Ok(authorities)
}

/// Return an error if we're not a validator in the given set (do not have keys).
async fn ensure_i_am_an_authority(
	keystore: &SyncCryptoStorePtr,
	authorities: &[AuthorityDiscoveryId],
) -> Result<(), util::Error> {
	let keys: Vec<_> = authorities.iter().map(|v| (v.to_raw_vec(), AuthorityDiscoveryId::ID)).collect();
	if CryptoStore::has_keys(&**keystore, &keys).await.found_any()
	{
		return Ok(());
	}

	Err(util::Error::NotAValidator)
}


impl State {
	/// 1. Determine if the current session index has changed.
	/// 2. If it has, determine relevant validators
	///    and issue a connection request.
	async fn handle_active_leaves(
		&mut self,
		ctx: &mut impl SubsystemContext,
		keystore: &SyncCryptoStorePtr,
		leaves: impl Iterator<Item = Hash>,
	) -> Result<(), util::Error> {
		for leaf in leaves {
			let current_index = util::request_session_index_for_child(leaf, ctx.sender()).await.await??;
			let maybe_new_session = match self.last_session_index {
				Some(i) if i <= current_index => None,
				_ => Some((current_index, leaf)),
			};

			if let Some((new_session, relay_parent)) = maybe_new_session {
				tracing::debug!(target: LOG_TARGET, %new_session, "New session detected");
				let authorities = determine_relevant_authorities(ctx, relay_parent).await?;
				ensure_i_am_an_authority(keystore, &authorities).await?;
				tracing::debug!(target: LOG_TARGET, num = ?authorities.len(), "Issuing a connection request");

				let request = validator_discovery::connect_to_authorities(
					ctx,
					authorities,
					PeerSet::Validation,
				).await;

				self.last_session_index = Some(new_session);
				self._last_connection_request = Some(request);
			}
		}

		Ok(())
	}
}

impl<Context> Subsystem<Context> for GossipSupport
where
	Context: SubsystemContext<Message = GossipSupportMessage> + Sync + Send,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = self.run(ctx)
			.map(|_| Ok(()))
			.boxed();

		SpawnedSubsystem {
			name: "gossip-support-subsystem",
			future,
		}
	}
}
