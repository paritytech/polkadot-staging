// Copyright 2017-2021 Parity Technologies (UK) Ltd.
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

//! A small set of wrapping types to cover most of our adversary test cases.
//!
//! This allows types with internal mutability to synchronize across
//! multiple subsystems and intercept or replace incoming and outgoing
//! messages on the overseer level.

use polkadot_node_subsystem::*;
pub use polkadot_node_subsystem::{overseer, messages::AllMessages, FromOverseer};
use std::future::Future;
use std::pin::Pin;

/// Filter incoming and outgoing messages.
pub trait MsgFilter: Send + Sync + Clone + 'static {
	/// The message type the original subsystm handles incoming.
	type Message: Send + 'static;

	/// Filter messages that are to be received by
	/// the subsystem.
	fn filter_in(&self, msg: FromOverseer<Self::Message>) -> Option<FromOverseer<Self::Message>> {
		Some(msg)
	}

	/// Modify outgoing messages.
	fn filter_out(&self, msg: AllMessages) -> Option<AllMessages> {
		Some(msg)
	}
}

/// A sender with the outgoing messages filtered.
#[derive(Clone)]
pub struct FilteredSender<Sender, Fil> {
	inner: Sender,
	message_filter: Fil,
}

#[async_trait::async_trait]
impl<Sender, Fil> overseer::SubsystemSenderAllMessages> for FilteredSender<Sender, Fil>
where
	Sender: overseer::SubsystemSender<AllMessages>,
	Fil: MsgFilter,
{
	async fn send_message(&mut self, msg: AllMessages) {
		if let Some(msg) = self.message_filter.filter_out(msg) {
			self.inner.send_message(msg).await;
		}
	}

	async fn send_messages<T>(&mut self, msgs: T)
	where
		T: IntoIterator<Item = AllMessages> + Send,
		T::IntoIter: Send,
	{
		for msg in msgs {
			self.send_message(msg).await;
		}
	}

	fn send_unbounded_message(&mut self, msg: AllMessages) {
		if let Some(msg) = self.message_filter.filter_out(msg) {
			self.inner.send_unbounded_message(msg);
		}
	}
}

/// A subsystem context, that filters the outgoing messages.
pub struct FilteredContext<Context: overseer::SubsystemContext + SubsystemContext, Fil: MsgFilter> {
	inner: Context,
	message_filter: Fil,
	sender: FilteredSender<<Context as overseer::SubsystemContext>::Sender, Fil>,
}

impl<Context, Fil> FilteredContext<Context, Fil>
where
	Context: overseer::SubsystemContext + SubsystemContext,
	Fil: MsgFilter<Message = <Context as overseer::SubsystemContext>::Message>,
{
	pub fn new(mut inner: Context, message_filter: Fil) -> Self {
		let sender = FilteredSender::<<Context as overseer::SubsystemContext>::Sender, Fil> {
			inner: inner.sender().clone(),
			message_filter: message_filter.clone(),
		};
		Self {
			inner,
			message_filter,
			sender,
		}
	}
}

#[async_trait::async_trait]
impl<Context, Fil> overseer::SubsystemContext for FilteredContext<Context, Fil>
where
	Context: overseer::SubsystemContext + SubsystemContext,
	Fil: MsgFilter<Message = <Context as overseer::SubsystemContext>::Message>,
	<Context as overseer::SubsystemContext>::AllMessages: From<<Context as overseer::SubsystemContext>::Message>,
{
	type Message = <Context as overseer::SubsystemContext>::Message;
	type Sender = FilteredSender<<Context as overseer::SubsystemContext>::Sender, Fil>;
	type Error = <Context as overseer::SubsystemContext>::Error;
	type AllMessages = <Context as overseer::SubsystemContext>::AllMessages;
	type Signal = <Context as overseer::SubsystemContext>::Signal;

	async fn try_recv(&mut self) -> Result<Option<FromOverseer<Self::Message>>, ()> {
		loop {
			match self.inner.try_recv().await? {
				None => return Ok(None),
				Some(msg) => {
					if let Some(msg) = self.message_filter.filter_in(msg) {
						return Ok(Some(msg));
					}
				}
			}
		}
	}

	async fn recv(&mut self) -> SubsystemResult<FromOverseer<Self::Message>> {
		loop {
			let msg = self.inner.recv().await?;
			if let Some(msg) = self.message_filter.filter_in(msg) {
				return Ok(msg);
			}
		}
	}

	fn spawn(
		&mut self,
		name: &'static str,
		s: Pin<Box<dyn Future<Output = ()> + Send>>,
	) -> SubsystemResult<()> {
		self.inner.spawn(name, s)
	}

	fn spawn_blocking(
		&mut self,
		name: &'static str,
		s: Pin<Box<dyn Future<Output = ()> + Send>>,
	) -> SubsystemResult<()> {
		self.inner.spawn_blocking(name, s)
	}

	fn sender(&mut self) -> &mut Self::Sender {
		&mut self.sender
	}
}

/// A subsystem to which incoming and outgoing filters are applied.
pub struct FilteredSubsystem<Sub, Fil> {
	subsystem: Sub,
	message_filter: Fil,
}

impl<Sub, Fil> FilteredSubsystem<Sub, Fil> {
	pub fn new(subsystem: Sub, message_filter: Fil) -> Self {
		Self {
			subsystem,
			message_filter,
		}
	}
}

impl<Context, Sub, Fil> overseer::overseer::Subsystem<Context> for FilteredSubsystem<Sub, Fil>
where
	Context: overseer::SubsystemContext + SubsystemContext + Sync + Send,
	Sub: overseer::Subsystem<FilteredContext<Context, Fil>, SubsystemError>,
	FilteredContext<Context, Fil>: overseer::SubsystemContext + SubsystemContext,
	Fil: MsgFilter<Message = <Context as overseer::SubsystemContext>::Message>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let ctx = FilteredContext::new(ctx, self.message_filter);
		overseer::Subsystem::<FilteredContext<Context, Fil>, SubsystemError>::start(self.subsystem, ctx)
	}
}
