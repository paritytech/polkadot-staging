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

use parity_scale_codec::{Encode, Decode};
use scale_info::TypeInfo;

use sp_std::prelude::Vec;
#[cfg(feature = "std")]
use sp_std::convert::TryInto;
#[cfg(feature = "std")]
use application_crypto::AppKey;
#[cfg(feature = "std")]
use sp_keystore::{CryptoStore, SyncCryptoStorePtr, Error as KeystoreError};

use primitives::RuntimeDebug;
use runtime_primitives::traits::AppVerify;

use crate::v0::{SigningContext, ValidatorId, ValidatorSignature, ValidatorIndex};

/// Signed data with signature already verified.
///
/// NOTE: This type does not have an Encode/Decode instance, as this would cancel out our
/// valid signature guarantees. If you need to encode/decode you have to convert into an
/// `UncheckedSigned` first.
///
/// `Signed` can easily be converted into `UncheckedSigned` and conversion back via `into_signed`
/// enforces a valid signature again.
#[derive(Clone, PartialEq, Eq, RuntimeDebug)]
pub struct Signed<Payload, RealPayload = Payload>(UncheckedSigned<Payload, RealPayload>);

/// Unchecked signed data, can be converted to `Signed` by checking the signature.
#[derive(Clone, PartialEq, Eq, RuntimeDebug, Encode, Decode, TypeInfo)]
pub struct UncheckedSigned<Payload, RealPayload = Payload> {
	/// The payload is part of the signed data. The rest is the signing context,
	/// which is known both at signing and at validation.
	payload: Payload,
	/// The index of the validator signing this statement.
	validator_index: ValidatorIndex,
	/// The signature by the validator of the signed payload.
	signature: ValidatorSignature,
	/// This ensures the real payload is tracked at the typesystem level.
	real_payload: sp_std::marker::PhantomData<RealPayload>,
}

impl<Payload: EncodeAs<RealPayload>, RealPayload: Encode> Signed<Payload, RealPayload> {
	/// Used to create a `Signed` from already existing parts.
	///
	/// The signature is checked as part of the process.
	#[cfg(feature = "std")]
	pub fn new<H: Encode>(
		payload: Payload,
		validator_index: ValidatorIndex,
		signature: ValidatorSignature,
		context: &SigningContext<H>,
		key: &ValidatorId,
	) -> Option<Self> {
		let s = UncheckedSigned {
			payload,
			validator_index,
			signature,
			real_payload: std::marker::PhantomData,
		};

		s.check_signature(context, key).ok()?;

		Some(Self(s))
	}

	/// Create a new `Signed` by signing data.
	#[cfg(feature = "std")]
	pub async fn sign<H: Encode>(
		keystore: &SyncCryptoStorePtr,
		payload: Payload,
		context: &SigningContext<H>,
		validator_index: ValidatorIndex,
		key: &ValidatorId,
	) -> Result<Option<Self>, KeystoreError> {
		let r = UncheckedSigned::sign(keystore, payload, context, validator_index, key).await?;
		Ok(r.map(Self))
	}

	/// Try to convert from `UncheckedSigned` by checking the signature.
	pub fn try_from_unchecked<H: Encode>(
		unchecked: UncheckedSigned<Payload, RealPayload>,
		context: &SigningContext<H>,
		key: &ValidatorId
	) -> Result<Self, UncheckedSigned<Payload, RealPayload>> {
		if unchecked.check_signature(context, key).is_ok() {
			Ok(Self(unchecked))
		} else {
			Err(unchecked)
		}
	}

	/// Get a reference to data as unchecked.
	pub fn as_unchecked(&self) -> &UncheckedSigned<Payload, RealPayload> {
		&self.0
	}

	/// Immutably access the payload.
	#[inline]
	pub fn payload(&self) -> &Payload {
		&self.0.payload
	}

	/// Immutably access the validator index.
	#[inline]
	pub fn validator_index(&self) -> ValidatorIndex {
		self.0.validator_index
	}

	/// Immutably access the signature.
	#[inline]
	pub fn signature(&self) -> &ValidatorSignature {
		&self.0.signature
	}

	/// Discard signing data, get the payload
	#[inline]
	pub fn into_payload(self) -> Payload {
		self.0.payload
	}

	/// Convert `Payload` into `RealPayload`.
	pub fn convert_payload(&self) -> Signed<RealPayload> where for<'a> &'a Payload: Into<RealPayload> {
		Signed(self.0.unchecked_convert_payload())
	}
}

// We can't bound this on `Payload: Into<RealPayload>` beacuse that conversion consumes
// the payload, and we don't want that. We can't bound it on `Payload: AsRef<RealPayload>`
// because there's no blanket impl of `AsRef<T> for T`. In the end, we just invent our
// own trait which does what we need: EncodeAs.
impl<Payload: EncodeAs<RealPayload>, RealPayload: Encode> UncheckedSigned<Payload, RealPayload> {
	/// Used to create a `UncheckedSigned` from already existing parts.
	///
	/// Signature is not checked here, hence `UncheckedSigned`.
	#[cfg(feature = "std")]
	pub fn new(
		payload: Payload,
		validator_index: ValidatorIndex,
		signature: ValidatorSignature,
	) -> Self {
		Self {
			payload,
			validator_index,
			signature,
			real_payload: std::marker::PhantomData,
		}
	}

	/// Check signature and convert to `Signed` if successful.
	pub fn try_into_checked<H: Encode>(
		self,
		context: &SigningContext<H>,
		key: &ValidatorId
	) -> Result<Signed<Payload, RealPayload>, Self> {
		Signed::try_from_unchecked(self, context, key)
	}

	/// Immutably access the payload.
	#[inline]
	pub fn unchecked_payload(&self) -> &Payload {
		&self.payload
	}

	/// Immutably access the validator index.
	#[inline]
	pub fn unchecked_validator_index(&self) -> ValidatorIndex {
		self.validator_index
	}

	/// Immutably access the signature.
	#[inline]
	pub fn unchecked_signature(&self) -> &ValidatorSignature {
		&self.signature
	}

	/// Discard signing data, get the payload
	#[inline]
	pub fn unchecked_into_payload(self) -> Payload {
		self.payload
	}

	/// Convert `Payload` into `RealPayload`.
	pub fn unchecked_convert_payload(&self) -> UncheckedSigned<RealPayload> where for<'a> &'a Payload: Into<RealPayload> {
		UncheckedSigned {
			signature: self.signature.clone(),
			validator_index: self.validator_index,
			payload: (&self.payload).into(),
			real_payload: sp_std::marker::PhantomData,
		}
	}

	fn payload_data<H: Encode>(payload: &Payload, context: &SigningContext<H>) -> Vec<u8> {
		// equivalent to (real_payload, context).encode()
		let mut out = payload.encode_as();
		out.extend(context.encode());
		out
	}

	/// Sign this payload with the given context and key, storing the validator index.
	#[cfg(feature = "std")]
	async fn sign<H: Encode>(
		keystore: &SyncCryptoStorePtr,
		payload: Payload,
		context: &SigningContext<H>,
		validator_index: ValidatorIndex,
		key: &ValidatorId,
	) -> Result<Option<Self>, KeystoreError> {
		let data = Self::payload_data(&payload, context);
		let signature = CryptoStore::sign_with(
			&**keystore,
			ValidatorId::ID,
			&key.into(),
			&data,
		).await?;

		let signature = match signature {
			Some(sig) => sig.try_into().map_err(|_| KeystoreError::KeyNotSupported(ValidatorId::ID))?,
			None => return Ok(None),
		};

		Ok(Some(Self {
			payload,
			validator_index,
			signature,
			real_payload: std::marker::PhantomData,
		}))
	}

	/// Validate the payload given the context and public key.
	fn check_signature<H: Encode>(&self, context: &SigningContext<H>, key: &ValidatorId) -> Result<(), ()> {
		let data = Self::payload_data(&self.payload, context);
		if self.signature.verify(data.as_slice(), key) { Ok(()) } else { Err(()) }
	}

}

impl<Payload, RealPayload> From<Signed<Payload, RealPayload>> for UncheckedSigned<Payload, RealPayload> {
	fn from(signed: Signed<Payload, RealPayload>) -> Self {
		signed.0
	}
}

/// This helper trait ensures that we can encode Statement as CompactStatement,
/// and anything as itself.
///
/// This resembles `parity_scale_codec::EncodeLike`, but it's distinct:
/// EncodeLike is a marker trait which asserts at the typesystem level that
/// one type's encoding is a valid encoding for another type. It doesn't
/// perform any type conversion when encoding.
///
/// This trait, on the other hand, provides a method which can be used to
/// simultaneously convert and encode one type as another.
pub trait EncodeAs<T> {
	/// Convert Self into T, then encode T.
	///
	/// This is useful when T is a subset of Self, reducing encoding costs;
	/// its signature also means that we do not need to clone Self in order
	/// to retain ownership, as we would if we were to do
	/// `self.clone().into().encode()`.
	fn encode_as(&self) -> Vec<u8>;
}

impl<T: Encode> EncodeAs<T> for T {
	fn encode_as(&self) -> Vec<u8> {
		self.encode()
	}
}
