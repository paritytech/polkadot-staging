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

//! Implements the Runtime API Subsystem
//!
//! This provides a clean, ownerless wrapper around the parachain-related runtime APIs. This crate
//! can also be used to cache responses from heavy runtime APIs.

#![deny(unused_crate_dependencies)]
#![warn(missing_docs)]

use polkadot_subsystem::{
	SubsystemError, SubsystemResult,
	FromOverseer, OverseerSignal, SpawnedSubsystem,
	SubsystemContext,
	errors::RuntimeApiError,
	messages::{
		RuntimeApiMessage, RuntimeApiRequest as Request,
	},
	overseer,
};
use polkadot_node_subsystem_util::metrics::{self, prometheus};
use polkadot_primitives::v1::{Block, BlockId, Hash, ParachainHost};

use sp_api::ProvideRuntimeApi;
use sp_authority_discovery::AuthorityDiscoveryApi;
use sp_core::traits::SpawnNamed;
use sp_consensus_babe::BabeApi;

use futures::{prelude::*, stream::FuturesUnordered, channel::oneshot, select};
use std::{sync::Arc, collections::VecDeque, pin::Pin};
use cache::{RequestResult, RequestResultCache};

mod cache;

#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "parachain::runtime-api";

/// The number of maximum runtime api requests can be executed in parallel. Further requests will be buffered.
const MAX_PARALLEL_REQUESTS: usize = 4;

/// The name of the blocking task that executes a runtime api request.
const API_REQUEST_TASK_NAME: &str = "polkadot-runtime-api-request";

/// The `RuntimeApiSubsystem`. See module docs for more details.
pub struct RuntimeApiSubsystem<Client> {
	client: Arc<Client>,
	metrics: Metrics,
	spawn_handle: Box<dyn SpawnNamed>,
	/// If there are [`MAX_PARALLEL_REQUESTS`] requests being executed, we buffer them in here until they can be executed.
	waiting_requests: VecDeque<(
		Pin<Box<dyn Future<Output = ()> + Send>>,
		oneshot::Receiver<Option<RequestResult>>,
	)>,
	/// All the active runtime api requests that are currently being executed.
	active_requests: FuturesUnordered<oneshot::Receiver<Option<RequestResult>>>,
	/// Requests results cache
	requests_cache: RequestResultCache,
}

impl<Client> RuntimeApiSubsystem<Client> {
	/// Create a new Runtime API subsystem wrapping the given client and metrics.
	pub fn new(client: Arc<Client>, metrics: Metrics, spawn_handle: impl SpawnNamed + 'static) -> Self {
		RuntimeApiSubsystem {
			client,
			metrics,
			spawn_handle: Box::new(spawn_handle),
			waiting_requests: Default::default(),
			active_requests: Default::default(),
			requests_cache: RequestResultCache::default(),
		}
	}
}

impl<Client, Context> overseer::overseer::Subsystem<Context> for RuntimeApiSubsystem<Client> where
	Client: ProvideRuntimeApi<Block> + Send + 'static + Sync,
	Client::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
	Context: overseer::SubsystemContext<Message = RuntimeApiMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		SpawnedSubsystem {
			future: run(ctx, self).boxed(),
			name: "runtime-api-subsystem",
		}
	}
}

impl<Client> RuntimeApiSubsystem<Client> where
	Client: ProvideRuntimeApi<Block> + Send + 'static + Sync,
	Client::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
{
	fn store_cache(&mut self, result: RequestResult) {
		use RequestResult::*;

		match result {
			Authorities(relay_parent, authorities) =>
				self.requests_cache.cache_authorities(relay_parent, authorities),
			Validators(relay_parent, validators) =>
				self.requests_cache.cache_validators(relay_parent, validators),
			ValidatorGroups(relay_parent, groups) =>
				self.requests_cache.cache_validator_groups(relay_parent, groups),
			AvailabilityCores(relay_parent, cores) =>
				self.requests_cache.cache_availability_cores(relay_parent, cores),
			PersistedValidationData(relay_parent, para_id, assumption, data) =>
				self.requests_cache.cache_persisted_validation_data((relay_parent, para_id, assumption), data),
			CheckValidationOutputs(relay_parent, para_id, commitments, b) =>
				self.requests_cache.cache_check_validation_outputs((relay_parent, para_id, commitments), b),
			SessionIndexForChild(relay_parent, session_index) =>
				self.requests_cache.cache_session_index_for_child(relay_parent, session_index),
			ValidationCode(relay_parent, para_id, assumption, code) =>
				self.requests_cache.cache_validation_code((relay_parent, para_id, assumption), code),
			ValidationCodeByHash(relay_parent, validation_code_hash, code) =>
				self.requests_cache.cache_validation_code_by_hash((relay_parent, validation_code_hash), code),
			CandidatePendingAvailability(relay_parent, para_id, candidate) =>
				self.requests_cache.cache_candidate_pending_availability((relay_parent, para_id), candidate),
			CandidateEvents(relay_parent, events) =>
				self.requests_cache.cache_candidate_events(relay_parent, events),
			SessionInfo(relay_parent, session_index, info) =>
				self.requests_cache.cache_session_info((relay_parent, session_index), info),
			DmqContents(relay_parent, para_id, messages) =>
				self.requests_cache.cache_dmq_contents((relay_parent, para_id), messages),
			InboundHrmpChannelsContents(relay_parent, para_id, contents) =>
				self.requests_cache.cache_inbound_hrmp_channel_contents((relay_parent, para_id), contents),
			CurrentBabeEpoch(relay_parent, epoch) =>
				self.requests_cache.cache_current_babe_epoch(relay_parent, epoch),
		}
	}

	fn query_cache(&mut self, relay_parent: Hash, request: Request) -> Option<Request> {
		macro_rules! query {
			// Just query by relay parent
			($cache_api_name:ident (), $sender:expr) => {{
				let sender = $sender;
				if let Some(value) = self.requests_cache.$cache_api_name(&relay_parent) {
					let _ = sender.send(Ok(value.clone()));
					self.metrics.on_cached_request();
					None
				} else {
					Some(sender)
				}
			}};
			// Query by relay parent + additional parameters
			($cache_api_name:ident ($($param:expr),+), $sender:expr) => {{
				let sender = $sender;
				if let Some(value) = self.requests_cache.$cache_api_name((relay_parent.clone(), $($param.clone()),+)) {
					self.metrics.on_cached_request();
					let _ = sender.send(Ok(value.clone()));
					None
				} else {
					Some(sender)
				}
			}}
		}

		match request {
			Request::Authorities(sender) => query!(authorities(), sender)
				.map(|sender| Request::Authorities(sender)),
			Request::Validators(sender) => query!(validators(), sender)
				.map(|sender| Request::Validators(sender)),
			Request::ValidatorGroups(sender) => query!(validator_groups(), sender)
				.map(|sender| Request::ValidatorGroups(sender)),
			Request::AvailabilityCores(sender) => query!(availability_cores(), sender)
				.map(|sender| Request::AvailabilityCores(sender)),
			Request::PersistedValidationData(para, assumption, sender) =>
				query!(persisted_validation_data(para, assumption), sender)
					.map(|sender| Request::PersistedValidationData(para, assumption, sender)),
			Request::CheckValidationOutputs(para, commitments, sender) =>
				query!(check_validation_outputs(para, commitments), sender)
					.map(|sender| Request::CheckValidationOutputs(para, commitments, sender)),
			Request::SessionIndexForChild(sender) =>
				query!(session_index_for_child(), sender)
					.map(|sender| Request::SessionIndexForChild(sender)),
			Request::ValidationCode(para, assumption, sender) =>
				query!(validation_code(para, assumption), sender)
					.map(|sender| Request::ValidationCode(para, assumption, sender)),
			Request::ValidationCodeByHash(validation_code_hash, sender) =>
				query!(validation_code_by_hash(validation_code_hash), sender)
					.map(|sender| Request::ValidationCodeByHash(validation_code_hash, sender)),
			Request::CandidatePendingAvailability(para, sender) =>
				query!(candidate_pending_availability(para), sender)
					.map(|sender| Request::CandidatePendingAvailability(para, sender)),
			Request::CandidateEvents(sender) => query!(candidate_events(), sender)
				.map(|sender| Request::CandidateEvents(sender)),
			Request::SessionInfo(index, sender) => query!(session_info(index), sender)
				.map(|sender| Request::SessionInfo(index, sender)),
			Request::DmqContents(id, sender) => query!(dmq_contents(id), sender)
				.map(|sender| Request::DmqContents(id, sender)),
			Request::InboundHrmpChannelsContents(id, sender) =>
				query!(inbound_hrmp_channels_contents(id), sender)
					.map(|sender| Request::InboundHrmpChannelsContents(id, sender)),
			Request::CurrentBabeEpoch(sender) =>
				query!(current_babe_epoch(), sender)
					.map(|sender| Request::CurrentBabeEpoch(sender)),
		}
	}

	/// Spawn a runtime api request.
	///
	/// If there are already [`MAX_PARALLEL_REQUESTS`] requests being executed, the request will be buffered.
	fn spawn_request(&mut self, relay_parent: Hash, request: Request) {
		let client = self.client.clone();
		let metrics = self.metrics.clone();
		let (sender, receiver) = oneshot::channel();

		let request = match self.query_cache(relay_parent.clone(), request) {
			Some(request) => request,
			None => return,
		};

		let request = async move {
			let result = make_runtime_api_request(
				client,
				metrics,
				relay_parent,
				request,
			);
			let _ = sender.send(result);
		}.boxed();

		if self.active_requests.len() >= MAX_PARALLEL_REQUESTS {
			self.waiting_requests.push_back((request, receiver));

			if self.waiting_requests.len() > MAX_PARALLEL_REQUESTS * 10 {
				tracing::warn!(
					target: LOG_TARGET,
					"{} runtime api requests waiting to be executed.",
					self.waiting_requests.len(),
				)
			}
		} else {
			self.spawn_handle.spawn_blocking(API_REQUEST_TASK_NAME, request);
			self.active_requests.push(receiver);
		}
	}

	/// Poll the active runtime api requests.
	async fn poll_requests(&mut self) {
		// If there are no active requests, this future should be pending forever.
		if self.active_requests.len() == 0 {
			return futures::pending!()
		}

		// If there are active requests, this will always resolve to `Some(_)` when a request is finished.
		if let Some(Ok(Some(result))) = self.active_requests.next().await {
			self.store_cache(result);
		}

		if let Some((req, recv)) = self.waiting_requests.pop_front() {
			self.spawn_handle.spawn_blocking(API_REQUEST_TASK_NAME, req);
			self.active_requests.push(recv);
		}
	}
}

async fn run<Client, Context>(
	mut ctx: Context,
	mut subsystem: RuntimeApiSubsystem<Client>,
) -> SubsystemResult<()> where
	Client: ProvideRuntimeApi<Block> + Send + Sync + 'static,
	Client::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
	Context: overseer::SubsystemContext<Message = RuntimeApiMessage>,
{
	loop {
		select! {
			req = ctx.recv().fuse() => match req? {
				FromOverseer::Signal(OverseerSignal::Conclude) => return Ok(()),
				FromOverseer::Signal(OverseerSignal::ActiveLeaves(_)) => {},
				FromOverseer::Signal(OverseerSignal::BlockFinalized(..)) => {},
				FromOverseer::Communication { msg } => match msg {
					RuntimeApiMessage::Request(relay_parent, request) => {
						subsystem.spawn_request(relay_parent, request);
					},
				}
			},
			_ = subsystem.poll_requests().fuse() => {},
		}
	}
}

fn make_runtime_api_request<Client>(
	client: Arc<Client>,
	metrics: Metrics,
	relay_parent: Hash,
	request: Request,
) -> Option<RequestResult>
where
	Client: ProvideRuntimeApi<Block>,
	Client::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
{
	let _timer = metrics.time_make_runtime_api_request();

	macro_rules! query {
		($req_variant:ident, $api_name:ident ($($param:expr),*), $sender:expr) => {{
			let sender = $sender;
			let api = client.runtime_api();
			let res = api.$api_name(&BlockId::Hash(relay_parent) $(, $param.clone() )*)
				.map_err(|e| RuntimeApiError::from(format!("{:?}", e)));
			metrics.on_request(res.is_ok());
			let _ = sender.send(res.clone());

			res.ok().map(|res| RequestResult::$req_variant(relay_parent, $( $param, )* res))
		}}
	}

	match request {
		Request::Authorities(sender) => query!(Authorities, authorities(), sender),
		Request::Validators(sender) => query!(Validators, validators(), sender),
		Request::ValidatorGroups(sender) => query!(ValidatorGroups, validator_groups(), sender),
		Request::AvailabilityCores(sender) => query!(AvailabilityCores, availability_cores(), sender),
		Request::PersistedValidationData(para, assumption, sender) =>
			query!(PersistedValidationData, persisted_validation_data(para, assumption), sender),
		Request::CheckValidationOutputs(para, commitments, sender) =>
			query!(CheckValidationOutputs, check_validation_outputs(para, commitments), sender),
		Request::SessionIndexForChild(sender) => query!(SessionIndexForChild, session_index_for_child(), sender),
		Request::ValidationCode(para, assumption, sender) =>
			query!(ValidationCode, validation_code(para, assumption), sender),
		Request::ValidationCodeByHash(validation_code_hash, sender) =>
			query!(ValidationCodeByHash, validation_code_by_hash(validation_code_hash), sender),
		Request::CandidatePendingAvailability(para, sender) =>
			query!(CandidatePendingAvailability, candidate_pending_availability(para), sender),
		Request::CandidateEvents(sender) => query!(CandidateEvents, candidate_events(), sender),
		Request::SessionInfo(index, sender) => query!(SessionInfo, session_info(index), sender),
		Request::DmqContents(id, sender) => query!(DmqContents, dmq_contents(id), sender),
		Request::InboundHrmpChannelsContents(id, sender) => query!(InboundHrmpChannelsContents, inbound_hrmp_channels_contents(id), sender),
		Request::CurrentBabeEpoch(sender) => query!(CurrentBabeEpoch, current_epoch(), sender),
	}
}

#[derive(Clone)]
struct MetricsInner {
	chain_api_requests: prometheus::CounterVec<prometheus::U64>,
	make_runtime_api_request: prometheus::Histogram,
}

/// Runtime API metrics.
#[derive(Default, Clone)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_request(&self, succeeded: bool) {
		if let Some(metrics) = &self.0 {
			if succeeded {
				metrics.chain_api_requests.with_label_values(&["succeeded"]).inc();
			} else {
				metrics.chain_api_requests.with_label_values(&["failed"]).inc();
			}
		}
	}

	fn on_cached_request(&self) {
		self.0.as_ref()
			.map(|metrics| metrics.chain_api_requests.with_label_values(&["cached"]).inc());
	}

	/// Provide a timer for `make_runtime_api_request` which observes on drop.
	fn time_make_runtime_api_request(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.make_runtime_api_request.start_timer())
	}
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry) -> Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			chain_api_requests: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"parachain_runtime_api_requests_total",
						"Number of Runtime API requests served.",
					),
					&["success"],
				)?,
				registry,
			)?,
			make_runtime_api_request: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_runtime_api_make_runtime_api_request",
						"Time spent within `runtime_api::make_runtime_api_request`",
					)
				)?,
				registry,
			)?,
		};
		Ok(Metrics(Some(metrics)))
	}
}
