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

use crate::{Client, FullBackend};
use polkadot_test_runtime::GetLastTimestamp;
use polkadot_primitives::v1::Block;
use sp_runtime::generic::BlockId;
use sp_api::ProvideRuntimeApi;
use sc_block_builder::BlockBuilderProvider;
use sp_state_machine::BasicExternalities;

/// An extension for the test client to init a Polkadot specific block builder.
pub trait InitPolkadotBlockBuilder {
	/// Init a Polkadot specific block builder that works for the test runtime.
	///
	/// This will automatically create and push the inherents for you to make the block valid for the test runtime.
	fn init_polkadot_block_builder<'a>(&'a self) -> sc_block_builder::BlockBuilder<'a, Block, Client, FullBackend>;

	/// Init a Polkadot specific block builder at a specific block that works for the test runtime.
	///
	/// Same as [`InitPolkadotBlockBuilder::init_polkadot_block_builder`] besides that it takes a [`BlockId`] to say
	/// which should be the parent block of the block that is being build.
	fn init_polkadot_block_builder_at<'a>(
		&'a self,
		at: &BlockId<Block>,
	) -> sc_block_builder::BlockBuilder<'a, Block, Client, FullBackend>;
}

impl InitPolkadotBlockBuilder for Client {
	fn init_polkadot_block_builder<'a>(
		&'a self,
	) -> sc_block_builder::BlockBuilder<'a, Block, Client, FullBackend> {
		let chain_info = self.chain_info();
		self.init_polkadot_block_builder_at(&BlockId::Hash(chain_info.best_hash))
	}

	fn init_polkadot_block_builder_at<'a>(
		&'a self,
		at: &BlockId<Block>,
	) -> sc_block_builder::BlockBuilder<'a, Block, Client, FullBackend> {
		let mut block_builder = self.new_block_at(at, Default::default(), false)
			.expect("Creates new block builder for test runtime");

		let mut inherent_data = sp_inherents::InherentData::new();
		let last_timestamp = self
			.runtime_api()
			.get_last_timestamp(&at)
			.expect("Get last timestamp");

		// `MinimumPeriod` is a storage parameter type that requires externalities to access the value.
		let minimum_period= BasicExternalities::new_empty()
			.execute_with(|| polkadot_test_runtime::MinimumPeriod::get());

		let timestamp = last_timestamp + minimum_period;

		inherent_data
			.put_data(sp_timestamp::INHERENT_IDENTIFIER, &timestamp)
			.expect("Put timestamp failed");

		let inherents = block_builder.create_inherents(inherent_data).expect("Creates inherents");

		inherents.into_iter().for_each(|ext| block_builder.push(ext).expect("Pushes inherent"));

		block_builder
	}
}
