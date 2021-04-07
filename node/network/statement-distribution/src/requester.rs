// Copyright 2021 Parity Technologies (UK) Ltd.
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

//! Large statement requesting background task logic.

use std::time::Duration;

use futures::{SinkExt, channel::{mpsc, oneshot}};

use polkadot_node_network_protocol::{
    PeerId,
    request_response::{
        OutgoingRequest, Recipient, Requests,
        v1::{
            StatementFetchingRequest, StatementFetchingResponse
        }
    }
};
use polkadot_node_subsystem_util::TimeoutExt;
use polkadot_primitives::v1::{CandidateHash, CommittedCandidateReceipt, Hash};

use crate::LOG_TARGET;

// In case we failed fetching from our known peers, how long we should wait before attempting a
// retry, even though we have not yet discovered any new peers. Or in other words how long to
// wait before retrying peers that already failed.
const RETRY_TIMEOUT: Duration = Duration::from_millis(500);

/// Messages coming from a background task.
pub enum RequesterMessage {
	/// Get an update of availble peers to try for fetching a given statement.
	GetMorePeers {
		relay_parent: Hash,
		candidate_hash: CandidateHash,
		tx: oneshot::Sender<Vec<PeerId>>
	},
	/// Fetching finished, ask for verification. If verification failes, task will continue asking
	/// peers for data.
	Verify {
		/// Relay parent this candidate is in the context of.
		relay_parent: Hash,
		/// The candidate we fetched data for.
		candidate_hash: CandidateHash,
		/// Data was fetched from this peer.
		from_peer: PeerId,
		/// Response we received from above peer.
		response: CommittedCandidateReceipt,
		/// Peers which failed providing the data.
		bad_peers: Vec<PeerId>,
		/// Tell requester task whether or not it has to carry on. This might happen if the fetched
		/// data was invalid for example.
		carry_on: oneshot::Sender<()>,
	},
	/// Ask subsystem to send a request for us.
	SendRequest(Requests),
}


/// A fetching task, taking care of fetching large statements via request/response.
///
/// A fetch task does not know about a particular `Statement` instead it just tries fetching a
/// `CommittedCandidateReceipt` from peers, whether or not this can be used to re-assemble one ore
/// many `SignedFullStatement`s needs to be verified by the caller.
pub async fn fetch(
	relay_parent: Hash,
	candidate_hash: CandidateHash,
	peers: Vec<PeerId>,
	mut sender: mpsc::Sender<RequesterMessage>,
) {
	// Peers we already tried (and failed).
	let mut tried_peers = Vec::new();
	// Peers left for trying out.
	let mut new_peers = peers;

	let req = StatementFetchingRequest {
		relay_parent,
		candidate_hash,
	};

	// We retry endlessly (with sleep periods), and rely on the subsystem to kill us eventually.
	loop {
		while let Some(peer) = new_peers.pop() {
			let (outgoing, pending_response) = OutgoingRequest::new(
				Recipient::Peer(peer),
				req.clone(),
			);
			if let Err(err) = sender.feed(
				RequesterMessage::SendRequest(Requests::StatementFetching(outgoing))
			).await {
				tracing::info!(
					target: LOG_TARGET,
					?err,
					"Sending request failed, node might be shutting down - exiting."
				);
				return
			}
			match pending_response.await {
				Ok(StatementFetchingResponse::Statement(statement)) => {
					let (carry_on_tx, carry_on) = oneshot::channel();
					if let Err(err) = sender.send(
						RequesterMessage::Verify {
							relay_parent,
							candidate_hash,
							from_peer: peer,
							response: statement,
							bad_peers: tried_peers.clone(),
							carry_on: carry_on_tx,
						}
						).await {
						tracing::info!(
							target: LOG_TARGET,
							?err,
							"Sending task response failed: This should not happen."
						);
					}
					match carry_on.await {
						Err(_) => {}
						Ok(()) => {
							// The below push peer gets skipped intentionally, we don't want to try
							// this peer again.
							continue
						},
					}
					// We are done now.
					return
				},
				Err(err) => {
					tracing::debug!(
						target: LOG_TARGET,
						?err,
						"Receiving response failed with error - trying next peer."
					);
				}
			}

			tried_peers.push(peer);
		}

		new_peers = std::mem::take(&mut tried_peers);

		// All our peers failed us - try getting new ones before trying again:
		match try_get_new_peers(relay_parent, candidate_hash, &mut sender).await {
			Ok(Some(mut peers)) => {
				// New arrivals will be tried first:
				new_peers.append(&mut peers);
			}
			// No new peers, try the old ones again (if we have any):
			Ok(None) => {
				// Note: In case we don't have any more peers, we will just keep asking for new
				// peers, which is exactly what we want.
			},
			Err(()) => return,
		}
	}
}

/// Try getting new peers from subsystem.
///
/// If there are non, we will return after a timeout with `None`.
async fn try_get_new_peers(
	relay_parent: Hash,
	candidate_hash: CandidateHash,
	sender: &mut mpsc::Sender<RequesterMessage>
) -> Result<Option<Vec<PeerId>>, ()> {
	let (tx, rx) = oneshot::channel();

	if let Err(err) = sender.send(
		RequesterMessage::GetMorePeers { relay_parent, candidate_hash, tx }
	).await {
		tracing::debug!(
			target: LOG_TARGET,
			?err,
			"Failed sending background task message, subsystem probably moved on."
		);
		return Err(())
	}

	match rx.timeout(RETRY_TIMEOUT).await.transpose() {
		Err(_) => {
			tracing::debug!(
				target: LOG_TARGET,
				"Failed fetching more peers."
			);
			Err(())
		}
		Ok(val) => Ok(val)
	}
}
