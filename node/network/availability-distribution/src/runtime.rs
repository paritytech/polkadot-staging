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

//! Convenient interface to the runtime.

use lru::LruCache;

use sp_application_crypto::AppKey;
use sp_core::crypto::Public;
use sp_keystore::{CryptoStore, SyncCryptoStorePtr};

use polkadot_node_subsystem_util::{
	request_session_index_for_child, request_session_info,
};
use polkadot_primitives::v1::{GroupIndex, Hash, SessionIndex, SessionInfo, ValidatorId, ValidatorIndex};
use polkadot_subsystem::SubsystemContext;

use super::{
	error::recv_runtime,
	Error,
};

/// Caching of session info as needed by availability distribution.
///
/// It should be ensured that a cached session stays live in the cache as long as we might need it.
pub struct Runtime {
	/// Get the session index for a given relay parent.
	///
	/// We query this up to a 100 times per block, so caching it here without roundtrips over the
	/// overseer seems sensible.
	session_index_cache: LruCache<Hash, SessionIndex>,

	/// Look up cached sessions by SessionIndex.
	session_info_cache: LruCache<SessionIndex, ExtendedSessionInfo>,

	/// Key store for determining whether we are a validator and what `ValidatorIndex` we have.
	keystore: SyncCryptoStorePtr,
}

/// SessionInfo with additional useful data for validator nodes.
pub struct ExtendedSessionInfo {
	/// Actual session info as fetched from the runtime.
	pub session_info: SessionInfo,
	/// Contains useful information about ourselves, in case this node is a validator.
	pub validator_info: ValidatorInfo,
}

/// Information about ourself, in case we are an `Authority`.
///
/// This data is derived from the `SessionInfo` and our key as found in the keystore.
pub struct ValidatorInfo {
	/// The index this very validator has in `SessionInfo` vectors, if any.
	pub our_index: Option<ValidatorIndex>,
	/// The group we belong to, if any.
	pub our_group: Option<GroupIndex>,
}

impl Runtime {
	/// Create a new `Runtime` for convenient runtime fetches.
	pub fn new(keystore: SyncCryptoStorePtr) -> Self {
		Self {
			// 5 relatively conservative, 1 to 2 should suffice:
			session_index_cache: LruCache::new(5),
			// We need to cache the current and the last session the most:
			session_info_cache: LruCache::new(2),
			keystore,
		}
	}

	/// Retrieve the current session index.
	pub async fn get_session_index<Context>(
		&mut self,
		ctx: &mut Context,
		parent: Hash,
	) -> Result<SessionIndex, Error>
	where
		Context: SubsystemContext,
	{
		match self.session_index_cache.get(&parent) {
			Some(index) => Ok(*index),
			None => {
				let index =
					recv_runtime(request_session_index_for_child(parent, ctx.sender()).await)
						.await?;
				self.session_index_cache.put(parent, index);
				Ok(index)
			}
		}
	}

	/// Get `ExtendedSessionInfo` by relay parent hash.
	pub async fn get_session_info<'a, Context>(
		&'a mut self,
		ctx: &mut Context,
		parent: Hash,
	) -> Result<&'a ExtendedSessionInfo, Error>
	where
		Context: SubsystemContext,
	{
		let session_index = self.get_session_index(ctx, parent).await?;

		self.get_session_info_by_index(ctx, parent, session_index).await
	}

	/// Get `ExtendedSessionInfo` by session index.
	///
	/// `request_session_info` still requires the parent to be passed in, so we take the parent
	/// in addition to the `SessionIndex`.
	pub async fn get_session_info_by_index<'a, Context>(
		&'a mut self,
		ctx: &mut Context,
		parent: Hash,
		session_index: SessionIndex,
	) -> Result<&'a ExtendedSessionInfo, Error>
	where
		Context: SubsystemContext,
	{
		if !self.session_info_cache.contains(&session_index) {
			let session_info =
				recv_runtime(request_session_info(parent, session_index, ctx.sender()).await)
					.await?
					.ok_or(Error::NoSuchSession(session_index))?;
			let validator_info = self.get_validator_info(&session_info).await?;

			let full_info = ExtendedSessionInfo {
				session_info,
				validator_info,
			};

			self.session_info_cache.put(session_index, full_info);
		}
		Ok(
			self.session_info_cache.get(&session_index)
				.expect("We just put the value there. qed.")
		)
	}

	/// Build `ValidatorInfo` for the current session.
	///
	///
	/// Returns: `None` if not a validator.
	async fn get_validator_info(
		&self,
		session_info: &SessionInfo,
	) -> Result<ValidatorInfo, Error>
	{
		if let Some(our_index) = self.get_our_index(&session_info.validators).await {
			// Get our group index:
			let our_group = session_info.validator_groups
				.iter()
				.enumerate()
				.find_map(|(i, g)| {
					g.iter().find_map(|v| {
						if *v == our_index {
							Some(GroupIndex(i as u32))
						} else {
							None
						}
					})
				}
			);
			let info = ValidatorInfo {
				our_index: Some(our_index),
				our_group,
			};
			return Ok(info)
		}
		return Ok(ValidatorInfo { our_index: None, our_group: None })
	}

	/// Get our `ValidatorIndex`.
	///
	/// Returns: None if we are not a validator.
	async fn get_our_index(&self, validators: &[ValidatorId]) -> Option<ValidatorIndex> {
		let keys: Vec<_> = validators.iter().map(|v| (v.to_raw_vec(), ValidatorId::ID)).collect();
		CryptoStore::has_keys(&*self.keystore, &keys).await.into_found().next().and_then(|i| {
			return Some(ValidatorIndex(i as u32));
		});

		None
	}
}
