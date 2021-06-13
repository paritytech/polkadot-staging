// Copyright 2017-2020 Parity Technologies (UK) Ltd.
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

//! A malicious actor.

#![warn(missing_docs)]

use color_eyre::eyre;
use polkadot_cli::{
	Cli,
	service::{
		OverseerGen,
		OverseerGenArgs,
		RealOverseerGen,
		SpawnNamed,
		Block,
		ParachainHost,
		ProvideRuntimeApi,
		AuthorityDiscoveryApi,
	},
	Error,
};
use std::sync::Arc;

struct MisbehaveVariantA;

impl OverseerGen for MisbehaveVariantA {
	fn generate<'a, Spawner, RuntimeClient>(&self, args: OverseerGenArgs<'a, Spawner, RuntimeClient>) -> Result<(Overseer<Spawner, Arc<RuntimeClient>>, OverseerHandler), Error>
	where
		RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
		RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
		Spawner: 'static + SpawnNamed + Clone + Unpin
	{
		let gen = RealOverseerGen;
		RealOverseerGen::generate::<'a, Spawner, RuntimeClient>(gen, args)
	}
}

fn main() -> eyre::Result<()> {
	color_eyre::install()?;
	let cli = Cli::from_args();
	assert_matches::assert_matches!(cli.subcommand, None);
	polkadot_cli::run_node(cli, MisbehaveVariantA)?;
	Ok(())
}
