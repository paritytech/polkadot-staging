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

//! Tests for the subsystem.
//!
//! These primarily revolve around having a backend which is shared between
//! both the test code and the tested subsystem, and which also gives the
//! test code the ability to wait for write operations to occur.

use super::*;
use std::collections::{HashMap, BTreeMap};
use std::sync::Arc;

use futures::channel::oneshot;
use parity_scale_codec::Encode;
use parking_lot::Mutex;
use sp_core::testing::TaskExecutor;
use assert_matches::assert_matches;

use polkadot_primitives::v1::{BlakeTwo256, HashT, ConsensusLog};
use polkadot_subsystem::{jaeger, ActiveLeavesUpdate, ActivatedLeaf, LeafStatus};
use polkadot_subsystem::messages::AllMessages;
use polkadot_node_subsystem_test_helpers as test_helpers;

#[derive(Default)]
struct TestBackendInner {
	leaves: LeafEntrySet,
	block_entries: HashMap<Hash, BlockEntry>,
	blocks_by_number: BTreeMap<BlockNumber, Vec<Hash>>,
	stagnant_at: BTreeMap<Timestamp, Vec<Hash>>,
	// earlier wakers at the back.
	write_wakers: Vec<oneshot::Sender<()>>,
}

#[derive(Clone)]
struct TestBackend {
	inner: Arc<Mutex<TestBackendInner>>,
}

impl TestBackend {
	// Yields a receiver which will be woken up on some future write
	// to the backend along with its position (starting at 0) in the
	// queue.
	//
	// Our tests assume that there is only one task calling this function
	// and the index is useful to get a waker that will trigger after
	// some known amount of writes to the backend that happen internally
	// inside the subsystem.
	//
	// It's important to call this function at points where no writes
	// are pending to the backend. This requires knowing some details
	// about the internals of the subsystem, so the abstraction leaks
	// somewhat, but this is acceptable enough.
	fn await_next_write(&self) -> (usize, oneshot::Receiver<()>) {
		let (tx, rx) = oneshot::channel();

		let mut inner = self.inner.lock();
		let pos = inner.write_wakers.len();
		inner.write_wakers.insert(0, tx);

		(pos, rx)
	}
}

impl Default for TestBackend {
	fn default() -> Self {
		TestBackend {
			inner: Default::default(),
		}
	}
}

impl Backend for TestBackend {
	fn load_block_entry(&self, hash: &Hash) -> Result<Option<BlockEntry>, Error> {
		Ok(self.inner.lock().block_entries.get(hash).map(|e| e.clone()))
	}
	fn load_leaves(&self) -> Result<LeafEntrySet, Error> {
		Ok(self.inner.lock().leaves.clone())
	}
	fn load_stagnant_at(&self, timestamp: Timestamp) -> Result<Vec<Hash>, Error> {
		Ok(self.inner.lock().stagnant_at.get(&timestamp).map_or(Vec::new(), |s| s.clone()))
	}
	fn load_stagnant_at_up_to(&self, up_to: Timestamp)
		-> Result<Vec<(Timestamp, Vec<Hash>)>, Error>
	{
		Ok(self.inner.lock().stagnant_at.range(..=up_to).map(|(t, v)| (*t, v.clone())).collect())
	}
	fn load_first_block_number(&self) -> Result<Option<BlockNumber>, Error> {
		Ok(self.inner.lock().blocks_by_number.range(..).map(|(k, _)| *k).next())
	}
	fn load_blocks_by_number(&self, number: BlockNumber) -> Result<Vec<Hash>, Error> {
		Ok(self.inner.lock().blocks_by_number.get(&number).map_or(Vec::new(), |v| v.clone()))
	}

	fn write<I>(&mut self, ops: I) -> Result<(), Error>
		where I: IntoIterator<Item = BackendWriteOp>
	{
		let mut inner = self.inner.lock();

		for op in ops {
			match op {
				BackendWriteOp::WriteBlockEntry(entry) => {
					inner.block_entries.insert(entry.block_hash, entry);
				}
				BackendWriteOp::WriteBlocksByNumber(number, hashes) => {
					inner.blocks_by_number.insert(number, hashes);
				}
				BackendWriteOp::WriteViableLeaves(leaves) => {
					inner.leaves = leaves;
				}
				BackendWriteOp::WriteStagnantAt(time, hashes) => {
					inner.stagnant_at.insert(time, hashes);
				}
				BackendWriteOp::DeleteBlocksByNumber(number) => {
					inner.blocks_by_number.remove(&number);
				}
				BackendWriteOp::DeleteBlockEntry(hash) => {
					inner.block_entries.remove(&hash);
				}
				BackendWriteOp::DeleteStagnantAt(time) => {
					inner.stagnant_at.remove(&time);
				}
			}
		}

		if let Some(waker) = inner.write_wakers.pop() {
			let _ = waker.send(());
		}
		Ok(())
	}
}

type VirtualOverseer = test_helpers::TestSubsystemContextHandle<ChainSelectionMessage>;

fn test_harness<T: Future<Output=VirtualOverseer>>(
	test: impl FnOnce(TestBackend, VirtualOverseer) -> T
) {
	let pool = TaskExecutor::new();
	let (context, virtual_overseer) = test_helpers::make_subsystem_context(pool);

	let backend = TestBackend::default();
	let subsystem = crate::run(context, backend.clone());

	let test_fut = test(backend, virtual_overseer);
	let test_and_conclude = async move {
		let mut virtual_overseer = test_fut.await;
		virtual_overseer.send(OverseerSignal::Conclude.into()).await;

		// Ensure no messages are pending when the subsystem shuts down.
		assert!(virtual_overseer.try_recv().await.is_none());
	};
	futures::executor::block_on(futures::future::join(subsystem, test_and_conclude));
}

// Answer requests from the subsystem about the finalized block.
async fn answer_finalized_block_info(
	overseer: &mut VirtualOverseer,
	finalized_number: BlockNumber,
	finalized_hash: Hash,
) {
	assert_matches!(
		overseer.recv().await,
		AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(tx)) => {
			let _ = tx.send(Ok(finalized_number));
		}
	);

	assert_matches!(
		overseer.recv().await,
		AllMessages::ChainApi(ChainApiMessage::FinalizedBlockHash(n, tx)) => {
			assert_eq!(n, finalized_number);
			let _ = tx.send(Ok(Some(finalized_hash)));
		}
	);
}

async fn answer_header_request(
	overseer: &mut VirtualOverseer,
	maybe_header: impl Into<Option<Header>>,
) {
	assert_matches!(
		overseer.recv().await,
		AllMessages::ChainApi(ChainApiMessage::BlockHeader(hash, tx)) => {
			let maybe_header = maybe_header.into();
			assert!(maybe_header.as_ref().map_or(true, |h| h.hash() == hash));
			let _ = tx.send(Ok(maybe_header));
		}
	)
}

async fn answer_weight_request(
	overseer: &mut VirtualOverseer,
	hash: Hash,
	weight: impl Into<Option<BlockWeight>>,
) {
	assert_matches!(
		overseer.recv().await,
		AllMessages::ChainApi(ChainApiMessage::BlockWeight(h, tx)) => {
			assert_eq!(h, hash);
			let _ = tx.send(Ok(weight.into()));
		}
	)
}

fn child_header(parent_number: BlockNumber, parent_hash: Hash) -> Header {
	Header {
		parent_hash,
		number: parent_number + 1,
		state_root: Default::default(),
		extrinsics_root: Default::default(),
		digest: Default::default()
	}
}

fn salt_header(header: &mut Header, salt: impl Encode) {
	header.state_root = BlakeTwo256::hash_of(&salt)
}

fn add_reversions(
	header: &mut Header,
	reversions: impl IntoIterator<Item=BlockNumber>,
) {
	for log in reversions.into_iter().map(ConsensusLog::Revert) {
		header.digest.logs.push(log.into())
	}
}

// Builds a chain on top of the given base, with one block for each
// provided weight.
fn construct_chain_on_base(
	weights: impl IntoIterator<Item = BlockWeight>,
	base_number: BlockNumber,
	base_hash: Hash,
	mut mutate: impl FnMut(&mut Header),
) -> (Hash, Vec<(Header, BlockWeight)>) {
	let mut parent_number = base_number;
	let mut parent_hash = base_hash;

	let mut chain = Vec::new();
	for weight in weights {
		let mut header = child_header(parent_number, parent_hash);
		mutate(&mut header);

		parent_number = header.number;
		parent_hash = header.hash();
		chain.push((header, weight));
	}

	(parent_hash, chain)
}

// import blocks 1-by-1. If `finalized_base` is supplied,
// it will be answered before the first block in `answers.
async fn import_blocks_into(
	virtual_overseer: &mut VirtualOverseer,
	backend: &TestBackend,
	mut finalized_base: Option<(BlockNumber, Hash)>,
	blocks: Vec<(Header, BlockWeight)>,
) {
	for (header, weight) in blocks {
		let (_, write_rx) = backend.await_next_write();

		let hash = header.hash();
		virtual_overseer.send(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(
			ActivatedLeaf {
				hash,
				number: header.number,
				status: LeafStatus::Fresh,
				span: Arc::new(jaeger::Span::Disabled),
			}
		)).into()).await;

		if let Some((f_n, f_h)) = finalized_base.take() {
			answer_finalized_block_info(virtual_overseer, f_n, f_h).await;
		}

		answer_header_request(virtual_overseer, header.clone()).await;
		answer_weight_request(virtual_overseer, hash, weight).await;

		write_rx.await.unwrap();
	}
}

async fn import_chains_into_empty(
	virtual_overseer: &mut VirtualOverseer,
	backend: &TestBackend,
	finalized_number: BlockNumber,
	finalized_hash: Hash,
	chains: Vec<Vec<(Header, BlockWeight)>>,
) {
	for (i, chain)in chains.into_iter().enumerate() {
		let finalized_base = Some((finalized_number, finalized_hash)).filter(|_| i == 0);
		import_blocks_into(
			virtual_overseer,
			backend,
			finalized_base,
			chain,
		).await;
	}
}

// Import blocks all at once. This assumes that the ancestor is known/finalized
// but none of the other blocks.
// import blocks 1-by-1. If `finalized_base` is supplied,
// it will be answered before the first block.
//
// some pre-blocks may need to be supplied to answer ancestry requests
// that gather batches beyond the beginning of the new chain.
// pre-blocks are those already known by the subsystem, however,
// the subsystem has no way of knowin that until requesting ancestry.
async fn import_all_blocks_into(
	virtual_overseer: &mut VirtualOverseer,
	backend: &TestBackend,
	finalized_base: Option<(BlockNumber, Hash)>,
	pre_blocks: Vec<Header>,
	blocks: Vec<(Header, BlockWeight)>,
) {
	assert!(blocks.len() > 1, "gap only makes sense if importing multiple blocks");

	let head = blocks.last().unwrap().0.clone();
	let head_hash = head.hash();

	let (_, write_rx) = backend.await_next_write();
	virtual_overseer.send(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(
		ActivatedLeaf {
			hash: head_hash,
			number: head.number,
			status: LeafStatus::Fresh,
			span: Arc::new(jaeger::Span::Disabled),
		}
	)).into()).await;

	if let Some((f_n, f_h)) = finalized_base {
		answer_finalized_block_info(virtual_overseer, f_n, f_h).await;
	}

	// Head is always fetched first.
	answer_header_request(virtual_overseer, head).await;

	// Answer header and ancestry requests until the parent of head
	// is imported.
	{
		let find_block_header = |expected_hash| {
			pre_blocks.iter().cloned()
				.chain(blocks.iter().map(|(h, _)| h.clone()))
				.find(|hdr| hdr.hash() == expected_hash)
				.unwrap()
		};

		let mut behind_head = 0;
		loop {
			let nth_ancestor_of_head = |n: usize| {
				// blocks: [d, e, f, head]
				// pre: [a, b, c]
				//
				// [a, b, c, d, e, f, head]
				// [6, 5, 4, 3, 2, 1, 0]

				let new_ancestry_end = blocks.len() - 1;
				if n > new_ancestry_end {
					// [6, 5, 4] -> [2, 1, 0]
					let n_in_pre = n - blocks.len();
					let pre_blocks_end = pre_blocks.len() - 1;
					pre_blocks[pre_blocks_end - n_in_pre].clone()
				} else {
					let blocks_end = blocks.len() - 1;
					blocks[blocks_end - n].0.clone()
				}
			};

			match virtual_overseer.recv().await {
				AllMessages::ChainApi(ChainApiMessage::Ancestors {
					hash: h,
					k,
					response_channel: tx,
				}) => {
					let prev_response = nth_ancestor_of_head(behind_head);
					assert_eq!(h, prev_response.hash());

					let _ = tx.send(Ok(
						(0..k as usize).map(|n| n + behind_head + 1)
							.map(nth_ancestor_of_head)
							.map(|h| h.hash())
							.collect()
					));

					for _ in 0..k {
						assert_matches!(
							virtual_overseer.recv().await,
							AllMessages::ChainApi(ChainApiMessage::BlockHeader(h, tx)) => {
								let header = find_block_header(h);
								let _ = tx.send(Ok(Some(header)));
							}
						)
					}

					behind_head = behind_head + k as usize;
				}
				AllMessages::ChainApi(ChainApiMessage::BlockHeader(h, tx)) => {
					let header = find_block_header(h);
					let _ = tx.send(Ok(Some(header)));

					// Assuming that `determine_new_blocks` uses these
					// instead of ancestry: 1.
					behind_head += 1;
				}
				AllMessages::ChainApi(ChainApiMessage::BlockWeight(h, tx)) => {
					let (_, weight) = blocks.iter().find(|(hdr, _)| hdr.hash() == h).unwrap();
					let _ = tx.send(Ok(Some(*weight)));

					// Last weight has been returned. Time to go.
					if h == head_hash { break }
				}
				_ => panic!("unexpected message"),
			}
		}
	}
	write_rx.await.unwrap();
}

async fn finalize_block(
	virtual_overseer: &mut VirtualOverseer,
	backend: &TestBackend,
	block_number: BlockNumber,
	block_hash: Hash,
) {
	let (_, write_tx) = backend.await_next_write();

	virtual_overseer.send(
		OverseerSignal::BlockFinalized(block_hash, block_number).into()
	).await;

	write_tx.await.unwrap();
}

fn extract_info_from_chain(i: usize, chain: &[(Header, BlockWeight)])
	-> (BlockNumber, Hash, BlockWeight)
{
	let &(ref header, weight) = &chain[i];

	(header.number, header.hash(), weight)
}

fn assert_backend_contains<'a>(
	backend: &TestBackend,
	headers: impl IntoIterator<Item = &'a Header>,
) {
	for header in headers {
		let hash = header.hash();
		assert!(
			backend.load_blocks_by_number(header.number).unwrap().contains(&hash),
			"blocks at {} does not contain {}",
			header.number,
			hash,
		);
		assert!(
			backend.load_block_entry(&hash).unwrap().is_some(),
			"no entry found for {}",
			hash,
		);
	}
}

fn assert_backend_contains_chains(
	backend: &TestBackend,
	chains: Vec<Vec<(Header, BlockWeight)>>,
) {
	for chain in chains {
		assert_backend_contains(
			backend,
			chain.iter().map(|&(ref hdr, _)| hdr)
		)
	}
}

fn assert_leaves(
	backend: &TestBackend,
	leaves: Vec<Hash>,
) {
	assert_eq!(
		backend.load_leaves().unwrap().into_hashes_descending().into_iter().collect::<Vec<_>>(),
		leaves,
	)
}

#[test]
fn no_op_subsystem_run() {
	test_harness(|_, virtual_overseer| async move { virtual_overseer });
}

#[test]
fn import_direct_child_of_finalized_on_empty() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		let child = child_header(finalized_number, finalized_hash);
		let child_hash = child.hash();
		let child_weight = 1;
		let child_number = child.number;

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			vec![(child.clone(), child_weight)],
		).await;

		assert_eq!(backend.load_first_block_number().unwrap().unwrap(), child_number);
		assert_backend_contains(&backend, &[child]);
		assert_leaves(&backend, vec![child_hash]);

		virtual_overseer
	})
}

#[test]
fn import_chain_on_finalized_incrementally() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		let (head_hash, chain) = construct_chain_on_base(
			vec![1, 2, 3, 4, 5],
			finalized_number,
			finalized_hash,
			|_| {}
		);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			chain.clone(),
		).await;

		assert_eq!(backend.load_first_block_number().unwrap().unwrap(), 1);
		assert_backend_contains(&backend, chain.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![head_hash]);

		virtual_overseer
	})
}

#[test]
fn import_two_subtrees_on_finalized() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		let (a_hash, chain_a) = construct_chain_on_base(
			vec![1],
			finalized_number,
			finalized_hash,
			|_| {}
		);

		let (b_hash, chain_b) = construct_chain_on_base(
			vec![2],
			finalized_number,
			finalized_hash,
			|h| salt_header(h, b"b"),
		);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			chain_a.clone(),
		).await;

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			None,
			chain_b.clone(),
		).await;

		assert_eq!(backend.load_first_block_number().unwrap().unwrap(), 1);
		assert_backend_contains(&backend, chain_a.iter().map(|&(ref h, _)| h));
		assert_backend_contains(&backend, chain_b.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![b_hash, a_hash]);

		virtual_overseer
	})
}

#[test]
fn import_two_subtrees_on_nonzero_finalized() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 100;
		let finalized_hash = Hash::repeat_byte(0);

		let (a_hash, chain_a) = construct_chain_on_base(
			vec![1],
			finalized_number,
			finalized_hash,
			|_| {}
		);

		let (b_hash, chain_b) = construct_chain_on_base(
			vec![2],
			finalized_number,
			finalized_hash,
			|h| salt_header(h, b"b"),
		);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			chain_a.clone(),
		).await;

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			None,
			chain_b.clone(),
		).await;

		assert_eq!(backend.load_first_block_number().unwrap().unwrap(), 101);
		assert_backend_contains(&backend, chain_a.iter().map(|&(ref h, _)| h));
		assert_backend_contains(&backend, chain_b.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![b_hash, a_hash]);

		virtual_overseer
	})
}

#[test]
fn leaves_ordered_by_weight_and_then_number() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		// F <- A1 <- A2 <- A3
		//      A1 <- B2
		// F <- C1 <- C2
		//
		// expected_leaves: [(C2, 3), (A3, 2), (B2, 2)]

		let (a3_hash, chain_a) = construct_chain_on_base(
			vec![1, 1, 2],
			finalized_number,
			finalized_hash,
			|_| {}
		);

		let (_, a1_hash, _) = extract_info_from_chain(0, &chain_a);

		let (b2_hash, chain_b) = construct_chain_on_base(
			vec![2],
			1,
			a1_hash,
			|h| salt_header(h, b"b"),
		);

		let (c2_hash, chain_c) = construct_chain_on_base(
			vec![1, 3],
			finalized_number,
			finalized_hash,
			|h| salt_header(h, b"c"),
		);

		import_chains_into_empty(
			&mut virtual_overseer,
			&backend,
			finalized_number,
			finalized_hash,
			vec![chain_a.clone(), chain_b.clone(), chain_c.clone()],
		).await;

		assert_eq!(backend.load_first_block_number().unwrap().unwrap(), 1);
		assert_backend_contains(&backend, chain_a.iter().map(|&(ref h, _)| h));
		assert_backend_contains(&backend, chain_b.iter().map(|&(ref h, _)| h));
		assert_backend_contains(&backend, chain_c.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![c2_hash, a3_hash, b2_hash]);
		virtual_overseer
	});
}

#[test]
fn subtrees_imported_even_with_gaps() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		// F <- A1 <- A2 <- A3
		//            A2 <- B3 <- B4 <- B5

		let (a3_hash, chain_a) = construct_chain_on_base(
			vec![1, 2, 3],
			finalized_number,
			finalized_hash,
			|_| {}
		);

		let (_, a2_hash, _) = extract_info_from_chain(1, &chain_a);

		let (b5_hash, chain_b) = construct_chain_on_base(
			vec![4, 4, 5],
			2,
			a2_hash,
			|h| salt_header(h, b"b"),
		);

		import_all_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			Vec::new(),
			chain_a.clone(),
		).await;

		import_all_blocks_into(
			&mut virtual_overseer,
			&backend,
			None,
			vec![chain_a[0].0.clone(), chain_a[1].0.clone()],
			chain_b.clone(),
		).await;

		assert_eq!(backend.load_first_block_number().unwrap().unwrap(), 1);
		assert_backend_contains(&backend, chain_a.iter().map(|&(ref h, _)| h));
		assert_backend_contains(&backend, chain_b.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![b5_hash, a3_hash]);

		virtual_overseer
	});
}

#[test]
fn reversion_removes_viability_of_chain() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		// F <- A1 <- A2 <- A3.
		//
		// A3 reverts A1

		let (_a3_hash, chain_a) = construct_chain_on_base(
			vec![1, 2, 3],
			finalized_number,
			finalized_hash,
			|h| if h.number == 3 { add_reversions(h, Some(1)) }
		);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			chain_a.clone(),
		).await;

		assert_backend_contains(&backend, chain_a.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![]);

		virtual_overseer
	});
}

#[test]
fn reversion_removes_viability_and_finds_ancestor_as_leaf() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		// F <- A1 <- A2 <- A3.
		//
		// A3 reverts A2

		let (_a3_hash, chain_a) = construct_chain_on_base(
			vec![1, 2, 3],
			finalized_number,
			finalized_hash,
			|h| if h.number == 3 { add_reversions(h, Some(2)) }
		);

		let (_, a1_hash, _) = extract_info_from_chain(0, &chain_a);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			chain_a.clone(),
		).await;

		assert_backend_contains(&backend, chain_a.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![a1_hash]);

		virtual_overseer
	});
}

#[test]
fn ancestor_of_unviable_is_not_leaf_if_has_children() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		// F <- A1 <- A2 <- A3.
		//      A1 <- B2
		//
		// A3 reverts A2

		let (a2_hash, chain_a) = construct_chain_on_base(
			vec![1, 2],
			finalized_number,
			finalized_hash,
			|_| {}
		);

		let (_, a1_hash, _) = extract_info_from_chain(0, &chain_a);

		let (_a3_hash, chain_a_ext) = construct_chain_on_base(
			vec![3],
			2,
			a2_hash,
			|h| add_reversions(h, Some(2)),
		);

		let (b2_hash, chain_b) = construct_chain_on_base(
			vec![1],
			1,
			a1_hash,
			|h| salt_header(h, b"b")
		);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			chain_a.clone(),
		).await;

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			None,
			chain_b.clone(),
		).await;

		assert_backend_contains(&backend, chain_a.iter().map(|&(ref h, _)| h));
		assert_backend_contains(&backend, chain_b.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![a2_hash, b2_hash]);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			None,
			chain_a_ext.clone(),
		).await;

		assert_backend_contains(&backend, chain_a.iter().map(|&(ref h, _)| h));
		assert_backend_contains(&backend, chain_a_ext.iter().map(|&(ref h, _)| h));
		assert_backend_contains(&backend, chain_b.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![b2_hash]);

		virtual_overseer
	});
}

#[test]
fn self_and_future_reversions_are_ignored() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		// F <- A1 <- A2 <- A3.
		//
		// A3 reverts itself and future blocks. ignored.

		let (a3_hash, chain_a) = construct_chain_on_base(
			vec![1, 2, 3],
			finalized_number,
			finalized_hash,
			|h| if h.number == 3 { add_reversions(h, vec![3, 4, 100]) }
		);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			chain_a.clone(),
		).await;

		assert_backend_contains(&backend, chain_a.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![a3_hash]);

		virtual_overseer
	});
}

#[test]
fn revert_finalized_is_ignored() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 10;
		let finalized_hash = Hash::repeat_byte(0);

		// F <- A1 <- A2 <- A3.
		//
		// A3 reverts itself and future blocks. ignored.

		let (a3_hash, chain_a) = construct_chain_on_base(
			vec![1, 2, 3],
			finalized_number,
			finalized_hash,
			|h| if h.number == 13 { add_reversions(h, vec![10, 9, 8, 0, 1]) }
		);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			chain_a.clone(),
		).await;

		assert_backend_contains(&backend, chain_a.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![a3_hash]);

		virtual_overseer
	});
}

#[test]
fn reversion_affects_viability_of_all_subtrees() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		// F <- A1 <- A2 <- A3.
		//            A2 <- B3 <- B4
		//
		// B4 reverts A2.

		let (a3_hash, chain_a) = construct_chain_on_base(
			vec![1, 2, 3],
			finalized_number,
			finalized_hash,
			|_| {}
		);

		let (_, a1_hash, _) = extract_info_from_chain(0, &chain_a);
		let (_, a2_hash, _) = extract_info_from_chain(1, &chain_a);

		let (_b4_hash, chain_b) = construct_chain_on_base(
			vec![3, 4],
			2,
			a2_hash,
			|h| {
				salt_header(h, b"b");
				if h.number == 4 {
					add_reversions(h, Some(2));
				}
			}
		);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			chain_a.clone(),
		).await;

		assert_leaves(&backend, vec![a3_hash]);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			None,
			chain_b.clone(),
		).await;

		assert_backend_contains(&backend, chain_a.iter().map(|&(ref h, _)| h));
		assert_backend_contains(&backend, chain_b.iter().map(|&(ref h, _)| h));
		assert_leaves(&backend, vec![a1_hash]);

		virtual_overseer
	});
}

#[test]
fn finalize_viable_prunes_subtrees() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		//            A2 <- X3
		// F <- A1 <- A2 <- A3.
		//      A1 <- B2
		// F <- C1 <- C2 <- C3
		//            C2 <- D3
		//
		// Finalize A2. Only A2, A3, and X3 should remain.

		let (a3_hash, chain_a) = construct_chain_on_base(
			vec![1, 2, 10],
			finalized_number,
			finalized_hash,
			|h| salt_header(h, b"a"),
		);

		let (_, a1_hash, _) = extract_info_from_chain(0, &chain_a);
		let (_, a2_hash, _) = extract_info_from_chain(1, &chain_a);

		let (x3_hash, chain_x) = construct_chain_on_base(
			vec![3],
			2,
			a2_hash,
			|h| salt_header(h, b"x"),
		);

		let (b2_hash, chain_b) = construct_chain_on_base(
			vec![6],
			1,
			a1_hash,
			|h| salt_header(h, b"b"),
		);

		let (c3_hash, chain_c) = construct_chain_on_base(
			vec![1, 2, 8],
			finalized_number,
			finalized_hash,
			|h| salt_header(h, b"c"),
		);
		let (_, c2_hash, _) = extract_info_from_chain(1, &chain_c);

		let (d3_hash, chain_d) = construct_chain_on_base(
			vec![7],
			2,
			c2_hash,
			|h| salt_header(h, b"d"),
		);

		let all_chains = vec![
			chain_a.clone(),
			chain_x.clone(),
			chain_b.clone(),
			chain_c.clone(),
			chain_d.clone(),
		];

		import_chains_into_empty(
			&mut virtual_overseer,
			&backend,
			finalized_number,
			finalized_hash,
			all_chains.clone(),
		).await;

		assert_backend_contains_chains(
			&backend,
			all_chains.clone(),
		);
		assert_leaves(&backend, vec![a3_hash, c3_hash, d3_hash, b2_hash, x3_hash]);

		// Finalize block A2. Now lots of blocks should go missing.
		finalize_block(
			&mut virtual_overseer,
			&backend,
			2,
			a2_hash,
		).await;


		// A2 <- A3
		// A2 <- X3

		assert_leaves(&backend, vec![a3_hash, x3_hash]);
		assert_eq!(
			backend.load_first_block_number().unwrap().unwrap(),
			3,
		);

		assert_eq!(
			backend.load_blocks_by_number(3).unwrap(),
			vec![a3_hash, x3_hash],
		);

		virtual_overseer
	});
}

#[test]
fn finalization_does_not_clobber_unviability() {
	test_harness(|backend, mut virtual_overseer| async move {
		let finalized_number = 0;
		let finalized_hash = Hash::repeat_byte(0);

		// F <- A1 <- A2 <- A3
		// A3 reverts A2.
		// Finalize A1.

		let (_a3_hash, chain_a) = construct_chain_on_base(
			vec![1, 2, 10],
			finalized_number,
			finalized_hash,
			|h| {
				salt_header(h, b"a");
				if h.number == 3 {
					add_reversions(h, Some(2));
				}
			}
		);

		let (_, a1_hash, _) = extract_info_from_chain(0, &chain_a);

		import_blocks_into(
			&mut virtual_overseer,
			&backend,
			Some((finalized_number, finalized_hash)),
			chain_a.clone(),
		).await;

		finalize_block(
			&mut virtual_overseer,
			&backend,
			1,
			a1_hash,
		).await;

		let a2_onwards = chain_a[1..].to_vec();
		assert_leaves(&backend, vec![]);
		assert_backend_contains_chains(&backend, vec![a2_onwards]);

		virtual_overseer
	});
}

// TODO [now]: finalize an unviable block with viable descendants
// TODO [now]: finalize an unviable block with unviable descendants down the line

// TODO [now]; test find best leaf containing with no leaves.
// TODO [now]: find best leaf containing when required is finalized
// TODO [now]: find best leaf containing when required is unfinalized.
// TODO [now]: find best leaf containing when required is ancestor of many leaves.

// TODO [now]: test assumption that each approved block gives 1 DB write.
// TODO [now]: test assumption that each finalized block gives 1 DB write.
