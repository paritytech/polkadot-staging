// Copyright 2019-2020 Parity Technologies (UK) Ltd.
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

#![cfg(not(any(target_os = "android", target_os = "unknown")))]

use std::{env, path::PathBuf, process, sync::Arc, sync::atomic};
use crate::primitives::{ValidationParams, ValidationResult};
use super::{validate_candidate_internal, ValidationError, InvalidCandidate, InternalError};
use parking_lot::Mutex;
use log::{debug, trace};
use futures::executor::ThreadPool;
use sp_core::traits::{SpawnNamed, SpawnLimit};

const WORKER_ARG: &'static str = "validation-worker";
/// CLI Argument to start in validation worker mode.
pub const WORKER_ARGS: &[&'static str] = &[WORKER_ARG];

const LOG_TARGET: &'static str = "parachain::validation-worker";

mod workspace;

/// Execution timeout in seconds;
#[cfg(debug_assertions)]
pub const EXECUTION_TIMEOUT_SEC: u64 = 30;

#[cfg(not(debug_assertions))]
pub const EXECUTION_TIMEOUT_SEC: u64 = 5;

#[derive(Clone)]
struct TaskExecutor(ThreadPool);

impl TaskExecutor {
	fn new() -> Result<Self, String> {
		ThreadPool::new().map_err(|e| e.to_string()).map(Self)
	}
}

impl SpawnNamed for TaskExecutor {
	fn spawn_blocking(&self, _: &'static str, future: futures::future::BoxFuture<'static, ()>) {
		self.0.spawn_ok(future);
	}

	fn spawn(
		&self,
		_: &'static str,
		future: futures::future::BoxFuture<'static, ()>,
	) -> Option<sp_core::traits::TaskHandle> {
		use futures::task::SpawnExt;
		if let Some(handle) = self.0.spawn_with_handle(future).ok() {
			Some(Box::new(Handle(Some(handle))))
		} else {
			None
		}
	}
}

pub struct Handle(Option<futures::future::RemoteHandle<()>>);

impl sp_core::traits::TaskHandleTrait for Handle {
	fn dismiss(&mut self) {
		// drop the remote handle to free pool.
		self.0.take();
	}
}

#[cfg(feature = "std")]
impl Drop for Handle {
	fn drop(&mut self) {
		// avoid dropping thread when we don't
		// use the handle.
		self.0.take().map(|inner| inner.forget());
	}
}

impl SpawnLimit for TaskExecutor {
	fn try_reserve(&self, number_of_tasks: usize) -> usize {
		// Unbounded, the task executor is runing a pool internally.
		number_of_tasks
	}

	fn release(&self, _number_of_tasks: usize) {
	}
}

/// A pool of hosts.
#[derive(Clone, Debug)]
pub struct ValidationPool {
	hosts: Arc<Vec<Mutex<ValidationHost>>>,
}

const DEFAULT_NUM_HOSTS: usize = 8;

impl ValidationPool {
	/// Creates a validation pool with the default configuration.
	pub fn new() -> ValidationPool {
		ValidationPool {
			hosts: Arc::new((0..DEFAULT_NUM_HOSTS).map(|_| Default::default()).collect()),
		}
	}

	/// Validate a candidate under the given validation code using the next free validation host.
	///
	/// This will fail if the validation code is not a proper parachain validation module.
	///
	/// This function will use `std::env::current_exe()` with the arguments that consist of [`WORKER_ARGS`]
	/// with appended `cache_base_path` (if any).
	pub fn validate_candidate(
		&self,
		validation_code: &[u8],
		params: ValidationParams,
		cache_base_path: Option<&str>,
	) -> Result<ValidationResult, ValidationError> {
		use std::{iter, borrow::Cow};

		let worker_cli_args = match cache_base_path {
			Some(cache_base_path) => {
				let worker_cli_args: Vec<&str> = WORKER_ARGS
					.into_iter()
					.cloned()
					.chain(iter::once(cache_base_path))
					.collect();
				Cow::from(worker_cli_args)
			}
			None => Cow::from(WORKER_ARGS),
		};

		self.validate_candidate_custom(
			validation_code,
			params,
			&env::current_exe().map_err(|err| ValidationError::Internal(err.into()))?,
			&worker_cli_args,
		)
	}

	/// Validate a candidate under the given validation code using the next free validation host.
	///
	/// This will fail if the validation code is not a proper parachain validation module.
	///
	/// This function will use the command and the arguments provided in the function's arguments to run the worker.
	pub fn validate_candidate_custom(
		&self,
		validation_code: &[u8],
		params: ValidationParams,
		command: &PathBuf,
		args: &[&str],
	) -> Result<ValidationResult, ValidationError> {
		for host in self.hosts.iter() {
			if let Some(mut host) = host.try_lock() {
				return host.validate_candidate(validation_code, params, command, args);
			}
		}

		// all workers are busy, just wait for the first one
		self.hosts[0]
			.lock()
			.validate_candidate(validation_code, params, command, args)
	}
}

/// Validation worker process entry point. Runs a loop waiting for candidates to validate
/// and sends back results via shared memory.
pub fn run_worker(mem_id: &str, cache_base_path: Option<PathBuf>) -> Result<(), String> {
	let mut worker_handle = match workspace::open(mem_id) {
		Err(e) => {
			debug!(
				target: LOG_TARGET,
				"{} Error opening shared memory: {:?}",
				process::id(),
				e
			);
			return Err(e);
		}
		Ok(h) => h,
	};

	let exit = Arc::new(atomic::AtomicBool::new(false));
	let task_executor = TaskExecutor::new()?;
	// spawn parent monitor thread
	let watch_exit = exit.clone();
	std::thread::spawn(move || {
		use std::io::Read;
		let mut in_data = Vec::new();
		// pipe terminates when parent process exits
		std::io::stdin().read_to_end(&mut in_data).ok();
		debug!(
			target: LOG_TARGET,
			"{} Parent process is dead. Exiting",
			process::id()
		);
		exit.store(true, atomic::Ordering::Relaxed);
	});

	worker_handle.signal_ready()?;

	let executor = super::ExecutorCache::new(cache_base_path);

	loop {
		if watch_exit.load(atomic::Ordering::Relaxed) {
			break;
		}

		debug!(
			target: LOG_TARGET,
			"{} Waiting for candidate",
			process::id()
		);
		let work_item = match worker_handle.wait_for_work(3) {
			Err(workspace::WaitForWorkErr::Wait(e)) => {
				trace!(
					target: LOG_TARGET,
					"{} Timeout waiting for candidate: {:?}",
					process::id(),
					e
				);
				continue;
			}
			Err(workspace::WaitForWorkErr::FailedToDecode(e)) => {
				return Err(e);
			}
			Ok(work_item) => work_item,
		};

		debug!(target: LOG_TARGET, "{} Processing candidate", process::id());
		let result = validate_candidate_internal(
			&executor,
			work_item.code,
			work_item.params,
			task_executor.clone(),
		);

		debug!(
			target: LOG_TARGET,
			"{} Candidate validated: {:?}",
			process::id(),
			result
		);

		let result_header = match result {
			Ok(r) => workspace::ValidationResultHeader::Ok(r),
			Err(ValidationError::Internal(e)) => workspace::ValidationResultHeader::Error(
				workspace::WorkerValidationError::InternalError(e.to_string()),
			),
			Err(ValidationError::InvalidCandidate(e)) => workspace::ValidationResultHeader::Error(
				workspace::WorkerValidationError::ValidationError(e.to_string()),
			),
		};
		worker_handle
			.report_result(result_header)
			.map_err(|e| format!("error reporting result: {:?}", e))?;
	}
	Ok(())
}

unsafe impl Send for ValidationHost {}

#[derive(Default, Debug)]
struct ValidationHost {
	worker: Option<process::Child>,
	host_handle: Option<workspace::HostHandle>,
	id: u32,
}

impl Drop for ValidationHost {
	fn drop(&mut self) {
		if let Some(ref mut worker) = &mut self.worker {
			worker.kill().ok();
		}
	}
}

impl ValidationHost {
	fn start_worker(&mut self, cmd: &PathBuf, args: &[&str]) -> Result<(), InternalError> {
		if let Some(ref mut worker) = self.worker {
			// Check if still alive
			if let Ok(None) = worker.try_wait() {
				// Still running
				return Ok(());
			}
		}

		let host_handle =
			workspace::create().map_err(|msg| InternalError::FailedToCreateSharedMemory(msg))?;

		debug!(
			target: LOG_TARGET,
			"Starting worker at {:?} with arguments: {:?} and {:?}",
			cmd,
			args,
			host_handle.id(),
		);
		let worker = process::Command::new(cmd)
			.args(args)
			.arg(host_handle.id())
			.stdin(process::Stdio::piped())
			.spawn()?;
		self.id = worker.id();
		self.worker = Some(worker);

		host_handle
			.wait_until_ready(EXECUTION_TIMEOUT_SEC)
			.map_err(|e| InternalError::WorkerStartTimeout(format!("{:?}", e)))?;
		self.host_handle = Some(host_handle);
		Ok(())
	}

	/// Validate a candidate under the given validation code.
	///
	/// This will fail if the validation code is not a proper parachain validation module.
	pub fn validate_candidate(
		&mut self,
		validation_code: &[u8],
		params: ValidationParams,
		binary: &PathBuf,
		args: &[&str],
	) -> Result<ValidationResult, ValidationError> {
		// First, check if need to spawn the child process
		self.start_worker(binary, args)?;

		let host_handle = self
			.host_handle
			.as_mut()
			.expect("host_handle is always `Some` after `start_worker` completes successfully");

		debug!(target: LOG_TARGET, "{} Signaling candidate", self.id);
		match host_handle.request_validation(validation_code, params) {
			Ok(()) => {}
			Err(workspace::RequestValidationErr::CodeTooLarge { actual, max }) => {
				return Err(ValidationError::InvalidCandidate(
					InvalidCandidate::CodeTooLarge(actual, max),
				));
			}
			Err(workspace::RequestValidationErr::ParamsTooLarge { actual, max }) => {
				return Err(ValidationError::InvalidCandidate(
					InvalidCandidate::ParamsTooLarge(actual, max),
				));
			}
			Err(workspace::RequestValidationErr::Signal(msg)) => {
				return Err(ValidationError::Internal(InternalError::FailedToSignal(msg)));
			}
			Err(workspace::RequestValidationErr::WriteData(msg)) => {
				return Err(ValidationError::Internal(InternalError::FailedToWriteData(msg)));
			}
		}

		debug!(target: LOG_TARGET, "{} Waiting for results", self.id);
		let result_header = match host_handle.wait_for_result(EXECUTION_TIMEOUT_SEC) {
			Ok(inner_result) => inner_result,
			Err(assumed_timeout) => {
				debug!(target: LOG_TARGET, "Worker timeout: {:?}", assumed_timeout);
				if let Some(mut worker) = self.worker.take() {
					worker.kill().ok();
				}
				return Err(ValidationError::InvalidCandidate(InvalidCandidate::Timeout));
			}
		};

		match result_header {
			workspace::ValidationResultHeader::Ok(result) => Ok(result),
			workspace::ValidationResultHeader::Error(
				workspace::WorkerValidationError::InternalError(e),
			) => {
				debug!(
					target: LOG_TARGET,
					"{} Internal validation error: {}", self.id, e
				);
				Err(ValidationError::Internal(InternalError::WasmWorker(e)))
			}
			workspace::ValidationResultHeader::Error(
				workspace::WorkerValidationError::ValidationError(e),
			) => {
				debug!(
					target: LOG_TARGET,
					"{} External validation error: {}", self.id, e
				);
				Err(ValidationError::InvalidCandidate(
					InvalidCandidate::ExternalWasmExecutor(e),
				))
			}
		}
	}
}
