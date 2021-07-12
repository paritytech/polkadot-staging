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

#![deny(unused_crate_dependencies)]

use proc_macro2::{Span, Ident, TokenStream};
use syn::{parse2, Result};
use quote::{quote, ToTokens};

mod impl_builder;
mod impl_subsystem_ctx;
mod impl_overseer;
mod impl_channels_out;
mod impl_dispatch;
mod impl_message_wrapper;
mod impl_trait;
mod parse_attr;
mod parse_struct;

use impl_builder::*;
use impl_channels_out::*;
use impl_dispatch::*;
use impl_message_wrapper::*;
use impl_subsystem_ctx::*;
use impl_trait::*;
use impl_overseer::*;
use parse_attr::*;
use parse_struct::*;

#[cfg(test)]
mod tests;

#[proc_macro_attribute]
pub fn overlord(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
	let attr: TokenStream = attr.into();
	let item: TokenStream = item.into();
	impl_overseer_gen(attr, item).unwrap_or_else(|err| err.to_compile_error()).into()
}

pub(crate) fn impl_overseer_gen(attr: TokenStream, orig: TokenStream) -> Result<proc_macro2::TokenStream> {
	let args: AttrArgs = parse2(attr)?;
	let message_wrapper = args.message_wrapper;

	let of: OverseerGuts = parse2(orig)?;

	use proc_macro_crate::{crate_name, FoundCrate};
	let crate_name = crate_name("polkadot-overseer-gen")
		.expect("Support crate polkadot-overseer-gen is present in `Cargo.toml`. qed");
	let support_crate_name = match crate_name {
		FoundCrate::Itself => quote!{crate},
		FoundCrate::Name(name) => Ident::new(&name, Span::call_site()).to_token_stream(),
	};

	let info = OverseerInfo {
		support_crate_name,
		subsystems: of.subsystems,
		baggage: of.baggage,
		overseer_name: of.name,
		message_wrapper,
		message_channel_capacity: args.message_channel_capacity,
		signal_channel_capacity: args.signal_channel_capacity,
		extern_event_ty: args.extern_event_ty,
		extern_signal_ty: args.extern_signal_ty,
		extern_error_ty: args.extern_error_ty,
		extern_network_ty: args.extern_network_ty,
		outgoing_ty: args.outgoing_ty,
	};

	let mut additive = impl_overseer_struct(&info);
	additive.extend(impl_builder(&info));

	additive.extend(impl_overseen_subsystem(&info));
	additive.extend(impl_channels_out_struct(&info));
	additive.extend(impl_subsystem_ctx(&info));

	additive.extend(impl_message_wrapper_enum(&info)?);
	additive.extend(impl_dispatch(&info));

	additive.extend(impl_trait(&info));
	Ok(additive)
}
