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


//! Make the set of voting bag thresholds to be used in `voter_bags.rs`.
//!
//! Generally speaking this script can be run once per runtime and never
//! touched again. It can be reused to regenerate a wholly different 
//! quantity of bags, or if the existential deposit changes, etc. 

use pallet_staking::voter_bags::make_bags::generate_thresholds_module;
use std::path::{Path, PathBuf};
use structopt::{StructOpt, clap::arg_enum};
use westend_runtime::Runtime as WestendRuntime;
use kusama_runtime::Runtime as KusamaRuntime;
use polkadot_runtime::Runtime as PolkadotRuntime;

arg_enum!{
	#[derive(Debug)]
	enum Runtime {
		Westend,
		Kusama,
		Polkadot,
	}
}

impl Runtime {
	fn generate_thresholds(&self) -> Box<dyn FnOnce(usize, &Path) -> Result<(), std::io::Error>> {
		match self {
			Runtime::Westend => Box::new(generate_thresholds_module::<WestendRuntime>),
			Runtime::Kusama => Box::new(generate_thresholds_module::<KusamaRuntime>),
			Runtime::Polkadot => Box::new(generate_thresholds_module::<PolkadotRuntime>),
		}
	}
}

#[derive(Debug, StructOpt)]
struct Opt {
	/// How many bags to generate.
	#[structopt(
		long,
		default_value = "200",
	)]
	n_bags: usize,

	/// Which runtime to generate.
	#[structopt(
		long,
		case_insensitive = true,
		default_value = "Polkadot",
		possible_values = &Runtime::variants(),
	)]
	runtime: Runtime,

	/// Where to write the output.
	output: PathBuf,
}

fn main() -> Result<(), std::io::Error> {
	let Opt {n_bags, output, runtime } = Opt::from_args();
	let mut ext = sp_io::TestExternalities::new_empty();
	ext.execute_with(|| runtime.generate_thresholds()(n_bags, &output))
}
