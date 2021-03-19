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

//! # Overseer
//!
//! `overseer` implements the Overseer architecture described in the
//! [implementers-guide](https://w3f.github.io/parachain-implementers-guide/node/index.html).
//! For the motivations behind implementing the overseer itself you should
//! check out that guide, documentation in this crate will be mostly discussing
//! technical stuff.
//!
//! An `Overseer` is something that allows spawning/stopping and overseing
//! asynchronous tasks as well as establishing a well-defined and easy to use
//! protocol that the tasks can use to communicate with each other. It is desired
//! that this protocol is the only way tasks communicate with each other, however
//! at this moment there are no foolproof guards against other ways of communication.
//!
//! The `Overseer` is instantiated with a pre-defined set of `Subsystems` that
//! share the same behavior from `Overseer`'s point of view.
//!
//! ```text
//!                              +-----------------------------+
//!                              |         Overseer            |
//!                              +-----------------------------+
//!
//!             ................|  Overseer "holds" these and uses |..............
//!             .                  them to (re)start things                      .
//!             .                                                                .
//!             .  +-------------------+                +---------------------+  .
//!             .  |   Subsystem1      |                |   Subsystem2        |  .
//!             .  +-------------------+                +---------------------+  .
//!             .           |                                       |            .
//!             ..................................................................
//!                         |                                       |
//!                       start()                                 start()
//!                         V                                       V
//!             ..................| Overseer "runs" these |.......................
//!             .  +--------------------+               +---------------------+  .
//!             .  | SubsystemInstance1 |               | SubsystemInstance2  |  .
//!             .  +--------------------+               +---------------------+  .
//!             ..................................................................
//! ```

// #![deny(unused_results)]
// unused dependencies can not work for test and examples at the same time
// yielding false positives
#![warn(missing_docs)]

use std::fmt::{self, Debug};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use std::collections::{hash_map, HashMap};

use futures::channel::{oneshot, mpsc};
use futures::{
	poll, select,
	future::BoxFuture,
	stream::{FuturesUnordered, Fuse},
	Future, FutureExt, StreamExt,
};
use futures_timer::Delay;
use oorandom::Rand32;

use polkadot_primitives::v1::{Block, BlockNumber, Hash};
use client::{BlockImportNotification, BlockchainEvents, FinalityNotification};

use polkadot_subsystem::messages::{
	CandidateValidationMessage, CandidateBackingMessage,
	CandidateSelectionMessage, ChainApiMessage, StatementDistributionMessage,
	AvailabilityDistributionMessage, BitfieldSigningMessage, BitfieldDistributionMessage,
	ProvisionerMessage, PoVDistributionMessage, RuntimeApiMessage,
	AvailabilityStoreMessage, NetworkBridgeMessage, AllMessages, CollationGenerationMessage,
	CollatorProtocolMessage, AvailabilityRecoveryMessage, ApprovalDistributionMessage,
	ApprovalVotingMessage, GossipSupportMessage,
};
pub use polkadot_subsystem::{
	Subsystem, SubsystemContext, OverseerSignal, FromOverseer, SubsystemError, SubsystemResult,
	SpawnedSubsystem, ActiveLeavesUpdate, DummySubsystem, jaeger,
};
use polkadot_node_subsystem_util::{TimeoutExt, metrics::{self, prometheus}, metered, Metronome};
use polkadot_node_primitives::SpawnNamed;

// A capacity of bounded channels inside the overseer.
const CHANNEL_CAPACITY: usize = 1024;
// A graceful `Overseer` teardown time delay.
const STOP_DELAY: u64 = 1;
// Target for logs.
const LOG_TARGET: &'static str = "parachain::overseer";
// Rate at which messages are timed.
const MESSAGE_TIMER_METRIC_CAPTURE_RATE: f64 = 0.005;



/// A type of messages that are sent from [`Subsystem`] to [`Overseer`].
///
/// It wraps a system-wide [`AllMessages`] type that represents all possible
/// messages in the system.
///
/// [`AllMessages`]: enum.AllMessages.html
/// [`Subsystem`]: trait.Subsystem.html
/// [`Overseer`]: struct.Overseer.html
enum ToOverseer {
	/// This is a message sent by a `Subsystem`.
	SubsystemMessage(AllMessages),

	/// A message that wraps something the `Subsystem` is desiring to
	/// spawn on the overseer and a `oneshot::Sender` to signal the result
	/// of the spawn.
	SpawnJob {
		name: &'static str,
		s: BoxFuture<'static, ()>,
	},

	/// Same as `SpawnJob` but for blocking tasks to be executed on a
	/// dedicated thread pool.
	SpawnBlockingJob {
		name: &'static str,
		s: BoxFuture<'static, ()>,
	},
}

/// An event telling the `Overseer` on the particular block
/// that has been imported or finalized.
///
/// This structure exists solely for the purposes of decoupling
/// `Overseer` code from the client code and the necessity to call
/// `HeaderBackend::block_number_from_id()`.
#[derive(Debug, Clone)]
pub struct BlockInfo {
	/// hash of the block.
	pub hash: Hash,
	/// hash of the parent block.
	pub parent_hash: Hash,
	/// block's number.
	pub number: BlockNumber,
}

impl From<BlockImportNotification<Block>> for BlockInfo {
	fn from(n: BlockImportNotification<Block>) -> Self {
		BlockInfo {
			hash: n.hash,
			parent_hash: n.header.parent_hash,
			number: n.header.number,
		}
	}
}

impl From<FinalityNotification<Block>> for BlockInfo {
	fn from(n: FinalityNotification<Block>) -> Self {
		BlockInfo {
			hash: n.hash,
			parent_hash: n.header.parent_hash,
			number: n.header.number,
		}
	}
}

/// Some event from the outer world.
enum Event {
	BlockImported(BlockInfo),
	BlockFinalized(BlockInfo),
	MsgToSubsystem(AllMessages),
	ExternalRequest(ExternalRequest),
	Stop,
}

/// Some request from outer world.
enum ExternalRequest {
	WaitForActivation {
		hash: Hash,
		response_channel: oneshot::Sender<SubsystemResult<()>>,
	},
}

/// A handler used to communicate with the [`Overseer`].
///
/// [`Overseer`]: struct.Overseer.html
#[derive(Clone)]
pub struct OverseerHandler {
	events_tx: metered::MeteredSender<Event>,
}

impl OverseerHandler {
	/// Inform the `Overseer` that that some block was imported.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	pub async fn block_imported(&mut self, block: BlockInfo) {
		self.send_and_log_error(Event::BlockImported(block)).await
	}

	/// Send some message to one of the `Subsystem`s.
	#[tracing::instrument(level = "trace", skip(self, msg), fields(subsystem = LOG_TARGET))]
	pub async fn send_msg(&mut self, msg: impl Into<AllMessages>) {
		self.send_and_log_error(Event::MsgToSubsystem(msg.into())).await
	}

	/// Inform the `Overseer` that some block was finalized.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	pub async fn block_finalized(&mut self, block: BlockInfo) {
		self.send_and_log_error(Event::BlockFinalized(block)).await
	}

	/// Wait for a block with the given hash to be in the active-leaves set.
	/// This method is used for external code like `Proposer` that doesn't subscribe to Overseer's signals.
	///
	/// The response channel responds if the hash was activated and is closed if the hash was deactivated.
	/// Note that due the fact the overseer doesn't store the whole active-leaves set, only deltas,
	/// the response channel may never return if the hash was deactivated before this call.
	/// In this case, it's the caller's responsibility to ensure a timeout is set.
	#[tracing::instrument(level = "trace", skip(self, response_channel), fields(subsystem = LOG_TARGET))]
	pub async fn wait_for_activation(&mut self, hash: Hash, response_channel: oneshot::Sender<SubsystemResult<()>>) {
		self.send_and_log_error(Event::ExternalRequest(ExternalRequest::WaitForActivation {
			hash,
			response_channel
		})).await
	}

	/// Tell `Overseer` to shutdown.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	pub async fn stop(&mut self) {
		self.send_and_log_error(Event::Stop).await
	}

	async fn send_and_log_error(&mut self, event: Event) {
		if self.events_tx.send(event).await.is_err() {
			tracing::info!(target: LOG_TARGET, "Failed to send an event to Overseer");
		}
	}
}

/// Glues together the [`Overseer`] and `BlockchainEvents` by forwarding
/// import and finality notifications into the [`OverseerHandler`].
///
/// [`Overseer`]: struct.Overseer.html
/// [`OverseerHandler`]: struct.OverseerHandler.html
pub async fn forward_events<P: BlockchainEvents<Block>>(
	client: Arc<P>,
	mut handler: OverseerHandler,
) {
	let mut finality = client.finality_notification_stream();
	let mut imports = client.import_notification_stream();

	loop {
		select! {
			f = finality.next() => {
				match f {
					Some(block) => {
						handler.block_finalized(block.into()).await;
					}
					None => break,
				}
			},
			i = imports.next() => {
				match i {
					Some(block) => {
						handler.block_imported(block.into()).await;
					}
					None => break,
				}
			},
			complete => break,
		}
	}
}

impl Debug for ToOverseer {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			ToOverseer::SubsystemMessage(msg) => {
				write!(f, "OverseerMessage::SubsystemMessage({:?})", msg)
			}
			ToOverseer::SpawnJob { .. } => write!(f, "OverseerMessage::Spawn(..)"),
			ToOverseer::SpawnBlockingJob { .. } => write!(f, "OverseerMessage::SpawnBlocking(..)")
		}
	}
}

/// A running instance of some [`Subsystem`].
///
/// [`Subsystem`]: trait.Subsystem.html
struct SubsystemInstance<M> {
	tx: metered::MeteredSender<FromOverseer<M>>,
	name: &'static str,
}

type MaybeTimer = Option<metrics::prometheus::prometheus::HistogramTimer>;

#[derive(Debug)]
struct MaybeTimed<T> {
	timer: MaybeTimer,
	t: T,
}

impl<T> MaybeTimed<T> {
	fn into_inner(self) -> T {
		self.t
	}
}

impl<T> From<T> for MaybeTimed<T> {
	fn from(t: T) -> Self {
		Self { timer: None, t }
	}
}

/// A context type that is given to the [`Subsystem`] upon spawning.
/// It can be used by [`Subsystem`] to communicate with other [`Subsystem`]s
/// or to spawn it's [`SubsystemJob`]s.
///
/// [`Overseer`]: struct.Overseer.html
/// [`Subsystem`]: trait.Subsystem.html
/// [`SubsystemJob`]: trait.SubsystemJob.html
#[derive(Debug)]
pub struct OverseerSubsystemContext<M>{
	rx: metered::MeteredReceiver<FromOverseer<M>>,
	tx: metered::UnboundedMeteredSender<MaybeTimed<ToOverseer>>,
	metrics: Metrics,
	rng: Rand32,
	threshold: u32,
}

impl<M> OverseerSubsystemContext<M> {
	/// Create a new `OverseerSubsystemContext`.
	///
	/// `increment` determines the initial increment of the internal RNG.
	/// The internal RNG is used to determine which messages are timed.
	///
	/// `capture_rate` determines what fraction of messages are timed. Its value is clamped
	/// to the range `0.0..=1.0`.
	fn new(
		rx: metered::MeteredReceiver<FromOverseer<M>>,
		tx: metered::UnboundedMeteredSender<MaybeTimed<ToOverseer>>,
		metrics: Metrics,
		increment: u64,
		mut capture_rate: f64,
	) -> Self {
		let rng = Rand32::new_inc(0, increment);

		if capture_rate < 0.0 {
			capture_rate = 0.0;
		} else if capture_rate > 1.0 {
			capture_rate = 1.0;
		}
		let threshold = (capture_rate * u32::MAX as f64) as u32;

		OverseerSubsystemContext { rx, tx, metrics, rng, threshold }
	}

	/// Create a new `OverseserSubsystemContext` with no metering.
	///
	/// Intended for tests.
	#[allow(unused)]
	fn new_unmetered(
		rx: metered::MeteredReceiver<FromOverseer<M>>,
		tx: metered::UnboundedMeteredSender<MaybeTimed<ToOverseer>>,
	) -> Self {
		let metrics = Metrics::default();
		OverseerSubsystemContext::new(rx, tx, metrics, 0, 0.0)
	}

	fn maybe_timed<T>(&mut self, t: T) -> MaybeTimed<T> {
		let timer = if self.rng.rand_u32() <= self.threshold {
			self.metrics.time_message_hold()
		} else {
			None
		};

		MaybeTimed { timer, t }
	}
}

#[async_trait::async_trait]
impl<M: Send + 'static> SubsystemContext for OverseerSubsystemContext<M> {
	type Message = M;

	async fn try_recv(&mut self) -> Result<Option<FromOverseer<M>>, ()> {
		match poll!(self.rx.next()) {
			Poll::Ready(Some(msg)) => Ok(Some(msg)),
			Poll::Ready(None) => Err(()),
			Poll::Pending => Ok(None),
		}
	}

	async fn recv(&mut self) -> SubsystemResult<FromOverseer<M>> {
		self.rx.next().await
			.ok_or(SubsystemError::Context(
				"No more messages in rx queue to process"
				.to_owned()
			))
	}

	async fn spawn(&mut self, name: &'static str, s: Pin<Box<dyn Future<Output = ()> + Send>>)
		-> SubsystemResult<()>
	{
		self.send_timed(ToOverseer::SpawnJob {
			name,
			s,
		}).map_err(|s| s.into_send_error().into())
	}

	async fn spawn_blocking(&mut self, name: &'static str, s: Pin<Box<dyn Future<Output = ()> + Send>>)
		-> SubsystemResult<()>
	{
		self.send_timed(ToOverseer::SpawnBlockingJob {
			name,
			s,
		}).map_err(|s| s.into_send_error().into())
	}

	async fn send_message(&mut self, msg: AllMessages) {
		self.send_and_log_error(ToOverseer::SubsystemMessage(msg))
	}

	async fn send_messages<T>(&mut self, msgs: T)
		where T: IntoIterator<Item = AllMessages> + Send, T::IntoIter: Send
	{
		for msg in msgs {
			self.send_and_log_error(ToOverseer::SubsystemMessage(msg));
		}
	}
}

impl<M> OverseerSubsystemContext<M> {
	fn send_and_log_error(&mut self, msg: ToOverseer) {
		if self.send_timed(msg).is_err() {
			tracing::debug!(
				target: LOG_TARGET,
				msg_type = std::any::type_name::<M>(),
				"Failed to send a message to Overseer",
			);
		}
	}

	fn send_timed(&mut self, msg: ToOverseer) -> Result<
		(),
		mpsc::TrySendError<MaybeTimed<ToOverseer>>,
	>
	{
		let msg = self.maybe_timed(msg);
		self.tx.unbounded_send(msg)
	}
}

/// A subsystem that we oversee.
///
/// Ties together the [`Subsystem`] itself and it's running instance
/// (which may be missing if the [`Subsystem`] is not running at the moment
/// for whatever reason).
///
/// [`Subsystem`]: trait.Subsystem.html
struct OverseenSubsystem<M> {
	instance: Option<SubsystemInstance<M>>,
}

impl<M> OverseenSubsystem<M> {
	/// Send a message to the wrapped subsystem.
	///
	/// If the inner `instance` is `None`, nothing is happening.
	async fn send_message(&mut self, msg: M) -> SubsystemResult<()> {
		const MESSAGE_TIMEOUT: Duration = Duration::from_secs(10);

		if let Some(ref mut instance) = self.instance {
			match instance.tx.send(
				FromOverseer::Communication { msg }
			).timeout(MESSAGE_TIMEOUT).await
			{
				None => {
					tracing::error!(target: LOG_TARGET, "Subsystem {} appears unresponsive.", instance.name);
					Err(SubsystemError::SubsystemStalled(instance.name))
				}
				Some(res) => res.map_err(Into::into),
			}
		} else {
			Ok(())
		}
	}

	/// Send a signal to the wrapped subsystem.
	///
	/// If the inner `instance` is `None`, nothing is happening.
	async fn send_signal(&mut self, signal: OverseerSignal) -> SubsystemResult<()> {
		const SIGNAL_TIMEOUT: Duration = Duration::from_secs(10);

		if let Some(ref mut instance) = self.instance {
			match instance.tx.send(FromOverseer::Signal(signal)).timeout(SIGNAL_TIMEOUT).await {
				None => {
					tracing::error!(target: LOG_TARGET, "Subsystem {} appears unresponsive.", instance.name);
					Err(SubsystemError::SubsystemStalled(instance.name))
				}
				Some(res) => res.map_err(Into::into),
			}
		} else {
			Ok(())
		}
	}
}

/// The `Overseer` itself.
pub struct Overseer<S> {
	/// A candidate validation subsystem.
	candidate_validation_subsystem: OverseenSubsystem<CandidateValidationMessage>,

	/// A candidate backing subsystem.
	candidate_backing_subsystem: OverseenSubsystem<CandidateBackingMessage>,

	/// A candidate selection subsystem.
	candidate_selection_subsystem: OverseenSubsystem<CandidateSelectionMessage>,

	/// A statement distribution subsystem.
	statement_distribution_subsystem: OverseenSubsystem<StatementDistributionMessage>,

	/// An availability distribution subsystem.
	availability_distribution_subsystem: OverseenSubsystem<AvailabilityDistributionMessage>,

	/// An availability recovery subsystem.
	availability_recovery_subsystem: OverseenSubsystem<AvailabilityRecoveryMessage>,

	/// A bitfield signing subsystem.
	bitfield_signing_subsystem: OverseenSubsystem<BitfieldSigningMessage>,

	/// A bitfield distribution subsystem.
	bitfield_distribution_subsystem: OverseenSubsystem<BitfieldDistributionMessage>,

	/// A provisioner subsystem.
	provisioner_subsystem: OverseenSubsystem<ProvisionerMessage>,

	/// A PoV distribution subsystem.
	pov_distribution_subsystem: OverseenSubsystem<PoVDistributionMessage>,

	/// A runtime API subsystem.
	runtime_api_subsystem: OverseenSubsystem<RuntimeApiMessage>,

	/// An availability store subsystem.
	availability_store_subsystem: OverseenSubsystem<AvailabilityStoreMessage>,

	/// A network bridge subsystem.
	network_bridge_subsystem: OverseenSubsystem<NetworkBridgeMessage>,

	/// A Chain API subsystem.
	chain_api_subsystem: OverseenSubsystem<ChainApiMessage>,

	/// A Collation Generation subsystem.
	collation_generation_subsystem: OverseenSubsystem<CollationGenerationMessage>,

	/// A Collator Protocol subsystem.
	collator_protocol_subsystem: OverseenSubsystem<CollatorProtocolMessage>,

	/// An Approval Distribution subsystem.
	approval_distribution_subsystem: OverseenSubsystem<ApprovalDistributionMessage>,

	/// An Approval Voting subsystem.
	approval_voting_subsystem: OverseenSubsystem<ApprovalVotingMessage>,

	/// A Gossip Support subsystem.
	gossip_support_subsystem: OverseenSubsystem<GossipSupportMessage>,

	/// Spawner to spawn tasks to.
	s: S,

	/// Here we keep handles to spawned subsystems to be notified when they terminate.
	running_subsystems: FuturesUnordered<BoxFuture<'static, SubsystemResult<()>>>,

	/// Gather running subsystems' outbound streams into one.
	to_overseer_rx: Fuse<metered::UnboundedMeteredReceiver<MaybeTimed<ToOverseer>>>,

	/// Events that are sent to the overseer from the outside world
	events_rx: metered::MeteredReceiver<Event>,

	/// External listeners waiting for a hash to be in the active-leave set.
	activation_external_listeners: HashMap<Hash, Vec<oneshot::Sender<SubsystemResult<()>>>>,

	/// Stores the [`jaeger::Span`] per active leaf.
	span_per_active_leaf: HashMap<Hash, Arc<jaeger::Span>>,

	/// A set of leaves that `Overseer` starts working with.
	///
	/// Drained at the beginning of `run` and never used again.
	leaves: Vec<(Hash, BlockNumber)>,

	/// The set of the "active leaves".
	active_leaves: HashMap<Hash, BlockNumber>,

	/// Various Prometheus metrics.
	metrics: Metrics,
}

/// This struct is passed as an argument to create a new instance of an [`Overseer`].
///
/// As any entity that satisfies the interface may act as a [`Subsystem`] this allows
/// mocking in the test code:
///
/// Each [`Subsystem`] is supposed to implement some interface that is generic over
/// message type that is specific to this [`Subsystem`]. At the moment not all
/// subsystems are implemented and the rest can be mocked with the [`DummySubsystem`].
pub struct AllSubsystems<
	CV = (), CB = (), CS = (), SD = (), AD = (), AR = (), BS = (), BD = (), P = (),
	PoVD = (), RA = (), AS = (), NB = (), CA = (), CG = (), CP = (), ApD = (), ApV = (),
	GS = (),
> {
	/// A candidate validation subsystem.
	pub candidate_validation: CV,
	/// A candidate backing subsystem.
	pub candidate_backing: CB,
	/// A candidate selection subsystem.
	pub candidate_selection: CS,
	/// A statement distribution subsystem.
	pub statement_distribution: SD,
	/// An availability distribution subsystem.
	pub availability_distribution: AD,
	/// An availability recovery subsystem.
	pub availability_recovery: AR,
	/// A bitfield signing subsystem.
	pub bitfield_signing: BS,
	/// A bitfield distribution subsystem.
	pub bitfield_distribution: BD,
	/// A provisioner subsystem.
	pub provisioner: P,
	/// A PoV distribution subsystem.
	pub pov_distribution: PoVD,
	/// A runtime API subsystem.
	pub runtime_api: RA,
	/// An availability store subsystem.
	pub availability_store: AS,
	/// A network bridge subsystem.
	pub network_bridge: NB,
	/// A Chain API subsystem.
	pub chain_api: CA,
	/// A Collation Generation subsystem.
	pub collation_generation: CG,
	/// A Collator Protocol subsystem.
	pub collator_protocol: CP,
	/// An Approval Distribution subsystem.
	pub approval_distribution: ApD,
	/// An Approval Voting subsystem.
	pub approval_voting: ApV,
	/// A Connection Request Issuer subsystem.
	pub gossip_support: GS,
}

impl<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS>
	AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS>
{
	/// Create a new instance of [`AllSubsystems`].
	///
	/// Each subsystem is set to [`DummySystem`].
	///
	///# Note
	///
	/// Because of a bug in rustc it is required that when calling this function,
	/// you provide a "random" type for the first generic parameter:
	///
	/// ```
	/// polkadot_overseer::AllSubsystems::<()>::dummy();
	/// ```
	pub fn dummy() -> AllSubsystems<
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
	> {
		AllSubsystems {
			candidate_validation: DummySubsystem,
			candidate_backing: DummySubsystem,
			candidate_selection: DummySubsystem,
			statement_distribution: DummySubsystem,
			availability_distribution: DummySubsystem,
			availability_recovery: DummySubsystem,
			bitfield_signing: DummySubsystem,
			bitfield_distribution: DummySubsystem,
			provisioner: DummySubsystem,
			pov_distribution: DummySubsystem,
			runtime_api: DummySubsystem,
			availability_store: DummySubsystem,
			network_bridge: DummySubsystem,
			chain_api: DummySubsystem,
			collation_generation: DummySubsystem,
			collator_protocol: DummySubsystem,
			approval_distribution: DummySubsystem,
			approval_voting: DummySubsystem,
			gossip_support: DummySubsystem,
		}
	}

	/// Replace the `candidate_validation` instance in `self`.
	pub fn replace_candidate_validation<NEW>(
		self,
		candidate_validation: NEW,
	) -> AllSubsystems<NEW, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `candidate_backing` instance in `self`.
	pub fn replace_candidate_backing<NEW>(
		self,
		candidate_backing: NEW,
	) -> AllSubsystems<CV, NEW, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `candidate_selection` instance in `self`.
	pub fn replace_candidate_selection<NEW>(
		self,
		candidate_selection: NEW,
	) -> AllSubsystems<CV, CB, NEW, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `statement_distribution` instance in `self`.
	pub fn replace_statement_distribution<NEW>(
		self,
		statement_distribution: NEW,
	) -> AllSubsystems<CV, CB, CS, NEW, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `availability_distribution` instance in `self`.
	pub fn replace_availability_distribution<NEW>(
		self,
		availability_distribution: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, NEW, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `availability_recovery` instance in `self`.
	pub fn replace_availability_recovery<NEW>(
		self,
		availability_recovery: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, NEW, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `bitfield_signing` instance in `self`.
	pub fn replace_bitfield_signing<NEW>(
		self,
		bitfield_signing: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, NEW, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `bitfield_distribution` instance in `self`.
	pub fn replace_bitfield_distribution<NEW>(
		self,
		bitfield_distribution: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, NEW, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `provisioner` instance in `self`.
	pub fn replace_provisioner<NEW>(
		self,
		provisioner: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, NEW, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `pov_distribution` instance in `self`.
	pub fn replace_pov_distribution<NEW>(
		self,
		pov_distribution: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, NEW, RA, AS, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `runtime_api` instance in `self`.
	pub fn replace_runtime_api<NEW>(
		self,
		runtime_api: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, NEW, AS, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `availability_store` instance in `self`.
	pub fn replace_availability_store<NEW>(
		self,
		availability_store: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, NEW, NB, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `network_bridge` instance in `self`.
	pub fn replace_network_bridge<NEW>(
		self,
		network_bridge: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NEW, CA, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `chain_api` instance in `self`.
	pub fn replace_chain_api<NEW>(
		self,
		chain_api: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, NEW, CG, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `collation_generation` instance in `self`.
	pub fn replace_collation_generation<NEW>(
		self,
		collation_generation: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, NEW, CP, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `collator_protocol` instance in `self`.
	pub fn replace_collator_protocol<NEW>(
		self,
		collator_protocol: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, NEW, ApD, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `approval_distribution` instance in `self`.
	pub fn replace_approval_distribution<NEW>(
		self,
		approval_distribution: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, NEW, ApV, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `approval_voting` instance in `self`.
	pub fn replace_approval_voting<NEW>(
		self,
		approval_voting: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, NEW, GS> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting,
			gossip_support: self.gossip_support,
		}
	}

	/// Replace the `gossip_support` instance in `self`.
	pub fn replace_gossip_support<NEW>(
		self,
		gossip_support: NEW,
	) -> AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, NEW> {
		AllSubsystems {
			candidate_validation: self.candidate_validation,
			candidate_backing: self.candidate_backing,
			candidate_selection: self.candidate_selection,
			statement_distribution: self.statement_distribution,
			availability_distribution: self.availability_distribution,
			availability_recovery: self.availability_recovery,
			bitfield_signing: self.bitfield_signing,
			bitfield_distribution: self.bitfield_distribution,
			provisioner: self.provisioner,
			pov_distribution: self.pov_distribution,
			runtime_api: self.runtime_api,
			availability_store: self.availability_store,
			network_bridge: self.network_bridge,
			chain_api: self.chain_api,
			collation_generation: self.collation_generation,
			collator_protocol: self.collator_protocol,
			approval_distribution: self.approval_distribution,
			approval_voting: self.approval_voting,
			gossip_support,
		}
	}
}

/// Overseer Prometheus metrics.
#[derive(Clone)]
struct MetricsInner {
	activated_heads_total: prometheus::Counter<prometheus::U64>,
	deactivated_heads_total: prometheus::Counter<prometheus::U64>,
	messages_relayed_total: prometheus::Counter<prometheus::U64>,
	message_relay_timings: prometheus::Histogram,
	to_overseer_channel_queue_size: prometheus::Gauge<prometheus::U64>,
	from_overseer_channel_queue_size: prometheus::Gauge<prometheus::U64>,
}

#[derive(Default, Clone)]
struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_head_activated(&self) {
		if let Some(metrics) = &self.0 {
			metrics.activated_heads_total.inc();
		}
	}

	fn on_head_deactivated(&self) {
		if let Some(metrics) = &self.0 {
			metrics.deactivated_heads_total.inc();
		}
	}

	fn on_message_relayed(&self) {
		if let Some(metrics) = &self.0 {
			metrics.messages_relayed_total.inc();
		}
	}

	/// Provide a timer for the duration between receiving a message and passing it to `route_message`
	fn time_message_hold(&self) -> MaybeTimer {
		self.0.as_ref().map(|metrics| metrics.message_relay_timings.start_timer())
	}

	fn channel_fill_level_snapshot(&self, from_overseer: usize, to_overseer: usize) {
		self.0.as_ref().map(|metrics| metrics.to_overseer_channel_queue_size.set(to_overseer as u64));
		self.0.as_ref().map(|metrics| metrics.from_overseer_channel_queue_size.set(from_overseer as u64));
	}
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry) -> Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			activated_heads_total: prometheus::register(
				prometheus::Counter::new(
					"parachain_activated_heads_total",
					"Number of activated heads."
				)?,
				registry,
			)?,
			deactivated_heads_total: prometheus::register(
				prometheus::Counter::new(
					"parachain_deactivated_heads_total",
					"Number of deactivated heads."
				)?,
				registry,
			)?,
			messages_relayed_total: prometheus::register(
				prometheus::Counter::new(
					"parachain_messages_relayed_total",
					"Number of messages relayed by Overseer."
				)?,
				registry,
			)?,
			message_relay_timings: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts {
						common_opts: prometheus::Opts::new(
							"parachain_overseer_messages_relay_timings",
							"Time spent holding a message in the overseer before passing it to `route_message`",
						),
						// guessing at the desired resolution, but we know that messages will time
						// out after 0.5 seconds, so the bucket set below seems plausible:
						// `0.0001 * (1.6 ^ 18) ~= 0.472`. Prometheus auto-generates a final bucket
						// for all values between the final value and `+Inf`, so this should work.
						//
						// The documented legal range for the inputs are:
						//
						// - `> 0.0`
						// - `> 1.0`
						// - `! 0`
						buckets: prometheus::exponential_buckets(0.0001, 1.6, 18).expect("inputs are within documented range; qed"),
					}
				)?,
				registry,
			)?,
			from_overseer_channel_queue_size: prometheus::register(
				prometheus::Gauge::<prometheus::U64>::with_opts(
					prometheus::Opts::new(
						"parachain_from_overseer_channel_queue_size",
						"Number of elements sitting in the channel waiting to be processed.",
					),
				)?,
				registry,
			)?,
			to_overseer_channel_queue_size: prometheus::register(
				prometheus::Gauge::<prometheus::U64>::with_opts(
					prometheus::Opts::new(
						"parachain_to_overseer_channel_queue_size",
						"Number of elements sitting in the channel waiting to be processed.",
					),
				)?,
				registry,
			)?,
		};
		Ok(Metrics(Some(metrics)))
	}
}

impl fmt::Debug for Metrics {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Metrics {{...}}")
	}
}

impl<S> Overseer<S>
where
	S: SpawnNamed,
{
	/// Create a new instance of the `Overseer` with a fixed set of [`Subsystem`]s.
	///
	/// ```text
	///                  +------------------------------------+
	///                  |            Overseer                |
	///                  +------------------------------------+
	///                    /            |             |      \
	///      ................. subsystems...................................
	///      . +-----------+    +-----------+   +----------+   +---------+ .
	///      . |           |    |           |   |          |   |         | .
	///      . +-----------+    +-----------+   +----------+   +---------+ .
	///      ...............................................................
	///                              |
	///                        probably `spawn`
	///                            a `job`
	///                              |
	///                              V
	///                         +-----------+
	///                         |           |
	///                         +-----------+
	///
	/// ```
	///
	/// [`Subsystem`]: trait.Subsystem.html
	///
	/// # Example
	///
	/// The [`Subsystems`] may be any type as long as they implement an expected interface.
	/// Here, we create a mock validation subsystem and a few dummy ones and start the `Overseer` with them.
	/// For the sake of simplicity the termination of the example is done with a timeout.
	/// ```
	/// # use std::time::Duration;
	/// # use futures::{executor, pin_mut, select, FutureExt};
	/// # use futures_timer::Delay;
	/// # use polkadot_overseer::{Overseer, AllSubsystems};
	/// # use polkadot_subsystem::{
	/// #     Subsystem, DummySubsystem, SpawnedSubsystem, SubsystemContext,
	/// #     messages::CandidateValidationMessage,
	/// # };
	///
	/// struct ValidationSubsystem;
	///
	/// impl<C> Subsystem<C> for ValidationSubsystem
	///     where C: SubsystemContext<Message=CandidateValidationMessage>
	/// {
	///     fn start(
	///         self,
	///         mut ctx: C,
	///     ) -> SpawnedSubsystem {
	///         SpawnedSubsystem {
	///             name: "validation-subsystem",
	///             future: Box::pin(async move {
	///                 loop {
	///                     Delay::new(Duration::from_secs(1)).await;
	///                 }
	///             }),
	///         }
	///     }
	/// }
	///
	/// # fn main() { executor::block_on(async move {
	/// let spawner = sp_core::testing::TaskExecutor::new();
	/// let all_subsystems = AllSubsystems::<()>::dummy().replace_candidate_validation(ValidationSubsystem);
	/// let (overseer, _handler) = Overseer::new(
	///     vec![],
	///     all_subsystems,
	///     None,
	///     spawner,
	/// ).unwrap();
	///
	/// let timer = Delay::new(Duration::from_millis(50)).fuse();
	///
	/// let overseer_fut = overseer.run().fuse();
	/// pin_mut!(timer);
	/// pin_mut!(overseer_fut);
	///
	/// select! {
	///     _ = overseer_fut => (),
	///     _ = timer => (),
	/// }
	/// #
	/// # }); }
	/// ```
	pub fn new<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS>(
		leaves: impl IntoIterator<Item = BlockInfo>,
		all_subsystems: AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, PoVD, RA, AS, NB, CA, CG, CP, ApD, ApV, GS>,
		prometheus_registry: Option<&prometheus::Registry>,
		mut s: S,
	) -> SubsystemResult<(Self, OverseerHandler)>
	where
		CV: Subsystem<OverseerSubsystemContext<CandidateValidationMessage>> + Send,
		CB: Subsystem<OverseerSubsystemContext<CandidateBackingMessage>> + Send,
		CS: Subsystem<OverseerSubsystemContext<CandidateSelectionMessage>> + Send,
		SD: Subsystem<OverseerSubsystemContext<StatementDistributionMessage>> + Send,
		AD: Subsystem<OverseerSubsystemContext<AvailabilityDistributionMessage>> + Send,
		AR: Subsystem<OverseerSubsystemContext<AvailabilityRecoveryMessage>> + Send,
		BS: Subsystem<OverseerSubsystemContext<BitfieldSigningMessage>> + Send,
		BD: Subsystem<OverseerSubsystemContext<BitfieldDistributionMessage>> + Send,
		P: Subsystem<OverseerSubsystemContext<ProvisionerMessage>> + Send,
		PoVD: Subsystem<OverseerSubsystemContext<PoVDistributionMessage>> + Send,
		RA: Subsystem<OverseerSubsystemContext<RuntimeApiMessage>> + Send,
		AS: Subsystem<OverseerSubsystemContext<AvailabilityStoreMessage>> + Send,
		NB: Subsystem<OverseerSubsystemContext<NetworkBridgeMessage>> + Send,
		CA: Subsystem<OverseerSubsystemContext<ChainApiMessage>> + Send,
		CG: Subsystem<OverseerSubsystemContext<CollationGenerationMessage>> + Send,
		CP: Subsystem<OverseerSubsystemContext<CollatorProtocolMessage>> + Send,
		ApD: Subsystem<OverseerSubsystemContext<ApprovalDistributionMessage>> + Send,
		ApV: Subsystem<OverseerSubsystemContext<ApprovalVotingMessage>> + Send,
		GS: Subsystem<OverseerSubsystemContext<GossipSupportMessage>> + Send,
	{
		let (events_tx, events_rx) = metered::channel(CHANNEL_CAPACITY, "overseer_events");

		let handler = OverseerHandler {
			events_tx: events_tx.clone(),
		};

		let metrics = <Metrics as metrics::Metrics>::register(prometheus_registry)?;

		let (to_overseer_tx, to_overseer_rx) = metered::unbounded("to_overseer");

		{
			let meter_from_overseer = events_rx.meter().clone();
			let meter_to_overseer = to_overseer_rx.meter().clone();
			let metronome_metrics = metrics.clone();
			let metronome = Metronome::new(std::time::Duration::from_millis(950))
			.for_each(move |_| {
				metronome_metrics.channel_fill_level_snapshot(meter_from_overseer.queue_count(), meter_to_overseer.queue_count());

				async move {
					()
				}
			});
			s.spawn("metrics_metronome", Box::pin(metronome));
		}

		let mut running_subsystems = FuturesUnordered::new();

		let mut seed = 0x533d; // arbitrary

		let candidate_validation_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.candidate_validation,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let candidate_backing_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.candidate_backing,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let candidate_selection_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.candidate_selection,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let statement_distribution_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.statement_distribution,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let availability_distribution_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.availability_distribution,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let availability_recovery_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.availability_recovery,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let bitfield_signing_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.bitfield_signing,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let bitfield_distribution_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.bitfield_distribution,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let provisioner_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.provisioner,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let pov_distribution_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.pov_distribution,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let runtime_api_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.runtime_api,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let availability_store_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.availability_store,
			&metrics,
			&mut seed,
			TaskKind::Blocking,
		)?;

		let network_bridge_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.network_bridge,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let chain_api_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.chain_api,
			&metrics,
			&mut seed,
			TaskKind::Blocking,
		)?;

		let collation_generation_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.collation_generation,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;


		let collator_protocol_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.collator_protocol,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let approval_distribution_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.approval_distribution,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let approval_voting_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.approval_voting,
			&metrics,
			&mut seed,
			TaskKind::Blocking,
		)?;

		let gossip_support_subsystem = spawn(
			&mut s,
			&mut running_subsystems,
			metered::UnboundedMeteredSender::<_>::clone(&to_overseer_tx),
			all_subsystems.gossip_support,
			&metrics,
			&mut seed,
			TaskKind::Regular,
		)?;

		let leaves = leaves
			.into_iter()
			.map(|BlockInfo { hash, parent_hash: _, number }| (hash, number))
			.collect();

		let active_leaves = HashMap::new();
		let activation_external_listeners = HashMap::new();

		let this = Self {
			candidate_validation_subsystem,
			candidate_backing_subsystem,
			candidate_selection_subsystem,
			statement_distribution_subsystem,
			availability_distribution_subsystem,
			availability_recovery_subsystem,
			bitfield_signing_subsystem,
			bitfield_distribution_subsystem,
			provisioner_subsystem,
			pov_distribution_subsystem,
			runtime_api_subsystem,
			availability_store_subsystem,
			network_bridge_subsystem,
			chain_api_subsystem,
			collation_generation_subsystem,
			collator_protocol_subsystem,
			approval_distribution_subsystem,
			approval_voting_subsystem,
			gossip_support_subsystem,
			s,
			running_subsystems,
			to_overseer_rx: to_overseer_rx.fuse(),
			events_rx,
			activation_external_listeners,
			leaves,
			active_leaves,
			metrics,
			span_per_active_leaf: Default::default(),
		};

		Ok((this, handler))
	}

	// Stop the overseer.
	async fn stop(mut self) {
		let _ = self.candidate_validation_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.candidate_backing_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.candidate_selection_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.statement_distribution_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.availability_distribution_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.availability_recovery_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.bitfield_signing_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.bitfield_distribution_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.provisioner_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.pov_distribution_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.runtime_api_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.availability_store_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.network_bridge_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.chain_api_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.collator_protocol_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.collation_generation_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.approval_distribution_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.approval_voting_subsystem.send_signal(OverseerSignal::Conclude).await;
		let _ = self.gossip_support_subsystem.send_signal(OverseerSignal::Conclude).await;

		let mut stop_delay = Delay::new(Duration::from_secs(STOP_DELAY)).fuse();

		loop {
			select! {
				_ = self.running_subsystems.next() => {
					if self.running_subsystems.is_empty() {
						break;
					}
				},
				_ = stop_delay => break,
				complete => break,
			}
		}
	}

	/// Run the `Overseer`.
	#[tracing::instrument(skip(self), fields(subsystem = LOG_TARGET))]
	pub async fn run(mut self) -> SubsystemResult<()> {
		let mut update = ActiveLeavesUpdate::default();

		for (hash, number) in std::mem::take(&mut self.leaves) {
			let _ = self.active_leaves.insert(hash, number);
			let span = self.on_head_activated(&hash, None);
			update.activated.push((hash, span));
		}

		if !update.is_empty() {
			self.broadcast_signal(OverseerSignal::ActiveLeaves(update)).await?;
		}

		loop {
			select! {
				msg = self.events_rx.next().fuse() => {
					let msg = if let Some(msg) = msg {
						msg
					} else {
						continue
					};

					match msg {
						Event::MsgToSubsystem(msg) => {
							self.route_message(msg.into()).await?;
						}
						Event::Stop => {
							self.stop().await;
							return Ok(());
						}
						Event::BlockImported(block) => {
							self.block_imported(block).await?;
						}
						Event::BlockFinalized(block) => {
							self.block_finalized(block).await?;
						}
						Event::ExternalRequest(request) => {
							self.handle_external_request(request);
						}
					}
				},
				msg = self.to_overseer_rx.next() => {
					let MaybeTimed { timer, t: msg } = match msg {
						Some(m) => m,
						None => {
							// This is a fused stream so we will shut down after receiving all
							// shutdown notifications.
							continue
						}
					};

					match msg {
						ToOverseer::SubsystemMessage(msg) => {
							let msg = MaybeTimed { timer, t: msg };
							self.route_message(msg).await?
						},
						ToOverseer::SpawnJob { name, s } => {
							self.spawn_job(name, s);
						}
						ToOverseer::SpawnBlockingJob { name, s } => {
							self.spawn_blocking_job(name, s);
						}
					}
				},
				res = self.running_subsystems.next().fuse() => {
					let finished = if let Some(finished) = res {
						finished
					} else {
						continue
					};

					tracing::error!(target: LOG_TARGET, subsystem = ?finished, "subsystem finished unexpectedly");
					self.stop().await;
					return finished;
				},
			}
		}
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn block_imported(&mut self, block: BlockInfo) -> SubsystemResult<()> {
		match self.active_leaves.entry(block.hash) {
			hash_map::Entry::Vacant(entry) => entry.insert(block.number),
			hash_map::Entry::Occupied(entry) => {
				debug_assert_eq!(*entry.get(), block.number);
				return Ok(());
			}
		};

		let span = self.on_head_activated(&block.hash, Some(block.parent_hash));
		let mut update = ActiveLeavesUpdate::start_work(block.hash, span);

		if let Some(number) = self.active_leaves.remove(&block.parent_hash) {
			debug_assert_eq!(block.number.saturating_sub(1), number);
			update.deactivated.push(block.parent_hash);
			self.on_head_deactivated(&block.parent_hash);
		}

		self.clean_up_external_listeners();

		self.broadcast_signal(OverseerSignal::ActiveLeaves(update)).await
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn block_finalized(&mut self, block: BlockInfo) -> SubsystemResult<()> {
		let mut update = ActiveLeavesUpdate::default();

		self.active_leaves.retain(|h, n| {
			if *n <= block.number {
				update.deactivated.push(*h);
				false
			} else {
				true
			}
		});

		for deactivated in &update.deactivated {
			self.on_head_deactivated(deactivated)
		}

		self.broadcast_signal(OverseerSignal::BlockFinalized(block.hash, block.number)).await?;

		// If there are no leaves being deactivated, we don't need to send an update.
		//
		// Our peers will be informed about our finalized block the next time we activating/deactivating some leaf.
		if !update.is_empty() {
			self.broadcast_signal(OverseerSignal::ActiveLeaves(update)).await?;
		}

		Ok(())
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn broadcast_signal(&mut self, signal: OverseerSignal) -> SubsystemResult<()> {
		self.candidate_validation_subsystem.send_signal(signal.clone()).await?;
		self.candidate_backing_subsystem.send_signal(signal.clone()).await?;
		self.candidate_selection_subsystem.send_signal(signal.clone()).await?;
		self.statement_distribution_subsystem.send_signal(signal.clone()).await?;
		self.availability_distribution_subsystem.send_signal(signal.clone()).await?;
		self.availability_recovery_subsystem.send_signal(signal.clone()).await?;
		self.bitfield_signing_subsystem.send_signal(signal.clone()).await?;
		self.bitfield_distribution_subsystem.send_signal(signal.clone()).await?;
		self.provisioner_subsystem.send_signal(signal.clone()).await?;
		self.pov_distribution_subsystem.send_signal(signal.clone()).await?;
		self.runtime_api_subsystem.send_signal(signal.clone()).await?;
		self.availability_store_subsystem.send_signal(signal.clone()).await?;
		self.network_bridge_subsystem.send_signal(signal.clone()).await?;
		self.chain_api_subsystem.send_signal(signal.clone()).await?;
		self.collator_protocol_subsystem.send_signal(signal.clone()).await?;
		self.collation_generation_subsystem.send_signal(signal.clone()).await?;
		self.approval_distribution_subsystem.send_signal(signal.clone()).await?;
		self.approval_voting_subsystem.send_signal(signal.clone()).await?;
		self.gossip_support_subsystem.send_signal(signal).await?;

		Ok(())
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn route_message(&mut self, msg: MaybeTimed<AllMessages>) -> SubsystemResult<()> {
		let msg = msg.into_inner();
		self.metrics.on_message_relayed();
		match msg {
			AllMessages::CandidateValidation(msg) => {
				self.candidate_validation_subsystem.send_message(msg).await?;
			},
			AllMessages::CandidateBacking(msg) => {
				self.candidate_backing_subsystem.send_message(msg).await?;
			},
			AllMessages::CandidateSelection(msg) => {
				self.candidate_selection_subsystem.send_message(msg).await?;
			},
			AllMessages::StatementDistribution(msg) => {
				self.statement_distribution_subsystem.send_message(msg).await?;
			},
			AllMessages::AvailabilityDistribution(msg) => {
				self.availability_distribution_subsystem.send_message(msg).await?;
			},
			AllMessages::AvailabilityRecovery(msg) => {
				self.availability_recovery_subsystem.send_message(msg).await?;
			},
			AllMessages::BitfieldDistribution(msg) => {
				self.bitfield_distribution_subsystem.send_message(msg).await?;
			},
			AllMessages::BitfieldSigning(msg) => {
				self.bitfield_signing_subsystem.send_message(msg).await?;
			},
			AllMessages::Provisioner(msg) => {
				self.provisioner_subsystem.send_message(msg).await?;
			},
			AllMessages::PoVDistribution(msg) => {
				self.pov_distribution_subsystem.send_message(msg).await?;
			},
			AllMessages::RuntimeApi(msg) => {
				self.runtime_api_subsystem.send_message(msg).await?;
			},
			AllMessages::AvailabilityStore(msg) => {
				self.availability_store_subsystem.send_message(msg).await?;
			},
			AllMessages::NetworkBridge(msg) => {
				self.network_bridge_subsystem.send_message(msg).await?;
			},
			AllMessages::ChainApi(msg) => {
				self.chain_api_subsystem.send_message(msg).await?;
			},
			AllMessages::CollationGeneration(msg) => {
				self.collation_generation_subsystem.send_message(msg).await?;
			},
			AllMessages::CollatorProtocol(msg) => {
				self.collator_protocol_subsystem.send_message(msg).await?;
			},
			AllMessages::ApprovalDistribution(msg) => {
				self.approval_distribution_subsystem.send_message(msg).await?;
			},
			AllMessages::ApprovalVoting(msg) => {
				self.approval_voting_subsystem.send_message(msg).await?;
			},
			AllMessages::GossipSupport(msg) => {
				self.gossip_support_subsystem.send_message(msg).await?;
			},
		}

		Ok(())
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn on_head_activated(&mut self, hash: &Hash, parent_hash: Option<Hash>) -> Arc<jaeger::Span> {
		self.metrics.on_head_activated();
		if let Some(listeners) = self.activation_external_listeners.remove(hash) {
			for listener in listeners {
				// it's fine if the listener is no longer interested
				let _ = listener.send(Ok(()));
			}
		}

		let mut span = jaeger::Span::new(*hash, "leaf-activated");

		if let Some(parent_span) = parent_hash.and_then(|h| self.span_per_active_leaf.get(&h)) {
			span.add_follows_from(&*parent_span);
		}

		let span = Arc::new(span);
		self.span_per_active_leaf.insert(*hash, span.clone());
		span
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn on_head_deactivated(&mut self, hash: &Hash) {
		self.metrics.on_head_deactivated();
		self.activation_external_listeners.remove(hash);
		self.span_per_active_leaf.remove(hash);
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn clean_up_external_listeners(&mut self) {
		self.activation_external_listeners.retain(|_, v| {
			// remove dead listeners
			v.retain(|c| !c.is_canceled());
			!v.is_empty()
		})
	}

	#[tracing::instrument(level = "trace", skip(self, request), fields(subsystem = LOG_TARGET))]
	fn handle_external_request(&mut self, request: ExternalRequest) {
		match request {
			ExternalRequest::WaitForActivation { hash, response_channel } => {
				if self.active_leaves.get(&hash).is_some() {
					// it's fine if the listener is no longer interested
					let _ = response_channel.send(Ok(()));
				} else {
					self.activation_external_listeners.entry(hash).or_default().push(response_channel);
				}
			}
		}
	}

	fn spawn_job(&mut self, name: &'static str, j: BoxFuture<'static, ()>) {
		self.s.spawn(name, j);
	}

	fn spawn_blocking_job(&mut self, name: &'static str, j: BoxFuture<'static, ()>) {
		self.s.spawn_blocking(name, j);
	}
}

enum TaskKind {
	Regular,
	Blocking,
}

fn spawn<S: SpawnNamed, M: Send + 'static>(
	spawner: &mut S,
	futures: &mut FuturesUnordered<BoxFuture<'static, SubsystemResult<()>>>,
	to_overseer: metered::UnboundedMeteredSender<MaybeTimed<ToOverseer>>,
	s: impl Subsystem<OverseerSubsystemContext<M>>,
	metrics: &Metrics,
	seed: &mut u64,
	task_kind: TaskKind,
) -> SubsystemResult<OverseenSubsystem<M>> {
	let (to_tx, to_rx) = metered::channel(CHANNEL_CAPACITY, "subsystem_spawn");
	let ctx = OverseerSubsystemContext::new(
		to_rx,
		to_overseer,
		metrics.clone(),
		*seed,
		MESSAGE_TIMER_METRIC_CAPTURE_RATE,
	);
	let SpawnedSubsystem { future, name } = s.start(ctx);

	// increment the seed now that it's been used, so the next context will have its own distinct RNG
	*seed += 1;

	let (tx, rx) = oneshot::channel();

	let fut = Box::pin(async move {
		if let Err(e) = future.await {
			tracing::error!(subsystem=name, err = ?e, "subsystem exited with error");
		} else {
			tracing::debug!(subsystem=name, "subsystem exited without an error");
		}
		let _ = tx.send(());
	});

	match task_kind {
		TaskKind::Regular => {
			// Not using dismiss handle, channel communication instead.
			let _ = spawner.spawn(name, fut);
		},
		TaskKind::Blocking => spawner.spawn_blocking(name, fut),
	}

	futures.push(Box::pin(rx.map(|e| { tracing::warn!(err = ?e, "dropping error"); Ok(()) })));

	let instance = Some(SubsystemInstance {
		tx: to_tx,
		name,
	});

	Ok(OverseenSubsystem {
		instance,
	})
}

#[cfg(test)]
mod tests {
	use std::sync::atomic;
	use std::collections::HashMap;
	use futures::{executor, pin_mut, select, FutureExt, pending};

	use polkadot_primitives::v1::{BlockData, CollatorPair, PoV, CandidateHash};
	use polkadot_subsystem::{messages::RuntimeApiRequest, messages::NetworkBridgeEvent, jaeger};
	use polkadot_node_primitives::{CollationResult, CollationGenerationConfig};
	use polkadot_node_network_protocol::{PeerId, UnifiedReputationChange};
	use polkadot_node_subsystem_util::metered;

	use sp_core::crypto::Pair as _;

	use super::*;

	struct TestSubsystem1(metered::MeteredSender<usize>);

	impl<C> Subsystem<C> for TestSubsystem1
		where C: SubsystemContext<Message=CandidateValidationMessage>
	{
		fn start(self, mut ctx: C) -> SpawnedSubsystem {
			let mut sender = self.0;
			SpawnedSubsystem {
				name: "test-subsystem-1",
				future: Box::pin(async move {
					let mut i = 0;
					loop {
						match ctx.recv().await {
							Ok(FromOverseer::Communication { .. }) => {
								let _ = sender.send(i).await;
								i += 1;
								continue;
							}
							Ok(FromOverseer::Signal(OverseerSignal::Conclude)) => return Ok(()),
							Err(_) => return Ok(()),
							_ => (),
						}
					}
				}),
			}
		}
	}

	struct TestSubsystem2(metered::MeteredSender<usize>);

	impl<C> Subsystem<C> for TestSubsystem2
		where C: SubsystemContext<Message=CandidateBackingMessage>
	{
		fn start(self, mut ctx: C) -> SpawnedSubsystem {
			let sender = self.0.clone();
			SpawnedSubsystem {
				name: "test-subsystem-2",
				future: Box::pin(async move {
					let _sender = sender;
					let mut c: usize = 0;
					loop {
						if c < 10 {
							let (tx, _) = oneshot::channel();
							ctx.send_message(
								AllMessages::CandidateValidation(
									CandidateValidationMessage::ValidateFromChainState(
										Default::default(),
										PoV {
											block_data: BlockData(Vec::new()),
										}.into(),
										tx,
									)
								)
							).await;
							c += 1;
							continue;
						}
						match ctx.try_recv().await {
							Ok(Some(FromOverseer::Signal(OverseerSignal::Conclude))) => {
								break;
							}
							Ok(Some(_)) => {
								continue;
							}
							Err(_) => return Ok(()),
							_ => (),
						}
						pending!();
					}

					Ok(())
				}),
			}
		}
	}

	struct ReturnOnStart;

	impl<C> Subsystem<C> for ReturnOnStart
		where C: SubsystemContext<Message=CandidateBackingMessage>
	{
		fn start(self, mut _ctx: C) -> SpawnedSubsystem {
			SpawnedSubsystem {
				name: "test-subsystem-4",
				future: Box::pin(async move {
					// Do nothing and exit.
					Ok(())
				}),
			}
		}
	}


	// Checks that a minimal configuration of two jobs can run and exchange messages.
	#[test]
	fn overseer_works() {
		let spawner = sp_core::testing::TaskExecutor::new();

		executor::block_on(async move {
			let (s1_tx, s1_rx) = metered::channel::<usize>(64, "overseer_test");
			let (s2_tx, s2_rx) = metered::channel::<usize>(64, "overseer_test");

			let mut s1_rx = s1_rx.fuse();
			let mut s2_rx = s2_rx.fuse();

			let all_subsystems = AllSubsystems::<()>::dummy()
				.replace_candidate_validation(TestSubsystem1(s1_tx))
				.replace_candidate_backing(TestSubsystem2(s2_tx));

			let (overseer, mut handler) = Overseer::new(
				vec![],
				all_subsystems,
				None,
				spawner,
			).unwrap();
			let overseer_fut = overseer.run().fuse();

			pin_mut!(overseer_fut);

			let mut s1_results = Vec::new();
			let mut s2_results = Vec::new();

			loop {
				select! {
					_ = overseer_fut => break,
					s1_next = s1_rx.next() => {
						match s1_next {
							Some(msg) => {
								s1_results.push(msg);
								if s1_results.len() == 10 {
									handler.stop().await;
								}
							}
							None => break,
						}
					},
					s2_next = s2_rx.next() => {
						match s2_next {
							Some(_) => s2_results.push(s2_next),
							None => break,
						}
					},
					complete => break,
				}
			}

			assert_eq!(s1_results, (0..10).collect::<Vec<_>>());
		});
	}

	// Checks activated/deactivated metrics are updated properly.
	#[test]
	fn overseer_metrics_work() {
		let spawner = sp_core::testing::TaskExecutor::new();

		executor::block_on(async move {
			let first_block_hash = [1; 32].into();
			let second_block_hash = [2; 32].into();
			let third_block_hash = [3; 32].into();

			let first_block = BlockInfo {
				hash: first_block_hash,
				parent_hash: [0; 32].into(),
				number: 1,
			};
			let second_block = BlockInfo {
				hash: second_block_hash,
				parent_hash: first_block_hash,
				number: 2,
			};
			let third_block = BlockInfo {
				hash: third_block_hash,
				parent_hash: second_block_hash,
				number: 3,
			};

			let all_subsystems = AllSubsystems::<()>::dummy();
			let registry = prometheus::Registry::new();
			let (overseer, mut handler) = Overseer::new(
				vec![first_block],
				all_subsystems,
				Some(&registry),
				spawner,
			).unwrap();
			let overseer_fut = overseer.run().fuse();

			pin_mut!(overseer_fut);

			handler.block_imported(second_block).await;
			handler.block_imported(third_block).await;
			handler.send_msg(AllMessages::CandidateValidation(test_candidate_validation_msg())).await;
			handler.stop().await;

			select! {
				res = overseer_fut => {
					assert!(res.is_ok());
					let metrics = extract_metrics(&registry);
					assert_eq!(metrics["activated"], 3);
					assert_eq!(metrics["deactivated"], 2);
					assert_eq!(metrics["relayed"], 1);
				},
				complete => (),
			}
		});
	}

	fn extract_metrics(registry: &prometheus::Registry) -> HashMap<&'static str, u64> {
		let gather = registry.gather();
		assert_eq!(gather[0].get_name(), "parachain_activated_heads_total");
		assert_eq!(gather[1].get_name(), "parachain_deactivated_heads_total");
		assert_eq!(gather[2].get_name(), "parachain_from_overseer_channel_queue_size");
		assert_eq!(gather[3].get_name(), "parachain_messages_relayed_total");
		assert_eq!(gather[4].get_name(), "parachain_overseer_messages_relay_timings");
		assert_eq!(gather[5].get_name(), "parachain_to_overseer_channel_queue_size");
		let activated = gather[0].get_metric()[0].get_counter().get_value() as u64;
		let deactivated = gather[1].get_metric()[0].get_counter().get_value() as u64;
		let relayed = gather[3].get_metric()[0].get_counter().get_value() as u64;
		let mut result = HashMap::new();
		result.insert("activated", activated);
		result.insert("deactivated", deactivated);
		result.insert("relayed", relayed);
		result
	}

	// Spawn a subsystem that immediately exits.
	//
	// Should immediately conclude the overseer itself.
	#[test]
	fn overseer_ends_on_subsystem_exit() {
		let spawner = sp_core::testing::TaskExecutor::new();

		executor::block_on(async move {
			let all_subsystems = AllSubsystems::<()>::dummy()
				.replace_candidate_backing(ReturnOnStart);
			let (overseer, _handle) = Overseer::new(
				vec![],
				all_subsystems,
				None,
				spawner,
			).unwrap();

			overseer.run().await.unwrap();
		})
	}

	struct TestSubsystem5(metered::MeteredSender<OverseerSignal>);

	impl<C> Subsystem<C> for TestSubsystem5
		where C: SubsystemContext<Message=CandidateValidationMessage>
	{
		fn start(self, mut ctx: C) -> SpawnedSubsystem {
			let mut sender = self.0.clone();

			SpawnedSubsystem {
				name: "test-subsystem-5",
				future: Box::pin(async move {
					loop {
						match ctx.try_recv().await {
							Ok(Some(FromOverseer::Signal(OverseerSignal::Conclude))) => break,
							Ok(Some(FromOverseer::Signal(s))) => {
								sender.send(s).await.unwrap();
								continue;
							},
							Ok(Some(_)) => continue,
							Err(_) => break,
							_ => (),
						}
						pending!();
					}

					Ok(())
				}),
			}
		}
	}

	struct TestSubsystem6(metered::MeteredSender<OverseerSignal>);

	impl<C> Subsystem<C> for TestSubsystem6
		where C: SubsystemContext<Message=CandidateBackingMessage>
	{
		fn start(self, mut ctx: C) -> SpawnedSubsystem {
			let mut sender = self.0.clone();

			SpawnedSubsystem {
				name: "test-subsystem-6",
				future: Box::pin(async move {
					loop {
						match ctx.try_recv().await {
							Ok(Some(FromOverseer::Signal(OverseerSignal::Conclude))) => break,
							Ok(Some(FromOverseer::Signal(s))) => {
								sender.send(s).await.unwrap();
								continue;
							},
							Ok(Some(_)) => continue,
							Err(_) => break,
							_ => (),
						}
						pending!();
					}

					Ok(())
				}),
			}
		}
	}

	// Tests that starting with a defined set of leaves and receiving
	// notifications on imported blocks triggers expected `StartWork` and `StopWork` heartbeats.
	#[test]
	fn overseer_start_stop_works() {
		let spawner = sp_core::testing::TaskExecutor::new();

		executor::block_on(async move {
			let first_block_hash = [1; 32].into();
			let second_block_hash = [2; 32].into();
			let third_block_hash = [3; 32].into();

			let first_block = BlockInfo {
				hash: first_block_hash,
				parent_hash: [0; 32].into(),
				number: 1,
			};
			let second_block = BlockInfo {
				hash: second_block_hash,
				parent_hash: first_block_hash,
				number: 2,
			};
			let third_block = BlockInfo {
				hash: third_block_hash,
				parent_hash: second_block_hash,
				number: 3,
			};

			let (tx_5, mut rx_5) = metered::channel(64, "overseer_test");
			let (tx_6, mut rx_6) = metered::channel(64, "overseer_test");
			let all_subsystems = AllSubsystems::<()>::dummy()
				.replace_candidate_validation(TestSubsystem5(tx_5))
				.replace_candidate_backing(TestSubsystem6(tx_6));
			let (overseer, mut handler) = Overseer::new(
				vec![first_block],
				all_subsystems,
				None,
				spawner,
			).unwrap();

			let overseer_fut = overseer.run().fuse();
			pin_mut!(overseer_fut);

			let mut ss5_results = Vec::new();
			let mut ss6_results = Vec::new();

			handler.block_imported(second_block).await;
			handler.block_imported(third_block).await;

			let expected_heartbeats = vec![
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(
					first_block_hash,
					Arc::new(jaeger::Span::Disabled),
				)),
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
					activated: [(second_block_hash, Arc::new(jaeger::Span::Disabled))].as_ref().into(),
					deactivated: [first_block_hash].as_ref().into(),
				}),
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
					activated: [(third_block_hash, Arc::new(jaeger::Span::Disabled))].as_ref().into(),
					deactivated: [second_block_hash].as_ref().into(),
				}),
			];

			loop {
				select! {
					res = overseer_fut => {
						assert!(res.is_ok());
						break;
					},
					res = rx_5.next() => {
						if let Some(res) = res {
							ss5_results.push(res);
						}
					}
					res = rx_6.next() => {
						if let Some(res) = res {
							ss6_results.push(res);
						}
					}
					complete => break,
				}

				if ss5_results.len() == expected_heartbeats.len() &&
					ss6_results.len() == expected_heartbeats.len() {
						handler.stop().await;
				}
			}

			assert_eq!(ss5_results, expected_heartbeats);
			assert_eq!(ss6_results, expected_heartbeats);
		});
	}

	// Tests that starting with a defined set of leaves and receiving
	// notifications on imported blocks triggers expected `StartWork` and `StopWork` heartbeats.
	#[test]
	fn overseer_finalize_works() {
		let spawner = sp_core::testing::TaskExecutor::new();

		executor::block_on(async move {
			let first_block_hash = [1; 32].into();
			let second_block_hash = [2; 32].into();
			let third_block_hash = [3; 32].into();

			let first_block = BlockInfo {
				hash: first_block_hash,
				parent_hash: [0; 32].into(),
				number: 1,
			};
			let second_block = BlockInfo {
				hash: second_block_hash,
				parent_hash: [42; 32].into(),
				number: 2,
			};
			let third_block = BlockInfo {
				hash: third_block_hash,
				parent_hash: second_block_hash,
				number: 3,
			};

			let (tx_5, mut rx_5) = metered::channel(64, "overseer_test");
			let (tx_6, mut rx_6) = metered::channel(64, "overseer_test");

			let all_subsystems = AllSubsystems::<()>::dummy()
				.replace_candidate_validation(TestSubsystem5(tx_5))
				.replace_candidate_backing(TestSubsystem6(tx_6));

			// start with two forks of different height.
			let (overseer, mut handler) = Overseer::new(
				vec![first_block, second_block],
				all_subsystems,
				None,
				spawner,
			).unwrap();

			let overseer_fut = overseer.run().fuse();
			pin_mut!(overseer_fut);

			let mut ss5_results = Vec::new();
			let mut ss6_results = Vec::new();

			// this should stop work on both forks we started with earlier.
			handler.block_finalized(third_block).await;

			let expected_heartbeats = vec![
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
					activated: [
						(first_block_hash, Arc::new(jaeger::Span::Disabled)),
						(second_block_hash, Arc::new(jaeger::Span::Disabled)),
					].as_ref().into(),
					..Default::default()
				}),
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
					deactivated: [first_block_hash, second_block_hash].as_ref().into(),
					..Default::default()
				}),
				OverseerSignal::BlockFinalized(third_block_hash, 3),
			];

			loop {
				select! {
					res = overseer_fut => {
						assert!(res.is_ok());
						break;
					},
					res = rx_5.next() => {
						if let Some(res) = res {
							ss5_results.push(res);
						}
					}
					res = rx_6.next() => {
						if let Some(res) = res {
							ss6_results.push(res);
						}
					}
					complete => break,
				}

				if ss5_results.len() == expected_heartbeats.len() && ss6_results.len() == expected_heartbeats.len() {
					handler.stop().await;
				}
			}

			assert_eq!(ss5_results.len(), expected_heartbeats.len());
			assert_eq!(ss6_results.len(), expected_heartbeats.len());

			// Notifications on finality for multiple blocks at once
			// may be received in different orders.
			for expected in expected_heartbeats {
				assert!(ss5_results.contains(&expected));
				assert!(ss6_results.contains(&expected));
			}
		});
	}

	#[test]
	fn do_not_send_empty_leaves_update_on_block_finalization() {
		let spawner = sp_core::testing::TaskExecutor::new();

		executor::block_on(async move {
			let imported_block = BlockInfo {
				hash: Hash::random(),
				parent_hash: Hash::random(),
				number: 1,
			};

			let finalized_block = BlockInfo {
				hash: Hash::random(),
				parent_hash: Hash::random(),
				number: 1,
			};

			let (tx_5, mut rx_5) = metered::channel(64, "overseer_test");

			let all_subsystems = AllSubsystems::<()>::dummy()
				.replace_candidate_backing(TestSubsystem6(tx_5));

			let (overseer, mut handler) = Overseer::new(
				Vec::new(),
				all_subsystems,
				None,
				spawner,
			).unwrap();

			let overseer_fut = overseer.run().fuse();
			pin_mut!(overseer_fut);

			let mut ss5_results = Vec::new();

			handler.block_finalized(finalized_block.clone()).await;
			handler.block_imported(imported_block.clone()).await;

			let expected_heartbeats = vec![
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
					activated: [
						(imported_block.hash, Arc::new(jaeger::Span::Disabled)),
					].as_ref().into(),
					..Default::default()
				}),
				OverseerSignal::BlockFinalized(finalized_block.hash, 1),
			];

			loop {
				select! {
					res = overseer_fut => {
						assert!(res.is_ok());
						break;
					},
					res = rx_5.next() => {
						if let Some(res) = dbg!(res) {
							ss5_results.push(res);
						}
					}
				}

				if ss5_results.len() == expected_heartbeats.len() {
					handler.stop().await;
				}
			}

			assert_eq!(ss5_results.len(), expected_heartbeats.len());

			for expected in expected_heartbeats {
				assert!(ss5_results.contains(&expected));
			}
		});
	}

	#[derive(Clone)]
	struct CounterSubsystem {
		stop_signals_received: Arc<atomic::AtomicUsize>,
		signals_received: Arc<atomic::AtomicUsize>,
		msgs_received: Arc<atomic::AtomicUsize>,
	}

	impl CounterSubsystem {
		fn new(
			stop_signals_received: Arc<atomic::AtomicUsize>,
			signals_received: Arc<atomic::AtomicUsize>,
			msgs_received: Arc<atomic::AtomicUsize>,
		) -> Self {
			Self {
				stop_signals_received,
				signals_received,
				msgs_received,
			}
		}
	}

	impl<C, M> Subsystem<C> for CounterSubsystem
		where
			C: SubsystemContext<Message=M>,
			M: Send,
	{
		fn start(self, mut ctx: C) -> SpawnedSubsystem {
			SpawnedSubsystem {
				name: "counter-subsystem",
				future: Box::pin(async move {
					loop {
						match ctx.try_recv().await {
							Ok(Some(FromOverseer::Signal(OverseerSignal::Conclude))) => {
								self.stop_signals_received.fetch_add(1, atomic::Ordering::SeqCst);
								break;
							},
							Ok(Some(FromOverseer::Signal(_))) => {
								self.signals_received.fetch_add(1, atomic::Ordering::SeqCst);
								continue;
							},
							Ok(Some(FromOverseer::Communication { .. })) => {
								self.msgs_received.fetch_add(1, atomic::Ordering::SeqCst);
								continue;
							},
							Err(_) => (),
							_ => (),
						}
						pending!();
					}

					Ok(())
				}),
			}
		}
	}

	fn test_candidate_validation_msg() -> CandidateValidationMessage {
		let (sender, _) = oneshot::channel();
		let pov = Arc::new(PoV { block_data: BlockData(Vec::new()) });
		CandidateValidationMessage::ValidateFromChainState(Default::default(), pov, sender)
	}

	fn test_candidate_backing_msg() -> CandidateBackingMessage {
		let (sender, _) = oneshot::channel();
		CandidateBackingMessage::GetBackedCandidates(Default::default(), Vec::new(), sender)
	}

	fn test_candidate_selection_msg() -> CandidateSelectionMessage {
		CandidateSelectionMessage::default()
	}

	fn test_chain_api_msg() -> ChainApiMessage {
		let (sender, _) = oneshot::channel();
		ChainApiMessage::FinalizedBlockNumber(sender)
	}

	fn test_collator_generation_msg() -> CollationGenerationMessage {
		CollationGenerationMessage::Initialize(CollationGenerationConfig {
			key: CollatorPair::generate().0,
			collator: Box::new(|_, _| TestCollator.boxed()),
			para_id: Default::default(),
		})
	}
	struct TestCollator;

	impl Future for TestCollator {
		type Output = Option<CollationResult>;

		fn poll(self: Pin<&mut Self>, _cx: &mut futures::task::Context) -> Poll<Self::Output> {
			panic!("at the Disco")
		}
	}

	impl Unpin for TestCollator {}

	fn test_collator_protocol_msg() -> CollatorProtocolMessage {
		CollatorProtocolMessage::CollateOn(Default::default())
	}

	fn test_network_bridge_event<M>() -> NetworkBridgeEvent<M> {
		NetworkBridgeEvent::PeerDisconnected(PeerId::random())
	}

	fn test_statement_distribution_msg() -> StatementDistributionMessage {
		StatementDistributionMessage::NetworkBridgeUpdateV1(test_network_bridge_event())
	}

	fn test_availability_recovery_msg() -> AvailabilityRecoveryMessage {
		let (sender, _) = oneshot::channel();
		AvailabilityRecoveryMessage::RecoverAvailableData(
			Default::default(),
			Default::default(),
			None,
			sender,
		)
	}

	fn test_bitfield_distribution_msg() -> BitfieldDistributionMessage {
		BitfieldDistributionMessage::NetworkBridgeUpdateV1(test_network_bridge_event())
	}

	fn test_provisioner_msg() -> ProvisionerMessage {
		let (sender, _) = oneshot::channel();
		ProvisionerMessage::RequestInherentData(Default::default(), sender)
	}

	fn test_pov_distribution_msg() -> PoVDistributionMessage {
		PoVDistributionMessage::NetworkBridgeUpdateV1(test_network_bridge_event())
	}

	fn test_runtime_api_msg() -> RuntimeApiMessage {
		let (sender, _) = oneshot::channel();
		RuntimeApiMessage::Request(Default::default(), RuntimeApiRequest::Validators(sender))
	}

	fn test_availability_store_msg() -> AvailabilityStoreMessage {
		let (sender, _) = oneshot::channel();
		AvailabilityStoreMessage::QueryAvailableData(CandidateHash(Default::default()), sender)
	}

	fn test_network_bridge_msg() -> NetworkBridgeMessage {
		NetworkBridgeMessage::ReportPeer(PeerId::random(), UnifiedReputationChange::BenefitMinor(""))
	}

	fn test_approval_distribution_msg() -> ApprovalDistributionMessage {
		ApprovalDistributionMessage::NewBlocks(Default::default())
	}

	fn test_approval_voting_msg() -> ApprovalVotingMessage {
		let (sender, _) = oneshot::channel();
		ApprovalVotingMessage::ApprovedAncestor(Default::default(), 0, sender)
	}

	// Checks that `stop`, `broadcast_signal` and `broadcast_message` are implemented correctly.
	#[test]
	fn overseer_all_subsystems_receive_signals_and_messages() {
		let spawner = sp_core::testing::TaskExecutor::new();

		executor::block_on(async move {
			let stop_signals_received = Arc::new(atomic::AtomicUsize::new(0));
			let signals_received = Arc::new(atomic::AtomicUsize::new(0));
			let msgs_received = Arc::new(atomic::AtomicUsize::new(0));

			let subsystem = CounterSubsystem::new(
				stop_signals_received.clone(),
				signals_received.clone(),
				msgs_received.clone(),
			);

			let all_subsystems = AllSubsystems {
				candidate_validation: subsystem.clone(),
				candidate_backing: subsystem.clone(),
				candidate_selection: subsystem.clone(),
				collation_generation: subsystem.clone(),
				collator_protocol: subsystem.clone(),
				statement_distribution: subsystem.clone(),
				availability_distribution: subsystem.clone(),
				availability_recovery: subsystem.clone(),
				bitfield_signing: subsystem.clone(),
				bitfield_distribution: subsystem.clone(),
				provisioner: subsystem.clone(),
				pov_distribution: subsystem.clone(),
				runtime_api: subsystem.clone(),
				availability_store: subsystem.clone(),
				network_bridge: subsystem.clone(),
				chain_api: subsystem.clone(),
				approval_distribution: subsystem.clone(),
				approval_voting: subsystem.clone(),
				gossip_support: subsystem.clone(),
			};
			let (overseer, mut handler) = Overseer::new(
				vec![],
				all_subsystems,
				None,
				spawner,
			).unwrap();
			let overseer_fut = overseer.run().fuse();

			pin_mut!(overseer_fut);

			// send a signal to each subsystem
			handler.block_imported(BlockInfo {
				hash: Default::default(),
				parent_hash: Default::default(),
				number: Default::default(),
			}).await;

			// send a msg to each subsystem
			// except for BitfieldSigning and GossipSupport as the messages are not instantiable
			handler.send_msg(AllMessages::CandidateValidation(test_candidate_validation_msg())).await;
			handler.send_msg(AllMessages::CandidateBacking(test_candidate_backing_msg())).await;
			handler.send_msg(AllMessages::CandidateSelection(test_candidate_selection_msg())).await;
			handler.send_msg(AllMessages::CollationGeneration(test_collator_generation_msg())).await;
			handler.send_msg(AllMessages::CollatorProtocol(test_collator_protocol_msg())).await;
			handler.send_msg(AllMessages::StatementDistribution(test_statement_distribution_msg())).await;
			handler.send_msg(AllMessages::AvailabilityRecovery(test_availability_recovery_msg())).await;
			// handler.send_msg(AllMessages::BitfieldSigning(test_bitfield_signing_msg())).await;
			// handler.send_msg(AllMessages::GossipSupport(test_bitfield_signing_msg())).await;
			handler.send_msg(AllMessages::BitfieldDistribution(test_bitfield_distribution_msg())).await;
			handler.send_msg(AllMessages::Provisioner(test_provisioner_msg())).await;
			handler.send_msg(AllMessages::PoVDistribution(test_pov_distribution_msg())).await;
			handler.send_msg(AllMessages::RuntimeApi(test_runtime_api_msg())).await;
			handler.send_msg(AllMessages::AvailabilityStore(test_availability_store_msg())).await;
			handler.send_msg(AllMessages::NetworkBridge(test_network_bridge_msg())).await;
			handler.send_msg(AllMessages::ChainApi(test_chain_api_msg())).await;
			handler.send_msg(AllMessages::ApprovalDistribution(test_approval_distribution_msg())).await;
			handler.send_msg(AllMessages::ApprovalVoting(test_approval_voting_msg())).await;

			// send a stop signal to each subsystems
			handler.stop().await;

			select! {
				res = overseer_fut => {
					const NUM_SUBSYSTEMS: usize = 19;

					assert_eq!(stop_signals_received.load(atomic::Ordering::SeqCst), NUM_SUBSYSTEMS);
					assert_eq!(signals_received.load(atomic::Ordering::SeqCst), NUM_SUBSYSTEMS);
					// -3 for BitfieldSigning, GossipSupport and AvailabilityDistribution
					assert_eq!(msgs_received.load(atomic::Ordering::SeqCst), NUM_SUBSYSTEMS - 3);

					assert!(res.is_ok());
				},
				complete => (),
			}
		});
	}
}
