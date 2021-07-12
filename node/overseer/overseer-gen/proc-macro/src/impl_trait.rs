
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
pub(crate) fn impl_subsystem_ctx(info: &OverseerInfo) -> proc_macro2::TokenStream {
	let signal = &info.extern_signal_ty;
	let message_wrapper = &info.message_wrapper;
	let error = &info.extern_error_ty;
	let support_crate = info.support_crate_name();

    let subsystem_context_trait = Ident::new("SubsystemContext", Span::call_site());

    let ts = quote!{


        /// A `Result` type that wraps [`SubsystemError`].
        ///
        /// [`SubsystemError`]: struct.SubsystemError.html
        pub type SubsystemResult<T> = ::std::result::Result<T, #error>;

        /// Specialized message type originating from the overseer.
        pub type FromOverseer<M0> = #support_crate ::FromOverseer<M0, #signal>;

        /// Specialized subsystem instance type of subsystems consuming a particular message type.
        pub type SubsystemInstance<M1> = #support_crate ::SubsystemInstance<M1, #signal>;

        /// Spawned subsystem.
        pub type SpawnedSubsystem = #support_crate ::SpawnedSubsystem< #error >;

        /// A context type that is given to the [`Subsystem`] upon spawning.
        /// It can be used by [`Subsystem`] to obtain a sender to communicate
        /// with other [`Subsystem`]s or spawn jobs via the overseer instance.
        #[ #support_crate ::async_trait]
        pub trait #subsystem_context_trait: Send + 'static {
            /// The message type of this context. Subsystems launched with this context will expect
            /// to receive messages of this type. Commonly uses the wrapping enum commonly called
            /// `AllMessages`.
            type Message: std::fmt::Debug + Send + 'static;
            /// The sender type as provided by `sender()` and underlying.
            type Sender: #support_crate ::SubsystemSender< #message_wrapper >;

            /// Try to asynchronously receive a message.
            ///
            /// This has to be used with caution, if you loop over this without
            /// using `pending!()` macro you will end up with a busy loop!
            async fn try_recv(&mut self) -> ::std::result::Result<Option<#support_crate ::FromOverseer<Self::Message, #signal>>, ()>;

            /// Receive a message.
            async fn recv(&mut self) -> ::std::result::Result<#support_crate ::FromOverseer<Self::Message, #signal>, #error>;

            /// Spawn a child task on the executor.
            fn spawn(
                &mut self,
                name: &'static str,
                s: ::std::pin::Pin<Box<dyn crate::Future<Output = ()> + Send>>
            ) -> ::std::result::Result<(), #error>;

            /// Spawn a blocking child task on the executor's dedicated thread pool.
            fn spawn_blocking(
                &mut self,
                name: &'static str,
                s: ::std::pin::Pin<Box<dyn crate::Future<Output = ()> + Send>>,
            ) -> ::std::result::Result<(), #error>;

            /// Send a direct message to some other `Subsystem`, routed based on message type.
            async fn send_message<X>(&mut self, msg: X)
                where
                    #message_wrapper: From<X>,
                    X: Send,
            {
                self.sender().send_message(<#message_wrapper as ::std::convert::From<X>>::from(msg)).await
            }

            /// Send multiple direct messages to other `Subsystem`s, routed based on message type.
            async fn send_messages<X, T>(&mut self, msgs: T)
                where
                    T: IntoIterator<Item = X> + Send,
                    T::IntoIter: Send,
                    #message_wrapper: From<X>,
                    X: Send,
            {
                self.sender()
                    .send_messages(
                        msgs.into_iter()
                            .map(<#message_wrapper as ::std::convert::From<X>>::from)
                    ).await
            }

            /// Send a message using the unbounded connection.
            fn send_unbounded_message<X>(&mut self, msg: X)
            where
                #message_wrapper: From<X>,
                X: Send,
            {
                self.sender().send_unbounded_message(<#message_wrapper as ::std::convert::From<X>>::from(msg))
            }

            /// Obtain the sender.
            fn sender(&mut self) -> &mut Self::Sender;
        }


        /// A trait that describes the [`Subsystem`]s that can run on the [`Overseer`].
        ///
        /// It is generic over the message type circulating in the system.
        /// The idea that we want some type containing persistent state that
        /// can spawn actually running subsystems when asked.
        ///
        /// [`Overseer`]: struct.Overseer.html
        /// [`Subsystem`]: trait.Subsystem.html
        pub trait Subsystem<Ctx>
        where
            Ctx: SubsystemContext,
        {
            /// Start this `Subsystem` and return `SpawnedSubsystem`.
            fn start(self, ctx: Ctx) -> #support_crate:: SpawnedSubsystem < #error >;
        }
    };


    ts
}
