// Copyright 2019-2021 Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

//! Defines traits which represent a common interface for Substrate pallets which want to
//! incorporate bridge functionality.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Codec, Decode, Encode, EncodeLike};
use core::clone::Clone;
use core::cmp::Eq;
use core::default::Default;
use core::fmt::Debug;
use scale_info::TypeInfo;
#[cfg(feature = "std")]
use serde::{Deserialize, Serialize};
use sp_finality_grandpa::{AuthorityList, ConsensusLog, SetId, GRANDPA_ENGINE_ID};
use sp_runtime::RuntimeDebug;
use sp_runtime::{generic::OpaqueDigestItemId, traits::Header as HeaderT};

pub mod justification;

/// A type that can be used as a parameter in a dispatchable function.
///
/// When using `decl_module` all arguments for call functions must implement this trait.
pub trait Parameter: Codec + EncodeLike + Clone + Eq + Debug {}
impl<T> Parameter for T where T: Codec + EncodeLike + Clone + Eq + Debug {}

/// A GRANDPA Authority List and ID.
#[derive(Default, Encode, Decode, RuntimeDebug, PartialEq, Clone, TypeInfo)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
pub struct AuthoritySet {
	/// List of GRANDPA authorities for the current round.
	pub authorities: AuthorityList,
	/// Monotonic identifier of the current GRANDPA authority set.
	pub set_id: SetId,
}

impl AuthoritySet {
	/// Create a new GRANDPA Authority Set.
	pub fn new(authorities: AuthorityList, set_id: SetId) -> Self {
		Self { authorities, set_id }
	}
}

/// Data required for initializing the bridge pallet.
///
/// The bridge needs to know where to start its sync from, and this provides that initial context.
#[derive(Default, Encode, Decode, RuntimeDebug, PartialEq, Eq, Clone, TypeInfo)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
pub struct InitializationData<H: HeaderT> {
	/// The header from which we should start syncing.
	pub header: H,
	/// The initial authorities of the pallet.
	pub authority_list: AuthorityList,
	/// The ID of the initial authority set.
	pub set_id: SetId,
	/// Should the pallet block transaction immediately after initialization.
	pub is_halted: bool,
}

/// base trait for verifying transaction inclusion proofs.
pub trait InclusionProofVerifier {
	/// Transaction type.
	type Transaction: Parameter;
	/// Transaction inclusion proof type.
	type TransactionInclusionProof: Parameter;

	/// Verify that transaction is a part of given block.
	///
	/// Returns Some(transaction) if proof is valid and None otherwise.
	fn verify_transaction_inclusion_proof(proof: &Self::TransactionInclusionProof) -> Option<Self::Transaction>;
}

/// A trait for pallets which want to keep track of finalized headers from a bridged chain.
pub trait HeaderChain<H, E> {
	/// Get the best finalized header known to the header chain.
	fn best_finalized() -> H;

	/// Get the best authority set known to the header chain.
	fn authority_set() -> AuthoritySet;

	/// Write a header finalized by GRANDPA to the underlying pallet storage.
	fn append_header(header: H) -> Result<(), E>;
}

impl<H: Default, E> HeaderChain<H, E> for () {
	fn best_finalized() -> H {
		H::default()
	}

	fn authority_set() -> AuthoritySet {
		AuthoritySet::default()
	}

	fn append_header(_header: H) -> Result<(), E> {
		Ok(())
	}
}

/// Abstract finality proof that is justifying block finality.
pub trait FinalityProof<Number>: Clone + Send + Sync + Debug {
	/// Return number of header that this proof is generated for.
	fn target_header_number(&self) -> Number;
}

/// Find header digest that schedules next GRANDPA authorities set.
pub fn find_grandpa_authorities_scheduled_change<H: HeaderT>(
	header: &H,
) -> Option<sp_finality_grandpa::ScheduledChange<H::Number>> {
	let id = OpaqueDigestItemId::Consensus(&GRANDPA_ENGINE_ID);

	let filter_log = |log: ConsensusLog<H::Number>| match log {
		ConsensusLog::ScheduledChange(change) => Some(change),
		_ => None,
	};

	// find the first consensus digest with the right ID which converts to
	// the right kind of consensus log.
	header.digest().convert_first(|l| l.try_to(id).and_then(filter_log))
}
