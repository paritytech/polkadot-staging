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

//! The Network Bridge Subsystem - protocol multiplexer for Polkadot.

#![deny(unused_crate_dependencies)]
#![warn(missing_docs)]


use parity_scale_codec::{Encode, Decode};
use parking_lot::Mutex;
use futures::prelude::*;
use futures::stream::BoxStream;
use polkadot_subsystem::messages::DisputeDistributionMessage;
use sc_network::Event as NetworkEvent;
use sp_consensus::SyncOracle;

use polkadot_overseer::{
	Subsystem,
};
use polkadot_subsystem::{
	overseer,
	OverseerSignal,
	FromOverseer,
	SpawnedSubsystem,
	SubsystemContext,
	SubsystemSender,
	errors::{SubsystemError, SubsystemResult},
	ActivatedLeaf, ActiveLeavesUpdate,
	messages::{
		AllMessages, StatementDistributionMessage,
		NetworkBridgeMessage, CollatorProtocolMessage, NetworkBridgeEvent,
	},
};
use polkadot_primitives::v1::{Hash, BlockNumber};
use polkadot_node_network_protocol::{
	PeerId, peer_set::PeerSet, View, v1 as protocol_v1, OurView, UnifiedReputationChange as Rep,
	ObservedRole,
};
use polkadot_node_subsystem_util::metrics::{self, prometheus};

/// Peer set infos for network initialization.
///
/// To be added to [`NetworkConfiguration::extra_sets`].
pub use polkadot_node_network_protocol::peer_set::{peer_sets_info, IsAuthority};

use std::collections::HashSet;
use std::collections::{HashMap, hash_map};
use std::sync::Arc;

mod validator_discovery;

/// Actual interfacing to the network based on the `Network` trait.
///
/// Defines the `Network` trait with an implementation for an `Arc<NetworkService>`.
mod network;
use network::{Network, send_message};

/// Request multiplexer for combining the multiple request sources into a single `Stream` of `AllMessages`.
mod multiplexer;
pub use multiplexer::RequestMultiplexer;

use crate::network::get_peer_id_by_authority_id;

#[cfg(test)]
mod tests;

/// The maximum amount of heads a peer is allowed to have in their view at any time.
///
/// We use the same limit to compute the view sent to peers locally.
const MAX_VIEW_HEADS: usize = 5;


const MALFORMED_MESSAGE_COST: Rep = Rep::CostMajor("Malformed Network-bridge message");
const UNCONNECTED_PEERSET_COST: Rep = Rep::CostMinor("Message sent to un-connected peer-set");
const MALFORMED_VIEW_COST: Rep = Rep::CostMajor("Malformed view");
const EMPTY_VIEW_COST: Rep = Rep::CostMajor("Peer sent us an empty view");

// network bridge log target
const LOG_TARGET: &'static str = "parachain::network-bridge";

/// Metrics for the network bridge.
#[derive(Clone, Default)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_peer_connected(&self, peer_set: PeerSet) {
		self.0.as_ref().map(|metrics| metrics
			.connected_events
			.with_label_values(&[peer_set.get_protocol_name_static()])
			.inc()
		);
	}

	fn on_peer_disconnected(&self, peer_set: PeerSet) {
		self.0.as_ref().map(|metrics| metrics
			.disconnected_events
			.with_label_values(&[peer_set.get_protocol_name_static()])
			.inc()
		);
	}

	fn note_peer_count(&self, peer_set: PeerSet, count: usize) {
		self.0.as_ref().map(|metrics| metrics
			.peer_count
			.with_label_values(&[peer_set.get_protocol_name_static()])
			.set(count as u64)
		);
	}

	fn on_notification_received(&self, peer_set: PeerSet, size: usize) {
		if let Some(metrics) = self.0.as_ref() {
			metrics.notifications_received
				.with_label_values(&[peer_set.get_protocol_name_static()])
				.inc();

			metrics.bytes_received
				.with_label_values(&[peer_set.get_protocol_name_static()])
				.inc_by(size as u64);
		}
	}

	fn on_notification_sent(&self, peer_set: PeerSet, size: usize, to_peers: usize) {
		if let Some(metrics) = self.0.as_ref() {
			metrics.notifications_sent
				.with_label_values(&[peer_set.get_protocol_name_static()])
				.inc_by(to_peers as u64);

			metrics.bytes_sent
				.with_label_values(&[peer_set.get_protocol_name_static()])
				.inc_by((size * to_peers) as u64);
		}
	}

	fn note_desired_peer_count(&self, peer_set: PeerSet, size: usize) {
		self.0.as_ref().map(|metrics| metrics
			.desired_peer_count
			.with_label_values(&[peer_set.get_protocol_name_static()])
			.set(size as u64)
		);
	}
}

#[derive(Clone)]
struct MetricsInner {
	peer_count: prometheus::GaugeVec<prometheus::U64>,
	connected_events: prometheus::CounterVec<prometheus::U64>,
	disconnected_events: prometheus::CounterVec<prometheus::U64>,
	desired_peer_count: prometheus::GaugeVec<prometheus::U64>,

	notifications_received: prometheus::CounterVec<prometheus::U64>,
	notifications_sent: prometheus::CounterVec<prometheus::U64>,

	bytes_received: prometheus::CounterVec<prometheus::U64>,
	bytes_sent: prometheus::CounterVec<prometheus::U64>,
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry)
		-> std::result::Result<Self, prometheus::PrometheusError>
	{
		let metrics = MetricsInner {
			peer_count: prometheus::register(
				prometheus::GaugeVec::new(
					prometheus::Opts::new(
						"parachain_peer_count",
						"The number of peers on a parachain-related peer-set",
					),
					&["protocol"]
				)?,
				registry,
			)?,
			connected_events: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"parachain_peer_connect_events_total",
						"The number of peer connect events on a parachain notifications protocol",
					),
					&["protocol"]
				)?,
				registry,
			)?,
			disconnected_events: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"parachain_peer_disconnect_events_total",
						"The number of peer disconnect events on a parachain notifications protocol",
					),
					&["protocol"]
				)?,
				registry,
			)?,
			desired_peer_count: prometheus::register(
				prometheus::GaugeVec::new(
					prometheus::Opts::new(
						"parachain_desired_peer_count",
						"The number of peers that the local node is expected to connect to on a parachain-related peer-set",
					),
					&["protocol"]
				)?,
				registry,
			)?,
			notifications_received: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"parachain_notifications_received_total",
						"The number of notifications received on a parachain protocol",
					),
					&["protocol"]
				)?,
				registry,
			)?,
			notifications_sent: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"parachain_notifications_sent_total",
						"The number of notifications sent on a parachain protocol",
					),
					&["protocol"]
				)?,
				registry,
			)?,
			bytes_received: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"parachain_notification_bytes_received_total",
						"The number of bytes received on a parachain notification protocol",
					),
					&["protocol"]
				)?,
				registry,
			)?,
			bytes_sent: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"parachain_notification_bytes_sent_total",
						"The number of bytes sent on a parachain notification protocol",
					),
					&["protocol"]
				)?,
				registry,
			)?,
		};

		Ok(Metrics(Some(metrics)))
	}
}

/// Messages from and to the network.
///
/// As transmitted to and received from subsystems.
#[derive(Debug, Encode, Decode, Clone)]
pub enum WireMessage<M> {
	/// A message from a peer on a specific protocol.
	#[codec(index = 1)]
	ProtocolMessage(M),
	/// A view update from a peer.
	#[codec(index = 2)]
	ViewUpdate(View),
}

/// The network bridge subsystem.
pub struct NetworkBridge<N, AD> {
	/// `Network` trait implementing type.
	network_service: N,
	authority_discovery_service: AD,
	request_multiplexer: RequestMultiplexer,
	sync_oracle: Box<dyn SyncOracle + Send>,
	metrics: Metrics,
}

impl<N, AD> NetworkBridge<N, AD> {
	/// Create a new network bridge subsystem with underlying network service and authority discovery service.
	///
	/// This assumes that the network service has had the notifications protocol for the network
	/// bridge already registered. See [`peers_sets_info`](peers_sets_info).
	pub fn new(
		network_service: N,
		authority_discovery_service: AD,
		request_multiplexer: RequestMultiplexer,
		sync_oracle: Box<dyn SyncOracle + Send>,
		metrics: Metrics,
	) -> Self {
		NetworkBridge {
			network_service,
			authority_discovery_service,
			request_multiplexer,
			sync_oracle,
			metrics,
		}
	}
}

impl<Net, AD, Context> overseer::Subsystem<Context> for NetworkBridge<Net, AD>
	where
		Net: Network + Sync,
		AD: validator_discovery::AuthorityDiscovery + Clone,
		Context: overseer::SubsystemContext<Message = NetworkBridgeMessage>,
{
	fn start(mut self, ctx: Context) -> SpawnedSubsystem {
		// The stream of networking events has to be created at initialization, otherwise the
		// networking might open connections before the stream of events has been grabbed.
		let network_stream = self.network_service.event_stream();

		// Swallow error because failure is fatal to the node and we log with more precision
		// within `run_network`.
		let future = run_network(self, ctx, network_stream)
			.map_err(|e| {
				SubsystemError::with_origin("network-bridge", e)
			})
			.boxed();
		SpawnedSubsystem {
			name: "network-bridge-subsystem",
			future,
		}
	}
}

struct PeerData {
	/// The Latest view sent by the peer.
	view: View,
}

#[derive(Debug)]
enum UnexpectedAbort {
	/// Received error from overseer:
	SubsystemError(SubsystemError),
	/// The stream of incoming events concluded.
	EventStreamConcluded,
	/// The stream of incoming requests concluded.
	RequestStreamConcluded,
}

impl From<SubsystemError> for UnexpectedAbort {
	fn from(e: SubsystemError) -> Self {
		UnexpectedAbort::SubsystemError(e)
	}
}

#[derive(Default, Clone)]
struct Shared(Arc<Mutex<SharedInner>>);

#[derive(Default)]
struct SharedInner {
	local_view: Option<View>,
	validation_peers: HashMap<PeerId, PeerData>,
	collation_peers: HashMap<PeerId, PeerData>,
}

enum Mode {
	Syncing(Box<dyn SyncOracle + Send>),
	Active,
}

async fn handle_subsystem_messages<Context, N, AD>(
	mut ctx: Context,
	mut network_service: N,
	mut authority_discovery_service: AD,
	shared: Shared,
	sync_oracle: Box<dyn SyncOracle + Send>,
	metrics: Metrics,
) -> Result<(), UnexpectedAbort>
where
	Context: overseer::SubsystemContext<Message = NetworkBridgeMessage>,
	N: Network,
	AD: validator_discovery::AuthorityDiscovery + Clone,
{
	// This is kept sorted, descending, by block number.
	let mut live_heads: Vec<ActivatedLeaf> = Vec::with_capacity(MAX_VIEW_HEADS);
	let mut finalized_number = 0;
	let mut validator_discovery = validator_discovery::Service::<N, AD>::new();

	let mut mode = Mode::Syncing(sync_oracle);

	loop {
		futures::select! {
			msg = ctx.recv().fuse() => match msg {
				Ok(FromOverseer::Signal(OverseerSignal::ActiveLeaves(active_leaves))) => {
					let ActiveLeavesUpdate { activated, deactivated } = active_leaves;
					tracing::trace!(
						target: LOG_TARGET,
						action = "ActiveLeaves",
						num_activated = %activated.len(),
						num_deactivated = %deactivated.len(),
					);

					for activated in activated {
						let pos = live_heads
							.binary_search_by(|probe| probe.number.cmp(&activated.number).reverse())
							.unwrap_or_else(|i| i);

						live_heads.insert(pos, activated);
					}
					live_heads.retain(|h| !deactivated.contains(&h.hash));

					// if we're done syncing, set the mode to `Mode::Active`.
					// Otherwise, we don't need to send view updates.
					{
						let is_done_syncing = match mode {
							Mode::Active => true,
							Mode::Syncing(ref mut sync_oracle) => !sync_oracle.is_major_syncing(),
						};

						if is_done_syncing {
							mode = Mode::Active;

							update_our_view(
								&mut network_service,
								&mut ctx,
								&live_heads,
								&shared,
								finalized_number,
								&metrics,
							);
						}
					}
				}
				Ok(FromOverseer::Signal(OverseerSignal::BlockFinalized(_hash, number))) => {
					tracing::trace!(
						target: LOG_TARGET,
						action = "BlockFinalized"
					);

					debug_assert!(finalized_number < number);

					// we don't send the view updates here, but delay them until the next `ActiveLeaves`
					// otherwise it might break assumptions of some of the subsystems
					// that we never send the same `ActiveLeavesUpdate`
					finalized_number = number;
				}
				Ok(FromOverseer::Signal(OverseerSignal::Conclude)) => {
					return Ok(());
				}
				Ok(FromOverseer::Communication { msg }) => match msg {
					NetworkBridgeMessage::ReportPeer(peer, rep) => {
						if !rep.is_benefit() {
							tracing::debug!(
								target: LOG_TARGET,
								?peer,
								?rep,
								action = "ReportPeer"
							);
						}
						network_service.report_peer(peer, rep);
					}
					NetworkBridgeMessage::DisconnectPeer(peer, peer_set) => {
						tracing::trace!(
							target: LOG_TARGET,
							action = "DisconnectPeer",
							?peer,
							peer_set = ?peer_set,
						);
						network_service.disconnect_peer(peer, peer_set);
					}
					NetworkBridgeMessage::SendValidationMessage(peers, msg) => {
						tracing::trace!(
							target: LOG_TARGET,
							action = "SendValidationMessages",
							num_messages = 1,
						);

						send_message(
							&mut network_service,
							peers,
							PeerSet::Validation,
							WireMessage::ProtocolMessage(msg),
							&metrics,
						);
					}
					NetworkBridgeMessage::SendValidationMessages(msgs) => {
						tracing::trace!(
							target: LOG_TARGET,
							action = "SendValidationMessages",
							num_messages = %msgs.len(),
						);

						for (peers, msg) in msgs {
							send_message(
								&mut network_service,
								peers,
								PeerSet::Validation,
								WireMessage::ProtocolMessage(msg),
								&metrics,
							);
						}
					}
					NetworkBridgeMessage::SendCollationMessage(peers, msg) => {
						tracing::trace!(
							target: LOG_TARGET,
							action = "SendCollationMessages",
							num_messages = 1,
						);

						send_message(
							&mut network_service,
							peers,
							PeerSet::Collation,
							WireMessage::ProtocolMessage(msg),
							&metrics,
						);
					}
					NetworkBridgeMessage::SendCollationMessages(msgs) => {
						tracing::trace!(
							target: LOG_TARGET,
							action = "SendCollationMessages",
							num_messages = %msgs.len(),
						);

						for (peers, msg) in msgs {
							send_message(
								&mut network_service,
								peers,
								PeerSet::Collation,
								WireMessage::ProtocolMessage(msg),
								&metrics,
							);
						}
					}
					NetworkBridgeMessage::SendRequests(reqs, if_disconnected) => {
						tracing::trace!(
							target: LOG_TARGET,
							action = "SendRequests",
							num_requests = %reqs.len(),
						);

						for req in reqs {
							network_service
								.start_request(&mut authority_discovery_service, req, if_disconnected)
								.await;
						}
					}
					NetworkBridgeMessage::ConnectToValidators {
						validator_ids,
						peer_set,
						failed,
					} => {
						tracing::trace!(
							target: LOG_TARGET,
							action = "ConnectToValidators",
							peer_set = ?peer_set,
							ids = ?validator_ids,
							"Received a validator connection request",
						);

						metrics.note_desired_peer_count(peer_set, validator_ids.len());

						let (ns, ads) = validator_discovery.on_request(
							validator_ids,
							peer_set,
							failed,
							network_service,
							authority_discovery_service,
						).await;

						network_service = ns;
						authority_discovery_service = ads;
					}
					NetworkBridgeMessage::NewGossipTopology {
						our_neighbors,
					} => {
						tracing::debug!(
							target: LOG_TARGET,
							action = "NewGossipTopology",
							neighbors = our_neighbors.len(),
							"Gossip topology has changed",
						);

						let ads = &mut authority_discovery_service;
						let mut gossip_peers = HashSet::with_capacity(our_neighbors.len());
						for authority in our_neighbors {
							let addr = get_peer_id_by_authority_id(
								ads,
								authority.clone(),
							).await;

							if let Some(peer_id) = addr {
								gossip_peers.insert(peer_id);
							}
						}

						dispatch_validation_event_to_all_unbounded(
							NetworkBridgeEvent::NewGossipTopology(gossip_peers),
							ctx.sender(),
						);
					}
				}
				Err(e) => return Err(e.into()),
			},
		}
	}
}

async fn handle_network_messages<AD: validator_discovery::AuthorityDiscovery>(
	mut sender: impl SubsystemSender,
	mut network_service: impl Network,
	network_stream: BoxStream<'static, NetworkEvent>,
	mut authority_discovery_service: AD,
	mut request_multiplexer: RequestMultiplexer,
	metrics: Metrics,
	shared: Shared,
) -> Result<(), UnexpectedAbort> {
	let mut network_stream = network_stream.fuse();
	loop {
		futures::select! {
			network_event = network_stream.next() => match network_event {
				None => return Err(UnexpectedAbort::EventStreamConcluded),
				Some(NetworkEvent::Dht(_))
				| Some(NetworkEvent::SyncConnected { .. })
				| Some(NetworkEvent::SyncDisconnected { .. }) => {}
				Some(NetworkEvent::NotificationStreamOpened { remote: peer, protocol, role, .. }) => {
					let role = ObservedRole::from(role);
					let peer_set = match PeerSet::try_from_protocol_name(&protocol) {
						None => continue,
						Some(peer_set) => peer_set,
					};

					tracing::debug!(
						target: LOG_TARGET,
						action = "PeerConnected",
						peer_set = ?peer_set,
						peer = ?peer,
						role = ?role
					);

					let local_view = {
						let mut shared = shared.0.lock();
						let peer_map = match peer_set {
							PeerSet::Validation => &mut shared.validation_peers,
							PeerSet::Collation => &mut shared.collation_peers,
						};

						match peer_map.entry(peer.clone()) {
							hash_map::Entry::Occupied(_) => continue,
							hash_map::Entry::Vacant(vacant) => {
								vacant.insert(PeerData { view: View::default() });
							}
						}

						metrics.on_peer_connected(peer_set);
						metrics.note_peer_count(peer_set, peer_map.len());

						shared.local_view.clone().unwrap_or(View::default())
					};

					let maybe_authority =
						authority_discovery_service
							.get_authority_id_by_peer_id(peer).await;

					match peer_set {
						PeerSet::Validation => {
							dispatch_validation_events_to_all(
								vec![
									NetworkBridgeEvent::PeerConnected(peer.clone(), role, maybe_authority),
									NetworkBridgeEvent::PeerViewChange(
										peer.clone(),
										View::default(),
									),
								],
								&mut sender,
							).await;

							send_message(
								&mut network_service,
								vec![peer],
								PeerSet::Validation,
								WireMessage::<protocol_v1::ValidationProtocol>::ViewUpdate(
									local_view,
								),
								&metrics,
							);
						}
						PeerSet::Collation => {
							dispatch_collation_events_to_all(
								vec![
									NetworkBridgeEvent::PeerConnected(peer.clone(), role, maybe_authority),
									NetworkBridgeEvent::PeerViewChange(
										peer.clone(),
										View::default(),
									),
								],
								&mut sender,
							).await;

							send_message(
								&mut network_service,
								vec![peer],
								PeerSet::Collation,
								WireMessage::<protocol_v1::CollationProtocol>::ViewUpdate(
									local_view,
								),
								&metrics,
							);
						}
					}
				}
				Some(NetworkEvent::NotificationStreamClosed { remote: peer, protocol }) => {
					let peer_set = match PeerSet::try_from_protocol_name(&protocol) {
						None => continue,
						Some(peer_set) => peer_set,
					};

					tracing::debug!(
						target: LOG_TARGET,
						action = "PeerDisconnected",
						peer_set = ?peer_set,
						peer = ?peer
					);

					let was_connected = {
						let mut shared = shared.0.lock();
						let peer_map = match peer_set {
							PeerSet::Validation => &mut shared.validation_peers,
							PeerSet::Collation => &mut shared.collation_peers,
						};

						let w = peer_map.remove(&peer).is_some();

						metrics.on_peer_disconnected(peer_set);
						metrics.note_peer_count(peer_set, peer_map.len());

						w
					};

					if was_connected {
						match peer_set {
							PeerSet::Validation => dispatch_validation_event_to_all(
								NetworkBridgeEvent::PeerDisconnected(peer),
								&mut sender,
							).await,
							PeerSet::Collation => dispatch_collation_event_to_all(
								NetworkBridgeEvent::PeerDisconnected(peer),
								&mut sender,
							).await,
						}
					}
				}
				Some(NetworkEvent::NotificationsReceived { remote, messages }) => {
					let v_messages: Result<Vec<_>, _> = messages
						.iter()
						.filter(|(protocol, _)| {
							protocol == &PeerSet::Validation.into_protocol_name()
						})
						.map(|(_, msg_bytes)| {
							WireMessage::decode(&mut msg_bytes.as_ref())
								.map(|m| (m, msg_bytes.len()))
						})
						.collect();

					let v_messages = match v_messages {
						Err(_) => {
							tracing::debug!(
								target: LOG_TARGET,
								action = "ReportPeer"
							);

							network_service.report_peer(remote, MALFORMED_MESSAGE_COST);
							continue;
						}
						Ok(v) => v,
					};

					let c_messages: Result<Vec<_>, _> = messages
						.iter()
						.filter(|(protocol, _)| {
							protocol == &PeerSet::Collation.into_protocol_name()
						})
						.map(|(_, msg_bytes)| {
							WireMessage::decode(&mut msg_bytes.as_ref())
								.map(|m| (m, msg_bytes.len()))
						})
						.collect();

					match c_messages {
						Err(_) => {
							tracing::debug!(
								target: LOG_TARGET,
								action = "ReportPeer"
							);

							network_service.report_peer(remote, MALFORMED_MESSAGE_COST);
							continue;
						}
						Ok(c_messages) => {
							if v_messages.is_empty() && c_messages.is_empty() {
								continue;
							} else {
								tracing::trace!(
									target: LOG_TARGET,
									action = "PeerMessages",
									peer = ?remote,
									num_validation_messages = %v_messages.len(),
									num_collation_messages = %c_messages.len()
								);

								if !v_messages.is_empty() {
									let (events, reports) = handle_peer_messages(
										remote.clone(),
										PeerSet::Validation,
										&mut shared.0.lock().validation_peers,
										v_messages,
										&metrics,
									);

									for report in reports {
										network_service.report_peer(remote.clone(), report);
									}

									dispatch_validation_events_to_all(events, &mut sender).await;
								}

								if !c_messages.is_empty() {
									let (events, reports) = handle_peer_messages(
										remote.clone(),
										PeerSet::Collation,
										&mut shared.0.lock().collation_peers,
										c_messages,
										&metrics,
									);

									for report in reports {
										network_service.report_peer(remote.clone(), report);
									}


									dispatch_collation_events_to_all(events, &mut sender).await;
								}
							}
						}
					}
				}
			},
			req_res_event = request_multiplexer.next() => match req_res_event {
				None => return Err(UnexpectedAbort::RequestStreamConcluded),
				Some(Err(err)) => {
					network_service.report_peer(err.peer, MALFORMED_MESSAGE_COST);
				}
				Some(Ok(msg)) => {
					sender.send_message(msg).await;
				}
			},
		}
	}
}

/// Main driver, processing network events and messages from other subsystems.
///
/// THIS IS A HACK. We need to ensure we never hold the mutex across a `.await` boundary
/// and `parking_lot` currently does not provide `Send`, which helps us enforce that.
/// If this breaks, we need to find another way to protect ourselves.
///
/// ```compile_fail
/// #use parking_lot::MutexGuard;
/// #fn is_send<T: Send>();
/// #is_send::<parking_lot::MutexGuard<'static, ()>();
/// ```
async fn run_network<N, AD, Context>(
	bridge: NetworkBridge<N, AD>,
	mut ctx: Context,
	network_stream: BoxStream<'static, NetworkEvent>,
) -> SubsystemResult<()>
where
	N: Network,
	AD: validator_discovery::AuthorityDiscovery + Clone,
	Context: overseer::SubsystemContext<Message=NetworkBridgeMessage>,
{
	let shared = Shared::default();

	let NetworkBridge {
		network_service,
		mut request_multiplexer,
		authority_discovery_service,
		metrics,
		sync_oracle,
	} = bridge;

	let statement_receiver = request_multiplexer
		.get_statement_fetching()
		.expect("Gets initialized, must be `Some` on startup. qed.");

	let dispute_receiver = request_multiplexer
		.get_dispute_sending()
		.expect("Gets initialized, must be `Some` on startup. qed.");

	let (remote, network_event_handler) = handle_network_messages::<>(
		ctx.sender().clone(),
		network_service.clone(),
		network_stream,
		authority_discovery_service.clone(),
		request_multiplexer,
		metrics.clone(),
		shared.clone(),
	).remote_handle();

	ctx.spawn("network-bridge-network-worker", Box::pin(remote))?;

	ctx.send_message(
		DisputeDistributionMessage::DisputeSendingReceiver(dispute_receiver)
	).await;
	ctx.send_message(
		StatementDistributionMessage::StatementFetchingReceiver(statement_receiver)
	).await;

	let subsystem_event_handler = handle_subsystem_messages(
		ctx,
		network_service,
		authority_discovery_service,
		shared,
		sync_oracle,
		metrics,
	);

	futures::pin_mut!(subsystem_event_handler);

	match futures::future::select(subsystem_event_handler, network_event_handler)
		.await
		.factor_first()
		.0
	{
		Ok(()) => Ok(()),
		Err(UnexpectedAbort::SubsystemError(err)) => {
			tracing::warn!(
				target: LOG_TARGET,
				err = ?err,
				"Shutting down Network Bridge due to error"
			);

			Err(SubsystemError::Context(format!(
				"Received SubsystemError from overseer: {:?}",
				err
			)))
		}
		Err(UnexpectedAbort::EventStreamConcluded) => {
			tracing::info!(
				target: LOG_TARGET,
				"Shutting down Network Bridge: underlying request stream concluded"
			);
			Err(SubsystemError::Context(
				"Incoming network event stream concluded.".to_string(),
			))
		}
		Err(UnexpectedAbort::RequestStreamConcluded) => {
			tracing::info!(
				target: LOG_TARGET,
				"Shutting down Network Bridge: underlying request stream concluded"
			);
			Err(SubsystemError::Context(
				"Incoming network request stream concluded".to_string(),
			))
		}
	}
}

fn construct_view(live_heads: impl DoubleEndedIterator<Item = Hash>, finalized_number: BlockNumber) -> View {
	View::new(
		live_heads.take(MAX_VIEW_HEADS),
		finalized_number,
	)
}

fn update_our_view(
	net: &mut impl Network,
	ctx: &mut impl SubsystemContext<Message=NetworkBridgeMessage, AllMessages=AllMessages>,
	live_heads: &[ActivatedLeaf],
	shared: &Shared,
	finalized_number: BlockNumber,
	metrics: &Metrics,
) {
	let new_view = construct_view(live_heads.iter().map(|v| v.hash), finalized_number);

	let (validation_peers, collation_peers) = {
		let mut shared = shared.0.lock();

		// We only want to send a view update when the heads changed.
		// A change in finalized block number only is _not_ sufficient.
		//
		// If this is the first view update since becoming active, but our view is empty,
		// there is no need to send anything.
		match shared.local_view {
			Some(ref v) if v.check_heads_eq(&new_view) => {
				return;
			}
			None if live_heads.is_empty() => {
				shared.local_view = Some(new_view);
				return;
			}
			_ => {
				shared.local_view = Some(new_view.clone());
			}

		}

		(
			shared.validation_peers.keys().cloned().collect::<Vec<_>>(),
			shared.collation_peers.keys().cloned().collect::<Vec<_>>(),
		)
	};

	send_validation_message(
		net,
		validation_peers,
		WireMessage::ViewUpdate(new_view.clone()),
		metrics,
	);

	send_collation_message(
		net,
		collation_peers,
		WireMessage::ViewUpdate(new_view),
		metrics,
	);

	let our_view = OurView::new(
		live_heads.iter().take(MAX_VIEW_HEADS).cloned().map(|a| (a.hash, a.span)),
		finalized_number,
	);

	dispatch_validation_event_to_all_unbounded(
		NetworkBridgeEvent::OurViewChange(our_view.clone()),
		ctx.sender(),
	);

	dispatch_collation_event_to_all_unbounded(
		NetworkBridgeEvent::OurViewChange(our_view),
		ctx.sender(),
	);
}

// Handle messages on a specific peer-set. The peer is expected to be connected on that
// peer-set.
fn handle_peer_messages<M>(
	peer: PeerId,
	peer_set: PeerSet,
	peers: &mut HashMap<PeerId, PeerData>,
	messages: Vec<(WireMessage<M>, usize)>,
	metrics: &Metrics,
) -> (Vec<NetworkBridgeEvent<M>>, Vec<Rep>) {
	let peer_data = match peers.get_mut(&peer) {
		None => {
			return (Vec::new(), vec![UNCONNECTED_PEERSET_COST]);
		},
		Some(d) => d,
	};

	let mut outgoing_messages = Vec::with_capacity(messages.len());
	let mut reports = Vec::new();

	for (message, size_bytes) in messages {
		metrics.on_notification_received(peer_set, size_bytes);

		outgoing_messages.push(match message {
			WireMessage::ViewUpdate(new_view) => {
				if new_view.len() > MAX_VIEW_HEADS ||
					new_view.finalized_number < peer_data.view.finalized_number
				{
					reports.push(MALFORMED_VIEW_COST);
					continue
				} else if new_view.is_empty() {
					reports.push(EMPTY_VIEW_COST);
					continue
				} else if new_view == peer_data.view {
					continue
				} else {
					peer_data.view = new_view;

					NetworkBridgeEvent::PeerViewChange(
						peer.clone(),
						peer_data.view.clone(),
					)
				}
			}
			WireMessage::ProtocolMessage(message) => {
				NetworkBridgeEvent::PeerMessage(peer.clone(), message)
			}
		})
	}

	(outgoing_messages, reports)
}

fn send_validation_message(
	net: &mut impl Network,
	peers: Vec<PeerId>,
	message: WireMessage<protocol_v1::ValidationProtocol>,
	metrics: &Metrics,
) {
	send_message(net, peers, PeerSet::Validation, message, metrics);
}

fn send_collation_message(
	net: &mut impl Network,
	peers: Vec<PeerId>,
	message: WireMessage<protocol_v1::CollationProtocol>,
	metrics: &Metrics,
) {
	send_message(net, peers, PeerSet::Collation, message, metrics)
}


async fn dispatch_validation_event_to_all(
	event: NetworkBridgeEvent<protocol_v1::ValidationProtocol>,
	ctx: &mut impl SubsystemSender
) {
	dispatch_validation_events_to_all(std::iter::once(event), ctx).await
}

async fn dispatch_collation_event_to_all(
	event: NetworkBridgeEvent<protocol_v1::CollationProtocol>,
	ctx: &mut impl SubsystemSender
) {
	dispatch_collation_events_to_all(std::iter::once(event), ctx).await
}

fn dispatch_validation_event_to_all_unbounded(
	event: NetworkBridgeEvent<protocol_v1::ValidationProtocol>,
	ctx: &mut impl SubsystemSender
) {
	for msg in AllMessages::dispatch_iter(event) {
		ctx.send_unbounded_message(msg);
	}
}

fn dispatch_collation_event_to_all_unbounded(
	event: NetworkBridgeEvent<protocol_v1::CollationProtocol>,
	ctx: &mut impl SubsystemSender
) {
	if let Some(msg) = event.focus().ok().map(CollatorProtocolMessage::NetworkBridgeUpdateV1) {
		ctx.send_unbounded_message(msg.into());
	}
}

async fn dispatch_validation_events_to_all<I>(
	events: I,
	ctx: &mut impl SubsystemSender
)
	where
		I: IntoIterator<Item = NetworkBridgeEvent<protocol_v1::ValidationProtocol>>,
		I::IntoIter: Send,
{
	ctx.send_messages(events.into_iter().flat_map(AllMessages::dispatch_iter)).await
}

async fn dispatch_collation_events_to_all<I>(
	events: I,
	ctx: &mut impl SubsystemSender
)
	where
		I: IntoIterator<Item = NetworkBridgeEvent<protocol_v1::CollationProtocol>>,
		I::IntoIter: Send,
{
	let messages_for = |event: NetworkBridgeEvent<protocol_v1::CollationProtocol>| {
		event.focus().ok().map(|m| AllMessages::CollatorProtocol(
			CollatorProtocolMessage::NetworkBridgeUpdateV1(m)
		))
	};

	ctx.send_messages(events.into_iter().flat_map(messages_for)).await
}
