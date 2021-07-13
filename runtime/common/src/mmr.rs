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

//! A pallet responsible for creating Merkle Mountain Range (MMR) leaf for current block.

use beefy_primitives::ValidatorSetId;
use sp_core::H256;
use sp_runtime::traits::Convert;
use sp_std::prelude::*;
use frame_support::RuntimeDebug;
use pallet_mmr::primitives::LeafDataProvider;
use parity_scale_codec::{Encode, Decode};
use scale_info::TypeInfo;
use runtime_parachains::paras;
pub use pallet::*;

/// A BEEFY consensus digest item with MMR root hash.
pub struct DepositBeefyDigest<T>(sp_std::marker::PhantomData<T>);

impl<T> pallet_mmr::primitives::OnNewRoot<beefy_primitives::MmrRootHash> for DepositBeefyDigest<T> where
	T: pallet_mmr::Config<Hash = beefy_primitives::MmrRootHash>,
	T: pallet_beefy::Config,
{
	fn on_new_root(root: &<T as pallet_mmr::Config>::Hash) {
		let digest = sp_runtime::generic::DigestItem::Consensus(
			beefy_primitives::BEEFY_ENGINE_ID,
			parity_scale_codec::Encode::encode(
				&beefy_primitives::ConsensusLog::<<T as pallet_beefy::Config>::BeefyId>::MmrRoot(*root)
			),
		);
		<frame_system::Pallet<T>>::deposit_log(digest);
	}
}

/// Convert BEEFY secp256k1 public keys into uncompressed form
pub struct UncompressBeefyEcdsaKeys;
impl Convert<beefy_primitives::crypto::AuthorityId, Vec<u8>> for UncompressBeefyEcdsaKeys {
	fn convert(a: beefy_primitives::crypto::AuthorityId) -> Vec<u8> {
		use sp_core::crypto::Public;
		let compressed_key = a.as_slice();
		// TODO [ToDr] Temporary workaround until we have a better way to get uncompressed keys.
		secp256k1::PublicKey::parse_slice(compressed_key, Some(secp256k1::PublicKeyFormat::Compressed))
			.map(|pub_key| pub_key.serialize().to_vec())
			.map_err(|_| {
				log::error!(target: "runtime::beefy", "Invalid BEEFY PublicKey format!");
			})
			.unwrap_or_default()
	}
}

/// A leaf that gets added every block to the MMR constructed by [pallet_mmr].
#[derive(RuntimeDebug, PartialEq, Eq, Clone, Encode, Decode)]
pub struct MmrLeaf<BlockNumber, Hash, MerkleRoot> {
	/// Current block parent number and hash.
	pub parent_number_and_hash: (BlockNumber, Hash),
	/// A merkle root of all registered parachain heads.
	pub parachain_heads: MerkleRoot,
	/// A merkle root of the next BEEFY authority set.
	pub beefy_next_authority_set: BeefyNextAuthoritySet<MerkleRoot>,
}

/// Details of the next BEEFY authority set.
#[derive(RuntimeDebug, Default, PartialEq, Eq, Clone, Encode, Decode, TypeInfo)]
pub struct BeefyNextAuthoritySet<MerkleRoot> {
	/// Id of the next set.
	///
	/// Id is required to correlate BEEFY signed commitments with the validator set.
	/// Light Client can easily verify that the commitment witness it is getting is
	/// produced by the latest validator set.
	pub id: ValidatorSetId,
	/// Number of validators in the set.
	///
	/// Some BEEFY Light Clients may use an interactive protocol to verify only subset
	/// of signatures. We put set length here, so that these clients can verify the minimal
	/// number of required signatures.
	pub len: u32,
	/// Merkle Root Hash build from BEEFY AuthorityIds.
	///
	/// This is used by Light Clients to confirm that the commitments are signed by the correct
	/// validator set. Light Clients using interactive protocol, might verify only subset of
	/// signatures, hence don't require the full list here (will receive inclusion proofs).
	pub root: MerkleRoot,
}

type MerkleRootOf<T> = <T as pallet_mmr::Config>::Hash;

/// A type that is able to return current list of parachain heads that end up in the MMR leaf.
pub trait ParachainHeadsProvider {
	/// Return a list of encoded parachain heads.
	fn encoded_heads() -> Vec<Vec<u8>>;
}

/// A default implementation for runtimes without parachains.
impl ParachainHeadsProvider for () {
	fn encoded_heads() -> Vec<Vec<u8>> {
		Default::default()
	}
}

impl<T: Config + paras::Config> ParachainHeadsProvider for paras::Pallet<T> {
	fn encoded_heads() -> Vec<Vec<u8>> {
		paras::Pallet::<T>::parachains()
			.into_iter()
			.map(paras::Pallet::<T>::para_head)
			.map(|maybe_para_head| maybe_para_head.encode())
			.collect()
	}
}

#[frame_support::pallet]
pub mod pallet {
	use frame_support::pallet_prelude::*;
	use super::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	/// The module's configuration trait.
	#[pallet::config]
	#[pallet::disable_frame_system_supertrait_check]
	pub trait Config: pallet_mmr::Config + pallet_beefy::Config {
		/// Convert BEEFY AuthorityId to a form that would end up in the Merkle Tree.
		///
		/// For instance for ECDSA (secp256k1) we want to store uncompressed public keys (65 bytes)
		/// to simplify using them on Ethereum chain, but the rest of the Substrate codebase
		/// is storing them compressed (33 bytes) for efficiency reasons.
		type BeefyAuthorityToMerkleLeaf: Convert<<Self as pallet_beefy::Config>::BeefyId, Vec<u8>>;

		/// Retrieve a list of current parachain heads.
		///
		/// The trait is implemented for `paras` module, but since not all chains might have parachains,
		/// and we want to keep the MMR leaf structure uniform, it's possible to use `()` as well to
		/// simply put dummy data to the leaf.
		type ParachainHeads: ParachainHeadsProvider;
	}

	/// Details of next BEEFY authority set.
	///
	/// This storage entry is used as cache for calls to [`update_beefy_next_authority_set`].
	#[pallet::storage]
	#[pallet::getter(fn beefy_next_authorities)]
	pub type BeefyNextAuthorities<T: Config> = StorageValue<
		_,
		BeefyNextAuthoritySet<MerkleRootOf<T>>,
		ValueQuery,
	>;
}

impl<T: Config> LeafDataProvider for Pallet<T> where
	MerkleRootOf<T>: From<H256>,
{
	type LeafData = MmrLeaf<
		<T as frame_system::Config>::BlockNumber,
		<T as frame_system::Config>::Hash,
		MerkleRootOf<T>,
	>;

	fn leaf_data() -> Self::LeafData {
		MmrLeaf {
			parent_number_and_hash: frame_system::Pallet::<T>::leaf_data(),
			parachain_heads: Pallet::<T>::parachain_heads_merkle_root(),
			beefy_next_authority_set: Pallet::<T>::update_beefy_next_authority_set(),
		}
	}
}

impl<T: Config> Pallet<T> where
	MerkleRootOf<T>: From<H256>,
	<T as pallet_beefy::Config>::BeefyId:
{
	/// Returns latest root hash of a merkle tree constructed from all registered parachain headers.
	///
	/// NOTE this does not include parathreads - only parachains are part of the merkle tree.
	///
	/// NOTE This is an initial and inefficient implementation, which re-constructs
	/// the merkle tree every block. Instead we should update the merkle root in [Self::on_initialize]
	/// call of this pallet and update the merkle tree efficiently (use on-chain storage to persist inner nodes).
	fn parachain_heads_merkle_root() -> MerkleRootOf<T> {
		let para_heads = T::ParachainHeads::encoded_heads();
		sp_io::trie::keccak_256_ordered_root(para_heads).into()
	}

	/// Returns details of the next BEEFY authority set.
	///
	/// Details contain authority set id, authority set length and a merkle root,
	/// constructed from uncompressed secp256k1 public keys of the next BEEFY authority set.
	///
	/// This function will use a storage-cached entry in case the set didn't change, or compute and cache
	/// new one in case it did.
	fn update_beefy_next_authority_set() -> BeefyNextAuthoritySet<MerkleRootOf<T>> {
		let id = pallet_beefy::Pallet::<T>::validator_set_id() + 1;
		let current_next = Self::beefy_next_authorities();
		// avoid computing the merkle tree if validator set id didn't change.
		if id == current_next.id {
			return current_next;
		}

		let beefy_public_keys = pallet_beefy::Pallet::<T>::next_authorities()
			.into_iter()
			.map(T::BeefyAuthorityToMerkleLeaf::convert)
			.collect::<Vec<_>>();
		let len = beefy_public_keys.len() as u32;
		let root: MerkleRootOf<T> = sp_io::trie::keccak_256_ordered_root(beefy_public_keys).into();
		let next_set = BeefyNextAuthoritySet {
			id,
			len,
			root,
		};
		// cache the result
		BeefyNextAuthorities::<T>::put(&next_set);
		next_set
	}
}
