// Copyright 2020-2021 Parity Technologies (UK) Ltd.
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

//! The bitfield distribution
//!
//! In case this node is a validator, gossips its own signed availability bitfield
//! for a particular relay parent.
//! Independently of that, gossips on received messages from peers to other interested peers.

#![deny(unused_crate_dependencies)]

use futures::{channel::oneshot, FutureExt};

use polkadot_subsystem::messages::*;
use polkadot_subsystem::{
	PerLeafSpan, ActiveLeavesUpdate, FromOverseer, OverseerSignal, SpawnedSubsystem,
	SubsystemContext, SubsystemResult, SubsystemError,
	jaeger,
	overseer,
};
use polkadot_node_subsystem_util::{
	metrics::{self, prometheus},
	self as util, MIN_GOSSIP_PEERS,
};
use polkadot_primitives::v1::{Hash, SignedAvailabilityBitfield, SigningContext, ValidatorId};
use polkadot_node_network_protocol::{v1 as protocol_v1, PeerId, View, UnifiedReputationChange as Rep, OurView};
use std::collections::{HashMap, HashSet};

#[cfg(test)]
mod tests;

const COST_SIGNATURE_INVALID: Rep = Rep::CostMajor("Bitfield signature invalid");
const COST_VALIDATOR_INDEX_INVALID: Rep = Rep::CostMajor("Bitfield validator index invalid");
const COST_MISSING_PEER_SESSION_KEY: Rep = Rep::CostMinor("Missing peer session key");
const COST_NOT_IN_VIEW: Rep = Rep::CostMinor("Not interested in that parent hash");
const COST_PEER_DUPLICATE_MESSAGE: Rep = Rep::CostMinorRepeated("Peer sent the same message multiple times");
const BENEFIT_VALID_MESSAGE_FIRST: Rep = Rep::BenefitMinorFirst("Valid message with new information");
const BENEFIT_VALID_MESSAGE: Rep = Rep::BenefitMinor("Valid message");

/// Checked signed availability bitfield that is distributed
/// to other peers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BitfieldGossipMessage {
	/// The relay parent this message is relative to.
	relay_parent: Hash,
	/// The actual signed availability bitfield.
	signed_availability: SignedAvailabilityBitfield,
}

impl BitfieldGossipMessage {
	fn into_validation_protocol(self) -> protocol_v1::ValidationProtocol {
		protocol_v1::ValidationProtocol::BitfieldDistribution(
			self.into_network_message()
		)
	}

	fn into_network_message(self)
		-> protocol_v1::BitfieldDistributionMessage
	{
		protocol_v1::BitfieldDistributionMessage::Bitfield(
			self.relay_parent,
			self.signed_availability.into(),
		)
	}
}

/// Data used to track information of peers and relay parents the
/// overseer ordered us to work on.
#[derive(Default, Debug)]
struct ProtocolState {
	/// Track all active peers and their views
	/// to determine what is relevant to them.
	peer_views: HashMap<PeerId, View>,

	/// Track all our neighbors in the current gossip topology.
	/// We're not necessarily connected to all of them.
	gossip_peers: HashSet<PeerId>,

	/// Our current view.
	view: OurView,

	/// Additional data particular to a relay parent.
	per_relay_parent: HashMap<Hash, PerRelayParentData>,
}

/// Data for a particular relay parent.
#[derive(Debug)]
struct PerRelayParentData {
	/// Signing context for a particular relay parent.
	signing_context: SigningContext,

	/// Set of validators for a particular relay parent.
	validator_set: Vec<ValidatorId>,

	/// Set of validators for a particular relay parent for which we
	/// received a valid `BitfieldGossipMessage`.
	/// Also serves as the list of known messages for peers connecting
	/// after bitfield gossips were already received.
	one_per_validator: HashMap<ValidatorId, BitfieldGossipMessage>,

	/// Avoid duplicate message transmission to our peers.
	message_sent_to_peer: HashMap<PeerId, HashSet<ValidatorId>>,

	/// Track messages that were already received by a peer
	/// to prevent flooding.
	message_received_from_peer: HashMap<PeerId, HashSet<ValidatorId>>,

	/// The span for this leaf/relay parent.
	span: PerLeafSpan,
}

impl PerRelayParentData {
	/// Create a new instance.
	fn new(signing_context: SigningContext, validator_set: Vec<ValidatorId>, span: PerLeafSpan) -> Self {
		Self {
			signing_context,
			validator_set,
			span,
			one_per_validator: Default::default(),
			message_sent_to_peer: Default::default(),
			message_received_from_peer: Default::default(),
		}
	}

	/// Determines if that particular message signed by a validator is needed by the given peer.
	fn message_from_validator_needed_by_peer(
		&self,
		peer: &PeerId,
		validator: &ValidatorId,
	) -> bool {
		self.message_sent_to_peer.get(peer).map(|v| !v.contains(validator)).unwrap_or(true)
			&& self.message_received_from_peer.get(peer).map(|v| !v.contains(validator)).unwrap_or(true)
	}
}

const LOG_TARGET: &str = "parachain::bitfield-distribution";

/// The bitfield distribution subsystem.
pub struct BitfieldDistribution {
	metrics: Metrics,
}

impl BitfieldDistribution {
	/// Create a new instance of the `BitfieldDistribution` subsystem.
	pub fn new(metrics: Metrics) -> Self {
		Self { metrics }
	}

	/// Start processing work as passed on from the Overseer.
	async fn run<Context>(self, mut ctx: Context)
	where
		Context: overseer::SubsystemContext<Message = BitfieldDistributionMessage>,
	{
		// work: process incoming messages from the overseer and process accordingly.
		let mut state = ProtocolState::default();
		loop {
			let message = match ctx.recv().await {
				Ok(message) => message,
				Err(e) => {
					tracing::debug!(target: LOG_TARGET, err = ?e, "Failed to receive a message from Overseer, exiting");
					return;
				},
			};
			match message {
				FromOverseer::Communication {
					msg: BitfieldDistributionMessage::DistributeBitfield(hash, signed_availability),
				} => {
					tracing::trace!(
						target: LOG_TARGET,
						?hash,
						"Processing DistributeBitfield"
					);
					handle_bitfield_distribution(
						&mut ctx,
						&mut state,
						&self.metrics,
						hash,
						signed_availability,
					).await;
				}
				FromOverseer::Communication {
					msg: BitfieldDistributionMessage::NetworkBridgeUpdateV1(event),
				} => {
					tracing::trace!(target: LOG_TARGET, "Processing NetworkMessage");
					// a network message was received
					handle_network_msg(&mut ctx, &mut state, &self.metrics, event).await;
				}
				FromOverseer::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate { activated, .. })) => {
					let _timer = self.metrics.time_active_leaves_update();

					for activated in activated {
						let relay_parent = activated.hash;

						tracing::trace!(target: LOG_TARGET, relay_parent = %relay_parent, "activated");
						let span = PerLeafSpan::new(activated.span, "bitfield-distribution");
						let _span = span.child("query-basics");

						// query validator set and signing context per relay_parent once only
						match query_basics(&mut ctx, relay_parent).await {
							Ok(Some((validator_set, signing_context))) => {
								// If our runtime API fails, we don't take down the node,
								// but we might alter peers' reputations erroneously as a result
								// of not having the correct bookkeeping. If we have lost a race
								// with state pruning, it is unlikely that peers will be sending
								// us anything to do with this relay-parent anyway.
								let _ = state.per_relay_parent.insert(
									relay_parent,
									PerRelayParentData::new(signing_context, validator_set, span),
								);
							}
							Err(e) => {
								tracing::warn!(target: LOG_TARGET, err = ?e, "query_basics has failed");
							}
							_ => {},
						}
					}
				}
				FromOverseer::Signal(OverseerSignal::BlockFinalized(hash, number)) => {
					tracing::trace!(target: LOG_TARGET, hash = %hash, number = %number, "block finalized");
				}
				FromOverseer::Signal(OverseerSignal::Conclude) => {
					tracing::trace!(target: LOG_TARGET, "Conclude");
					return;
				}
			}
		}
	}
}

/// Modify the reputation of a peer based on its behavior.
async fn modify_reputation<Context>(
	ctx: &mut Context,
	peer: PeerId,
	rep: Rep,
)
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	tracing::trace!(target: LOG_TARGET, ?rep, peer_id = %peer, "reputation change");

	ctx.send_message(
		NetworkBridgeMessage::ReportPeer(peer, rep),
	)
	.await
}

/// Distribute a given valid and signature checked bitfield message.
///
/// For this variant the source is this node.
async fn handle_bitfield_distribution<Context>(
	ctx: &mut Context,
	state: &mut ProtocolState,
	metrics: &Metrics,
	relay_parent: Hash,
	signed_availability: SignedAvailabilityBitfield,
)
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	let _timer = metrics.time_handle_bitfield_distribution();

	// Ignore anything the overseer did not tell this subsystem to work on
	let mut job_data = state.per_relay_parent.get_mut(&relay_parent);
	let job_data: &mut _ = if let Some(ref mut job_data) = job_data {
		job_data
	} else {
		tracing::trace!(
			target: LOG_TARGET,
			relay_parent = %relay_parent,
			"Not supposed to work on relay parent related data",
		);

		return;
	};
	let validator_set = &job_data.validator_set;
	if validator_set.is_empty() {
		tracing::trace!(target: LOG_TARGET, relay_parent = %relay_parent, "validator set is empty");
		return;
	}

	let validator_index = signed_availability.validator_index().0 as usize;
	let validator = if let Some(validator) = validator_set.get(validator_index) {
		validator.clone()
	} else {
		tracing::trace!(target: LOG_TARGET, "Could not find a validator for index {}", validator_index);
		return;
	};

	let msg = BitfieldGossipMessage {
		relay_parent,
		signed_availability,
	};

	let gossip_peers = &state.gossip_peers;
	let peer_views = &mut state.peer_views;
	relay_message(ctx, job_data, gossip_peers, peer_views, validator, msg).await;

	metrics.on_own_bitfield_gossipped();
}

/// Distribute a given valid and signature checked bitfield message.
///
/// Can be originated by another subsystem or received via network from another peer.
async fn relay_message<Context>(
	ctx: &mut Context,
	job_data: &mut PerRelayParentData,
	gossip_peers: &HashSet<PeerId>,
	peer_views: &mut HashMap<PeerId, View>,
	validator: ValidatorId,
	message: BitfieldGossipMessage,
)
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	let span = job_data.span.child("relay-msg");

	let _span = span.child("provisionable");
	// notify the overseer about a new and valid signed bitfield
	ctx.send_message(
		ProvisionerMessage::ProvisionableData(
			message.relay_parent,
			ProvisionableData::Bitfield(
				message.relay_parent,
				message.signed_availability.clone(),
			),
		),
	)
	.await;

	drop(_span);

	let _span = span.child("interested-peers");
	// pass on the bitfield distribution to all interested peers
	let interested_peers = peer_views
		.iter()
		.filter_map(|(peer, view)| {
			// check interest in the peer in this message's relay parent
			if view.contains(&message.relay_parent) {
				let message_needed = job_data.message_from_validator_needed_by_peer(&peer, &validator);
				if message_needed {
					Some(peer.clone())
				} else {
					None
				}
			} else {
				None
			}
		})
		.collect::<Vec<PeerId>>();
	let interested_peers = util::choose_random_subset(
		|e| gossip_peers.contains(e),
		interested_peers,
		MIN_GOSSIP_PEERS,
	);
	interested_peers.iter()
		.for_each(|peer|{
			// track the message as sent for this peer
			job_data.message_sent_to_peer
				.entry(peer.clone())
				.or_default()
				.insert(validator.clone());
		});

	drop(_span);

	if interested_peers.is_empty() {
		tracing::trace!(
			target: LOG_TARGET,
			relay_parent = %message.relay_parent,
			"no peers are interested in gossip for relay parent",
		);
	} else {
		let _span = span.child("gossip");
		ctx.send_message(
			NetworkBridgeMessage::SendValidationMessage(
				interested_peers,
				message.into_validation_protocol(),
			),
		)
		.await;
	}
}

/// Handle an incoming message from a peer.
async fn process_incoming_peer_message<Context>(
	ctx: &mut Context,
	state: &mut ProtocolState,
	metrics: &Metrics,
	origin: PeerId,
	message: protocol_v1::BitfieldDistributionMessage,
)
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	let protocol_v1::BitfieldDistributionMessage::Bitfield(relay_parent, bitfield) = message;
	tracing::trace!(
		target: LOG_TARGET,
		peer_id = %origin,
		?relay_parent,
		"received bitfield gossip from peer"
	);
	// we don't care about this, not part of our view.
	if !state.view.contains(&relay_parent) {
		modify_reputation(ctx, origin, COST_NOT_IN_VIEW).await;
		return;
	}

	// Ignore anything the overseer did not tell this subsystem to work on.
	let mut job_data = state.per_relay_parent.get_mut(&relay_parent);
	let job_data: &mut _ = if let Some(ref mut job_data) = job_data {
		job_data
	} else {
		modify_reputation(ctx, origin, COST_NOT_IN_VIEW).await;
		return;
	};

	let validator_index = bitfield.unchecked_validator_index();

	let mut _span = job_data.span
			.child("msg-received")
			.with_peer_id(&origin)
			.with_claimed_validator_index(validator_index)
			.with_stage(jaeger::Stage::BitfieldDistribution);

	let validator_set = &job_data.validator_set;
	if validator_set.is_empty() {
		tracing::trace!(
			target: LOG_TARGET,
			relay_parent = %relay_parent,
			?origin,
			"Validator set is empty",
		);
		modify_reputation(ctx, origin, COST_MISSING_PEER_SESSION_KEY).await;
		return;
	}

	// Use the (untrusted) validator index provided by the signed payload
	// and see if that one actually signed the availability bitset.
	let signing_context = job_data.signing_context.clone();
	let validator = if let Some(validator) = validator_set.get(validator_index.0 as usize) {
		validator.clone()
	} else {
		modify_reputation(ctx, origin, COST_VALIDATOR_INDEX_INVALID).await;
		return;
	};

	// Check if the peer already sent us a message for the validator denoted in the message earlier.
	// Must be done after validator index verification, in order to avoid storing an unbounded
	// number of set entries.
	let received_set = job_data
		.message_received_from_peer
		.entry(origin.clone())
		.or_default();

	if !received_set.contains(&validator) {
		received_set.insert(validator.clone());
	} else {
		tracing::trace!(
			target: LOG_TARGET,
			?validator_index,
			?origin,
			"Duplicate message",
		);
		modify_reputation(ctx, origin, COST_PEER_DUPLICATE_MESSAGE).await;
		return;
	};

	let one_per_validator = &mut (job_data.one_per_validator);

	// only relay_message a message of a validator once
	if let Some(old_message) = one_per_validator.get(&validator) {
		tracing::trace!(
			target: LOG_TARGET,
			?validator_index,
			"already received a message for validator",
		);
		if old_message.signed_availability.as_unchecked() == &bitfield {
			modify_reputation(ctx, origin, BENEFIT_VALID_MESSAGE).await;
		}
		return;
	}
	let signed_availability = match bitfield.try_into_checked(&signing_context, &validator) {
		Err(_) => {
			modify_reputation(ctx, origin, COST_SIGNATURE_INVALID).await;
			return;
		},
		Ok(bitfield) => bitfield,
	};

	let message = BitfieldGossipMessage {
		relay_parent,
		signed_availability,
	};

	metrics.on_bitfield_received();
	one_per_validator.insert(validator.clone(), message.clone());

	relay_message(ctx, job_data, &state.gossip_peers, &mut state.peer_views, validator, message).await;

	modify_reputation(ctx, origin, BENEFIT_VALID_MESSAGE_FIRST).await
}

/// Deal with network bridge updates and track what needs to be tracked
/// which depends on the message type received.
async fn handle_network_msg<Context>(
	ctx: &mut Context,
	state: &mut ProtocolState,
	metrics: &Metrics,
	bridge_message: NetworkBridgeEvent<protocol_v1::BitfieldDistributionMessage>,
)
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	let _timer = metrics.time_handle_network_msg();

	match bridge_message {
		NetworkBridgeEvent::PeerConnected(peerid, role, _) => {
			tracing::trace!(
				target: LOG_TARGET,
				?peerid,
				?role,
				"Peer connected",
			);
			// insert if none already present
			state.peer_views.entry(peerid).or_default();
		}
		NetworkBridgeEvent::PeerDisconnected(peerid) => {
			tracing::trace!(
				target: LOG_TARGET,
				?peerid,
				"Peer disconnected",
			);
			// get rid of superfluous data
			state.peer_views.remove(&peerid);
		}
		NetworkBridgeEvent::NewGossipTopology(peers) => {
			let newly_added: Vec<PeerId> = peers.difference(&state.gossip_peers).cloned().collect();
			state.gossip_peers = peers;
			for peer in newly_added {
				if let Some(view) = state.peer_views.remove(&peer) {
					handle_peer_view_change(ctx, state, peer, view).await;
				}
			}
		}
		NetworkBridgeEvent::PeerViewChange(peerid, view) => {
			tracing::trace!(
				target: LOG_TARGET,
				?peerid,
				?view,
				"Peer view change",
			);
			handle_peer_view_change(ctx, state, peerid, view).await;
		}
		NetworkBridgeEvent::OurViewChange(view) => {
			tracing::trace!(
				target: LOG_TARGET,
				?view,
				"Our view change",
			);
			handle_our_view_change(state, view);
		}
		NetworkBridgeEvent::PeerMessage(remote, message) =>
			process_incoming_peer_message(ctx, state, metrics, remote, message).await,
	}
}

/// Handle the changes necessary when our view changes.
fn handle_our_view_change(state: &mut ProtocolState, view: OurView) {
	let old_view = std::mem::replace(&mut (state.view), view);

	for added in state.view.difference(&old_view) {
		if !state.per_relay_parent.contains_key(&added) {
			tracing::warn!(
				target: LOG_TARGET,
				added = %added,
				"Our view contains {} but the overseer never told use we should work on this",
				&added
			);
		}
	}
	for removed in old_view.difference(&state.view) {
		// cleanup relay parents we are not interested in any more
		let _ = state.per_relay_parent.remove(&removed);
	}
}


// Send the difference between two views which were not sent
// to that particular peer.
async fn handle_peer_view_change<Context>(
	ctx: &mut Context,
	state: &mut ProtocolState,
	origin: PeerId,
	view: View,
)
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	let added = state.peer_views.entry(origin.clone()).or_default().replace_difference(view).cloned().collect::<Vec<_>>();

	let is_gossip_peer = state.gossip_peers.contains(&origin);
	let lucky = is_gossip_peer || util::gen_ratio(
		util::MIN_GOSSIP_PEERS.saturating_sub(state.gossip_peers.len()),
		util::MIN_GOSSIP_PEERS,
	);

	if !lucky {
		tracing::trace!(
			target: LOG_TARGET,
			?origin,
			"Peer view change is ignored",
		);
		return;
	}

	// Send all messages we've seen before and the peer is now interested
	// in to that peer.
	let delta_set: Vec<(ValidatorId, BitfieldGossipMessage)> = added
		.into_iter()
		.filter_map(|new_relay_parent_interest| {
			if let Some(job_data) = (&*state).per_relay_parent.get(&new_relay_parent_interest) {
				// Send all jointly known messages for a validator (given the current relay parent)
				// to the peer `origin`...
				let one_per_validator = job_data.one_per_validator.clone();
				let origin = origin.clone();
				Some(
					one_per_validator
						.into_iter()
						.filter(move |(validator, _message)| {
							// ..except for the ones the peer already has.
							job_data.message_from_validator_needed_by_peer(&origin, validator)
						}),
				)
			} else {
				// A relay parent is in the peers view, which is not in ours, ignore those.
				None
			}
		})
		.flatten()
		.collect();

	for (validator, message) in delta_set.into_iter() {
		send_tracked_gossip_message(ctx, state, origin.clone(), validator, message).await;
	}
}

/// Send a gossip message and track it in the per relay parent data.
async fn send_tracked_gossip_message<Context>(
	ctx: &mut Context,
	state: &mut ProtocolState,
	dest: PeerId,
	validator: ValidatorId,
	message: BitfieldGossipMessage,
)
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	let job_data = if let Some(job_data) = state.per_relay_parent.get_mut(&message.relay_parent) {
		job_data
	} else {
		return;
	};

	let _span = job_data.span.child("gossip");
	tracing::trace!(
		target: LOG_TARGET,
		?dest,
		?validator,
		relay_parent = ?message.relay_parent,
		"Sending gossip message"
	);

	job_data.message_sent_to_peer
		.entry(dest.clone())
		.or_default()
		.insert(validator.clone());

	ctx.send_message(
		NetworkBridgeMessage::SendValidationMessage(
			vec![dest],
			message.into_validation_protocol(),
		),
	).await;
}

impl<Context> overseer::overseer::Subsystem<Context> for BitfieldDistribution
where
	Context: overseer::SubsystemContext<Message = BitfieldDistributionMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = self.run(ctx)
			.map(|_| Ok(()))
			.boxed();

		SpawnedSubsystem {
			name: "bitfield-distribution-subsystem",
			future,
		}
	}
}

/// Query our validator set and signing context for a particular relay parent.
async fn query_basics<Context>(
	ctx: &mut Context,
	relay_parent: Hash,
) -> SubsystemResult<Option<(Vec<ValidatorId>, SigningContext)>>
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	let (validators_tx, validators_rx) = oneshot::channel();
	let (session_tx, session_rx) = oneshot::channel();

	// query validators
	ctx.send_message(RuntimeApiMessage::Request(
		relay_parent.clone(),
		RuntimeApiRequest::Validators(validators_tx),
	)).await;

	// query signing context
	ctx.send_message(RuntimeApiMessage::Request(
		relay_parent.clone(),
		RuntimeApiRequest::SessionIndexForChild(session_tx),
	)).await;

	match (validators_rx.await?, session_rx.await?) {
		(Ok(v), Ok(s)) => Ok(Some((
			v,
			SigningContext { parent_hash: relay_parent, session_index: s },
		))),
		(Err(e), _) | (_, Err(e)) => {
			tracing::warn!(target: LOG_TARGET, err = ?e, "Failed to fetch basics from runtime API");
			Ok(None)
		}
	}
}

#[derive(Clone)]
struct MetricsInner {
	gossipped_own_availability_bitfields: prometheus::Counter<prometheus::U64>,
	received_availability_bitfields: prometheus::Counter<prometheus::U64>,
	active_leaves_update: prometheus::Histogram,
	handle_bitfield_distribution: prometheus::Histogram,
	handle_network_msg: prometheus::Histogram,
}

/// Bitfield Distribution metrics.
#[derive(Default, Clone)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_own_bitfield_gossipped(&self) {
		if let Some(metrics) = &self.0 {
			metrics.gossipped_own_availability_bitfields.inc();
		}
	}

	fn on_bitfield_received(&self) {
		if let Some(metrics) = &self.0 {
			metrics.received_availability_bitfields.inc();
		}
	}

	/// Provide a timer for `active_leaves_update` which observes on drop.
	fn time_active_leaves_update(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.active_leaves_update.start_timer())
	}

	/// Provide a timer for `handle_bitfield_distribution` which observes on drop.
	fn time_handle_bitfield_distribution(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.handle_bitfield_distribution.start_timer())
	}

	/// Provide a timer for `handle_network_msg` which observes on drop.
	fn time_handle_network_msg(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.handle_network_msg.start_timer())
	}
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry) -> Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			gossipped_own_availability_bitfields: prometheus::register(
				prometheus::Counter::new(
					"parachain_gossipped_own_availabilty_bitfields_total",
					"Number of own availability bitfields sent to other peers."
				)?,
				registry,
			)?,
			received_availability_bitfields: prometheus::register(
				prometheus::Counter::new(
					"parachain_received_availabilty_bitfields_total",
					"Number of valid availability bitfields received from other peers."
				)?,
				registry,
			)?,
			active_leaves_update: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_bitfield_distribution_active_leaves_update",
						"Time spent within `bitfield_distribution::active_leaves_update`",
					)
				)?,
				registry,
			)?,
			handle_bitfield_distribution: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_bitfield_distribution_handle_bitfield_distribution",
						"Time spent within `bitfield_distribution::handle_bitfield_distribution`",
					)
				)?,
				registry,
			)?,
			handle_network_msg: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_bitfield_distribution_handle_network_msg",
						"Time spent within `bitfield_distribution::handle_network_msg`",
					)
				)?,
				registry,
			)?,
		};
		Ok(Metrics(Some(metrics)))
	}
}
