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

use std::{collections::{HashMap, HashSet, VecDeque}, pin::Pin, time::Duration};

use futures::{FutureExt, StreamExt, channel::oneshot, stream::FuturesUnordered, select, Future};
use sp_core::Pair;

use polkadot_primitives::v1::{
	AuthorityDiscoveryId, CandidateHash, CandidateReceipt, CollatorPair, CoreIndex, CoreState, GroupIndex, Hash,
	Id as ParaId,
};
use polkadot_subsystem::{
	overseer,
	FromOverseer, OverseerSignal, PerLeafSpan, SubsystemContext, jaeger,
	messages::{
		CollatorProtocolMessage, NetworkBridgeEvent, NetworkBridgeMessage,
	},
};
use polkadot_node_network_protocol::{
	OurView, PeerId, UnifiedReputationChange as Rep, View, peer_set::PeerSet,
	request_response::{
		IncomingRequest, request::OutgoingResponse, v1::{CollationFetchingRequest, CollationFetchingResponse}
	},
	v1 as protocol_v1,
};
use polkadot_node_subsystem_util::{
	TimeoutExt,
	metrics::{self, prometheus},
	runtime::{RuntimeInfo, get_availability_cores, get_group_rotation_info}
};
use polkadot_node_primitives::{SignedFullStatement, Statement, PoV};

use crate::error::{Fatal, NonFatal, log_error};
use super::{LOG_TARGET,  Result};

#[cfg(test)]
mod tests;

const COST_UNEXPECTED_MESSAGE: Rep = Rep::CostMinor("An unexpected message");
const COST_APPARENT_FLOOD: Rep = Rep::CostMinor("Message received when previous one was still being processed");

/// Time after starting an upload to a validator we will start another one to the next validator,
/// even if the upload was not finished yet.
///
/// This is to protect from a single slow validator preventing collations from happening.
///
/// With a collation size of 5Meg and bandwidth of 500Mbit/s (requirement for Kusama validators),
/// the transfer should be possible within 0.1 seconds. 400 milliseconds should therefore be
/// plenty and should be low enough for later validators to still be able to finish on time.
///
/// There is debug logging output, so we can adjust this value based on production results.
const MAX_UNSHARED_UPLOAD_TIME: Duration = Duration::from_millis(400);

#[derive(Clone, Default)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_advertisment_made(&self) {
		if let Some(metrics) = &self.0 {
			metrics.advertisements_made.inc();
		}
	}

	fn on_collation_sent_requested(&self) {
		if let Some(metrics) = &self.0 {
			metrics.collations_send_requested.inc();
		}
	}

	fn on_collation_sent(&self) {
		if let Some(metrics) = &self.0 {
			metrics.collations_sent.inc();
		}
	}

	/// Provide a timer for `process_msg` which observes on drop.
	fn time_process_msg(&self) -> Option<prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.process_msg.start_timer())
	}
}

#[derive(Clone)]
struct MetricsInner {
	advertisements_made: prometheus::Counter<prometheus::U64>,
	collations_sent: prometheus::Counter<prometheus::U64>,
	collations_send_requested: prometheus::Counter<prometheus::U64>,
	process_msg: prometheus::Histogram,
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry)
		-> std::result::Result<Self, prometheus::PrometheusError>
	{
		let metrics = MetricsInner {
			advertisements_made: prometheus::register(
				prometheus::Counter::new(
					"parachain_collation_advertisements_made_total",
					"A number of collation advertisements sent to validators.",
				)?,
				registry,
			)?,
			collations_send_requested: prometheus::register(
				prometheus::Counter::new(
					"parachain_collations_sent_requested_total",
					"A number of collations requested to be sent to validators.",
				)?,
				registry,
			)?,
			collations_sent: prometheus::register(
				prometheus::Counter::new(
					"parachain_collations_sent_total",
					"A number of collations sent to validators.",
				)?,
				registry,
			)?,
			process_msg: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_collator_protocol_collator_process_msg",
						"Time spent within `collator_protocol_collator::process_msg`",
					)
				)?,
				registry,
			)?,
		};

		Ok(Metrics(Some(metrics)))
	}
}

/// The group of validators that is assigned to our para at a given point of time.
///
/// This structure is responsible for keeping track of which validators belong to a certain group for a para. It also
/// stores a mapping from [`PeerId`] to [`ValidatorId`] as we learn about it over the lifetime of this object. Besides
/// that it also keeps track to which validators we advertised our collation.
#[derive(Debug)]
struct ValidatorGroup {
	/// All [`AuthorityDiscoveryId`]'s that are assigned to us in this group.
	discovery_ids: HashSet<AuthorityDiscoveryId>,
	/// All [`ValidatorId`]'s of the current group to that we advertised our collation.
	advertised_to: HashSet<AuthorityDiscoveryId>,
}

impl ValidatorGroup {
	/// Returns `true` if we should advertise our collation to the given peer.
	fn should_advertise_to(&self, peer_ids: &HashMap<PeerId, AuthorityDiscoveryId>, peer: &PeerId)
		-> bool {
		match peer_ids.get(peer) {
			Some(discovery_id) => !self.advertised_to.contains(discovery_id),
			None => false,
		}
	}

	/// Should be called after we advertised our collation to the given `peer` to keep track of it.
	fn advertised_to_peer(&mut self, peer_ids: &HashMap<PeerId, AuthorityDiscoveryId>, peer: &PeerId) {
		if let Some(validator_id) = peer_ids.get(peer) {
			self.advertised_to.insert(validator_id.clone());
		}
	}
}

impl From<HashSet<AuthorityDiscoveryId>> for ValidatorGroup {
	fn from(discovery_ids: HashSet<AuthorityDiscoveryId>) -> Self {
		Self {
			discovery_ids,
			advertised_to: HashSet::new(),
		}
	}
}

/// The status of a collation as seen from the collator.
enum CollationStatus {
	/// The collation was created, but we did not advertise it to any validator.
	Created,
	/// The collation was advertised to at least one validator.
	Advertised,
	/// The collation was requested by at least one validator.
	Requested,
}

impl CollationStatus {
	/// Advance to the [`Self::Advertised`] status.
	///
	/// This ensures that `self` isn't already [`Self::Requested`].
	fn advance_to_advertised(&mut self) {
		if !matches!(self, Self::Requested) {
			*self = Self::Advertised;
		}
	}

	/// Advance to the [`Self::Requested`] status.
	fn advance_to_requested(&mut self) {
		*self = Self::Requested;
	}
}

/// A collation built by the collator.
struct Collation {
	receipt: CandidateReceipt,
	pov: PoV,
	status: CollationStatus,
}

/// Stores the state for waiting collation fetches.
#[derive(Default)]
struct WaitingCollationFetches {
	/// Is there currently a collation getting fetched?
	collation_fetch_active: bool,
	/// The collation fetches waiting to be fulfilled.
	waiting: VecDeque<IncomingRequest<CollationFetchingRequest>>,
	/// All peers that are waiting or actively uploading.
	///
	/// We will not accept multiple requests from the same peer, otherwise our DoS protection of
	/// moving on to the next peer after `MAX_UNSHARED_UPLOAD_TIME` would be pointless.
	waiting_peers: HashSet<PeerId>,
}

type ActiveCollationFetches = FuturesUnordered<Pin<Box<dyn Future<Output = (Hash, PeerId)> + Send + 'static>>>;

struct State {
	/// Our network peer id.
	local_peer_id: PeerId,

	/// Our collator pair.
	collator_pair: CollatorPair,

	/// The para this collator is collating on.
	/// Starts as `None` and is updated with every `CollateOn` message.
	collating_on: Option<ParaId>,

	/// Track all active peers and their views
	/// to determine what is relevant to them.
	peer_views: HashMap<PeerId, View>,

	/// Our own view.
	view: OurView,

	/// Span per relay parent.
	span_per_relay_parent: HashMap<Hash, PerLeafSpan>,

	/// Possessed collations.
	///
	/// We will keep up to one local collation per relay-parent.
	collations: HashMap<Hash, Collation>,

	/// The result senders per collation.
	collation_result_senders: HashMap<CandidateHash, oneshot::Sender<SignedFullStatement>>,

	/// Our validator groups per active leaf.
	our_validators_groups: HashMap<Hash, ValidatorGroup>,

	/// The mapping from [`PeerId`] to [`ValidatorId`]. This is filled over time as we learn the [`PeerId`]'s
	/// by `PeerConnected` events.
	peer_ids: HashMap<PeerId, AuthorityDiscoveryId>,

	/// Metrics.
	metrics: Metrics,

	/// All collation fetching requests that are still waiting to be answered.
	///
	/// They are stored per relay parent, when our view changes and the relay parent moves out, we will cancel the fetch
	/// request.
	waiting_collation_fetches: HashMap<Hash, WaitingCollationFetches>,

	/// Active collation fetches.
	///
	/// Each future returns the relay parent of the finished collation fetch.
	active_collation_fetches: ActiveCollationFetches,
}

impl State {
	/// Creates a new `State` instance with the given parameters and setting all remaining
	/// state fields to their default values (i.e. empty).
	fn new(local_peer_id: PeerId, collator_pair: CollatorPair, metrics: Metrics) -> State {
		State {
			local_peer_id,
			collator_pair,
			metrics,
			collating_on: Default::default(),
			peer_views: Default::default(),
			view: Default::default(),
			span_per_relay_parent: Default::default(),
			collations: Default::default(),
			collation_result_senders: Default::default(),
			our_validators_groups: Default::default(),
			peer_ids: Default::default(),
			waiting_collation_fetches: Default::default(),
			active_collation_fetches: Default::default(),
		}
	}

	/// Get all peers which have the given relay parent in their view.
	fn peers_interested_in_leaf(&self, relay_parent: &Hash) -> Vec<PeerId> {
		self.peer_views
			.iter()
			.filter(|(_, v)| v.contains(relay_parent))
			.map(|(peer, _)| *peer)
			.collect()
	}
}

/// Distribute a collation.
///
/// Figure out the core our para is assigned to and the relevant validators.
/// Issue a connection request to these validators.
/// If the para is not scheduled or next up on any core, at the relay-parent,
/// or the relay-parent isn't in the active-leaves set, we ignore the message
/// as it must be invalid in that case - although this indicates a logic error
/// elsewhere in the node.
async fn distribute_collation<Context>(
	ctx: &mut Context,
	runtime: &mut RuntimeInfo,
	state: &mut State,
	id: ParaId,
	receipt: CandidateReceipt,
	pov: PoV,
	result_sender: Option<oneshot::Sender<SignedFullStatement>>,
) -> Result<()>
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>,
{
	let relay_parent = receipt.descriptor.relay_parent;

	// This collation is not in the active-leaves set.
	if !state.view.contains(&relay_parent) {
		tracing::warn!(
			target: LOG_TARGET,
			?relay_parent,
			"distribute collation message parent is outside of our view",
		);

		return Ok(());
	}

	// We have already seen collation for this relay parent.
	if state.collations.contains_key(&relay_parent) {
		return Ok(());
	}

	// Determine which core the para collated-on is assigned to.
	// If it is not scheduled then ignore the message.
	let (our_core, num_cores) = match determine_core(ctx, id, relay_parent).await? {
		Some(core) => core,
		None => {
			tracing::warn!(
				target: LOG_TARGET,
				para_id = %id,
				?relay_parent,
				"looks like no core is assigned to {} at {}", id, relay_parent,
			);

			return Ok(())
		}
	};

	// Determine the group on that core and the next group on that core.
	let (current_validators, next_validators) =
		determine_our_validators(ctx, runtime, our_core, num_cores, relay_parent,).await?;

	if current_validators.validators.is_empty() && next_validators.validators.is_empty() {
		tracing::warn!(
			target: LOG_TARGET,
			core = ?our_core,
			"there are no validators assigned to core",
		);

		return Ok(());
	}

	tracing::debug!(
		target: LOG_TARGET,
		para_id = %id,
		relay_parent = %relay_parent,
		candidate_hash = ?receipt.hash(),
		pov_hash = ?pov.hash(),
		core = ?our_core,
		?current_validators,
		?next_validators,
		"Accepted collation, connecting to validators."
	);

	let validator_group: HashSet<_> = current_validators.validators.iter().map(Clone::clone).collect();

	// Issue a discovery request for the validators of the current group and the next group:
	connect_to_validators(
		ctx,
		current_validators.validators
			.into_iter()
			.chain(next_validators.validators.into_iter())
			.collect(),
	).await;

	state.our_validators_groups.insert(relay_parent, validator_group.into());

	if let Some(result_sender) = result_sender {
		state.collation_result_senders.insert(receipt.hash(), result_sender);
	}

	state.collations.insert(relay_parent, Collation { receipt, pov, status: CollationStatus::Created });

	let interested = state.peers_interested_in_leaf(&relay_parent);
	// Make sure already connected peers get collations:
	for peer_id in interested {
		advertise_collation(ctx, state, relay_parent, peer_id).await;
	}

	Ok(())
}

/// Get the Id of the Core that is assigned to the para being collated on if any
/// and the total number of cores.
async fn determine_core<Context>(
	ctx: &mut Context,
	para_id: ParaId,
	relay_parent: Hash,
) -> Result<Option<(CoreIndex, usize)>>
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>,
{
	let cores = get_availability_cores(ctx, relay_parent).await?;

	for (idx, core) in cores.iter().enumerate() {
		if let CoreState::Scheduled(occupied) = core {
			if occupied.para_id == para_id {
				return Ok(Some(((idx as u32).into(), cores.len())));
			}
		}
	}

	Ok(None)
}

/// Validators of a particular group index.
#[derive(Debug)]
struct GroupValidators {
	/// The group those validators belong to.
	group: GroupIndex,
	/// The validators of above group (their discovery keys).
	validators: Vec<AuthorityDiscoveryId>,
}

/// Figure out current and next group of validators assigned to the para being collated on.
///
/// Returns [`ValidatorId`]'s of current and next group as determined based on the `relay_parent`.
async fn determine_our_validators<Context>(
	ctx: &mut Context,
	runtime: &mut RuntimeInfo,
	core_index: CoreIndex,
	cores: usize,
	relay_parent: Hash,
) -> Result<(GroupValidators, GroupValidators)>
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>,
{
	let session_index = runtime.get_session_index(ctx.sender(), relay_parent).await?;
	let info = &runtime.get_session_info_by_index(ctx.sender(), relay_parent, session_index)
		.await?
		.session_info;
	tracing::debug!(target: LOG_TARGET, ?session_index, "Received session info");
	let groups = &info.validator_groups;
	let rotation_info = get_group_rotation_info(ctx, relay_parent).await?;

	let current_group_index = rotation_info.group_for_core(core_index, cores);
	let current_validators = groups.get(current_group_index.0 as usize).map(|v| v.as_slice()).unwrap_or_default();

	let next_group_idx = (current_group_index.0 as usize + 1) % groups.len();
	let next_validators = groups.get(next_group_idx).map(|v| v.as_slice()).unwrap_or_default();

	let validators = &info.discovery_keys;

	let current_validators = current_validators.iter().map(|i| validators[i.0 as usize].clone()).collect();
	let next_validators = next_validators.iter().map(|i| validators[i.0 as usize].clone()).collect();

	let current_validators = GroupValidators {
		group: current_group_index,
		validators: current_validators,
	};
	let next_validators = GroupValidators {
		group: GroupIndex(next_group_idx as u32),
		validators: next_validators,
	};

	Ok((current_validators, next_validators))
}

/// Issue a `Declare` collation message to the given `peer`.
async fn declare<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer: PeerId,
)
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>,
{
	let declare_signature_payload = protocol_v1::declare_signature_payload(&state.local_peer_id);

	if let Some(para_id) = state.collating_on {
		let wire_message = protocol_v1::CollatorProtocolMessage::Declare(
			state.collator_pair.public(),
			para_id,
			state.collator_pair.sign(&declare_signature_payload),
		);

		ctx.send_message(
			NetworkBridgeMessage::SendCollationMessage(
				vec![peer],
				protocol_v1::CollationProtocol::CollatorProtocol(wire_message),
			)
		).await;
	}
}

/// Issue a connection request to a set of validators and
/// revoke the previous connection request.
async fn connect_to_validators<Context>(
	ctx: &mut Context,
	validator_ids: Vec<AuthorityDiscoveryId>,
)
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>,
{
	// ignore address resolution failure
	// will reissue a new request on new collation
	let (failed, _) = oneshot::channel();
	ctx.send_message(NetworkBridgeMessage::ConnectToValidators {
		validator_ids, peer_set: PeerSet::Collation, failed,
	}).await;
}

/// Advertise collation to the given `peer`.
///
/// This will only advertise a collation if there exists one for the given `relay_parent` and the given `peer` is
/// set as validator for our para at the given `relay_parent`.
async fn advertise_collation<Context>(
	ctx: &mut Context,
	state: &mut State,
	relay_parent: Hash,
	peer: PeerId,
)
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>,
{
	let should_advertise = state.our_validators_groups
		.get(&relay_parent)
		.map(|g| g.should_advertise_to(&state.peer_ids, &peer))
		.unwrap_or(false);

	match (state.collations.get_mut(&relay_parent), should_advertise) {
		(None, _) => {
			tracing::trace!(
				target: LOG_TARGET,
				?relay_parent,
				peer_id = %peer,
				"No collation to advertise.",
			);
			return
		},
		(_, false) => {
			tracing::debug!(
				target: LOG_TARGET,
				?relay_parent,
				peer_id = %peer,
				"Not advertising collation as we already advertised it to this validator.",
			);
			return
		}
		(Some(collation), true) => {
			tracing::debug!(
				target: LOG_TARGET,
				?relay_parent,
				peer_id = %peer,
				"Advertising collation.",
			);
			collation.status.advance_to_advertised()
		},
	}

	let wire_message = protocol_v1::CollatorProtocolMessage::AdvertiseCollation(
		relay_parent,
	);

	ctx.send_message(
		NetworkBridgeMessage::SendCollationMessage(
			vec![peer.clone()],
			protocol_v1::CollationProtocol::CollatorProtocol(wire_message),
		)
	).await;

	if let Some(validators) = state.our_validators_groups.get_mut(&relay_parent) {
		validators.advertised_to_peer(&state.peer_ids, &peer);
	}

	state.metrics.on_advertisment_made();
}

/// The main incoming message dispatching switch.
async fn process_msg<Context>(
	ctx: &mut Context,
	runtime: &mut RuntimeInfo,
	state: &mut State,
	msg: CollatorProtocolMessage,
) -> Result<()>
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>,
{
	use CollatorProtocolMessage::*;

	let _timer = state.metrics.time_process_msg();

	match msg {
		CollateOn(id) => {
			state.collating_on = Some(id);
		}
		DistributeCollation(receipt, pov, result_sender) => {
			let _span1 = state.span_per_relay_parent
				.get(&receipt.descriptor.relay_parent).map(|s| s.child("distributing-collation"));
			let _span2 = jaeger::Span::new(&pov, "distributing-collation");
			match state.collating_on {
				Some(id) if receipt.descriptor.para_id != id => {
					// If the ParaId of a collation requested to be distributed does not match
					// the one we expect, we ignore the message.
					tracing::warn!(
						target: LOG_TARGET,
						para_id = %receipt.descriptor.para_id,
						collating_on = %id,
						"DistributeCollation for unexpected para_id",
					);
				}
				Some(id) => {
					distribute_collation(ctx, runtime, state, id, receipt, pov, result_sender).await?;
				}
				None => {
					tracing::warn!(
						target: LOG_TARGET,
						para_id = %receipt.descriptor.para_id,
						"DistributeCollation message while not collating on any",
					);
				}
			}
		}
		ReportCollator(_) => {
			tracing::warn!(
				target: LOG_TARGET,
				"ReportCollator message is not expected on the collator side of the protocol",
			);
		}
		NetworkBridgeUpdateV1(event) => {
			if let Err(e) = handle_network_msg(
				ctx,
				runtime,
				state,
				event,
			).await {
				tracing::warn!(
					target: LOG_TARGET,
					err = ?e,
					"Failed to handle incoming network message",
				);
			}
		},
		CollationFetchingRequest(incoming) => {
			let _span = state.span_per_relay_parent.get(&incoming.payload.relay_parent).map(|s| s.child("request-collation"));
			match state.collating_on {
				Some(our_para_id) => {
					if our_para_id == incoming.payload.para_id {
						let (receipt, pov) = if let Some(collation) = state.collations.get_mut(&incoming.payload.relay_parent) {
							collation.status.advance_to_requested();
							(collation.receipt.clone(), collation.pov.clone())
						} else {
							tracing::warn!(
								target: LOG_TARGET,
								relay_parent = %incoming.payload.relay_parent,
								"received a `RequestCollation` for a relay parent we don't have collation stored.",
							);

							return Ok(());
						};

						state.metrics.on_collation_sent_requested();

						let _span = _span.as_ref().map(|s| s.child("sending"));

						let waiting = state.waiting_collation_fetches.entry(incoming.payload.relay_parent).or_default();

						if !waiting.waiting_peers.insert(incoming.peer) {
							tracing::debug!(
								target: LOG_TARGET,
								"Dropping incoming request as peer has a request in flight already."
							);
							ctx.send_message(
								NetworkBridgeMessage::ReportPeer(incoming.peer, COST_APPARENT_FLOOD)
							).await;
							return Ok(())
						}

						if waiting.collation_fetch_active {
							waiting.waiting.push_back(incoming);
						} else {
							waiting.collation_fetch_active = true;
							send_collation(state, incoming, receipt, pov).await;
						}
					} else {
						tracing::warn!(
							target: LOG_TARGET,
							for_para_id = %incoming.payload.para_id,
							our_para_id = %our_para_id,
							"received a `CollationFetchingRequest` for unexpected para_id",
						);
					}
				}
				None => {
					tracing::warn!(
						target: LOG_TARGET,
						for_para_id = %incoming.payload.para_id,
						"received a `RequestCollation` while not collating on any para",
					);
				}
			}
		}
		_ => {},
	}

	Ok(())
}

/// Issue a response to a previously requested collation.
async fn send_collation(
	state: &mut State,
	request: IncomingRequest<CollationFetchingRequest>,
	receipt: CandidateReceipt,
	pov: PoV,
) {
	let (tx, rx) = oneshot::channel();

	let relay_parent = request.payload.relay_parent;
	let peer_id = request.peer;

	let response = OutgoingResponse {
		result: Ok(CollationFetchingResponse::Collation(receipt, pov)),
		reputation_changes: Vec::new(),
		sent_feedback: Some(tx),
	};

	if let Err(_) = request.send_outgoing_response(response) {
		tracing::warn!(
			target: LOG_TARGET,
			"Sending collation response failed",
		);
	}

	state.active_collation_fetches.push(async move {
		let r = rx.timeout(MAX_UNSHARED_UPLOAD_TIME).await;
		if r.is_none() {
			tracing::debug!(
				target: LOG_TARGET,
				?relay_parent,
				?peer_id,
				"Sending collation to validator timed out, carrying on with next validator."
			);
		}
		(relay_parent, peer_id)
	}.boxed());

	state.metrics.on_collation_sent();
}

/// A networking messages switch.
async fn handle_incoming_peer_message<Context>(
	ctx: &mut Context,
	runtime: &mut RuntimeInfo,
	state: &mut State,
	origin: PeerId,
	msg: protocol_v1::CollatorProtocolMessage,
) -> Result<()>
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>,
{
	use protocol_v1::CollatorProtocolMessage::*;

	match msg {
		Declare(_, _, _) => {
			tracing::trace!(
				target: LOG_TARGET,
				?origin,
				"Declare message is not expected on the collator side of the protocol",
			);

			// If we are declared to, this is another collator, and we should disconnect.
			ctx.send_message(
				NetworkBridgeMessage::DisconnectPeer(origin, PeerSet::Collation)
			).await;
		}
		AdvertiseCollation(_) => {
			tracing::trace!(
				target: LOG_TARGET,
				?origin,
				"AdvertiseCollation message is not expected on the collator side of the protocol",
			);

			ctx.send_message(
				NetworkBridgeMessage::ReportPeer(origin.clone(), COST_UNEXPECTED_MESSAGE)
			).await;

			// If we are advertised to, this is another collator, and we should disconnect.
			ctx.send_message(
				NetworkBridgeMessage::DisconnectPeer(origin, PeerSet::Collation)
			).await;
		}
		CollationSeconded(relay_parent, statement) => {
			if !matches!(statement.unchecked_payload(), Statement::Seconded(_)) {
				tracing::warn!(
					target: LOG_TARGET,
					?statement,
					?origin,
					"Collation seconded message received with none-seconded statement.",
				);
			} else {
				let statement = runtime.check_signature(ctx.sender(), relay_parent, statement)
					.await?
					.map_err(NonFatal::InvalidStatementSignature)?;

				let removed = state.collation_result_senders
					.remove(&statement.payload().candidate_hash());

				if let Some(sender) = removed {
					tracing::trace!(
						target: LOG_TARGET,
						?statement,
						?origin,
						"received a `CollationSeconded`",
					);
					let _ = sender.send(statement);
				}
			}
		}
	}

	Ok(())
}

/// Our view has changed.
async fn handle_peer_view_change<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer_id: PeerId,
	view: View,
)
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>,
{
	let current = state.peer_views.entry(peer_id.clone()).or_default();

	let added: Vec<Hash> = view.difference(&*current).cloned().collect();

	*current = view;

	for added in added.into_iter() {
		advertise_collation(ctx, state, added, peer_id.clone()).await;
	}
}

/// Bridge messages switch.
async fn handle_network_msg<Context>(
	ctx: &mut Context,
	runtime: &mut RuntimeInfo,
	state: &mut State,
	bridge_message: NetworkBridgeEvent<protocol_v1::CollatorProtocolMessage>,
) -> Result<()>
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>,
{
	use NetworkBridgeEvent::*;

	match bridge_message {
		PeerConnected(peer_id, observed_role, maybe_authority) => {
			// If it is possible that a disconnected validator would attempt a reconnect
			// it should be handled here.
			tracing::trace!(
				target: LOG_TARGET,
				?peer_id,
				?observed_role,
				"Peer connected",
			);
			if let Some(authority) = maybe_authority {
				tracing::trace!(
					target: LOG_TARGET,
					?authority,
					?peer_id,
					"Connected to requested validator"
				);
				state.peer_ids.insert(peer_id, authority);

				declare(ctx, state, peer_id).await;
			}
		}
		PeerViewChange(peer_id, view) => {
			tracing::trace!(
				target: LOG_TARGET,
				?peer_id,
				?view,
				"Peer view change",
			);
			handle_peer_view_change(ctx, state, peer_id, view).await;
		}
		PeerDisconnected(peer_id) => {
			tracing::trace!(
				target: LOG_TARGET,
				?peer_id,
				"Peer disconnected",
			);
			state.peer_views.remove(&peer_id);
			state.peer_ids.remove(&peer_id);
		}
		OurViewChange(view) => {
			tracing::trace!(
				target: LOG_TARGET,
				?view,
				"Own view change",
			);
			handle_our_view_change(state, view).await?;
		}
		PeerMessage(remote, msg) => {
			handle_incoming_peer_message(ctx, runtime, state, remote, msg).await?;
		}
		NewGossipTopology(..) => {
			// impossibru!
		}
	}

	Ok(())
}

/// Handles our view changes.
async fn handle_our_view_change(
	state: &mut State,
	view: OurView,
) -> Result<()> {
	for removed in state.view.difference(&view) {
		tracing::debug!(target: LOG_TARGET, relay_parent = ?removed, "Removing relay parent because our view changed.");

		if let Some(collation) = state.collations.remove(removed) {
			state.collation_result_senders.remove(&collation.receipt.hash());

			match collation.status {
				CollationStatus::Created => tracing::warn!(
					target: LOG_TARGET,
					candidate_hash = ?collation.receipt.hash(),
					pov_hash = ?collation.pov.hash(),
					"Collation wasn't advertised to any validator.",
				),
				CollationStatus::Advertised => tracing::debug!(
					target: LOG_TARGET,
					candidate_hash = ?collation.receipt.hash(),
					pov_hash = ?collation.pov.hash(),
					"Collation was advertised but not requested by any validator.",
				),
				CollationStatus::Requested => tracing::debug!(
					target: LOG_TARGET,
					candidate_hash = ?collation.receipt.hash(),
					pov_hash = ?collation.pov.hash(),
					"Collation was requested.",
				)
			}
		}
		state.our_validators_groups.remove(removed);
		state.span_per_relay_parent.remove(removed);
		state.waiting_collation_fetches.remove(removed);
	}

	state.view = view;

	Ok(())
}

/// The collator protocol collator side main loop.
pub(crate) async fn run<Context>(
	mut ctx: Context,
	local_peer_id: PeerId,
	collator_pair: CollatorPair,
	metrics: Metrics,
) -> Result<()>
where
	Context: overseer::SubsystemContext<Message = CollatorProtocolMessage>
{
	use OverseerSignal::*;

	let mut state = State::new(local_peer_id, collator_pair, metrics);
	let mut runtime = RuntimeInfo::new(None);

	loop {
		select! {
			msg = ctx.recv().fuse() => match msg.map_err(Fatal::SubsystemReceive)? {
				FromOverseer::Communication { msg } => {
					log_error(
						process_msg(&mut ctx, &mut runtime, &mut state, msg).await,
						"Failed to process message"
					)?;
				},
				FromOverseer::Signal(ActiveLeaves(_update)) => {}
				FromOverseer::Signal(BlockFinalized(..)) => {}
				FromOverseer::Signal(Conclude) => return Ok(()),
			},
			(relay_parent, peer_id) = state.active_collation_fetches.select_next_some() => {
				let next = if let Some(waiting) = state.waiting_collation_fetches.get_mut(&relay_parent) {
					waiting.waiting_peers.remove(&peer_id);
					if let Some(next) = waiting.waiting.pop_front() {
						next
					} else {
						waiting.collation_fetch_active = false;
						continue
					}
				} else {
					// No waiting collation fetches means we already removed the relay parent from our view.
					continue
				};

				if let Some(collation) = state.collations.get(&relay_parent) {
					let receipt = collation.receipt.clone();
					let pov = collation.pov.clone();

					send_collation(&mut state, next, receipt, pov).await;
				}
			}
		}
	}
}
