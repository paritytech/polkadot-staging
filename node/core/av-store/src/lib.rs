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

//! Implements a `AvailabilityStoreSubsystem`.

#![recursion_limit="256"]
#![warn(missing_docs)]

use std::collections::{HashMap, HashSet, BTreeSet};
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH};

use parity_scale_codec::{Encode, Decode, Input, Error as CodecError};
use futures::{select, channel::oneshot, future, FutureExt};
use futures_timer::Delay;
use kvdb::{KeyValueDB, DBTransaction};

use polkadot_primitives::v1::{
	Hash, BlockNumber, CandidateEvent, ValidatorIndex, CandidateHash,
	CandidateReceipt, Header,
};
use polkadot_node_primitives::{
	ErasureChunk, AvailableData,
};
use polkadot_node_subsystem::{
	FromOverseer, OverseerSignal, SubsystemError,
	SubsystemContext, SpawnedSubsystem,
	overseer,
	ActiveLeavesUpdate,
	errors::{ChainApiError, RuntimeApiError},
	messages::{
		AvailabilityStoreMessage, ChainApiMessage, RuntimeApiMessage, RuntimeApiRequest,
	}
};
use polkadot_node_subsystem_util::{
	self as util,
	metrics::{self, prometheus},
};
use polkadot_subsystem::;
use bitvec::{vec::BitVec, order::Lsb0 as BitOrderLsb0};

#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "parachain::availability";

/// The following constants are used under normal conditions:

const AVAILABLE_PREFIX: &[u8; 9] = b"available";
const CHUNK_PREFIX: &[u8; 5] = b"chunk";
const META_PREFIX: &[u8; 4] = b"meta";
const UNFINALIZED_PREFIX: &[u8; 11] = b"unfinalized";
const PRUNE_BY_TIME_PREFIX: &[u8; 13] = b"prune_by_time";

// We have some keys we want to map to empty values because existence of the key is enough. We use this because
// rocksdb doesn't support empty values.
const TOMBSTONE_VALUE: &[u8] = &*b" ";

/// Unavailable blocks are kept for 1 hour.
const KEEP_UNAVAILABLE_FOR: Duration = Duration::from_secs(60 * 60);

/// Finalized data is kept for 25 hours.
const KEEP_FINALIZED_FOR: Duration = Duration::from_secs(25 * 60 * 60);

/// The pruning interval.
const PRUNING_INTERVAL: Duration = Duration::from_secs(60 * 5);

/// Unix time wrapper with big-endian encoding.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Ord)]
struct BETimestamp(u64);

impl Encode for BETimestamp {
	fn size_hint(&self) -> usize {
		std::mem::size_of::<u64>()
	}

	fn using_encoded<R, F: FnOnce(&[u8]) -> R>(&self, f: F) -> R {
		f(&self.0.to_be_bytes())
	}
}

impl Decode for BETimestamp {
	fn decode<I: Input>(value: &mut I) -> Result<Self, CodecError> {
		<[u8; 8]>::decode(value).map(u64::from_be_bytes).map(Self)
	}
}

impl From<Duration> for BETimestamp {
	fn from(d: Duration) -> Self {
		BETimestamp(d.as_secs())
	}
}

impl Into<Duration> for BETimestamp {
	fn into(self) -> Duration {
		Duration::from_secs(self.0)
	}
}

/// [`BlockNumber`] wrapper with big-endian encoding.
#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord)]
struct BEBlockNumber(BlockNumber);

impl Encode for BEBlockNumber {
	fn size_hint(&self) -> usize {
		std::mem::size_of::<BlockNumber>()
	}

	fn using_encoded<R, F: FnOnce(&[u8]) -> R>(&self, f: F) -> R {
		f(&self.0.to_be_bytes())
	}
}

impl Decode for BEBlockNumber {
	fn decode<I: Input>(value: &mut I) -> Result<Self, CodecError> {
		<[u8; std::mem::size_of::<BlockNumber>()]>::decode(value).map(BlockNumber::from_be_bytes).map(Self)
	}
}

#[derive(Debug, Encode, Decode)]
enum State {
	/// Candidate data was first observed at the given time but is not available in any block.
	#[codec(index = 0)]
	Unavailable(BETimestamp),
	/// The candidate was first observed at the given time and was included in the given list of unfinalized blocks, which may be
	/// empty. The timestamp here is not used for pruning. Either one of these blocks will be finalized or the state will regress to
	/// `State::Unavailable`, in which case the same timestamp will be reused. Blocks are sorted ascending first by block number and
	/// then hash.
	#[codec(index = 1)]
	Unfinalized(BETimestamp, Vec<(BEBlockNumber, Hash)>),
	/// Candidate data has appeared in a finalized block and did so at the given time.
	#[codec(index = 2)]
	Finalized(BETimestamp)
}

// Meta information about a candidate.
#[derive(Debug, Encode, Decode)]
struct CandidateMeta {
	state: State,
	data_available: bool,
	chunks_stored: BitVec<BitOrderLsb0, u8>,
}

fn query_inner<D: Decode>(
	db: &Arc<dyn KeyValueDB>,
	column: u32,
	key: &[u8],
) -> Result<Option<D>, Error> {
	match db.get(column, key) {
		Ok(Some(raw)) => {
			let res = D::decode(&mut &raw[..])?;
			Ok(Some(res))
		}
		Ok(None) => Ok(None),
		Err(e) => {
			tracing::warn!(target: LOG_TARGET, err = ?e, "Error reading from the availability store");
			Err(e.into())
		}
	}
}

fn write_available_data(
	tx: &mut DBTransaction,
	config: &Config,
	hash: &CandidateHash,
	available_data: &AvailableData,
) {
	let key = (AVAILABLE_PREFIX, hash).encode();

	tx.put_vec(config.col_data, &key[..], available_data.encode());
}

fn load_available_data(
	db: &Arc<dyn KeyValueDB>,
	config: &Config,
	hash: &CandidateHash,
) -> Result<Option<AvailableData>, Error> {
	let key = (AVAILABLE_PREFIX, hash).encode();

	query_inner(db, config.col_data, &key)
}

fn delete_available_data(
	tx: &mut DBTransaction,
	config: &Config,
	hash: &CandidateHash,
) {
	let key = (AVAILABLE_PREFIX, hash).encode();

	tx.delete(config.col_data, &key[..])
}

fn load_chunk(
	db: &Arc<dyn KeyValueDB>,
	config: &Config,
	candidate_hash: &CandidateHash,
	chunk_index: ValidatorIndex,
) -> Result<Option<ErasureChunk>, Error> {
	let key = (CHUNK_PREFIX, candidate_hash, chunk_index).encode();

	query_inner(db, config.col_data, &key)
}

fn write_chunk(
	tx: &mut DBTransaction,
	config: &Config,
	candidate_hash: &CandidateHash,
	chunk_index: ValidatorIndex,
	erasure_chunk: &ErasureChunk,
) {
	let key = (CHUNK_PREFIX, candidate_hash, chunk_index).encode();

	tx.put_vec(config.col_data, &key, erasure_chunk.encode());
}

fn delete_chunk(
	tx: &mut DBTransaction,
	config: &Config,
	candidate_hash: &CandidateHash,
	chunk_index: ValidatorIndex,
) {
	let key = (CHUNK_PREFIX, candidate_hash, chunk_index).encode();

	tx.delete(config.col_data, &key[..]);
}

fn load_meta(
	db: &Arc<dyn KeyValueDB>,
	config: &Config,
	hash: &CandidateHash,
) -> Result<Option<CandidateMeta>, Error> {
	let key = (META_PREFIX, hash).encode();

	query_inner(db, config.col_meta, &key)
}

fn write_meta(
	tx: &mut DBTransaction,
	config: &Config,
	hash: &CandidateHash,
	meta: &CandidateMeta,
) {
	let key = (META_PREFIX, hash).encode();

	tx.put_vec(config.col_meta, &key, meta.encode());
}

fn delete_meta(tx: &mut DBTransaction, config: &Config, hash: &CandidateHash) {
	let key = (META_PREFIX, hash).encode();
	tx.delete(config.col_meta, &key[..])
}

fn delete_unfinalized_height(
	tx: &mut DBTransaction,
	config: &Config,
	block_number: BlockNumber,
) {
	let prefix = (UNFINALIZED_PREFIX, BEBlockNumber(block_number)).encode();
	tx.delete_prefix(config.col_meta, &prefix);
}

fn delete_unfinalized_inclusion(
	tx: &mut DBTransaction,
	config: &Config,
	block_number: BlockNumber,
	block_hash: &Hash,
	candidate_hash: &CandidateHash,
) {
	let key = (
		UNFINALIZED_PREFIX,
		BEBlockNumber(block_number),
		block_hash,
		candidate_hash,
	).encode();

	tx.delete(config.col_meta, &key[..]);
}

fn delete_pruning_key(
	tx: &mut DBTransaction,
	config: &Config,
	t: impl Into<BETimestamp>,
	h: &CandidateHash,
) {
	let key = (PRUNE_BY_TIME_PREFIX, t.into(), h).encode();
	tx.delete(config.col_meta, &key);
}

fn write_pruning_key(
	tx: &mut DBTransaction,
	config: &Config,
	t: impl Into<BETimestamp>,
	h: &CandidateHash,
) {
	let t = t.into();
	let key = (PRUNE_BY_TIME_PREFIX, t, h).encode();
	tx.put(config.col_meta, &key, TOMBSTONE_VALUE);
}

fn finalized_block_range(finalized: BlockNumber) -> (Vec<u8>, Vec<u8>) {
	// We use big-endian encoding to iterate in ascending order.
	let start = UNFINALIZED_PREFIX.encode();
	let end = (UNFINALIZED_PREFIX, BEBlockNumber(finalized + 1)).encode();

	(start, end)
}

fn write_unfinalized_block_contains(
	tx: &mut DBTransaction,
	config: &Config,
	n: BlockNumber,
	h: &Hash,
	ch: &CandidateHash,
) {
	let key = (UNFINALIZED_PREFIX, BEBlockNumber(n), h, ch).encode();
	tx.put(config.col_meta, &key, TOMBSTONE_VALUE);
}

fn pruning_range(now: impl Into<BETimestamp>) -> (Vec<u8>, Vec<u8>) {
	let start = PRUNE_BY_TIME_PREFIX.encode();
	let end = (PRUNE_BY_TIME_PREFIX, BETimestamp(now.into().0 + 1)).encode();

	(start, end)
}

fn decode_unfinalized_key(s: &[u8]) -> Result<(BlockNumber, Hash, CandidateHash), CodecError> {
	if !s.starts_with(UNFINALIZED_PREFIX) {
		return Err("missing magic string".into());
	}

	<(BEBlockNumber, Hash, CandidateHash)>::decode(&mut &s[UNFINALIZED_PREFIX.len()..])
		.map(|(b, h, ch)| (b.0, h, ch))
}

fn decode_pruning_key(s: &[u8]) -> Result<(Duration, CandidateHash), CodecError> {
	if !s.starts_with(PRUNE_BY_TIME_PREFIX) {
		return Err("missing magic string".into());
	}

	<(BETimestamp, CandidateHash)>::decode(&mut &s[PRUNE_BY_TIME_PREFIX.len()..])
		.map(|(t, ch)| (t.into(), ch))
}

#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum Error {
	#[error(transparent)]
	RuntimeApi(#[from] RuntimeApiError),

	#[error(transparent)]
	ChainApi(#[from] ChainApiError),

	#[error(transparent)]
	Erasure(#[from] erasure::Error),

	#[error(transparent)]
	Io(#[from] io::Error),

	#[error(transparent)]
	Oneshot(#[from] oneshot::Canceled),

	#[error(transparent)]
	Subsystem(#[from] SubsystemError),

	#[error(transparent)]
	Time(#[from] SystemTimeError),

	#[error(transparent)]
	Codec(#[from] CodecError),

	#[error("Custom databases are not supported")]
	CustomDatabase,
}

impl Error {
	fn trace(&self) {
		match self {
			// don't spam the log with spurious errors
			Self::RuntimeApi(_) |
			Self::Oneshot(_) => tracing::debug!(target: LOG_TARGET, err = ?self),
			// it's worth reporting otherwise
			_ => tracing::warn!(target: LOG_TARGET, err = ?self),
		}
	}
}

/// Struct holding pruning timing configuration.
/// The only purpose of this structure is to use different timing
/// configurations in production and in testing.
#[derive(Clone)]
struct PruningConfig {
	/// How long unavailable data should be kept.
	keep_unavailable_for: Duration,

	/// How long finalized data should be kept.
	keep_finalized_for: Duration,

	/// How often to perform data pruning.
	pruning_interval: Duration,
}

impl Default for PruningConfig {
	fn default() -> Self {
		Self {
			keep_unavailable_for: KEEP_UNAVAILABLE_FOR,
			keep_finalized_for: KEEP_FINALIZED_FOR,
			pruning_interval: PRUNING_INTERVAL,
		}
	}
}

/// Configuration for the availability store.
#[derive(Debug, Clone, Copy)]
pub struct Config {
	/// The column family for availability data and chunks.
	pub col_data: u32,
	/// The column family for availability store meta information.
	pub col_meta: u32,
}

trait Clock: Send + Sync {
	// Returns time since unix epoch.
	fn now(&self) -> Result<Duration, Error>;
}

struct SystemClock;

impl Clock for SystemClock {
	fn now(&self) -> Result<Duration, Error> {
		SystemTime::now().duration_since(UNIX_EPOCH).map_err(Into::into)
	}
}

/// An implementation of the Availability Store subsystem.
pub struct AvailabilityStoreSubsystem {
	pruning_config: PruningConfig,
	config: Config,
	db: Arc<dyn KeyValueDB>,
	known_blocks: KnownUnfinalizedBlocks,
	finalized_number: Option<BlockNumber>,
	metrics: Metrics,
	clock: Box<dyn Clock>,
}

impl AvailabilityStoreSubsystem {
	/// Create a new `AvailabilityStoreSubsystem` with a given config on disk.
	pub fn new(
		db: Arc<dyn KeyValueDB>,
		config: Config,
		metrics: Metrics,
	) -> Self {
		Self::with_pruning_config_and_clock(
			db,
			config,
			PruningConfig::default(),
			Box::new(SystemClock),
			metrics,
		)
	}

	/// Create a new `AvailabilityStoreSubsystem` with a given config on disk.
	fn with_pruning_config_and_clock(
		db: Arc<dyn KeyValueDB>,
		config: Config,
		pruning_config: PruningConfig,
		clock: Box<dyn Clock>,
		metrics: Metrics,
	) -> Self {
		Self {
			pruning_config,
			config,
			db,
			metrics,
			clock,
			known_blocks: KnownUnfinalizedBlocks::default(),
			finalized_number: None,
		}
	}
}

/// We keep the hashes and numbers of all unfinalized
/// processed blocks in memory.
#[derive(Default, Debug)]
struct KnownUnfinalizedBlocks {
	by_hash: HashSet<Hash>,
	by_number: BTreeSet<(BlockNumber, Hash)>,
}

impl KnownUnfinalizedBlocks {
	/// Check whether the block has been already processed.
	fn is_known(&self, hash: &Hash) -> bool {
		self.by_hash.contains(hash)
	}

	/// Insert a new block into the known set.
	fn insert(&mut self, hash: Hash, number: BlockNumber) {
		self.by_hash.insert(hash);
		self.by_number.insert((number, hash));
	}

	/// Prune all finalized blocks.
	fn prune_finalized(&mut self, finalized: BlockNumber) {
		// split_off returns everything after the given key, including the key
		let split_point = finalized.saturating_add(1);
		let mut finalized = self.by_number.split_off(&(split_point, Hash::zero()));
		// after split_off `finalized` actually contains unfinalized blocks, we need to swap
		std::mem::swap(&mut self.by_number, &mut finalized);
		for (_, block) in finalized {
			self.by_hash.remove(&block);
		}
	}
}

impl<Context> overseer::overseer::Subsystem<Context> for AvailabilityStoreSubsystem
where
	Context: overseer::SubsystemContext<Message = AvailabilityStoreMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = run(self, ctx)
			.map(|_| Ok(()))
			.boxed();

		SpawnedSubsystem {
			name: "availability-store-subsystem",
			future,
		}
	}
}

async fn run<Context>(mut subsystem: AvailabilityStoreSubsystem, mut ctx: Context)
where
	Context: overseer::SubsystemContext<Message = AvailabilityStoreMessage>,
{
	let mut next_pruning = Delay::new(subsystem.pruning_config.pruning_interval).fuse();

	loop {
		let res = run_iteration(&mut ctx, &mut subsystem, &mut next_pruning).await;
		match res {
			Err(e) => {
				e.trace();

				if let Error::Subsystem(SubsystemError::Context(_)) = e {
					break;
				}
			}
			Ok(true) => {
				tracing::info!(target: LOG_TARGET, "received `Conclude` signal, exiting");
				break;
			},
			Ok(false) => continue,
		}
	}
}

async fn run_iteration<Context>(
	ctx: &mut Context,
	subsystem: &mut AvailabilityStoreSubsystem,
	mut next_pruning: &mut future::Fuse<Delay>,
)
	-> Result<bool, Error>
where
	Context: overseer::SubsystemContext<Message = AvailabilityStoreMessage>,
{
	select! {
		incoming = ctx.recv().fuse() => {
			match incoming? {
				FromOverseer::Signal(OverseerSignal::Conclude) => return Ok(true),
				FromOverseer::Signal(OverseerSignal::ActiveLeaves(
					ActiveLeavesUpdate { activated, .. })
				) => {
					for activated in activated.into_iter() {
						let _timer = subsystem.metrics.time_block_activated();
						process_block_activated(ctx, subsystem, activated.hash).await?;
					}
				}
				FromOverseer::Signal(OverseerSignal::BlockFinalized(hash, number)) => {
					let _timer = subsystem.metrics.time_process_block_finalized();

					subsystem.finalized_number = Some(number);
					subsystem.known_blocks.prune_finalized(number);
					process_block_finalized(
						ctx,
						&subsystem,
						hash,
						number,
					).await?;
				}
				FromOverseer::Communication { msg } => {
					let _timer = subsystem.metrics.time_process_message();
					process_message(subsystem, msg)?;
				}
			}
		}
		_ = next_pruning => {
			// It's important to set the delay before calling `prune_all` because an error in `prune_all`
			// could lead to the delay not being set again. Then we would never prune anything anymore.
			*next_pruning = Delay::new(subsystem.pruning_config.pruning_interval).fuse();

			let _timer = subsystem.metrics.time_pruning();
			prune_all(&subsystem.db, &subsystem.config, &*subsystem.clock)?;
		}
	}

	Ok(false)
}

async fn process_block_activated<Context>(
	ctx: &mut Context,
	subsystem: &mut AvailabilityStoreSubsystem,
	activated: Hash,
) -> Result<(), Error>
where
	Context: overseer::SubsystemContext<Message = AvailabilityStoreMessage>,
{
	let now = subsystem.clock.now()?;

	let block_header = {
		let (tx, rx) = oneshot::channel();

		ctx.send_message(
			ChainApiMessage::BlockHeader(activated, tx)
		).await;

		match rx.await?? {
			None => return Ok(()),
			Some(n) => n,
		}
	};
	let block_number = block_header.number;

	let new_blocks = util::determine_new_blocks(
		ctx.sender(),
		|hash| -> Result<bool, Error> {
			Ok(subsystem.known_blocks.is_known(hash))
		},
		activated,
		&block_header,
		subsystem.finalized_number.unwrap_or(block_number.saturating_sub(1)),
	).await?;

	let mut tx = DBTransaction::new();
	// determine_new_blocks is descending in block height
	for (hash, header) in new_blocks.into_iter().rev() {
		process_new_head(
			ctx,
			&subsystem.db,
			&mut tx,
			&subsystem.config,
			&subsystem.pruning_config,
			now,
			hash,
			header,
		).await?;
		subsystem.known_blocks.insert(hash, block_number);
	}
	subsystem.db.write(tx)?;

	Ok(())
}

async fn process_new_head<Context>(
	ctx: &mut Context,
	db: &Arc<dyn KeyValueDB>,
	db_transaction: &mut DBTransaction,
	config: &Config,
	pruning_config: &PruningConfig,
	now: Duration,
	hash: Hash,
	header: Header,
) -> Result<(), Error>
where
	Context: overseer::SubsystemContext<Message = AvailabilityStoreMessage>,
{

	let candidate_events = {
		let (tx, rx) = oneshot::channel();
		ctx.send_message(
			RuntimeApiMessage::Request(hash, RuntimeApiRequest::CandidateEvents(tx))
		).await;

		rx.await??
	};

	// We need to request the number of validators based on the parent state,
	// as that is the number of validators used to create this block.
	let n_validators = {
		let (tx, rx) = oneshot::channel();
		ctx.send_message(
			RuntimeApiMessage::Request(header.parent_hash, RuntimeApiRequest::Validators(tx))
		).await;

		rx.await??.len()
	};

	for event in candidate_events {
		match event {
			CandidateEvent::CandidateBacked(receipt, _head, _core_index, _group_index) => {
				note_block_backed(
					db,
					db_transaction,
					config,
					pruning_config,
					now,
					n_validators,
					receipt,
				)?;
			}
			CandidateEvent::CandidateIncluded(receipt, _head, _core_index, _group_index) => {
				note_block_included(
					db,
					db_transaction,
					config,
					pruning_config,
					(header.number, hash),
					receipt,
				)?;
			}
			_ => {}
		}
	}

	Ok(())
}

fn note_block_backed(
	db: &Arc<dyn KeyValueDB>,
	db_transaction: &mut DBTransaction,
	config: &Config,
	pruning_config: &PruningConfig,
	now: Duration,
	n_validators: usize,
	candidate: CandidateReceipt,
) -> Result<(), Error> {
	let candidate_hash = candidate.hash();

	tracing::debug!(
		target: LOG_TARGET,
		?candidate_hash,
		"Candidate backed",
	);

	if load_meta(db, config, &candidate_hash)?.is_none() {
		let meta = CandidateMeta {
			state: State::Unavailable(now.into()),
			data_available: false,
			chunks_stored: bitvec::bitvec![BitOrderLsb0, u8; 0; n_validators],
		};

		let prune_at = now + pruning_config.keep_unavailable_for;

		write_pruning_key(db_transaction, config, prune_at, &candidate_hash);
		write_meta(db_transaction, config, &candidate_hash, &meta);
	}

	Ok(())
}

fn note_block_included(
	db: &Arc<dyn KeyValueDB>,
	db_transaction: &mut DBTransaction,
	config: &Config,
	pruning_config:&PruningConfig,
	block: (BlockNumber, Hash),
	candidate: CandidateReceipt,
) -> Result<(), Error> {
	let candidate_hash = candidate.hash();

	match load_meta(db, config, &candidate_hash)? {
		None => {
			// This is alarming. We've observed a block being included without ever seeing it backed.
			// Warn and ignore.
			tracing::warn!(
				target: LOG_TARGET,
				?candidate_hash,
				"Candidate included without being backed?",
			);
		}
		Some(mut meta) => {
			let be_block = (BEBlockNumber(block.0), block.1);

			tracing::debug!(
				target: LOG_TARGET,
				?candidate_hash,
				"Candidate included",
			);

			meta.state = match meta.state {
				State::Unavailable(at) => {
					let at_d: Duration = at.into();
					let prune_at = at_d + pruning_config.keep_unavailable_for;
					delete_pruning_key(db_transaction, config, prune_at, &candidate_hash);

					State::Unfinalized(at, vec![be_block])
				}
				State::Unfinalized(at, mut within) => {
					if let Err(i) = within.binary_search(&be_block) {
						within.insert(i, be_block);
						State::Unfinalized(at, within)
					} else {
						return Ok(());
					}
				}
				State::Finalized(_at) => {
					// This should never happen as a candidate would have to be included after
					// finality.
					return Ok(())
				}
			};

			write_unfinalized_block_contains(
				db_transaction,
				config,
				block.0,
				&block.1,
				&candidate_hash,
			);
			write_meta(db_transaction, config, &candidate_hash, &meta);
		}
	}

	Ok(())
}

macro_rules! peek_num {
	($iter:ident) => {
		match $iter.peek() {
			Some((k, _)) => decode_unfinalized_key(&k[..]).ok().map(|(b, _, _)| b),
			None => None
		}
	}
}

async fn process_block_finalized<Context>(
	ctx: &mut Context,
	subsystem: &AvailabilityStoreSubsystem,
	finalized_hash: Hash,
	finalized_number: BlockNumber,
) -> Result<(), Error>
where
	Context: overseer::SubsystemContext<Message = AvailabilityStoreMessage>,
{
	let now = subsystem.clock.now()?;

	let mut next_possible_batch = 0;
	loop {
		let mut db_transaction = DBTransaction::new();
		let (start_prefix, end_prefix) = finalized_block_range(finalized_number);

		// We have to do some juggling here of the `iter` to make sure it doesn't cross the `.await` boundary
		// as it is not `Send`. That is why we create the iterator once within this loop, drop it,
		// do an asynchronous request, and then instantiate the exact same iterator again.
		let batch_num = {
			let mut iter = subsystem.db.iter_with_prefix(subsystem.config.col_meta, &start_prefix)
				.take_while(|(k, _)| &k[..] < &end_prefix[..])
				.peekable();

			match peek_num!(iter) {
				None => break, // end of iterator.
				Some(n) => n,
			}
		};

		if batch_num < next_possible_batch { continue } // sanity.
		next_possible_batch = batch_num + 1;

		let batch_finalized_hash = if batch_num == finalized_number {
			finalized_hash
		} else {
			let (tx, rx) = oneshot::channel();
			ctx.send_message(ChainApiMessage::FinalizedBlockHash(batch_num, tx)).await;

			match rx.await?? {
				None => {
					tracing::warn!(target: LOG_TARGET,
						"Availability store was informed that block #{} is finalized, \
						but chain API has no finalized hash.",
						batch_num,
					);

					break
				}
				Some(h) => h,
			}
		};

		let iter = subsystem.db.iter_with_prefix(subsystem.config.col_meta, &start_prefix)
			.take_while(|(k, _)| &k[..] < &end_prefix[..])
			.peekable();

		let batch = load_all_at_finalized_height(iter, batch_num, batch_finalized_hash);

		// Now that we've iterated over the entire batch at this finalized height,
		// update the meta.

		delete_unfinalized_height(&mut db_transaction, &subsystem.config, batch_num);

		update_blocks_at_finalized_height(
			&subsystem,
			&mut db_transaction,
			batch,
			batch_num,
			now,
		)?;

		// We need to write at the end of the loop so the prefix iterator doesn't pick up the same values again
		// in the next iteration. Another unfortunate effect of having to re-initialize the iterator.
		subsystem.db.write(db_transaction)?;
	}

	Ok(())
}

// loads all candidates at the finalized height and maps them to `true` if finalized
// and `false` if unfinalized.
fn load_all_at_finalized_height(
	mut iter: std::iter::Peekable<impl Iterator<Item = (Box<[u8]>, Box<[u8]>)>>,
	block_number: BlockNumber,
	finalized_hash: Hash,
) -> impl IntoIterator<Item = (CandidateHash, bool)> {
	// maps candidate hashes to true if finalized, false otherwise.
	let mut candidates = HashMap::new();

	// Load all candidates that were included at this height.
	loop {
		match peek_num!(iter) {
			None => break, // end of iterator.
			Some(n) if n != block_number => break, // end of batch.
			_ => {}
		}

		let (k, _v) = iter.next().expect("`peek` used to check non-empty; qed");
		let (_, block_hash, candidate_hash) = decode_unfinalized_key(&k[..])
			.expect("`peek_num` checks validity of key; qed");

		if block_hash == finalized_hash {
			candidates.insert(candidate_hash, true);
		} else {
			candidates.entry(candidate_hash).or_insert(false);
		}
	}

	candidates
}

fn update_blocks_at_finalized_height(
	subsystem: &AvailabilityStoreSubsystem,
	db_transaction: &mut DBTransaction,
	candidates: impl IntoIterator<Item = (CandidateHash, bool)>,
	block_number: BlockNumber,
	now: Duration,
) -> Result<(), Error> {
	for (candidate_hash, is_finalized) in candidates {
		let mut meta = match load_meta(&subsystem.db, &subsystem.config, &candidate_hash)? {
			None => {
				tracing::warn!(
					target: LOG_TARGET,
					"Dangling candidate metadata for {}",
					candidate_hash,
				);

				continue;
			}
			Some(c) => c,
		};

		if is_finalized {
			// Clear everything else related to this block. We're finalized now!
			match meta.state {
				State::Finalized(_) => continue, // sanity
				State::Unavailable(at) => {
					// This is also not going to happen; the very fact that we are
					// iterating over the candidate here indicates that `State` should
					// be `Unfinalized`.
					delete_pruning_key(db_transaction, &subsystem.config, at, &candidate_hash);
				}
				State::Unfinalized(_, blocks) => {
					for (block_num, block_hash) in blocks.iter().cloned() {
						// this exact height is all getting cleared out anyway.
						if block_num.0 != block_number {
							delete_unfinalized_inclusion(
								db_transaction,
								&subsystem.config,
								block_num.0,
								&block_hash,
								&candidate_hash,
							);
						}
					}
				}
			}

			meta.state = State::Finalized(now.into());

			// Write the meta and a pruning record.
			write_meta(db_transaction, &subsystem.config, &candidate_hash, &meta);
			write_pruning_key(
				db_transaction,
				&subsystem.config,
				now + subsystem.pruning_config.keep_finalized_for,
				&candidate_hash,
			);
		} else {
			meta.state = match meta.state {
				State::Finalized(_) => continue, // sanity.
				State::Unavailable(_) => continue, // sanity.
				State::Unfinalized(at, mut blocks) => {
					// Clear out everything at this height.
					blocks.retain(|(n, _)| n.0 != block_number);

					// If empty, we need to go back to being unavailable as we aren't
					// aware of any blocks this is included in.
					if blocks.is_empty() {
						let at_d: Duration = at.into();
						let prune_at = at_d + subsystem.pruning_config.keep_unavailable_for;
						write_pruning_key(
							db_transaction,
							&subsystem.config,
							prune_at,
							&candidate_hash,
						);
						State::Unavailable(at)
					} else {
						State::Unfinalized(at, blocks)
					}
				}
			};

			// Update the meta entry.
			write_meta(db_transaction, &subsystem.config, &candidate_hash, &meta)
		}
	}

	Ok(())
}

fn process_message(
	subsystem: &mut AvailabilityStoreSubsystem,
	msg: AvailabilityStoreMessage,
) -> Result<(), Error> {
	match msg {
		AvailabilityStoreMessage::QueryAvailableData(candidate, tx) => {
			let _ = tx.send(load_available_data(&subsystem.db, &subsystem.config, &candidate)?);
		}
		AvailabilityStoreMessage::QueryDataAvailability(candidate, tx) => {
			let a = load_meta(&subsystem.db, &subsystem.config, &candidate)?.map_or(false, |m| m.data_available);
			let _ = tx.send(a);
		}
		AvailabilityStoreMessage::QueryChunk(candidate, validator_index, tx) => {
			let _timer = subsystem.metrics.time_get_chunk();
			let _ = tx.send(load_chunk(&subsystem.db, &subsystem.config, &candidate, validator_index)?);
		}
		AvailabilityStoreMessage::QueryAllChunks(candidate, tx) => {
			match load_meta(&subsystem.db, &subsystem.config, &candidate)? {
				None => {
					let _ = tx.send(Vec::new());
				}
				Some(meta) => {
					let mut chunks = Vec::new();

					for (index, _) in meta.chunks_stored.iter().enumerate().filter(|(_, b)| **b) {
						let _timer = subsystem.metrics.time_get_chunk();
						match load_chunk(
							&subsystem.db,
							&subsystem.config,
							&candidate,
							ValidatorIndex(index as _),
						)? {
							Some(c) => chunks.push(c),
							None => {
								tracing::warn!(
									target: LOG_TARGET,
									?candidate,
									index,
									"No chunk found for set bit in meta"
								);
							}
						}
					}

					let _ = tx.send(chunks);
				}
			}
		}
		AvailabilityStoreMessage::QueryChunkAvailability(candidate, validator_index, tx) => {
			let a = load_meta(&subsystem.db, &subsystem.config, &candidate)?
				.map_or(false, |m|
					*m.chunks_stored.get(validator_index.0 as usize).as_deref().unwrap_or(&false)
				);
			let _ = tx.send(a);
		}
		AvailabilityStoreMessage::StoreChunk {
			candidate_hash,
			chunk,
			tx,
		} => {
			subsystem.metrics.on_chunks_received(1);
			let _timer = subsystem.metrics.time_store_chunk();

			match store_chunk(&subsystem.db, &subsystem.config, candidate_hash, chunk) {
				Ok(true) => {
					let _ = tx.send(Ok(()));
				}
				Ok(false) => {
					let _ = tx.send(Err(()));
				}
				Err(e) => {
					let _ = tx.send(Err(()));
					return Err(e)
				}
			}
		}
		AvailabilityStoreMessage::StoreAvailableData(candidate, _our_index, n_validators, available_data, tx) => {
			subsystem.metrics.on_chunks_received(n_validators as _);

			let _timer = subsystem.metrics.time_store_available_data();

			let res = store_available_data(
				&subsystem,
				candidate,
				n_validators as _,
				available_data,
			);

			match res {
				Ok(()) => {
					let _ = tx.send(Ok(()));
				}
				Err(e) => {
					let _ = tx.send(Err(()));
					return Err(e)
				}
			}
		}
	}

	Ok(())
}

// Ok(true) on success, Ok(false) on failure, and Err on internal error.
fn store_chunk(
	db: &Arc<dyn KeyValueDB>,
	config: &Config,
	candidate_hash: CandidateHash,
	chunk: ErasureChunk,
) -> Result<bool, Error> {
	let mut tx = DBTransaction::new();

	let mut meta = match load_meta(db, config, &candidate_hash)? {
		Some(m) => m,
		None => return Ok(false), // we weren't informed of this candidate by import events.
	};

	match meta.chunks_stored.get(chunk.index.0 as usize).map(|b| *b) {
		Some(true) => return Ok(true), // already stored.
		Some(false) => {
			meta.chunks_stored.set(chunk.index.0 as usize, true);

			write_chunk(&mut tx, config, &candidate_hash, chunk.index, &chunk);
			write_meta(&mut tx, config, &candidate_hash, &meta);
		}
		None => return Ok(false), // out of bounds.
	}

	tracing::debug!(
		target: LOG_TARGET,
		?candidate_hash,
		chunk_index = %chunk.index.0,
		"Stored chunk index for candidate.",
	);

	db.write(tx)?;
	Ok(true)
}

// Ok(true) on success, Ok(false) on failure, and Err on internal error.
fn store_available_data(
	subsystem: &AvailabilityStoreSubsystem,
	candidate_hash: CandidateHash,
	n_validators: usize,
	available_data: AvailableData,
) -> Result<(), Error> {
	let mut tx = DBTransaction::new();

	let mut meta = match load_meta(&subsystem.db, &subsystem.config, &candidate_hash)? {
		Some(m) => {
			if m.data_available {
				return Ok(()); // already stored.
			}

			m
		},
		None => {
			let now = subsystem.clock.now()?;

			// Write a pruning record.
			let prune_at = now + subsystem.pruning_config.keep_unavailable_for;
			write_pruning_key(&mut tx, &subsystem.config, prune_at, &candidate_hash);

			CandidateMeta {
				state: State::Unavailable(now.into()),
				data_available: false,
				chunks_stored: BitVec::new(),
			}
		}
	};

	let chunks = erasure::obtain_chunks_v1(n_validators, &available_data)?;
	let branches = erasure::branches(chunks.as_ref());

	let erasure_chunks = chunks.iter()
		.zip(branches.map(|(proof, _)| proof))
		.enumerate()
		.map(|(index, (chunk, proof))| ErasureChunk {
			chunk: chunk.clone(),
			proof,
			index: ValidatorIndex(index as u32),
		});

	for chunk in erasure_chunks {
		write_chunk(&mut tx, &subsystem.config, &candidate_hash, chunk.index, &chunk);
	}

	meta.data_available = true;
	meta.chunks_stored = bitvec::bitvec![BitOrderLsb0, u8; 1; n_validators];

	write_meta(&mut tx, &subsystem.config,  &candidate_hash, &meta);
	write_available_data(&mut tx, &subsystem.config, &candidate_hash, &available_data);

	subsystem.db.write(tx)?;

	tracing::debug!(
		target: LOG_TARGET,
		?candidate_hash,
		"Stored data and chunks",
	);

	Ok(())
}

fn prune_all(db: &Arc<dyn KeyValueDB>, config: &Config, clock: &dyn Clock) -> Result<(), Error> {
	let now = clock.now()?;
	let (range_start, range_end) = pruning_range(now);

	let mut tx = DBTransaction::new();
	let iter = db.iter_with_prefix(config.col_meta, &range_start[..])
		.take_while(|(k, _)| &k[..] < &range_end[..]);

	for (k, _v) in iter {
		tx.delete(config.col_meta, &k[..]);

		let (_, candidate_hash) = match decode_pruning_key(&k[..]) {
			Ok(m) => m,
			Err(_) => continue, // sanity
		};

		delete_meta(&mut tx, config, &candidate_hash);

		// Clean up all attached data of the candidate.
		if let Some(meta) = load_meta(db, config, &candidate_hash)? {
			// delete available data.
			if meta.data_available {
				delete_available_data(&mut tx, config, &candidate_hash)
			}

			// delete chunks.
			for (i, b) in meta.chunks_stored.iter().enumerate() {
				if *b {
					delete_chunk(&mut tx, config, &candidate_hash, ValidatorIndex(i as _));
				}
			}

			// delete unfinalized block references. Pruning references don't need to be
			// manually taken care of as we are deleting them as we go in the outer loop.
			if let State::Unfinalized(_, blocks) = meta.state {
				for (block_number, block_hash) in blocks {
					delete_unfinalized_inclusion(
						&mut tx,
						config,
						block_number.0,
						&block_hash,
						&candidate_hash,
					);
				}
			}
		}
	}

	db.write(tx)?;
	Ok(())
}

#[derive(Clone)]
struct MetricsInner {
	received_availability_chunks_total: prometheus::Counter<prometheus::U64>,
	pruning: prometheus::Histogram,
	process_block_finalized: prometheus::Histogram,
	block_activated: prometheus::Histogram,
	process_message: prometheus::Histogram,
	store_available_data: prometheus::Histogram,
	store_chunk: prometheus::Histogram,
	get_chunk: prometheus::Histogram,
}

/// Availability metrics.
#[derive(Default, Clone)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_chunks_received(&self, count: usize) {
		if let Some(metrics) = &self.0 {
			use core::convert::TryFrom as _;
			// assume usize fits into u64
			let by = u64::try_from(count).unwrap_or_default();
			metrics.received_availability_chunks_total.inc_by(by);
		}
	}

	/// Provide a timer for `prune_povs` which observes on drop.
	fn time_pruning(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.pruning.start_timer())
	}

	/// Provide a timer for `process_block_finalized` which observes on drop.
	fn time_process_block_finalized(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.process_block_finalized.start_timer())
	}

	/// Provide a timer for `block_activated` which observes on drop.
	fn time_block_activated(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.block_activated.start_timer())
	}

	/// Provide a timer for `process_message` which observes on drop.
	fn time_process_message(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.process_message.start_timer())
	}

	/// Provide a timer for `store_available_data` which observes on drop.
	fn time_store_available_data(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.store_available_data.start_timer())
	}

	/// Provide a timer for `store_chunk` which observes on drop.
	fn time_store_chunk(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.store_chunk.start_timer())
	}

	/// Provide a timer for `get_chunk` which observes on drop.
	fn time_get_chunk(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.get_chunk.start_timer())
	}
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry) -> Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			received_availability_chunks_total: prometheus::register(
				prometheus::Counter::new(
					"parachain_received_availability_chunks_total",
					"Number of availability chunks received.",
				)?,
				registry,
			)?,
			pruning: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_pruning",
						"Time spent within `av_store::prune_all`",
					)
				)?,
				registry,
			)?,
			process_block_finalized: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_process_block_finalized",
						"Time spent within `av_store::process_block_finalized`",
					)
				)?,
				registry,
			)?,
			block_activated: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_block_activated",
						"Time spent within `av_store::process_block_activated`",
					)
				)?,
				registry,
			)?,
			process_message: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_process_message",
						"Time spent within `av_store::process_message`",
					)
				)?,
				registry,
			)?,
			store_available_data: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_store_available_data",
						"Time spent within `av_store::store_available_data`",
					)
				)?,
				registry,
			)?,
			store_chunk: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_store_chunk",
						"Time spent within `av_store::store_chunk`",
					)
				)?,
				registry,
			)?,
			get_chunk: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_get_chunk",
						"Time spent fetching requested chunks.`",
					)
				)?,
				registry,
			)?,
		};
		Ok(Metrics(Some(metrics)))
	}
}
