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
//! and issuing a connection request to the relevant validators
//! on every new session.
//!
//! In addition to that, it creates a gossip overlay topology
//! which limits the amount of messages sent and received
//! to be an order of sqrt of the validators. Our neighbors
//! in this graph will be forwarded to the network bridge with
//! the `NetworkBridgeMessage::NewGossipTopology` message.

use std::time::{Duration, Instant};
use futures::{channel::oneshot, FutureExt as _};
use rand::{SeedableRng, seq::SliceRandom as _};
use rand_chacha::ChaCha20Rng;
use polkadot_node_subsystem::{
	overseer,
	SubsystemError,
	FromOverseer, SpawnedSubsystem, SubsystemContext,
	messages::{
		GossipSupportMessage,
		NetworkBridgeMessage,
		RuntimeApiMessage,
		RuntimeApiRequest,
	},
	ActiveLeavesUpdate, OverseerSignal,
};
use polkadot_node_subsystem_util as util;
use polkadot_primitives::v1::{
	Hash, SessionIndex, AuthorityDiscoveryId,
};
use polkadot_node_network_protocol::peer_set::PeerSet;
use sp_keystore::{CryptoStore, SyncCryptoStorePtr};
use sp_application_crypto::{Public, AppKey};

#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "parachain::gossip-support";
// How much time should we wait to reissue a connection request
// since the last authority discovery resolution failure.
const BACKOFF_DURATION: Duration = Duration::from_secs(5);

/// Duration after which we consider low connectivity a problem.
///
/// Especially at startup low connectivity is expected (authority discovery cache needs to be
/// populated). Authority discovery on Kusama takes around 8 minutes, so warning after 10 minutes
/// should be fine:
///
/// https://github.com/paritytech/substrate/blob/fc49802f263529160635471c8a17888846035f5d/client/authority-discovery/src/lib.rs#L88
///
const LOW_CONNECTIVITY_WARN_DELAY: Duration = Duration::from_secs(600);

/// The Gossip Support subsystem.
pub struct GossipSupport {
	keystore: SyncCryptoStorePtr,
}

#[derive(Default)]
struct State {
	last_session_index: Option<SessionIndex>,
	// Some(timestamp) if we failed to resolve
	// at least a third of authorities the last time.
	// `None` otherwise.
	last_failure: Option<Instant>,

	/// First time we did not reach our connectivity threshold.
	///
	/// This is the time of the first failed attempt to connect to >2/3 of all validators in a
	/// potential sequence of failed attempts. It will be cleared once we reached >2/3
	/// connectivity.
	failure_start: Option<Instant>,
}

impl GossipSupport {
	/// Create a new instance of the [`GossipSupport`] subsystem.
	pub fn new(keystore: SyncCryptoStorePtr) -> Self {
		Self {
			keystore,
		}
	}

	async fn run<Context>(self, ctx: Context)
	where
		Context: overseer::SubsystemContext<Message = GossipSupportMessage>,
	{
		let mut state = State::default();
		self.run_inner(ctx, &mut state).await;
	}

	async fn run_inner<Context>(self, mut ctx: Context, state: &mut State)
	where
		Context: overseer::SubsystemContext<Message = GossipSupportMessage>,
	{
		let Self { keystore } = self;
		loop {
			let message = match ctx.recv().await {
				Ok(message) => message,
				Err(e) => {
					tracing::debug!(
						target: LOG_TARGET,
						err = ?e,
						"Failed to receive a message from Overseer, exiting",
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

async fn determine_relevant_authorities<Context>(
	ctx: &mut Context,
	relay_parent: Hash,
) -> Result<Vec<AuthorityDiscoveryId>, util::Error>
where
	Context: overseer::SubsystemContext<Message = GossipSupportMessage>,
{
	let authorities = util::request_authorities(relay_parent, ctx.sender()).await.await??;
	tracing::debug!(
		target: LOG_TARGET,
		authority_count = ?authorities.len(),
		"Determined relevant authorities",
	);
	Ok(authorities)
}

/// Return an error if we're not a validator in the given set (do not have keys).
/// Otherwise, returns the index of our keys in `authorities`.
async fn ensure_i_am_an_authority(
	keystore: &SyncCryptoStorePtr,
	authorities: &[AuthorityDiscoveryId],
) -> Result<usize, util::Error> {
	for (i, v) in authorities.iter().enumerate() {
		if CryptoStore::has_keys(
			&**keystore,
			&[(v.to_raw_vec(), AuthorityDiscoveryId::ID)]
		).await {
			return Ok(i);
		}
	}
	Err(util::Error::NotAValidator)
}

/// A helper function for making a `ConnectToValidators` request.
async fn connect_to_authorities<Context>(
	ctx: &mut Context,
	validator_ids: Vec<AuthorityDiscoveryId>,
	peer_set: PeerSet,
) -> oneshot::Receiver<usize>
where
	Context: overseer::SubsystemContext<Message = GossipSupportMessage>,
{
	let (failed, failed_rx) = oneshot::channel();
	ctx.send_message(
		NetworkBridgeMessage::ConnectToValidators {
			validator_ids,
			peer_set,
			failed,
		}
	).await;
	failed_rx
}

/// We partition the list of all sorted `authorities` into sqrt(len) groups of sqrt(len) size
/// and form a matrix where each validator is connected to all validators in its row and column.
/// This is similar to [web3] research proposed topology, except for the groups are not parachain
/// groups (because not all validators are parachain validators and the group size is small),
/// but formed randomly via BABE randomness from two epochs ago.
/// This limits the amount of gossip peers to 2 * sqrt(len) and ensures the diameter of 2.
///
/// [web3]: https://research.web3.foundation/en/latest/polkadot/networking/3-avail-valid.html#topology
async fn update_gossip_topology<Context>(
	ctx: &mut Context,
	our_index: usize,
	authorities: Vec<AuthorityDiscoveryId>,
	relay_parent: Hash,
) -> Result<(), util::Error>
where
	Context: overseer::SubsystemContext<Message = GossipSupportMessage>,
{
	// retrieve BABE randomness
	let random_seed = {
		let (tx, rx) = oneshot::channel();

		ctx.send_message(RuntimeApiMessage::Request(
			relay_parent,
			RuntimeApiRequest::CurrentBabeEpoch(tx),
		)).await;

		let randomness = rx.await??.randomness;
		let mut subject = [0u8; 40];
		subject[..8].copy_from_slice(b"gossipsu");
		subject[8..].copy_from_slice(&randomness);
		sp_core::blake2_256(&subject)
	};

	// shuffle the indices
	let mut rng: ChaCha20Rng = SeedableRng::from_seed(random_seed);
	let len = authorities.len();
	let mut indices: Vec<usize> = (0..len).collect();
	indices.shuffle(&mut rng);
	let our_shuffled_position = indices.iter()
		.position(|i| *i == our_index)
		.expect("our_index < len; indices contains it; qed");

	let neighbors = matrix_neighbors(our_shuffled_position, len);
	let our_neighbors = neighbors.map(|i| authorities[indices[i]].clone()).collect();

	ctx.send_message(
		NetworkBridgeMessage::NewGossipTopology {
			our_neighbors,
		}
	).await;

	Ok(())
}

/// Compute our row and column neighbors in a matrix
fn matrix_neighbors(our_index: usize, len: usize) -> impl Iterator<Item=usize> {
	assert!(our_index < len, "our_index is computed using `enumerate`; qed");

	// e.g. for size 11 the matrix would be
	//
	// 0  1  2
	// 3  4  5
	// 6  7  8
	// 9 10
	//
	// and for index 10, the neighbors would be 1, 4, 7, 9

	let sqrt = (len as f64).sqrt() as usize;
	let our_row = our_index / sqrt;
	let our_column = our_index % sqrt;
	let row_neighbors = our_row * sqrt..std::cmp::min(our_row * sqrt + sqrt, len);
	let column_neighbors = (our_column..len).step_by(sqrt);

	row_neighbors.chain(column_neighbors).filter(move |i| *i != our_index)
}

impl State {
	/// 1. Determine if the current session index has changed.
	/// 2. If it has, determine relevant validators
	///    and issue a connection request.
	async fn handle_active_leaves<Context>(
		&mut self,
		ctx: &mut Context,
		keystore: &SyncCryptoStorePtr,
		leaves: impl Iterator<Item = Hash>,
	) -> Result<(), util::Error>
	where
		Context: overseer::SubsystemContext<Message = GossipSupportMessage>,
	{
		for leaf in leaves {
			let current_index = util::request_session_index_for_child(leaf, ctx.sender()).await.await??;
			let since_failure = self.last_failure.map(|i| i.elapsed()).unwrap_or_default();
			let force_request = since_failure >= BACKOFF_DURATION;
			let leaf_session = Some((current_index, leaf));
			let maybe_new_session = match self.last_session_index {
				Some(i) if current_index <= i => None,
				_ => leaf_session,
			};

			let maybe_issue_connection = if force_request {
				leaf_session
			} else {
				maybe_new_session
			};

			if let Some((session_index, relay_parent)) = maybe_issue_connection {
				let is_new_session = maybe_new_session.is_some();
				if is_new_session {
					tracing::debug!(
						target: LOG_TARGET,
						%session_index,
						"New session detected",
					);
				}

				let authorities = determine_relevant_authorities(ctx, relay_parent).await?;
				let our_index = ensure_i_am_an_authority(keystore, &authorities).await?;

				self.issue_connection_request(ctx, authorities.clone()).await?;

				if is_new_session {
					self.last_session_index = Some(session_index);
					update_gossip_topology(ctx, our_index, authorities, relay_parent).await?;
				}
			}

		}

		Ok(())
	}

	async fn issue_connection_request<Context>(
		&mut self,
		ctx: &mut Context,
		authorities: Vec<AuthorityDiscoveryId>,
	) -> Result<(), util::Error>
	where
		Context: overseer::SubsystemContext<Message = GossipSupportMessage>,
	{
		let num = authorities.len();
		tracing::debug!(target: LOG_TARGET, %num, "Issuing a connection request");

		let failures = connect_to_authorities(
			ctx,
			authorities,
			PeerSet::Validation,
		).await;

		// we await for the request to be processed
		// this is fine, it should take much less time than one session
		let failures = failures.await.unwrap_or(num);

		// issue another request for the same session
		// if at least a third of the authorities were not resolved
		if failures >= num / 3 {
			let timestamp = Instant::now();
			match self.failure_start {
				None => self.failure_start = Some(timestamp),
				Some(first) if first.elapsed() >= LOW_CONNECTIVITY_WARN_DELAY => {
					tracing::warn!(
						target: LOG_TARGET,
						connected = ?(num - failures),
						target = ?num,
						"Low connectivity - authority lookup failed for too many validators."
					);

				}
				Some(_) => {
					tracing::debug!(
						target: LOG_TARGET,
						connected = ?(num - failures),
						target = ?num,
						"Low connectivity (due to authority lookup failures) - expected on startup."
					);
				}
			}
			self.last_failure = Some(timestamp);
		} else {
			self.last_failure = None;
			self.failure_start = None;
		};

		Ok(())
	}
}

impl<Context> overseer::overseer::Subsystem<Context> for GossipSupport
where
	Context: overseer::SubsystemContext<Message = GossipSupportMessage>,
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
