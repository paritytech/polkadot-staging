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

//! Utility module for subsystems
//!
//! Many subsystems have common interests such as canceling a bunch of spawned jobs,
//! or determining what their validator ID is. These common interests are factored into
//! this module.
//!
//! This crate also reexports Prometheus metric types which are expected to be implemented by subsystems.

#![warn(missing_docs)]

use polkadot_node_subsystem::{
	overseer,
	errors::RuntimeApiError,
	messages::{
		AllMessages,
		RuntimeApiMessage,
		RuntimeApiRequest,
		RuntimeApiSender,
		BoundToRelayParent,
	},
	ActiveLeavesUpdate, OverseerSignal,
	errors::{
		SubsystemError,
	},
	SpawnedSubsystem,
};

pub use overseer::{
	Subsystem,
	SubsystemContext,
	TimeoutExt,
	gen::OverseerError,
	gen::Timeout,
};

pub use polkadot_node_metrics::{
	Metronome,
	metrics,
};

use polkadot_node_jaeger as jaeger;
use futures::{channel::{mpsc, oneshot}, prelude::*, select, stream::{Stream, SelectAll}};
use parity_scale_codec::Encode;
use pin_project::pin_project;
use polkadot_primitives::v1::{
	CandidateEvent, CommittedCandidateReceipt, CoreState, EncodeAs, PersistedValidationData,
	GroupRotationInfo, Hash, Id as ParaId, OccupiedCoreAssumption,
	SessionIndex, Signed, SigningContext, ValidationCode, ValidatorId, ValidatorIndex, SessionInfo,
AuthorityDiscoveryId, GroupIndex,
};
use sp_core::{traits::SpawnNamed, Public};
use sp_application_crypto::AppKey;
use sp_keystore::{CryptoStore, SyncCryptoStorePtr, Error as KeystoreError};
use std::{
	collections::{HashMap, hash_map::Entry}, convert::TryFrom, marker::Unpin, pin::Pin, task::{Poll, Context},
	time::Duration, fmt, sync::Arc,
};
use thiserror::Error;

pub use metered_channel as metered;
pub use polkadot_node_network_protocol::MIN_GOSSIP_PEERS;

pub use determine_new_blocks::determine_new_blocks;
/// Error classification.
pub use error_handling::{Fault, unwrap_non_fatal};

/// These reexports are required so that external crates can use the `delegated_subsystem` macro properly.
pub mod reexports {
	pub use polkadot_overseer::gen::{
		SpawnNamed,
		SpawnedSubsystem,
		Subsystem,
		SubsystemContext,
	};
}

/// Convenient and efficient runtime info access.
pub mod runtime;
/// A rolling session window cache.
pub mod rolling_session_window;

mod determine_new_blocks;
mod error_handling;

#[cfg(test)]
mod tests;

/// Duration a job will wait after sending a stop signal before hard-aborting.
pub const JOB_GRACEFUL_STOP_DURATION: Duration = Duration::from_secs(1);
/// Capacity of channels to and from individual jobs
pub const JOB_CHANNEL_CAPACITY: usize = 64;

/// Utility errors
#[derive(Debug, Error)]
pub enum Error {
	/// Attempted to send or receive on a oneshot channel which had been canceled
	#[error(transparent)]
	Oneshot(#[from] oneshot::Canceled),
	/// Attempted to send on a MPSC channel which has been canceled
	#[error(transparent)]
	Mpsc(#[from] mpsc::SendError),
	/// A subsystem error
	#[error(transparent)]
	Subsystem(#[from] SubsystemError),
	/// An error in the Runtime API.
	#[error(transparent)]
	RuntimeApi(#[from] RuntimeApiError),
	/// The type system wants this even though it doesn't make sense
	#[error(transparent)]
	Infallible(#[from] std::convert::Infallible),
	/// Attempted to convert from an AllMessages to a FromJob, and failed.
	#[error("AllMessage not relevant to Job")]
	SenderConversion(String),
	/// The local node is not a validator.
	#[error("Node is not a validator")]
	NotAValidator,
	/// Already forwarding errors to another sender
	#[error("AlreadyForwarding")]
	AlreadyForwarding,
}

impl From<OverseerError> for Error {
	fn from(e: OverseerError) -> Self {
		Self::from(SubsystemError::from(e))
	}
}

/// A type alias for Runtime API receivers.
pub type RuntimeApiReceiver<T> = oneshot::Receiver<Result<T, RuntimeApiError>>;

/// Request some data from the `RuntimeApi`.
pub async fn request_from_runtime<RequestBuilder, Response, Sender>(
	parent: Hash,
	sender: &mut Sender,
	request_builder: RequestBuilder,
) -> RuntimeApiReceiver<Response>
where
	RequestBuilder: FnOnce(RuntimeApiSender<Response>) -> RuntimeApiRequest,
	Sender: SubsystemSender,
{
	let (tx, rx) = oneshot::channel();

	sender.send_message(RuntimeApiMessage::Request(parent, request_builder(tx)).into()).await;

	rx
}

/// Construct specialized request functions for the runtime.
///
/// These would otherwise get pretty repetitive.
macro_rules! specialize_requests {
	// expand return type name for documentation purposes
	(fn $func_name:ident( $( $param_name:ident : $param_ty:ty ),* ) -> $return_ty:ty ; $request_variant:ident;) => {
		specialize_requests!{
			named stringify!($request_variant) ; fn $func_name( $( $param_name : $param_ty ),* ) -> $return_ty ; $request_variant;
		}
	};

	// create a single specialized request function
	(named $doc_name:expr ; fn $func_name:ident( $( $param_name:ident : $param_ty:ty ),* ) -> $return_ty:ty ; $request_variant:ident;) => {
		#[doc = "Request `"]
		#[doc = $doc_name]
		#[doc = "` from the runtime"]
		pub async fn $func_name (
			parent: Hash,
			$(
				$param_name: $param_ty,
			)*
			sender: &mut impl SubsystemSender,
		) -> RuntimeApiReceiver<$return_ty>
		{
			request_from_runtime(parent, sender, |tx| RuntimeApiRequest::$request_variant(
				$( $param_name, )* tx
			)).await
		}
	};

	// recursive decompose
	(
		fn $func_name:ident( $( $param_name:ident : $param_ty:ty ),* ) -> $return_ty:ty ; $request_variant:ident;
		$(
			fn $t_func_name:ident( $( $t_param_name:ident : $t_param_ty:ty ),* ) -> $t_return_ty:ty ; $t_request_variant:ident;
		)+
	) => {
		specialize_requests!{
			fn $func_name( $( $param_name : $param_ty ),* ) -> $return_ty ; $request_variant ;
		}
		specialize_requests!{
			$(
				fn $t_func_name( $( $t_param_name : $t_param_ty ),* ) -> $t_return_ty ; $t_request_variant ;
			)+
		}
	};
}

specialize_requests! {
	fn request_authorities() -> Vec<AuthorityDiscoveryId>; Authorities;
	fn request_validators() -> Vec<ValidatorId>; Validators;
	fn request_validator_groups() -> (Vec<Vec<ValidatorIndex>>, GroupRotationInfo); ValidatorGroups;
	fn request_availability_cores() -> Vec<CoreState>; AvailabilityCores;
	fn request_persisted_validation_data(para_id: ParaId, assumption: OccupiedCoreAssumption) -> Option<PersistedValidationData>; PersistedValidationData;
	fn request_session_index_for_child() -> SessionIndex; SessionIndexForChild;
	fn request_validation_code(para_id: ParaId, assumption: OccupiedCoreAssumption) -> Option<ValidationCode>; ValidationCode;
	fn request_candidate_pending_availability(para_id: ParaId) -> Option<CommittedCandidateReceipt>; CandidatePendingAvailability;
	fn request_candidate_events() -> Vec<CandidateEvent>; CandidateEvents;
	fn request_session_info(index: SessionIndex) -> Option<SessionInfo>; SessionInfo;
}

/// From the given set of validators, find the first key we can sign with, if any.
pub async fn signing_key(validators: &[ValidatorId], keystore: &SyncCryptoStorePtr)
	-> Option<ValidatorId>
{
	signing_key_and_index(validators, keystore).await.map(|(k, _)| k)
}

/// From the given set of validators, find the first key we can sign with, if any, and return it
/// along with the validator index.
pub async fn signing_key_and_index(validators: &[ValidatorId], keystore: &SyncCryptoStorePtr)
	-> Option<(ValidatorId, ValidatorIndex)>
{
	for (i, v) in validators.iter().enumerate() {
		if CryptoStore::has_keys(&**keystore, &[(v.to_raw_vec(), ValidatorId::ID)]).await {
			return Some((v.clone(), ValidatorIndex(i as _)));
		}
	}
	None
}

/// Find the validator group the given validator index belongs to.
pub fn find_validator_group(groups: &[Vec<ValidatorIndex>], index: ValidatorIndex)
	-> Option<GroupIndex>
{
	groups.iter().enumerate().find_map(|(i, g)| if g.contains(&index) {
		Some(GroupIndex(i as _))
	} else {
		None
	})
}

/// Choose a random subset of `min` elements.
/// But always include `is_priority` elements.
pub fn choose_random_subset<T, F: FnMut(&T) -> bool>(is_priority: F, mut v: Vec<T>, min: usize) -> Vec<T> {
	use rand::seq::SliceRandom as _;

	// partition the elements into priority first
	// the returned index is when non_priority elements start
	let i = itertools::partition(&mut v, is_priority);

	if i >= min || v.len() <= i {
		v.truncate(i);
		return v;
	}

	let mut rng = rand::thread_rng();
	v[i..].shuffle(&mut rng);

	v.truncate(min);
	v
}

/// Returns a bool with a probability of `a / b` of being true.
pub fn gen_ratio(a: usize, b: usize) -> bool {
	use rand::Rng as _;
	let mut rng = rand::thread_rng();
	rng.gen_ratio(a as u32, b as u32)
}

/// Local validator information
///
/// It can be created if the local node is a validator in the context of a particular
/// relay chain block.
#[derive(Debug)]
pub struct Validator {
	signing_context: SigningContext,
	key: ValidatorId,
	index: ValidatorIndex,
}

impl Validator {
	/// Get a struct representing this node's validator if this node is in fact a validator in the context of the given block.
	pub async fn new(
		parent: Hash,
		keystore: SyncCryptoStorePtr,
		sender: &mut impl SubsystemSender,
	) -> Result<Self, Error>
	{
		// Note: request_validators and request_session_index_for_child do not and cannot
		// run concurrently: they both have a mutable handle to the same sender.
		// However, each of them returns a oneshot::Receiver, and those are resolved concurrently.
		let (validators, session_index) = futures::try_join!(
			request_validators(parent, sender).await,
			request_session_index_for_child(parent, sender).await,
		)?;

		let signing_context = SigningContext {
			session_index: session_index?,
			parent_hash: parent,
		};

		let validators = validators?;

		Self::construct(&validators, signing_context, keystore).await
	}

	/// Construct a validator instance without performing runtime fetches.
	///
	/// This can be useful if external code also needs the same data.
	pub async fn construct(
		validators: &[ValidatorId],
		signing_context: SigningContext,
		keystore: SyncCryptoStorePtr,
	) -> Result<Self, Error> {
		let (key, index) = signing_key_and_index(validators, &keystore)
			.await
			.ok_or(Error::NotAValidator)?;

		Ok(Validator {
			signing_context,
			key,
			index,
		})
	}

	/// Get this validator's id.
	pub fn id(&self) -> ValidatorId {
		self.key.clone()
	}

	/// Get this validator's local index.
	pub fn index(&self) -> ValidatorIndex {
		self.index
	}

	/// Get the current signing context.
	pub fn signing_context(&self) -> &SigningContext {
		&self.signing_context
	}

	/// Sign a payload with this validator
	pub async fn sign<Payload: EncodeAs<RealPayload>, RealPayload: Encode>(
		&self,
		keystore: SyncCryptoStorePtr,
		payload: Payload,
	) -> Result<Option<Signed<Payload, RealPayload>>, KeystoreError> {
		Signed::sign(&keystore, payload, &self.signing_context, self.index, &self.key).await
	}
}

struct AbortOnDrop(future::AbortHandle);

impl Drop for AbortOnDrop {
	fn drop(&mut self) {
		self.0.abort();
	}
}

/// A JobHandle manages a particular job for a subsystem.
struct JobHandle<ToJob> {
	_abort_handle: AbortOnDrop,
	to_job: mpsc::Sender<ToJob>,
}

impl<ToJob> JobHandle<ToJob> {
	/// Send a message to the job.
	async fn send_msg(&mut self, msg: ToJob) -> Result<(), Error> {
		self.to_job.send(msg).await.map_err(Into::into)
	}
}

/// Commands from a job to the broader subsystem.
pub enum FromJobCommand {
	/// Spawn a child task on the executor.
	Spawn(&'static str, Pin<Box<dyn Future<Output = ()> + Send>>),
	/// Spawn a blocking child task on the executor's dedicated thread pool.
	SpawnBlocking(&'static str, Pin<Box<dyn Future<Output = ()> + Send>>),
}

/// A sender for messages from jobs, as well as commands to the overseer.
pub struct JobSender<S: SubsystemSender> {
	sender: S,
	from_job: mpsc::Sender<FromJobCommand>,
}

// A custom clone impl, since M does not need to impl `Clone`
// which `#[derive(Clone)]` requires.
impl<S: SubsystemSender> Clone for JobSender<S> {
	fn clone(&self) -> Self {
		Self {
			sender: self.sender.clone(),
			from_job: self.from_job.clone(),
		}
	}
}

impl<S: SubsystemSender> JobSender<S> {
	/// Get access to the underlying subsystem sender.
	pub fn subsystem_sender(&mut self) -> &mut S {
		&mut self.sender
	}

	/// Send a direct message to some other `Subsystem`, routed based on message type.
	pub async fn send_message(&mut self, msg: impl Into<AllMessages>) {
		self.sender.send_message(msg.into()).await
	}

	/// Send multiple direct messages to other `Subsystem`s, routed based on message type.
	pub async fn send_messages<T, M>(&mut self, msgs: T)
	where
		T: IntoIterator<Item = M> + Send, T::IntoIter: Send,
		M: Into<AllMessages>,
	{
		self.sender.send_messages(msgs.into_iter().map(|m| m.into())).await
	}


	/// Send a message onto the unbounded queue of some other `Subsystem`, routed based on message
	/// type.
	///
	/// This function should be used only when there is some other bounding factor on the messages
	/// sent with it. Otherwise, it risks a memory leak.
	pub fn send_unbounded_message(&mut self, msg: impl Into<AllMessages>) {
		self.sender.send_unbounded_message(msg.into())
	}

	/// Send a command to the subsystem, to be relayed onwards to the overseer.
	pub async fn send_command(&mut self, msg: FromJobCommand) -> Result<(), mpsc::SendError> {
		self.from_job.send(msg).await
	}
}


#[async_trait::async_trait]
impl<S, M> overseer::SubsystemSender<M> for JobSender<S>
where
	M: Send + 'static + Into<AllMessages>,
	S: SubsystemSender + Clone,
{
	async fn send_message(&mut self, msg: M) {
		self.sender.send_message(msg.into()).await
	}

	async fn send_messages<T>(&mut self, msgs: T)
		where T: IntoIterator<Item = M> + Send, T::IntoIter: Send
	{
		self.sender.send_messages(msgs.into_iter().map(|m| m.into())).await
	}

	fn send_unbounded_message(&mut self, msg: M) {
		self.sender.send_unbounded_message(msg.into())
	}
}

impl fmt::Debug for FromJobCommand {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
		match self {
			Self::Spawn(name, _) => write!(fmt, "FromJobCommand::Spawn({})", name),
			Self::SpawnBlocking(name, _) => write!(fmt, "FromJobCommand::SpawnBlocking({})", name),
		}
	}
}

/// This trait governs jobs.
///
/// Jobs are instantiated and killed automatically on appropriate overseer messages.
/// Other messages are passed along to and from the job via the overseer to other subsystems.
pub trait JobTrait: Unpin + Sized {
	/// Message type used to send messages to the job.
	type ToJob: 'static + BoundToRelayParent + Send;
	/// Job runtime error.
	type Error: 'static + std::error::Error + Send;
	/// Extra arguments this job needs to run properly.
	///
	/// If no extra information is needed, it is perfectly acceptable to set it to `()`.
	type RunArgs: 'static + Send;
	/// Subsystem-specific Prometheus metrics.
	///
	/// Jobs spawned by one subsystem should share the same
	/// instance of metrics (use `.clone()`).
	/// The `delegate_subsystem!` macro should take care of this.
	type Metrics: 'static + metrics::Metrics + Send;

	/// Name of the job, i.e. `CandidateBackingJob`
	const NAME: &'static str;

	/// Run a job for the given relay `parent`.
	///
	/// The job should be ended when `receiver` returns `None`.
	fn run<S: SubsystemSender>(
		parent: Hash,
		span: Arc<jaeger::Span>,
		run_args: Self::RunArgs,
		metrics: Self::Metrics,
		receiver: mpsc::Receiver<Self::ToJob>,
		sender: JobSender<S>,
	) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>;
}

/// Error which can be returned by the jobs manager
///
/// Wraps the utility error type and the job-specific error
#[derive(Debug, Error)]
pub enum JobsError<JobError: std::fmt::Debug + std::error::Error + 'static> {
	/// utility error
	#[error("Utility")]
	Utility(#[source] Error),
	/// internal job error
	#[error("Internal")]
	Job(#[source] JobError),
}

/// Jobs manager for a subsystem
///
/// - Spawns new jobs for a given relay-parent on demand.
/// - Closes old jobs for a given relay-parent on demand.
/// - Dispatches messages to the appropriate job for a given relay-parent.
/// - When dropped, aborts all remaining jobs.
/// - implements `Stream<Item=FromJobCommand>`, collecting all messages from subordinate jobs.
#[pin_project]
struct Jobs<Spawner, ToJob> {
	spawner: Spawner,
	running: HashMap<Hash, JobHandle<ToJob>>,
	outgoing_msgs: SelectAll<mpsc::Receiver<FromJobCommand>>,
}

impl<Spawner, ToJob> Jobs<Spawner, ToJob>
where
	Spawner: SpawnNamed,
	ToJob: Send + 'static,
{
	/// Create a new Jobs manager which handles spawning appropriate jobs.
	pub fn new(spawner: Spawner) -> Self {
		Self {
			spawner,
			running: HashMap::new(),
			outgoing_msgs: SelectAll::new(),
		}
	}

	/// Spawn a new job for this `parent_hash`, with whatever args are appropriate.
	fn spawn_job<Job, Sender>(
		&mut self,
		parent_hash: Hash,
		span: Arc<jaeger::Span>,
		run_args: Job::RunArgs,
		metrics: Job::Metrics,
		sender: Sender,
	)
		where
			Job: JobTrait<ToJob = ToJob>,
			Sender: SubsystemSender,
	{
		let (to_job_tx, to_job_rx) = mpsc::channel(JOB_CHANNEL_CAPACITY);
		let (from_job_tx, from_job_rx) = mpsc::channel(JOB_CHANNEL_CAPACITY);

		let (future, abort_handle) = future::abortable(async move {
			if let Err(e) = Job::run(
				parent_hash,
				span,
				run_args,
				metrics,
				to_job_rx,
				JobSender {
					sender,
					from_job: from_job_tx,
				},
			).await {
				tracing::error!(
					job = Job::NAME,
					parent_hash = %parent_hash,
					err = ?e,
					"job finished with an error",
				);

				return Err(e);
			}

			Ok(())
		});

		self.spawner.spawn(Job::NAME, future.map(drop).boxed());
		self.outgoing_msgs.push(from_job_rx);

		let handle = JobHandle {
			_abort_handle: AbortOnDrop(abort_handle),
			to_job: to_job_tx,
		};

		self.running.insert(parent_hash, handle);
	}

	/// Stop the job associated with this `parent_hash`.
	pub async fn stop_job(&mut self, parent_hash: Hash) {
		self.running.remove(&parent_hash);
	}

	/// Send a message to the appropriate job for this `parent_hash`.
	async fn send_msg(&mut self, parent_hash: Hash, msg: ToJob) {
		if let Entry::Occupied(mut job) = self.running.entry(parent_hash) {
			if job.get_mut().send_msg(msg).await.is_err() {
				job.remove();
			}
		}
	}
}

impl<Spawner, ToJob> Stream for Jobs<Spawner, ToJob>
where
	Spawner: SpawnNamed,
{
	type Item = FromJobCommand;

	fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
		match futures::ready!(Pin::new(&mut self.outgoing_msgs).poll_next(cx)) {
			Some(msg) => Poll::Ready(Some(msg)),
			// Don't end if there are no jobs running
			None => Poll::Pending,
		}
	}
}

impl<Spawner, ToJob> stream::FusedStream for Jobs<Spawner, ToJob>
where
	Spawner: SpawnNamed,
{
	fn is_terminated(&self) -> bool {
		false
	}
}

/// Parameters to a job subsystem.
struct JobSubsystemParams<Spawner, RunArgs, Metrics> {
	/// A spawner for sub-tasks.
	spawner: Spawner,
	/// Arguments to each job.
	run_args: RunArgs,
	/// Metrics for the subsystem.
	metrics: Metrics,
}

/// A subsystem which wraps jobs.
///
/// Conceptually, this is very simple: it just loops forever.
///
/// - On incoming overseer messages, it starts or stops jobs as appropriate.
/// - On other incoming messages, if they can be converted into Job::ToJob and
///   include a hash, then they're forwarded to the appropriate individual job.
/// - On outgoing messages from the jobs, it forwards them to the overseer.
pub struct JobSubsystem<Job: JobTrait, Spawner> {
	params: JobSubsystemParams<Spawner, Job::RunArgs, Job::Metrics>,
	_marker: std::marker::PhantomData<Job>,
}

impl<Job: JobTrait, Spawner> JobSubsystem<Job, Spawner> {
	/// Create a new `JobSubsystem`.
	pub fn new(spawner: Spawner, run_args: Job::RunArgs, metrics: Job::Metrics) -> Self {
		JobSubsystem {
			params: JobSubsystemParams {
				spawner,
				run_args,
				metrics,
			},
			_marker: std::marker::PhantomData,
		}
	}

	/// Run the subsystem to completion.
	pub async fn run<Context>(self, mut ctx: Context)
		where
			Spawner: SpawnNamed + Send + Clone + Unpin + 'static,
			Context: SubsystemContext<Message=<Job as JobTrait>::ToJob, Signal=OverseerSignal>,
			<Context as SubsystemContext>::Sender: SubsystemSender,
			Job: 'static + JobTrait + Send,
			<Job as JobTrait>::RunArgs: Clone + Sync,
			<Job as JobTrait>::ToJob: Sync + From<<Context as polkadot_overseer::SubsystemContext>::Message>,
			<Job as JobTrait>::Metrics: Sync,
	{
		let JobSubsystem {
			params: JobSubsystemParams {
				spawner,
				run_args,
				metrics,
			},
			..
		} = self;

		let mut jobs = Jobs::<Spawner, Job::ToJob>::new(spawner);

		loop {
			select! {
				incoming = ctx.recv().fuse() => {
					match incoming {
						Ok(FromOverseer::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
							activated,
							deactivated,
						}))) => {
							for activated in activated {
								let sender = ctx.sender().clone();
								jobs.spawn_job::<Job, _>(
									activated.hash,
									activated.span,
									run_args.clone(),
									metrics.clone(),
									sender,
								)
							}

							for hash in deactivated {
								jobs.stop_job(hash).await;
							}
						}
						Ok(FromOverseer::Signal(OverseerSignal::Conclude)) => {
							jobs.running.clear();
							break;
						}
						Ok(FromOverseer::Signal(OverseerSignal::BlockFinalized(..))) => {}
						Ok(FromOverseer::Communication { msg }) => {
							if let Ok(to_job) = <<Context as SubsystemContext>::Message>::try_from(msg) {
								jobs.send_msg(to_job.relay_parent(), to_job).await;
							}
						}
						Err(err) => {
							tracing::error!(
								job = Job::NAME,
								err = ?err,
								"error receiving message from subsystem context for job",
							);
							break;
						}
					}
				}
				outgoing = jobs.next() => {
					// TODO verify the introduced .await here is not a problem
					// TODO it should only wait for the spawn to complete
					// TODO but not for anything beyond that
					let res = match outgoing.expect("the Jobs stream never ends; qed") {
						FromJobCommand::Spawn(name, task) => ctx.spawn(name, task),
						FromJobCommand::SpawnBlocking(name, task) => ctx.spawn_blocking(name, task),
					};

					if let Err(e) = res {
						tracing::warn!(err = ?e, "failed to handle command from job");
					}
				}
				complete => break,
			}
		}
	}
}

impl<Context, Job, Spawner> overseer::Subsystem<Context> for JobSubsystem<Job, Spawner>
where
	Spawner: SpawnNamed + Send + Clone + Unpin + 'static,
	Context: SubsystemContext<Message=Job::ToJob>,
	Job: 'static + JobTrait + Send,
	Job::RunArgs: Clone + Sync,
	<Job as JobTrait>::ToJob: Sync + From<<Context as SubsystemContext>::Message>,
	Job::Metrics: Sync,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = Box::pin(async move {
			self.run(ctx).await;
			Ok(())
		});

		SpawnedSubsystem {
			name: Job::NAME.strip_suffix("Job").unwrap_or(Job::NAME),
			future,
		}
	}
}
