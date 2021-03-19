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

//! WASM re-execution of a parachain candidate.
//! In the context of relay-chain candidate evaluation, there are some additional
//! steps to ensure that the provided input parameters are correct.
//! Assuming the parameters are correct, this module provides a wrapper around
//! a WASM VM for re-execution of a parachain candidate.

use std::{any::{TypeId, Any}, path::{Path, PathBuf}};
use crate::primitives::{ValidationParams, ValidationResult};
use parity_scale_codec::{Decode, Encode};
use sp_core::{storage::{ChildInfo, TrackedStorageKey}, traits::{CallInWasm, SpawnNamed}};
use sp_externalities::{Extensions, AsyncExternalities, TaskId, WorkerResult};
use sp_wasm_interface::HostFunctions as _;

#[cfg(not(any(target_os = "android", target_os = "unknown")))]
pub use validation_host::{run_worker, ValidationPool, EXECUTION_TIMEOUT_SEC, WORKER_ARGS};

mod validation_host;

/// The strategy we employ for isolating execution of wasm parachain validation function (PVF).
///
/// For a typical validator an external process is the default way to run PVF. The rationale is based
/// on the following observations:
///
/// (a) PVF is completely under control of parachain developers who may or may not be malicious.
/// (b) Collators are in charge of providing PoV who also may or may not be malicious.
/// (c) PVF is executed by a wasm engine based on optimizing compiler which is a very complex piece
///     of machinery.
///
/// (a) and (b) may lead to a situation where due to a combination of PVF and PoV the validation work
/// can stuck in an infinite loop, which can open up resource exhaustion or DoS attack vectors.
///
/// While some execution engines provide functionality to interrupt execution of wasm module from
/// another thread, there are also some caveats to that: there is no clean way to interrupt execution
/// if the control flow is in the host side and at the moment we haven't rigoriously vetted that all
/// host functions terminate or, at least, return in a short amount of time. Additionally, we want
/// some freedom on choosing wasm execution environment.
///
/// On top of that, execution in a separate process helps to minimize impact of (c) if exploited.
/// It's not only the risk of miscompilation, but it also includes risk of JIT-bombs, i.e. cases
/// of specially crafted code that take enourmous amounts of time and memory to compile.
///
/// At the same time, since PVF validates self-contained candidates, validation workers don't require
/// extensive communication with polkadot host, therefore there should be no observable performance penalty
/// coming from inter process communication.
///
/// All of the above should give a sense why isolation is crucial for a typical use-case.
///
/// However, in some cases, e.g. when running PVF validation on android (for whatever reason), we
/// cannot afford the luxury of process isolation and thus there is an option to run validation in
/// process. Also, running in process is convenient for testing.
#[derive(Clone, Debug)]
pub enum IsolationStrategy {
	/// The validation worker is ran in a thread inside the same process.
	InProcess,
	/// The validation worker is ran using the process' executable and the subcommand `validation-worker` is passed
	/// following by the address of the shared memory.
	#[cfg(not(any(target_os = "android", target_os = "unknown")))]
	ExternalProcessSelfHost {
		pool: ValidationPool,
		cache_base_path: Option<String>,
	},
	/// The validation worker is ran using the command provided and the argument provided. The address of the shared
	/// memory is added at the end of the arguments.
	#[cfg(not(any(target_os = "android", target_os = "unknown")))]
	ExternalProcessCustomHost {
		/// Validation pool.
		pool: ValidationPool,
		/// Path to the validation worker. The file must exists and be executable.
		binary: PathBuf,
		/// List of arguments passed to the validation worker. The address of the shared memory will be automatically
		/// added after the arguments.
		args: Vec<String>,
	},
}

impl IsolationStrategy {
	#[cfg(not(any(target_os = "android", target_os = "unknown")))]
	pub fn external_process_with_caching(cache_base_path: Option<&Path>) -> Self {
		// Convert cache path to string here so that we don't have to do that each time we launch
		// validation worker.
		let cache_base_path = cache_base_path.map(|path| path.display().to_string());

		Self::ExternalProcessSelfHost {
			pool: ValidationPool::new(),
			cache_base_path,
		}
	}
}

#[derive(Debug, thiserror::Error)]
/// Candidate validation error.
pub enum ValidationError {
	/// Validation failed due to internal reasons. The candidate might still be valid.
	#[error(transparent)]
	Internal(#[from] InternalError),
	/// Candidate is invalid.
	#[error(transparent)]
	InvalidCandidate(#[from] InvalidCandidate),
}

/// Error type that indicates invalid candidate.
#[derive(Debug, thiserror::Error)]
pub enum InvalidCandidate {
	/// Wasm executor error.
	#[error("WASM executor error")]
	WasmExecutor(#[from] sc_executor::error::Error),
	/// Call data is too large.
	#[error("Validation parameters are {0} bytes, max allowed is {1}")]
	ParamsTooLarge(usize, usize),
	/// Code size it too large.
	#[error("WASM code is {0} bytes, max allowed is {1}")]
	CodeTooLarge(usize, usize),
	/// Error decoding returned data.
	#[error("Validation function returned invalid data.")]
	BadReturn,
	#[error("Validation function timeout.")]
	Timeout,
	#[error("External WASM execution error: {0}")]
	ExternalWasmExecutor(String),
}

impl core::convert::From<String> for InvalidCandidate {
	fn from(s: String) -> Self {
		Self::ExternalWasmExecutor(s)
	}
}

/// Host error during candidate validation. This does not indicate an invalid candidate.
#[derive(Debug, thiserror::Error)]
pub enum InternalError {
	#[error("IO error: {0}")]
	Io(#[from] std::io::Error),

	#[error("System error: {0}")]
	System(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),

	#[cfg(not(any(target_os = "android", target_os = "unknown")))]
	#[error("Failed to create shared memory: {0}")]
	WorkerStartTimeout(String),

	#[cfg(not(any(target_os = "android", target_os = "unknown")))]
	#[error("Failed to create shared memory: {0}")]
	FailedToCreateSharedMemory(String),

	#[cfg(not(any(target_os = "android", target_os = "unknown")))]
	#[error("Failed to send a singal to worker: {0}")]
	FailedToSignal(String),

	#[cfg(not(any(target_os = "android", target_os = "unknown")))]
	#[error("Failed to send data to worker: {0}")]
	FailedToWriteData(&'static str),

	#[error("WASM worker error: {0}")]
	WasmWorker(String),
}

/// A cache of executors for different parachain Wasm instances.
///
/// This should be reused across candidate validation instances.
pub struct ExecutorCache(sc_executor::WasmExecutor);

impl ExecutorCache {
	/// Returns a new instance of an executor cache.
	///
	/// `cache_base_path` allows to specify a directory where the executor is allowed to store files
	/// for caching, e.g. compilation artifacts.
	pub fn new(cache_base_path: Option<PathBuf>) -> ExecutorCache {
		ExecutorCache(sc_executor::WasmExecutor::new(
			#[cfg(all(feature = "wasmtime", not(any(target_os = "android", target_os = "unknown"))))]
			sc_executor::WasmExecutionMethod::Compiled,
			#[cfg(any(not(feature = "wasmtime"), target_os = "android", target_os = "unknown"))]
			sc_executor::WasmExecutionMethod::Interpreted,
			// TODO: Make sure we don't use more than 1GB: https://github.com/paritytech/polkadot/issues/699
			Some(1024),
			HostFunctions::host_functions(),
			8,
			cache_base_path,
		))
	}
}

/// Validate a candidate under the given validation code.
///
/// This will fail if the validation code is not a proper parachain validation module.
pub fn validate_candidate(
	validation_code: &[u8],
	params: ValidationParams,
	isolation_strategy: &IsolationStrategy,
	spawner: impl SpawnNamed + 'static,
) -> Result<ValidationResult, ValidationError> {
	match isolation_strategy {
		IsolationStrategy::InProcess => {
			validate_candidate_internal(
				&ExecutorCache::new(None),
				validation_code,
				&params.encode(),
				spawner,
			)
		},
		#[cfg(not(any(target_os = "android", target_os = "unknown")))]
		IsolationStrategy::ExternalProcessSelfHost { pool, cache_base_path } => {
			pool.validate_candidate(validation_code, params, cache_base_path.as_deref())
		},
		#[cfg(not(any(target_os = "android", target_os = "unknown")))]
		IsolationStrategy::ExternalProcessCustomHost { pool, binary, args } => {
			let args: Vec<&str> = args.iter().map(|x| x.as_str()).collect();
			pool.validate_candidate_custom(validation_code, params, binary, &args)
		},
	}
}

/// The host functions provided by the wasm executor to the parachain wasm blob.
type HostFunctions = sp_io::SubstrateHostFunctions;

/// Validate a candidate under the given validation code.
///
/// This will fail if the validation code is not a proper parachain validation module.
pub fn validate_candidate_internal(
	executor: &ExecutorCache,
	validation_code: &[u8],
	encoded_call_data: &[u8],
	spawner: impl SpawnNamed + 'static,
) -> Result<ValidationResult, ValidationError> {
	let executor = &executor.0;

	let mut extensions = Extensions::new();
	extensions.register(sp_core::traits::TaskExecutorExt::new(spawner));
	extensions.register(sp_core::traits::CallInWasmExt::new(executor.clone()));

	let mut ext = ValidationExternalities(extensions);

	// Expensive, but not more-so than recompiling the wasm module.
	// And we need this hash to access the `sc_executor` cache.
	let code_hash = {
		use polkadot_core_primitives::{BlakeTwo256, HashT};
		BlakeTwo256::hash(validation_code)
	};

	let res = executor.call_in_wasm(
		validation_code,
		Some(code_hash.as_bytes().to_vec()),
		"validate_block",
		encoded_call_data,
		&mut ext,
		sp_core::traits::MissingHostFunctions::Allow,
	).map_err(|e| ValidationError::InvalidCandidate(e.into()))?;

	ValidationResult::decode(&mut &res[..])
		.map_err(|_| ValidationError::InvalidCandidate(InvalidCandidate::BadReturn).into())
}

/// The validation externalities that will panic on any storage related access. They just provide
/// access to the parachain extension.
struct ValidationExternalities(Extensions);

impl sp_externalities::Externalities for ValidationExternalities {
	fn storage(&self, _: &[u8]) -> Option<Vec<u8>> {
		panic!("storage: unsupported feature for parachain validation")
	}

	fn storage_hash(&self, _: &[u8]) -> Option<Vec<u8>> {
		panic!("storage_hash: unsupported feature for parachain validation")
	}

	fn child_storage_hash(&self, _: &ChildInfo, _: &[u8]) -> Option<Vec<u8>> {
		panic!("child_storage_hash: unsupported feature for parachain validation")
	}

	fn child_storage(&self, _: &ChildInfo, _: &[u8]) -> Option<Vec<u8>> {
		panic!("child_storage: unsupported feature for parachain validation")
	}

	fn kill_child_storage(&mut self, _: &ChildInfo, _: Option<u32>) -> (bool, u32) {
		panic!("kill_child_storage: unsupported feature for parachain validation")
	}

	fn clear_prefix(&mut self, _: &[u8]) {
		panic!("clear_prefix: unsupported feature for parachain validation")
	}

	fn clear_child_prefix(&mut self, _: &ChildInfo, _: &[u8]) {
		panic!("clear_child_prefix: unsupported feature for parachain validation")
	}

	fn place_storage(&mut self, _: Vec<u8>, _: Option<Vec<u8>>) {
		panic!("place_storage: unsupported feature for parachain validation")
	}

	fn place_child_storage(&mut self, _: &ChildInfo, _: Vec<u8>, _: Option<Vec<u8>>) {
		panic!("place_child_storage: unsupported feature for parachain validation")
	}

	fn storage_root(&mut self) -> Vec<u8> {
		panic!("storage_root: unsupported feature for parachain validation")
	}

	fn child_storage_root(&mut self, _: &ChildInfo) -> Vec<u8> {
		panic!("child_storage_root: unsupported feature for parachain validation")
	}

	fn storage_changes_root(&mut self, _: &[u8]) -> Result<Option<Vec<u8>>, ()> {
		panic!("storage_changes_root: unsupported feature for parachain validation")
	}

	fn next_child_storage_key(&self, _: &ChildInfo, _: &[u8]) -> Option<Vec<u8>> {
		panic!("next_child_storage_key: unsupported feature for parachain validation")
	}

	fn next_storage_key(&self, _: &[u8]) -> Option<Vec<u8>> {
		panic!("next_storage_key: unsupported feature for parachain validation")
	}

	fn storage_append(
		&mut self,
		_key: Vec<u8>,
		_value: Vec<u8>,
	) {
		panic!("storage_append: unsupported feature for parachain validation")
	}

	fn storage_start_transaction(&mut self) {
		panic!("storage_start_transaction: unsupported feature for parachain validation")
	}

	fn storage_rollback_transaction(&mut self) -> Result<(), ()> {
		panic!("storage_rollback_transaction: unsupported feature for parachain validation")
	}

	fn storage_commit_transaction(&mut self) -> Result<(), ()> {
		panic!("storage_commit_transaction: unsupported feature for parachain validation")
	}

	fn wipe(&mut self) {
		panic!("wipe: unsupported feature for parachain validation")
	}

	fn commit(&mut self) {
		panic!("commit: unsupported feature for parachain validation")
	}

	fn read_write_count(&self) -> (u32, u32, u32, u32) {
		panic!("read_write_count: unsupported feature for parachain validation")
	}

	fn reset_read_write_count(&mut self) {
		panic!("reset_read_write_count: unsupported feature for parachain validation")
	}

	fn get_whitelist(&self) -> Vec<TrackedStorageKey> {
		panic!("get_whitelist: unsupported feature for parachain validation")
	}

	fn set_whitelist(&mut self, _: Vec<TrackedStorageKey>) {
		panic!("set_whitelist: unsupported feature for parachain validation")
	}

	fn set_offchain_storage(&mut self, _: &[u8], _: std::option::Option<&[u8]>) {
		panic!("set_offchain_storage: unsupported feature for parachain validation")
	}

	fn get_worker_externalities(
		&mut self,
		_worker_id: TaskId,
	) -> Box<dyn AsyncExternalities> {
		panic!("get_worker_externalities: unsupported feature for parachain validation")
	}

	fn resolve_worker_result(&mut self, _result: WorkerResult) -> Option<Vec<u8>> {
		panic!("resolve_worker_result: unsupported feature for parachain validation")
	}

	fn dismiss_worker(&mut self, _id: TaskId) {
		panic!("dismiss_worker: unsupported feature for parachain validation")
	}
}

impl sp_externalities::ExtensionStore for ValidationExternalities {
	fn extension_by_type_id(&mut self, type_id: TypeId) -> Option<&mut dyn Any> {
		self.0.get_mut(type_id)
	}

	fn register_extension_with_type_id(
		&mut self,
		type_id: TypeId,
		extension: Box<dyn sp_externalities::Extension>,
	) -> Result<(), sp_externalities::Error> {
		self.0.register_with_type_id(type_id, extension)
	}

	fn deregister_extension_by_type_id(
		&mut self,
		type_id: TypeId,
	) -> Result<(), sp_externalities::Error> {
		if self.0.deregister(type_id) {
			Ok(())
		} else {
			Err(sp_externalities::Error::ExtensionIsNotRegistered(type_id))
		}
	}
}
