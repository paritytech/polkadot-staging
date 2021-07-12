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

use quote::quote;
use syn::Ident;

use super::*;

/// Implement a builder pattern for the `Overseer`-type,
/// which acts as the gateway to constructing the overseer.
pub(crate) fn impl_trait(info: &OverseerInfo) -> proc_macro2::TokenStream {
	let overseer_name = info.overseer_name.clone();
	let subsystem_sender_name = Ident::new(&(overseer_name.to_string() + "SubsystemSender"), overseer_name.span());
	let subsystem_ctx_name = Ident::new(&(overseer_name.to_string() + "SubsystemContext"), overseer_name.span());
	let consumes = &info.consumes();
	let signal = &info.extern_signal_ty;
	let wrapper_message = &info.message_wrapper;
	let error = &info.extern_error_ty;
	let support_crate = info.support_crate_name();

	let ts = quote! {
		/// Connector to send messages towards all subsystems,
		/// while tracking the which signals where already received.
		#[derive(Debug, Clone)]
		pub struct #subsystem_sender_name {
			/// Collection of channels to all subsystems.
			channels: ChannelsOut,
			/// Systemwide tick for which signals were received by all subsystems.
			signals_received: SignalsReceived,
		}

		/// impl for wrapping message type...
		#[#support_crate ::async_trait]
		impl #support_crate ::SubsystemSender< #wrapper_message > for #subsystem_sender_name {
			async fn send_message(&mut self, msg: #wrapper_message) {
				self.channels.send_and_log_error(self.signals_received.load(), msg).await;
			}

			async fn send_messages<T>(&mut self, msgs: T)
			where
				T: IntoIterator<Item = #wrapper_message> + Send,
				T::IntoIter: Send,
			{
				// This can definitely be optimized if necessary.
				for msg in msgs {
					self.send_message(msg).await;
				}
			}

			fn send_unbounded_message(&mut self, msg: #wrapper_message) {
				self.channels.send_unbounded_and_log_error(self.signals_received.load(), msg);
			}
		}

		// ... but also implement for all individual messages to avoid
		// the necessity for manual wrapping, and do the conversion
		// based on the generated `From::from` impl for the individual variants.
		#(
		#[#support_crate ::async_trait]
		impl #support_crate ::SubsystemSender< #consumes > for #subsystem_sender_name {
			async fn send_message(&mut self, msg: #consumes) {
				self.channels.send_and_log_error(self.signals_received.load(), #wrapper_message ::from ( msg )).await;
			}

			async fn send_messages<T>(&mut self, msgs: T)
			where
				T: IntoIterator<Item = #consumes> + Send,
				T::IntoIter: Send,
			{
				// This can definitely be optimized if necessary.
				for msg in msgs {
					self.send_message(msg).await;
				}
			}

			fn send_unbounded_message(&mut self, msg: #consumes) {
				self.channels.send_unbounded_and_log_error(self.signals_received.load(), #wrapper_message ::from ( msg ));
			}
		}
		)*

		/// A context type that is given to the [`Subsystem`] upon spawning.
		/// It can be used by [`Subsystem`] to communicate with other [`Subsystem`]s
		/// or to spawn it's [`SubsystemJob`]s.
		///
		/// [`Overseer`]: struct.Overseer.html
		/// [`Subsystem`]: trait.Subsystem.html
		/// [`SubsystemJob`]: trait.SubsystemJob.html
		#[derive(Debug)]
		#[allow(missing_docs)]
		pub struct #subsystem_ctx_name<M>{
			signals: #support_crate ::metered::MeteredReceiver< #signal >,
			messages: SubsystemIncomingMessages<M>,
			to_subsystems: #subsystem_sender_name,
			to_overseer: #support_crate ::metered::UnboundedMeteredSender<
				#support_crate ::ToOverseer
				>,
			signals_received: SignalsReceived,
			pending_incoming: Option<(usize, M)>,
		}

		impl<M> #subsystem_ctx_name<M> {
			/// Create a new context.
			fn new(
				signals: #support_crate ::metered::MeteredReceiver< #signal >,
				messages: SubsystemIncomingMessages<M>,
				to_subsystems: ChannelsOut,
				to_overseer: #support_crate ::metered::UnboundedMeteredSender<ToOverseer>,
			) -> Self {
				let signals_received = SignalsReceived::default();
				#subsystem_ctx_name {
					signals,
					messages,
					to_subsystems: #subsystem_sender_name {
						channels: to_subsystems,
						signals_received: signals_received.clone(),
					},
					to_overseer,
					signals_received,
					pending_incoming: None,
				}
			}
		}

		#[#support_crate ::async_trait]
		impl<M: std::fmt::Debug + Send + 'static> SubsystemContext for #subsystem_ctx_name<M>
		where
			#subsystem_sender_name: #support_crate:: SubsystemSender< #wrapper_message >,
			#wrapper_message: From<M>,
		{
			type Message = M;
			type Sender = #subsystem_sender_name;

			async fn try_recv(&mut self) -> ::std::result::Result<Option<#support_crate ::FromOverseer<M, #signal>>, ()> {
				match #support_crate ::poll!(self.recv()) {
					#support_crate ::Poll::Ready(msg) => Ok(Some(msg.map_err(|_| ())?)),
					#support_crate ::Poll::Pending => Ok(None),
				}
			}

			async fn recv(&mut self) -> ::std::result::Result<#support_crate ::FromOverseer<M, #signal>, #error> {
				loop {
					// If we have a message pending an overseer signal, we only poll for signals
					// in the meantime.
					if let Some((needs_signals_received, msg)) = self.pending_incoming.take() {
						if needs_signals_received <= self.signals_received.load() {
							return Ok(#support_crate ::FromOverseer::Communication { msg });
						} else {
							self.pending_incoming = Some((needs_signals_received, msg));

							// wait for next signal.
							let signal = self.signals.next().await
								.ok_or(#support_crate ::OverseerError::Context(
									"Signal channel is terminated and empty."
									.to_owned()
								))?;

							self.signals_received.inc();
							return Ok(#support_crate ::FromOverseer::Signal(signal))
						}
					}

					let mut await_message = self.messages.next().fuse();
					let mut await_signal = self.signals.next().fuse();
					let signals_received = self.signals_received.load();
					let pending_incoming = &mut self.pending_incoming;

					// Otherwise, wait for the next signal or incoming message.
					let from_overseer = #support_crate ::futures::select_biased! {
						signal = await_signal => {
							let signal = signal
								.ok_or(#support_crate ::OverseerError::Context(
									"Signal channel is terminated and empty."
									.to_owned()
								))?;

							#support_crate ::FromOverseer::Signal(signal)
						}
						msg = await_message => {
							let packet = msg
								.ok_or(#support_crate ::OverseerError::Context(
									"Message channel is terminated and empty."
									.to_owned()
								))?;

							if packet.signals_received > signals_received {
								// wait until we've received enough signals to return this message.
								*pending_incoming = Some((packet.signals_received, packet.message));
								continue;
							} else {
								// we know enough to return this message.
								#support_crate ::FromOverseer::Communication { msg: packet.message}
							}
						}
					};

					if let #support_crate ::FromOverseer::Signal(_) = from_overseer {
						self.signals_received.inc();
					}

					return Ok(from_overseer);
				}
			}

			fn sender(&mut self) -> &mut Self::Sender {
				&mut self.to_subsystems
			}

			fn spawn(&mut self, name: &'static str, s: Pin<Box<dyn Future<Output = ()> + Send>>)
				-> ::std::result::Result<(), #error>
			{
				self.to_overseer.unbounded_send(#support_crate ::ToOverseer::SpawnJob {
					name,
					s,
				}).map_err(|_| #support_crate ::OverseerError::TaskSpawn(name))?;
				Ok(())
			}

			fn spawn_blocking(&mut self, name: &'static str, s: Pin<Box<dyn Future<Output = ()> + Send>>)
				-> ::std::result::Result<(), #error>
			{
				self.to_overseer.unbounded_send(#support_crate ::ToOverseer::SpawnBlockingJob {
					name,
					s,
				}).map_err(|_| #support_crate ::OverseerError::TaskSpawn(name))?;
				Ok(())
			}
		}
	};

	ts
}
