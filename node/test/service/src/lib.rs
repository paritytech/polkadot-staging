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

//! Polkadot test service only.

#![warn(missing_docs)]

pub mod chain_spec;

pub use chain_spec::*;
use futures::future::Future;
use polkadot_overseer::OverseerHandler;
use polkadot_primitives::v1::{
	Id as ParaId, HeadData, ValidationCode, Balance, CollatorPair,
};
use polkadot_runtime_common::BlockHashCount;
use polkadot_service::{
	Error, NewFull, FullClient, ClientHandle, ExecuteWithClient, IsCollator,
};
use polkadot_node_subsystem::messages::{CollatorProtocolMessage, CollationGenerationMessage};
use polkadot_test_runtime::{
	Runtime, SignedExtra, SignedPayload, VERSION, ParasSudoWrapperCall, SudoCall, UncheckedExtrinsic,
};
use polkadot_node_primitives::{CollatorFn, CollationGenerationConfig};
use polkadot_runtime_parachains::paras::ParaGenesisArgs;
use sc_chain_spec::ChainSpec;
use sc_client_api::{execution_extensions::{ExecutionConfigs}, ExecutionConfig};
use sc_executor::native_executor_instance;
use sc_network::{
	config::{NetworkConfiguration, TransportConfig},
	multiaddr,
};
use service::{
	config::{DatabaseConfig, KeystoreConfig, MultiaddrWithPeerId, WasmExecutionMethod},
	RpcHandlers, TaskExecutor, TaskManager, KeepBlocks, TransactionStorageMode,
};
use service::{BasePath, Configuration, Role};
use sp_arithmetic::traits::SaturatedConversion;
use sp_blockchain::HeaderBackend;
use sp_keyring::Sr25519Keyring;
use sp_runtime::{codec::Encode, generic, traits::IdentifyAccount, MultiSigner};
use sp_state_machine::BasicExternalities;
use std::{sync::Arc, path::PathBuf};
use substrate_test_client::{BlockchainEventsExt, RpcHandlersExt, RpcTransactionOutput, RpcTransactionError};

native_executor_instance!(
	pub PolkadotTestExecutor,
	polkadot_test_runtime::api::dispatch,
	polkadot_test_runtime::native_version,
	frame_benchmarking::benchmarking::HostFunctions,
);

/// The client type being used by the test service.
pub type Client = FullClient<polkadot_test_runtime::RuntimeApi, PolkadotTestExecutor>;

pub use polkadot_service::FullBackend;

/// Create a new full node.
#[sc_tracing::logging::prefix_logs_with(config.network.node_name.as_str())]
pub fn new_full(
	config: Configuration,
	is_collator: IsCollator,
	worker_program_path: Option<PathBuf>,
) -> Result<
	NewFull<Arc<Client>>,
	Error,
> {
	polkadot_service::new_full::<polkadot_test_runtime::RuntimeApi, PolkadotTestExecutor, _>(
		config,
		is_collator,
		None,
		true,
		None,
		None,
		worker_program_path,
		polkadot_service::RealOverseerGen,
	)
}

/// A wrapper for the test client that implements `ClientHandle`.
pub struct TestClient(pub Arc<Client>);

impl ClientHandle for TestClient {
	fn execute_with<T: ExecuteWithClient>(&self, t: T) -> T::Output {
		T::execute_with_client::<_, _, polkadot_service::FullBackend>(t, self.0.clone())
	}
}

/// Create a Polkadot `Configuration`.
///
/// By default an in-memory socket will be used, therefore you need to provide boot
/// nodes if you want the future node to be connected to other nodes.
///
/// The `storage_update_func` function will be executed in an externalities provided environment
/// and can be used to make adjustments to the runtime genesis storage.
pub fn node_config(
	storage_update_func: impl Fn(),
	task_executor: TaskExecutor,
	key: Sr25519Keyring,
	boot_nodes: Vec<MultiaddrWithPeerId>,
	is_validator: bool,
) -> Configuration {
	let base_path = BasePath::new_temp_dir().expect("could not create temporary directory");
	let root = base_path.path();
	let role = if is_validator {
		Role::Authority
	} else {
		Role::Full
	};
	let key_seed = key.to_seed();
	let mut spec = polkadot_local_testnet_config();
	let mut storage = spec
		.as_storage_builder()
		.build_storage()
		.expect("could not build storage");

	BasicExternalities::execute_with_storage(&mut storage, storage_update_func);
	spec.set_storage(storage);

	let mut network_config = NetworkConfiguration::new(
		key_seed.to_string(),
		"network/test/0.1",
		Default::default(),
		None,
	);

	network_config.boot_nodes = boot_nodes;

	network_config.allow_non_globals_in_dht = true;

	let addr: multiaddr::Multiaddr = multiaddr::Protocol::Memory(rand::random()).into();
	network_config
		.listen_addresses
		.push(addr.clone());

	network_config
		.public_addresses
		.push(addr);

	network_config.transport = TransportConfig::MemoryOnly;

	Configuration {
		impl_name: "polkadot-test-node".to_string(),
		impl_version: "0.1".to_string(),
		role,
		task_executor,
		transaction_pool: Default::default(),
		network: network_config,
		keystore: KeystoreConfig::InMemory,
		keystore_remote: Default::default(),
		database: DatabaseConfig::RocksDb {
			path: root.join("db"),
			cache_size: 128,
		},
		state_cache_size: 16777216,
		state_cache_child_ratio: None,
		state_pruning: Default::default(),
		keep_blocks: KeepBlocks::All,
		transaction_storage: TransactionStorageMode::BlockBody,
		chain_spec: Box::new(spec),
		wasm_method: WasmExecutionMethod::Interpreted,
		wasm_runtime_overrides: Default::default(),
		// NOTE: we enforce the use of the native runtime to make the errors more debuggable
		execution_configs: ExecutionConfigs {
			syncing: ExecutionConfig::new_consensus(sc_client_api::ExecutionStrategy::NativeWhenPossible),
			importing: ExecutionConfig::new_consensus(sc_client_api::ExecutionStrategy::NativeWhenPossible),
			block_construction: ExecutionConfig::new_consensus(sc_client_api::ExecutionStrategy::NativeWhenPossible),
			offchain_worker: ExecutionConfig::new_consensus(sc_client_api::ExecutionStrategy::NativeWhenPossible),
			other: ExecutionConfig::new_consensus(sc_client_api::ExecutionStrategy::NativeWhenPossible),
		},
		rpc_http_threads: None,
		rpc_http: None,
		rpc_ws: None,
		rpc_ipc: None,
		rpc_ws_max_connections: None,
		rpc_cors: None,
		rpc_methods: Default::default(),
		rpc_max_payload: None,
		prometheus_config: None,
		telemetry_endpoints: None,
		telemetry_external_transport: None,
		default_heap_pages: None,
		offchain_worker: Default::default(),
		force_authoring: false,
		disable_grandpa: false,
		dev_key_seed: Some(key_seed),
		tracing_targets: None,
		tracing_receiver: Default::default(),
		max_runtime_instances: 8,
		announce_block: true,
		base_path: Some(base_path),
		informant_output_format: Default::default(),
		disable_log_reloading: false,
	}
}

/// Run a test validator node that uses the test runtime.
///
/// The node will be using an in-memory socket, therefore you need to provide boot nodes if you
/// want it to be connected to other nodes.
///
/// The `storage_update_func` function will be executed in an externalities provided environment
/// and can be used to make adjustments to the runtime genesis storage.
pub fn run_validator_node(
	task_executor: TaskExecutor,
	key: Sr25519Keyring,
	storage_update_func: impl Fn(),
	boot_nodes: Vec<MultiaddrWithPeerId>,
	worker_program_path: Option<PathBuf>,
) -> PolkadotTestNode {
	let config = node_config(storage_update_func, task_executor, key, boot_nodes, true);
	let multiaddr = config.network.listen_addresses[0].clone();
	let NewFull { task_manager, client, network, rpc_handlers, overseer_handler, .. } =
		new_full(config, IsCollator::No, worker_program_path).expect("could not create Polkadot test service");

	let overseer_handler = overseer_handler.expect("test node must have an overseer handler");
	let peer_id = network.local_peer_id().clone();
	let addr = MultiaddrWithPeerId { multiaddr, peer_id };

	PolkadotTestNode {
		task_manager,
		client,
		overseer_handler,
		addr,
		rpc_handlers,
	}
}

/// Run a test collator node that uses the test runtime.
///
/// The node will be using an in-memory socket, therefore you need to provide boot nodes if you
/// want it to be connected to other nodes.
///
/// The `storage_update_func` function will be executed in an externalities provided environment
/// and can be used to make adjustments to the runtime genesis storage.
///
/// # Note
///
/// The collator functionality still needs to be registered at the node! This can be done using
/// [`PolkadotTestNode::register_collator`].
pub fn run_collator_node(
	task_executor: TaskExecutor,
	key: Sr25519Keyring,
	storage_update_func: impl Fn(),
	boot_nodes: Vec<MultiaddrWithPeerId>,
	collator_pair: CollatorPair,
) -> PolkadotTestNode {
	let config = node_config(storage_update_func, task_executor, key, boot_nodes, false);
	let multiaddr = config.network.listen_addresses[0].clone();
	let NewFull {
		task_manager,
		client,
		network,
		rpc_handlers,
		overseer_handler,
		..
	} = new_full(config, IsCollator::Yes(collator_pair), None)
		.expect("could not create Polkadot test service");

	let overseer_handler = overseer_handler.expect("test node must have an overseer handler");
	let peer_id = network.local_peer_id().clone();
	let addr = MultiaddrWithPeerId { multiaddr, peer_id };

	PolkadotTestNode {
		task_manager,
		client,
		overseer_handler,
		addr,
		rpc_handlers,
	}
}

/// A Polkadot test node instance used for testing.
pub struct PolkadotTestNode {
	/// TaskManager's instance.
	pub task_manager: TaskManager,
	/// Client's instance.
	pub client: Arc<Client>,
	/// The overseer handler.
	pub overseer_handler: OverseerHandler,
	/// The `MultiaddrWithPeerId` to this node. This is useful if you want to pass it as "boot node" to other nodes.
	pub addr: MultiaddrWithPeerId,
	/// RPCHandlers to make RPC queries.
	pub rpc_handlers: RpcHandlers,
}

impl PolkadotTestNode {
	/// Send an extrinsic to this node.
	pub async fn send_extrinsic(
		&self,
		function: impl Into<polkadot_test_runtime::Call>,
		caller: Sr25519Keyring,
	) -> Result<RpcTransactionOutput, RpcTransactionError> {
		let extrinsic = construct_extrinsic(&*self.client, function, caller);

		self.rpc_handlers.send_transaction(extrinsic.into()).await
	}

	/// Register a parachain at this relay chain.
	pub async fn register_parachain(
		&self,
		id: ParaId,
		validation_code: impl Into<ValidationCode>,
		genesis_head: impl Into<HeadData>,
	) -> Result<(), RpcTransactionError> {
		let call = ParasSudoWrapperCall::sudo_schedule_para_initialize(
			id,
			ParaGenesisArgs {
				genesis_head: genesis_head.into(),
				validation_code: validation_code.into(),
				parachain: true,
			},
		);

		self.send_extrinsic(SudoCall::sudo(Box::new(call.into())), Sr25519Keyring::Alice).await.map(drop)
	}

	/// Wait for `count` blocks to be imported in the node and then exit. This function will not return if no blocks
	/// are ever created, thus you should restrict the maximum amount of time of the test execution.
	pub fn wait_for_blocks(&self, count: usize) -> impl Future<Output = ()> {
		self.client.wait_for_blocks(count)
	}

	/// Register the collator functionality in the overseer of this node.
	pub async fn register_collator(
		&mut self,
		collator_key: CollatorPair,
		para_id: ParaId,
		collator: CollatorFn,
	) {
		let config = CollationGenerationConfig {
			key: collator_key,
			collator,
			para_id,
		};

		self.overseer_handler
			.send_msg(CollationGenerationMessage::Initialize(config), "Collator")
			.await;

		self.overseer_handler
			.send_msg(CollatorProtocolMessage::CollateOn(para_id), "Collator")
			.await;
	}
}

/// Construct an extrinsic that can be applied to the test runtime.
pub fn construct_extrinsic(
	client: &Client,
	function: impl Into<polkadot_test_runtime::Call>,
	caller: Sr25519Keyring,
) -> UncheckedExtrinsic {
	let function = function.into();
	let current_block_hash = client.info().best_hash;
	let current_block = client.info().best_number.saturated_into();
	let genesis_block = client.hash(0).unwrap().unwrap();
	let nonce = 0;
	let period = BlockHashCount::get()
		.checked_next_power_of_two()
		.map(|c| c / 2)
		.unwrap_or(2) as u64;
	let tip = 0;
	let extra: SignedExtra = (
		frame_system::CheckSpecVersion::<Runtime>::new(),
		frame_system::CheckTxVersion::<Runtime>::new(),
		frame_system::CheckGenesis::<Runtime>::new(),
		frame_system::CheckEra::<Runtime>::from(generic::Era::mortal(period, current_block)),
		frame_system::CheckNonce::<Runtime>::from(nonce),
		frame_system::CheckWeight::<Runtime>::new(),
		pallet_transaction_payment::ChargeTransactionPayment::<Runtime>::from(tip),
	);
	let raw_payload = SignedPayload::from_raw(
		function.clone(),
		extra.clone(),
		(
			VERSION.spec_version,
			VERSION.transaction_version,
			genesis_block,
			current_block_hash,
			(),
			(),
			(),
		),
	);
	let signature = raw_payload.using_encoded(|e| caller.sign(e));
	UncheckedExtrinsic::new_signed(
		function.clone(),
		polkadot_test_runtime::Address::Id(caller.public().into()),
		polkadot_primitives::v0::Signature::Sr25519(signature.clone()),
		extra.clone(),
	)
}

/// Construct a transfer extrinsic.
pub fn construct_transfer_extrinsic(
	client: &Client,
	origin: sp_keyring::AccountKeyring,
	dest: sp_keyring::AccountKeyring,
	value: Balance,
) -> UncheckedExtrinsic {
	let function = polkadot_test_runtime::Call::Balances(
		pallet_balances::Call::transfer(
			MultiSigner::from(dest.public()).into_account().into(),
			value,
		),
	);

	construct_extrinsic(client, function, origin)
}
