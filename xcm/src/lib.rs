// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Cross-Consensus Message format data structures.

// NOTE, this crate is meant to be used in many different environments, notably wasm, but not
// necessarily related to FRAME or even Substrate.
//
// Hence, `no_std` rather than sp-runtime.
#![no_std]
extern crate alloc;

use parity_scale_codec::{Encode, Decode};
use scale_info::TypeInfo;
use derivative::Derivative;

pub mod v0;

mod double_encoded;
pub use double_encoded::DoubleEncoded;

/// A single XCM message, together with its version code.
#[derive(Derivative, Encode, Decode, TypeInfo)]
#[derivative(Clone(bound=""), Eq(bound=""), PartialEq(bound=""), Debug(bound=""))]
#[codec(encode_bound())]
#[codec(decode_bound())]
#[scale_info(bounds(), skip_type_params(Call))]
pub enum VersionedXcm<Call> {
	V0(v0::Xcm<Call>),
}

pub mod opaque {
	pub mod v0 {
		// Everything from v0
		pub use crate::v0::*;
		// Then override with the opaque types in v0
		pub use crate::v0::opaque::{Xcm, Order};
	}

	/// The basic VersionedXcm type which just uses the `Vec<u8>` as an encoded call.
	pub type VersionedXcm = super::VersionedXcm<()>;
}

/// A versioned multi-location, a relative location of a cross-consensus system identifier.
#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug)]
pub enum VersionedMultiLocation {
	V0(v0::MultiLocation),
}

/// A versioned multi-asset, an identifier for an asset within a consensus system.
#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug)]
pub enum VersionedMultiAsset {
	V0(v0::MultiAsset),
}
