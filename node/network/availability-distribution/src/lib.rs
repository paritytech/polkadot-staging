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

use futures::{future::Either, FutureExt, StreamExt, TryFutureExt};

use sp_keystore::SyncCryptoStorePtr;

use polkadot_subsystem::{
	messages::AvailabilityDistributionMessage, FromOverseer, OverseerSignal, SpawnedSubsystem,
	SubsystemContext, SubsystemError,
	overseer,
};

/// Error and [`Result`] type for this subsystem.
mod error;
use error::Fatal;
use error::{Result, log_error};

use polkadot_node_subsystem_util::runtime::RuntimeInfo;

/// `Requester` taking care of requesting chunks for candidates pending availability.
mod requester;
use requester::Requester;

/// Handing requests for PoVs during backing.
mod pov_requester;

/// Responding to erasure chunk requests:
mod responder;
use responder::{answer_chunk_request_log, answer_pov_request_log};

mod metrics;
/// Prometheus `Metrics` for availability distribution.
pub use metrics::Metrics;

#[cfg(test)]
mod tests;

const LOG_TARGET: &'static str = "parachain::availability-distribution";

/// The availability distribution subsystem.
pub struct AvailabilityDistributionSubsystem {
	/// Easy and efficient runtime access for this subsystem.
	runtime: RuntimeInfo,
	/// Prometheus metrics.
	metrics: Metrics,
}

impl<Context> overseer::overseer::Subsystem<Context> for AvailabilityDistributionSubsystem
where
	Context: overseer::SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = self
			.run(ctx)
			.map_err(|e| SubsystemError::with_origin("availability-distribution", e))
			.boxed();

		SpawnedSubsystem {
			name: "availability-distribution-subsystem",
			future,
		}
	}
}

impl AvailabilityDistributionSubsystem {

	/// Create a new instance of the availability distribution.
	pub fn new(keystore: SyncCryptoStorePtr, metrics: Metrics) -> Self {
		let runtime = RuntimeInfo::new(Some(keystore));
		Self { runtime,  metrics }
	}

	/// Start processing work as passed on from the Overseer.
	async fn run<Context>(mut self, mut ctx: Context) -> std::result::Result<(), Fatal>
	where
		Context: overseer::SubsystemContext<Message = AvailabilityDistributionMessage>,
	{
		let mut requester = Requester::new(self.metrics.clone()).fuse();
		loop {
			let action = {
				let mut subsystem_next = ctx.recv().fuse();
				futures::select! {
					subsystem_msg = subsystem_next => Either::Left(subsystem_msg),
					from_task = requester.next() => Either::Right(from_task),
				}
			};

			// Handle task messages sending:
			let message = match action {
				Either::Left(subsystem_msg) => {
					subsystem_msg.map_err(|e| Fatal::IncomingMessageChannel(e))?
				}
				Either::Right(from_task) => {
					let from_task = from_task.ok_or(Fatal::RequesterExhausted)?;
					ctx.send_message(from_task).await;
					continue;
				}
			};
			match message {
				FromOverseer::Signal(OverseerSignal::ActiveLeaves(update)) => {
					log_error(
						requester.get_mut().update_fetching_heads(&mut ctx, &mut self.runtime, update).await,
						"Error in Requester::update_fetching_heads"
					)?;
				}
				FromOverseer::Signal(OverseerSignal::BlockFinalized(..)) => {}
				FromOverseer::Signal(OverseerSignal::Conclude) => {
					return Ok(());
				}
				FromOverseer::Communication {
					msg: AvailabilityDistributionMessage::ChunkFetchingRequest(req),
				} => {
					answer_chunk_request_log(&mut ctx, req, &self.metrics).await
				}
				FromOverseer::Communication {
					msg: AvailabilityDistributionMessage::PoVFetchingRequest(req),
				} => {
					answer_pov_request_log(&mut ctx, req, &self.metrics).await
				}
				FromOverseer::Communication {
					msg: AvailabilityDistributionMessage::FetchPoV {
						relay_parent,
						from_validator,
						candidate_hash,
						pov_hash,
						tx,
					},
				} => {
					log_error(
						pov_requester::fetch_pov(
							&mut ctx,
							&mut self.runtime,
							relay_parent,
							from_validator,
							candidate_hash,
							pov_hash,
							tx,
						).await,
						"pov_requester::fetch_pov"
					)?;
				}
			}
		}
	}
}
