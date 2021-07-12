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
///
/// Elements tagged with `wip` are not covered here.
pub(crate) fn impl_builder(info: &OverseerInfo) -> proc_macro2::TokenStream {
	let overseer_name = info.overseer_name.clone();
	let builder = Ident::new(&(overseer_name.to_string() + "Builder"), overseer_name.span());
	let handle = Ident::new(&(overseer_name.to_string() + "Handle"), overseer_name.span());

	let subsystem_name = &info.subsystem_names_without_wip();
	let builder_generic_ty = &info.builder_generic_types();

	let channel_name = &info.channel_names_without_wip("");
	let channel_name_unbounded = &info.channel_names_without_wip("_unbounded");

	let channel_name_tx = &info.channel_names_without_wip("_tx");
	let channel_name_unbounded_tx = &info.channel_names_without_wip("_unbounded_tx");

	let channel_name_rx = &info.channel_names_without_wip("_rx");
	let channel_name_unbounded_rx = &info.channel_names_without_wip("_unbounded_rx");

	let baggage_generic_ty = &info.baggage_generic_types();
	let baggage_name = &info.baggage_names();
	let baggage_ty = &info.baggage_types();

	let error_ty = &info.extern_error_ty;

	let support_crate = info.support_crate_name();

	let blocking = &info
		.subsystems()
		.iter()
		.map(|x| {
			if x.blocking {
				quote! { Blocking }
			} else {
				quote! { Regular }
			}
		})
		.collect::<Vec<_>>();

	let generics = quote! {
		< S, #( #baggage_generic_ty, )* >
	};
	let where_clause = quote! {
		where
			S: #support_crate ::SpawnNamed,
	};

	let builder_generics = quote! {
		<S, #( #baggage_generic_ty, )* #( #builder_generic_ty, )* >
	};

	// all subsystems must have the same context
	// even if the overseer does not impose such a limit.
	let builder_additional_generics = quote! {
		<#( #builder_generic_ty, )* >
	};

	let consumes = &info.consumes();

	let subsyste_ctx_name = Ident::new(
		&(overseer_name.to_string() + "SubsystemContext"),
		overseer_name.span()
	);

	let builder_where_clause = quote! {
		where
			S: #support_crate ::SpawnNamed,
		#(
			#builder_generic_ty : Subsystem<#subsyste_ctx_name< #consumes >>,
		)*
	};

	let event = &info.extern_event_ty;

	let mut ts = quote! {
		impl #generics #overseer_name #generics #where_clause {
			/// Create a new overseer utilizing the builder.
			pub fn builder #builder_additional_generics () -> #builder #builder_generics
				#builder_where_clause
			{
				#builder :: default()
			}
		}

		/// Handle for an overseer.
		pub type #handle = #support_crate ::metered::MeteredSender< #event >;

		#[allow(missing_docs)]
		pub struct #builder #builder_generics {
			#(
				#subsystem_name : ::std::option::Option< #builder_generic_ty >,
			)*
			#(
				#baggage_name : ::std::option::Option< #baggage_ty >,
			)*
			spawner: ::std::option::Option< S >,
		}

		impl #builder_generics Default for #builder #builder_generics {
			fn default() -> Self {
				// explicitly assure the required traits are implemented
				fn trait_from_must_be_implemented<E>()
				where
					E: std::error::Error + Send + Sync + 'static + From<#support_crate ::OverseerError>
				{}

				trait_from_must_be_implemented::< #error_ty >();

				Self {
				#(
					#subsystem_name: None,
				)*
				#(
					#baggage_name: None,
				)*
					spawner: None,
				}
			}
		}

		impl #builder_generics #builder #builder_generics #builder_where_clause {
			/// The spawner to use for spawning tasks.
			pub fn spawner(mut self, spawner: S) -> Self
			where
				S: #support_crate ::SpawnNamed + Send
			{
				self.spawner = Some(spawner);
				self
			}

			#(
				/// Specify the particular subsystem implementation.
				pub fn #subsystem_name (mut self, subsystem: #builder_generic_ty ) -> Self {
					self. #subsystem_name = Some( subsystem );
					self
				}
			)*

			#(
				/// Attach the user defined addendum type.
				pub fn #baggage_name (mut self, baggage: #baggage_ty ) -> Self {
					self. #baggage_name = Some( baggage );
					self
				}
			)*

			/// Complete the construction and create the overseer type.
			pub fn build(mut self) -> ::std::result::Result<(#overseer_name #generics, #handle), #error_ty>
			{
				let (events_tx, events_rx) = #support_crate ::metered::channel::<
					#event
				>(SIGNAL_CHANNEL_CAPACITY);

				let handle: #handle = events_tx.clone();

				let (to_overseer_tx, to_overseer_rx) = #support_crate ::metered::unbounded::<
					ToOverseer
				>();

				#(
					let (#channel_name_tx, #channel_name_rx)
					=
						#support_crate ::metered::channel::<
							MessagePacket< #consumes >
						>(CHANNEL_CAPACITY);
				)*

				#(
					let (#channel_name_unbounded_tx, #channel_name_unbounded_rx) =
						#support_crate ::metered::unbounded::<
							MessagePacket< #consumes >
						>();
				)*

				let channels_out =
					ChannelsOut {
						#(
							#channel_name: #channel_name_tx .clone(),
						)*
						#(
							#channel_name_unbounded: #channel_name_unbounded_tx,
						)*
					};

				let mut spawner = self.spawner.expect("Spawner is set. qed");

				let mut running_subsystems = #support_crate ::FuturesUnordered::<
						BoxFuture<'static, ::std::result::Result<(), #error_ty > >
					>::new();

				#(
					// TODO generate a builder pattern that ensures this
					// TODO https://github.com/paritytech/polkadot/issues/3427
					let #subsystem_name = self. #subsystem_name .expect("All subsystem must exist with the builder pattern.");

					let unbounded_meter = #channel_name_unbounded_rx.meter().clone();

					let message_rx: SubsystemIncomingMessages< #consumes > = #support_crate ::select(
						#channel_name_rx, #channel_name_unbounded_rx
					);
					let (signal_tx, signal_rx) = #support_crate ::metered::channel(SIGNAL_CHANNEL_CAPACITY);
					let ctx = #subsyste_ctx_name::< #consumes >::new(
						signal_rx,
						message_rx,
						channels_out.clone(),
						to_overseer_tx.clone(),
					);

					let #subsystem_name: OverseenSubsystem< #consumes > =
						spawn::<_,_, #blocking, _, _, _>(
							&mut spawner,
							#channel_name_tx,
							signal_tx,
							unbounded_meter,
							channels_out.clone(),
							ctx,
							#subsystem_name,
							&mut running_subsystems,
						)?;
				)*

				#(
					let #baggage_name = self. #baggage_name .expect(
						&format!("Baggage variable `{1}` of `{0}` ",
							stringify!(#overseer_name),
							stringify!( #baggage_name )
						)
					);
				)*

				use #support_crate ::StreamExt;

				let to_overseer_rx = to_overseer_rx.fuse();
				let overseer = #overseer_name {
					#(
						#subsystem_name,
					)*

					#(
						#baggage_name,
					)*

					spawner,
					running_subsystems,
					events_rx,
					to_overseer_rx,
				};

				Ok((overseer, handle))
			}
		}
	};
	ts.extend(impl_task_kind(info));
	ts
}

pub(crate) fn impl_task_kind(info: &OverseerInfo) -> proc_macro2::TokenStream {
	let signal = &info.extern_signal_ty;
	let error_ty = &info.extern_error_ty;
	let support_crate = info.support_crate_name();

	let ts = quote! {

		use #support_crate ::FutureExt as _;

		/// Task kind to launch.
		pub trait TaskKind {
			/// Spawn a task, it depends on the implementer if this is blocking or not.
			fn launch_task<S: SpawnNamed>(spawner: &mut S, name: &'static str, future: BoxFuture<'static, ()>);
		}

		#[allow(missing_docs)]
		struct Regular;
		impl TaskKind for Regular {
			fn launch_task<S: SpawnNamed>(spawner: &mut S, name: &'static str, future: BoxFuture<'static, ()>) {
				spawner.spawn(name, future)
			}
		}

		#[allow(missing_docs)]
		struct Blocking;
		impl TaskKind for Blocking {
			fn launch_task<S: SpawnNamed>(spawner: &mut S, name: &'static str, future: BoxFuture<'static, ()>) {
				spawner.spawn_blocking(name, future)
			}
		}

		/// Spawn task of kind `self` using spawner `S`.
		pub fn spawn<S, M, TK, Ctx, E, SubSys>(
			spawner: &mut S,
			message_tx: #support_crate ::metered::MeteredSender<MessagePacket<M>>,
			signal_tx: #support_crate ::metered::MeteredSender< #signal >,
			// meter for the unbounded channel
			unbounded_meter: #support_crate ::metered::Meter,
			// connection to the subsystems
			channels_out: ChannelsOut,
			ctx: Ctx,
			subsystem: SubSys,
			futures: &mut #support_crate ::FuturesUnordered<BoxFuture<'static, ::std::result::Result<(), #error_ty> >>,
		) -> ::std::result::Result<OverseenSubsystem<M>, #error_ty >
		where
			S: #support_crate ::SpawnNamed,
			M: std::fmt::Debug + Send + 'static,
			TK: TaskKind,
			Ctx: SubsystemContext<Message=M>,
			E: std::error::Error + Send + Sync + 'static + From<#support_crate ::OverseerError>,
			SubSys: Subsystem<Ctx>,
		{
			let SpawnedSubsystem { future, name } = subsystem.start(ctx);

			let (tx, rx) = #support_crate ::oneshot::channel();

			let fut = Box::pin(async move {
				if let Err(e) = future.await {
					#support_crate ::tracing::error!(subsystem=name, err = ?e, "subsystem exited with error");
				} else {
					#support_crate ::tracing::debug!(subsystem=name, "subsystem exited without an error");
				}
				let _ = tx.send(());
			});

			<TK as TaskKind>::launch_task(spawner, name, fut);

			futures.push(Box::pin(
				rx.map(|e| {
					tracing::warn!(err = ?e, "dropping error");
					Ok(())
				})
			));

			let instance = Some(#support_crate ::SubsystemInstance:: < M, #signal > {
				meters: #support_crate ::SubsystemMeters {
					unbounded: unbounded_meter,
					bounded: message_tx.meter().clone(),
					signals: signal_tx.meter().clone(),
				},
				tx_signal: signal_tx,
				tx_bounded: message_tx,
				signals_received: 0,
				name,
			});

			Ok(OverseenSubsystem {
				instance,
			})
		}
	};

	ts
}
