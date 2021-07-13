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

use alloc::vec::Vec;
use parity_scale_codec::{Encode, Decode, DecodeLimit};

/// Maximum nesting level for XCM decoding.
pub const MAX_XCM_DECODE_DEPTH: u32 = 8;

/// Wrapper around the encoded and decoded versions of a value.
/// Caches the decoded value once computed.
#[derive(Encode, Decode, scale_info::TypeInfo)]
#[codec(encode_bound())]
#[codec(decode_bound())]
#[scale_info(bounds(), skip_type_params(T))]
pub struct DoubleEncoded<T> {
	encoded: Vec<u8>,
	#[codec(skip)]
	decoded: Option<T>,
}

impl<T> Clone for DoubleEncoded<T> {
	fn clone(&self) -> Self { Self { encoded: self.encoded.clone(), decoded: None } }
}

impl<T> PartialEq for DoubleEncoded<T> {
	fn eq(&self, other: &Self) -> bool { self.encoded.eq(&other.encoded) }
}
impl<T> Eq for DoubleEncoded<T> {}

impl<T> core::fmt::Debug for DoubleEncoded<T> {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { self.encoded.fmt(f) }
}

impl<T> From<Vec<u8>> for DoubleEncoded<T> {
	fn from(encoded: Vec<u8>) -> Self {
		Self { encoded, decoded: None }
	}
}

impl<T> DoubleEncoded<T> {
	pub fn into<S>(self) -> DoubleEncoded<S> { DoubleEncoded::from(self) }

	pub fn from<S>(e: DoubleEncoded<S>) -> Self {
		Self {
			encoded: e.encoded,
			decoded: None,
		}
	}

	/// Provides an API similar to `AsRef` that provides access to the inner value.
	/// `AsRef` implementation would expect an `&Option<T>` return type.
	pub fn as_ref(&self) -> Option<&T> {
		self.decoded.as_ref()
	}
}

impl<T: Decode> DoubleEncoded<T> {
	/// Decode the inner encoded value and store it.
	/// Returns a reference to the value in case of success and `Err(())` in case the decoding fails.
	pub fn ensure_decoded(&mut self) -> Result<&T, ()> {
		if self.decoded.is_none() {
			self.decoded = T::decode_all_with_depth_limit(
				MAX_XCM_DECODE_DEPTH,
				&mut &self.encoded[..],
			).ok();
		}
		self.decoded.as_ref().ok_or(())
	}

	/// Move the decoded value out or (if not present) decode `encoded`.
	pub fn take_decoded(&mut self) -> Result<T, ()> {
		self.decoded.take().or_else(|| {
			T::decode_all_with_depth_limit(
				MAX_XCM_DECODE_DEPTH,
				&mut &self.encoded[..],
			).ok()
		}).ok_or(())
	}

	/// Provides an API similar to `TryInto` that allows fallible conversion to the inner value type.
	/// `TryInto` implementation would collide with std blanket implementation based on `TryFrom`.
	pub fn try_into(mut self) -> Result<T, ()> {
		self.ensure_decoded()?;
		self.decoded.ok_or(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ensure_decoded_works() {
		let val: u64 = 42;
		let mut encoded: DoubleEncoded<_> = Encode::encode(&val).into();
		assert_eq!(encoded.ensure_decoded(), Ok(&val));
	}

	#[test]
	fn take_decoded_works() {
		let val: u64 = 42;
		let mut encoded: DoubleEncoded<_> = Encode::encode(&val).into();
		assert_eq!(encoded.take_decoded(), Ok(val));
	}

	#[test]
	fn try_into_works() {
		let val: u64 = 42;
		let encoded: DoubleEncoded<_> = Encode::encode(&val).into();
		assert_eq!(encoded.try_into(), Ok(val));
	}
}
