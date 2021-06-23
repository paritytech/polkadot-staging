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

//! Version 1 of the DB schema.

use kvdb::KeyValueDB;
use polkadot_node_primitives::approval::{DelayTranche, AssignmentCert};
use polkadot_primitives::v1::{
	ValidatorIndex, GroupIndex, CandidateReceipt, SessionIndex, CoreIndex,
	BlockNumber, Hash, CandidateHash, ValidatorSignature,
};
use sp_consensus_slots::Slot;
use parity_scale_codec::{Encode, Decode};

use std::collections::BTreeMap;
use bitvec::{vec::BitVec, order::Lsb0 as BitOrderLsb0};

//#[cfg(test)]
//pub mod tests;

// slot_duration * 2 + DelayTranche gives the number of delay tranches since the
// unix epoch.
#[derive(Encode, Decode, Clone, Copy, Debug, PartialEq)]
pub struct Tick(u64);

pub type Bitfield = BitVec<BitOrderLsb0, u8>;

/// The database config.
#[derive(Debug, Clone, Copy)]
pub struct Config {
	/// The column family in the database where data is stored.
	pub col_data: u32,
}

/// Details pertaining to our assignment on a block.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
pub struct OurAssignment {
	pub cert: AssignmentCert,
	pub tranche: DelayTranche,
	pub validator_index: ValidatorIndex,
	// Whether the assignment has been triggered already.
	pub triggered: bool,
}

/// Metadata regarding a specific tranche of assignments for a specific candidate.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
pub struct TrancheEntry {
	pub tranche: DelayTranche,
	// Assigned validators, and the instant we received their assignment, rounded
	// to the nearest tick.
	pub assignments: Vec<(ValidatorIndex, Tick)>,
}

/// Metadata regarding approval of a particular candidate within the context of some
/// particular block.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
pub struct ApprovalEntry {
	pub tranches: Vec<TrancheEntry>,
	pub backing_group: GroupIndex,
	pub our_assignment: Option<OurAssignment>,
	pub our_approval_sig: Option<ValidatorSignature>,
	// `n_validators` bits.
	pub assignments: Bitfield,
	pub approved: bool,
}

/// Metadata regarding approval of a particular candidate.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
pub struct CandidateEntry {
	pub candidate: CandidateReceipt,
	pub session: SessionIndex,
	// Assignments are based on blocks, so we need to track assignments separately
	// based on the block we are looking at.
	pub block_assignments: BTreeMap<Hash, ApprovalEntry>,
	pub approvals: Bitfield,
}

/// Metadata regarding approval of a particular block, by way of approval of the
/// candidates contained within it.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
pub struct BlockEntry {
	pub block_hash: Hash,
	pub block_number: BlockNumber,
	pub parent_hash: Hash,
	pub session: SessionIndex,
	pub slot: Slot,
	/// Random bytes derived from the VRF submitted within the block by the block
	/// author as a credential and used as input to approval assignment criteria.
	pub relay_vrf_story: [u8; 32],
	// The candidates included as-of this block and the index of the core they are
	// leaving. Sorted ascending by core index.
	pub candidates: Vec<(CoreIndex, CandidateHash)>,
	// A bitfield where the i'th bit corresponds to the i'th candidate in `candidates`.
	// The i'th bit is `true` iff the candidate has been approved in the context of this
	// block. The block can be considered approved if the bitfield has all bits set to `true`.
	pub approved_bitfield: Bitfield,
	pub children: Vec<Hash>,
}

impl From<crate::Tick> for Tick {
	fn from(tick: crate::Tick) -> Tick {
		Tick(tick)
	}
}

impl From<Tick> for crate::Tick {
	fn from(tick: Tick) -> crate::Tick {
		tick.0
	}
}

/// Errors while accessing things from the DB.
#[derive(Debug, derive_more::From, derive_more::Display)]
pub enum Error {
	Io(std::io::Error),
	InvalidDecoding(parity_scale_codec::Error),
}

impl std::error::Error for Error {}

/// Result alias for DB errors.
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn load_decode<D: Decode>(store: &dyn KeyValueDB, col_data: u32, key: &[u8]) -> Result<Option<D>>
{
	match store.get(col_data, key)? {
		None => Ok(None),
		Some(raw) => D::decode(&mut &raw[..])
			.map(Some)
			.map_err(Into::into),
	}
}

/// The key a given block entry is stored under.
pub(crate) fn block_entry_key(block_hash: &Hash) -> [u8; 46] {
	const BLOCK_ENTRY_PREFIX: [u8; 14] = *b"Approvals_blck";

	let mut key = [0u8; 14 + 32];
	key[0..14].copy_from_slice(&BLOCK_ENTRY_PREFIX);
	key[14..][..32].copy_from_slice(block_hash.as_ref());

	key
}

/// The key a given candidate entry is stored under.
pub(crate) fn candidate_entry_key(candidate_hash: &CandidateHash) -> [u8; 46] {
	const CANDIDATE_ENTRY_PREFIX: [u8; 14] = *b"Approvals_cand";

	let mut key = [0u8; 14 + 32];
	key[0..14].copy_from_slice(&CANDIDATE_ENTRY_PREFIX);
	key[14..][..32].copy_from_slice(candidate_hash.0.as_ref());

	key
}

/// The key a set of block hashes corresponding to a block number is stored under.
pub(crate) fn blocks_at_height_key(block_number: BlockNumber) -> [u8; 16] {
	const BLOCKS_AT_HEIGHT_PREFIX: [u8; 12] = *b"Approvals_at";

	let mut key = [0u8; 12 + 4];
	key[0..12].copy_from_slice(&BLOCKS_AT_HEIGHT_PREFIX);
	block_number.using_encoded(|s| key[12..16].copy_from_slice(s));

	key
}
