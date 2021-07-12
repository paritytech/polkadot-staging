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

//! Implements the Chain API Subsystem
//!
//! Provides access to the chain data. Every request may return an error.
//! At the moment, the implementation requires `Client` to implement `HeaderBackend`,
//! we may add more bounds in the future if we will need e.g. block bodies.
//!
//! Supported requests:
//! * Block hash to number
//! * Block hash to header
//! * Block weight (cumulative)
//! * Finalized block number to hash
//! * Last finalized block number
//! * Ancestors

#![deny(unused_crate_dependencies, unused_results)]
#![warn(missing_docs)]

use std::sync::Arc;

use futures::prelude::*;
use sc_client_api::AuxStore;
use sp_blockchain::HeaderBackend;

use polkadot_node_subsystem_util::metrics::{self, prometheus};
use polkadot_primitives::v1::{Block, BlockId};
use polkadot_subsystem::{
	overseer,
	messages::ChainApiMessage,
	FromOverseer, OverseerSignal, SpawnedSubsystem,
	SubsystemContext, SubsystemError, SubsystemResult,
};

#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "parachain::chain-api";

/// The Chain API Subsystem implementation.
pub struct ChainApiSubsystem<Client> {
	client: Arc<Client>,
	metrics: Metrics,
}

impl<Client> ChainApiSubsystem<Client> {
	/// Create a new Chain API subsystem with the given client.
	pub fn new(client: Arc<Client>, metrics: Metrics) -> Self {
		ChainApiSubsystem {
			client,
			metrics,
		}
	}
}

impl<Client, Context> overseer::overseer::Subsystem<Context> for ChainApiSubsystem<Client>
where
	Client: HeaderBackend<Block> + AuxStore + 'static,
	Context: overseer::SubsystemContext<Message = ChainApiMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = run::<Client, Context>(ctx, self)
			.map_err(|e| SubsystemError::with_origin("chain-api", e))
			.boxed();
		SpawnedSubsystem {
			future,
			name: "chain-api-subsystem",
		}
	}
}

async fn run<Client, Context>(
	mut ctx: Context,
	subsystem: ChainApiSubsystem<Client>,
) -> SubsystemResult<()>
where
	Client: HeaderBackend<Block> + AuxStore,
	Context: overseer::SubsystemContext<Message = ChainApiMessage>,
{
	loop {
		match ctx.recv().await? {
			FromOverseer::Signal(OverseerSignal::Conclude) => return Ok(()),
			FromOverseer::Signal(OverseerSignal::ActiveLeaves(_)) => {},
			FromOverseer::Signal(OverseerSignal::BlockFinalized(..)) => {},
			FromOverseer::Communication { msg } => match msg {
				ChainApiMessage::BlockNumber(hash, response_channel) => {
					let _timer = subsystem.metrics.time_block_number();
					let result = subsystem.client.number(hash).map_err(|e| e.to_string().into());
					subsystem.metrics.on_request(result.is_ok());
					let _ = response_channel.send(result);
				},
				ChainApiMessage::BlockHeader(hash, response_channel) => {
					let _timer = subsystem.metrics.time_block_header();
					let result = subsystem.client
						.header(BlockId::Hash(hash))
						.map_err(|e| e.to_string().into());
					subsystem.metrics.on_request(result.is_ok());
					let _ = response_channel.send(result);
				},
				ChainApiMessage::BlockWeight(hash, response_channel) => {
					let _timer = subsystem.metrics.time_block_weight();
					let result = sc_consensus_babe::block_weight(&*subsystem.client, hash)
						.map_err(|e| e.to_string().into());
					subsystem.metrics.on_request(result.is_ok());
					let _ = response_channel.send(result);
				}
				ChainApiMessage::FinalizedBlockHash(number, response_channel) => {
					let _timer = subsystem.metrics.time_finalized_block_hash();
					// Note: we don't verify it's finalized
					let result = subsystem.client.hash(number).map_err(|e| e.to_string().into());
					subsystem.metrics.on_request(result.is_ok());
					let _ = response_channel.send(result);
				},
				ChainApiMessage::FinalizedBlockNumber(response_channel) => {
					let _timer = subsystem.metrics.time_finalized_block_number();
					let result = subsystem.client.info().finalized_number;
					// always succeeds
					subsystem.metrics.on_request(true);
					let _ = response_channel.send(Ok(result));
				},
				ChainApiMessage::Ancestors { hash, k, response_channel } => {
					let _timer = subsystem.metrics.time_ancestors();
					tracing::span!(tracing::Level::TRACE, "ChainApiMessage::Ancestors", subsystem=LOG_TARGET, hash=%hash, k=k);

					let mut hash = hash;

					let next_parent = core::iter::from_fn(|| {
						let maybe_header = subsystem.client.header(BlockId::Hash(hash));
						match maybe_header {
							// propagate the error
							Err(e) => {
								let e = e.to_string().into();
								Some(Err(e))
							},
							// fewer than `k` ancestors are available
							Ok(None) => None,
							Ok(Some(header)) => {
								// stop at the genesis header.
								if header.number == 1 {
									None
								} else {
									hash = header.parent_hash;
									Some(Ok(hash))
								}
							}
						}
					});

					let result = next_parent.take(k).collect::<Result<Vec<_>, _>>();
					subsystem.metrics.on_request(result.is_ok());
					let _ = response_channel.send(result);
				},
			}
		}
	}
}

#[derive(Clone)]
struct MetricsInner {
	chain_api_requests: prometheus::CounterVec<prometheus::U64>,
	block_number: prometheus::Histogram,
	block_header: prometheus::Histogram,
	block_weight: prometheus::Histogram,
	finalized_block_hash: prometheus::Histogram,
	finalized_block_number: prometheus::Histogram,
	ancestors: prometheus::Histogram,
}

/// Chain API metrics.
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

	/// Provide a timer for `block_number` which observes on drop.
	fn time_block_number(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.block_number.start_timer())
	}

	/// Provide a timer for `block_header` which observes on drop.
	fn time_block_header(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.block_header.start_timer())
	}

	/// Provide a timer for `block_weight` which observes on drop.
	fn time_block_weight(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.block_weight.start_timer())
	}

	/// Provide a timer for `finalized_block_hash` which observes on drop.
	fn time_finalized_block_hash(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.finalized_block_hash.start_timer())
	}

	/// Provide a timer for `finalized_block_number` which observes on drop.
	fn time_finalized_block_number(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.finalized_block_number.start_timer())
	}

	/// Provide a timer for `ancestors` which observes on drop.
	fn time_ancestors(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.ancestors.start_timer())
	}
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry) -> Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			chain_api_requests: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"parachain_chain_api_requests_total",
						"Number of Chain API requests served.",
					),
					&["success"],
				)?,
				registry,
			)?,
			block_number: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_chain_api_block_number",
						"Time spent within `chain_api::block_number`",
					)
				)?,
				registry,
			)?,
			block_header: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_chain_api_block_headers",
						"Time spent within `chain_api::block_headers`",
					)
				)?,
				registry,
			)?,
			block_weight: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_chain_api_block_weight",
						"Time spent within `chain_api::block_weight`",
					)
				)?,
				registry,
			)?,
			finalized_block_hash: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_chain_api_finalized_block_hash",
						"Time spent within `chain_api::finalized_block_hash`",
					)
				)?,
				registry,
			)?,
			finalized_block_number: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_chain_api_finalized_block_number",
						"Time spent within `chain_api::finalized_block_number`",
					)
				)?,
				registry,
			)?,
			ancestors: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_chain_api_ancestors",
						"Time spent within `chain_api::ancestors`",
					)
				)?,
				registry,
			)?,
		};
		Ok(Metrics(Some(metrics)))
	}
}
