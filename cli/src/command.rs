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

use log::info;
use sp_runtime::traits::BlakeTwo256;
use service::{IsKusama, Block, self, RuntimeApiCollection, TFullClient};
use sp_api::ConstructRuntimeApi;
use sc_cli::{substrate_cli, SubstrateCli, Result};
use sc_executor::NativeExecutionDispatch;
use crate::cli::{Cli, Subcommand};

#[substrate_cli(
	impl_name = "parity-polkadot",
	support_url = "https://github.com/paritytech/polkadot/issues/new",
	copyright_start_year = 2017,
	executable_name = "polkadot",
)]
impl SubstrateCli for Cli {
	fn load_spec(&self, id: &str) -> std::result::Result<Box<dyn sc_service::ChainSpec>, String> {
		Ok(match id {
			"polkadot-dev" | "dev" => Box::new(service::chain_spec::polkadot_development_config()),
			"polkadot-local" => Box::new(service::chain_spec::polkadot_local_testnet_config()),
			"polkadot-staging" => Box::new(service::chain_spec::polkadot_staging_testnet_config()),
			"kusama-dev" => Box::new(service::chain_spec::kusama_development_config()),
			"kusama-local" => Box::new(service::chain_spec::kusama_local_testnet_config()),
			"kusama-staging" => Box::new(service::chain_spec::kusama_staging_testnet_config()),
			"westend" => Box::new(service::chain_spec::westend_config()?),
			"kusama" | "" => Box::new(service::chain_spec::kusama_config()?),
			path if self.run.force_kusama => {
				Box::new(service::KusamaChainSpec::from_json_file(std::path::PathBuf::from(path))?)
			},
			path => Box::new(service::PolkadotChainSpec::from_json_file(std::path::PathBuf::from(path))?),
		})
	}
}

/// Parses polkadot specific CLI arguments and run the service.
pub fn run() -> Result<()> {
	let cli = Cli::from_args();

	match &cli.subcommand {
		None => {
			let runtime = cli.create_runner(&cli.run.base)?;
			let config = runtime.config();
			let is_kusama = config.chain_spec.is_kusama();
			let authority_discovery_enabled = cli.run.authority_discovery_enabled;
			let grandpa_pause = if cli.run.grandpa_pause.is_empty() {
				None
			} else {
				Some((cli.run.grandpa_pause[0], cli.run.grandpa_pause[1]))
			};

			if is_kusama {
				info!("⛓  Native runtime: {}", service::KusamaExecutor::native_version().runtime_version);
				info!("----------------------------");
				info!("This chain is not in any way");
				info!("      endorsed by the       ");
				info!("     KUSAMA FOUNDATION      ");
				info!("----------------------------");

				run_node::<
					service::kusama_runtime::RuntimeApi,
					service::KusamaExecutor,
					service::kusama_runtime::UncheckedExtrinsic,
				>(runtime, authority_discovery_enabled, grandpa_pause)
			} else {
				info!("⛓  Native runtime: {}", service::PolkadotExecutor::native_version().runtime_version);

				run_node::<
					service::polkadot_runtime::RuntimeApi,
					service::PolkadotExecutor,
					service::polkadot_runtime::UncheckedExtrinsic,
				>(runtime, authority_discovery_enabled, grandpa_pause)
			}
		},
		Some(Subcommand::Base(subcommand)) => {
			let runtime = cli.create_runner(subcommand)?;
			let is_kusama = runtime.config().chain_spec.is_kusama();

			if is_kusama {
				runtime.run_subcommand(subcommand, |config|
					service::new_chain_ops::<
						service::kusama_runtime::RuntimeApi,
						service::KusamaExecutor,
						service::kusama_runtime::UncheckedExtrinsic,
					>(config)
				)
			} else {
				runtime.run_subcommand(subcommand, |config|
					service::new_chain_ops::<
						service::polkadot_runtime::RuntimeApi,
						service::PolkadotExecutor,
						service::polkadot_runtime::UncheckedExtrinsic,
					>(config)
				)
			}
		},
		Some(Subcommand::ValidationWorker(cmd)) => {
			sc_cli::init_logger("");

			if cfg!(feature = "browser") {
				Err(sc_cli::Error::Input("Cannot run validation worker in browser".into()))
			} else {
				#[cfg(not(feature = "browser"))]
				service::run_validation_worker(&cmd.mem_id)?;
				Ok(())
			}
		},
		Some(Subcommand::Benchmark(cmd)) => {
			let runtime = cli.create_runner(cmd)?;
			let is_kusama = runtime.config().chain_spec.is_kusama();

			if is_kusama {
				runtime.sync_run(|config| {
					cmd.run::<service::kusama_runtime::Block, service::KusamaExecutor>(config)
				})
			} else {
				runtime.sync_run(|config| {
					cmd.run::<service::polkadot_runtime::Block, service::PolkadotExecutor>(config)
				})
			}
		},
	}
}

fn run_node<R, D, E>(
	runtime: sc_cli::Runner<Cli>,
	authority_discovery_enabled: bool,
	grandpa_pause: Option<(u32, u32)>,
) -> sc_cli::Result<()>
where
	R: ConstructRuntimeApi<Block, service::TFullClient<Block, R, D>>
		+ Send + Sync + 'static,
	<R as ConstructRuntimeApi<Block, service::TFullClient<Block, R, D>>>::RuntimeApi:
		RuntimeApiCollection<E, StateBackend = sc_client_api::StateBackendFor<service::TFullBackend<Block>, Block>>,
	<R as ConstructRuntimeApi<Block, service::TLightClient<Block, R, D>>>::RuntimeApi:
		RuntimeApiCollection<E, StateBackend = sc_client_api::StateBackendFor<service::TLightBackend<Block>, Block>>,
	E: service::Codec + Send + Sync + 'static,
	D: service::NativeExecutionDispatch + 'static,
	// Rust bug: https://github.com/rust-lang/rust/issues/24159
	<<R as ConstructRuntimeApi<Block, TFullClient<Block, R, D>>>::RuntimeApi as sp_api::ApiExt<Block>>::StateBackend:
		sp_api::StateBackend<BlakeTwo256>,
	// Rust bug: https://github.com/rust-lang/rust/issues/43580
	R: ConstructRuntimeApi<
		Block,
		TLightClient<R, D>
	>,
{
	runtime.run_node(
		|config| service::new_light::<R, D, E>(config),
		|config| service::new_full::<R, D, E>(
			config,
			None,
			None,
			authority_discovery_enabled,
			6000,
			grandpa_pause,
		).map(|(s, _)| s),
	)
}

// We can't simply use `service::TLightClient` due to a
// Rust bug: https://github.com/rust-lang/rust/issues/43580
type TLightClient<Runtime, Dispatch> = sc_client::Client<
	sc_client::light::backend::Backend<sc_client_db::light::LightStorage<Block>, BlakeTwo256>,
	sc_client::light::call_executor::GenesisCallExecutor<
		sc_client::light::backend::Backend<sc_client_db::light::LightStorage<Block>, BlakeTwo256>,
		sc_client::LocalCallExecutor<
			sc_client::light::backend::Backend<sc_client_db::light::LightStorage<Block>, BlakeTwo256>,
			sc_executor::NativeExecutor<Dispatch>
		>
	>,
	Block,
	Runtime
>;
