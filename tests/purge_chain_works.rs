// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

use assert_cmd::cargo::cargo_bin;
use std::{convert::TryInto, process::Command, thread, time::Duration};
use tempfile::tempdir;

mod common;

#[test]
#[cfg(unix)]
fn purge_chain_works() {
	use nix::sys::signal::{kill, Signal::SIGINT};
	use nix::unistd::Pid;

	let tmpdir = tempdir().expect("could not create temp dir");

	let mut cmd = Command::new(cargo_bin("polkadot"))
		.args(&["--dev", "-d"])
		.arg(tmpdir.path())
		.spawn()
		.unwrap();

	// Let it produce some blocks.
	// poll once per second for faster failure
	for _ in 0..30 {
		thread::sleep(Duration::from_secs(1));
		assert!(cmd.try_wait().unwrap().is_none(), "the process should still be running");
	}

	// Stop the process
	kill(Pid::from_raw(cmd.id().try_into().unwrap()), SIGINT).unwrap();
	assert!(common::wait_for(&mut cmd, 30).map(|x| x.success()).unwrap_or_default());

	// Purge chain
	let status = Command::new(cargo_bin("polkadot"))
		.args(&["purge-chain", "--dev", "-d"])
		.arg(tmpdir.path())
		.arg("-y")
		.status()
		.unwrap();
	assert!(status.success());

	// Make sure that the chain folder exists, but `db/full` is deleted.
	assert!(tmpdir.path().join("chains/dev").exists());
	assert!(!tmpdir.path().join("chains/dev/db/full").exists());
}
