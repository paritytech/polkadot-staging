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

//! Migration code for the parachain's DB.


#![cfg(feature = "real-overseer")]

use std::fs;
use std::io::{self, Read, Write, ErrorKind};
use std::path::{Path, PathBuf};
use thiserror::Error;


type Version = u32;

/// Version file name.
const VERSION_FILE_NAME: &'static str = "parachain_db_version";

/// Current db version.
const CURRENT_VERSION: Version = 0;

#[derive(Error, Debug)]
pub enum Error {
	#[error("I/O error when reading/writing the version")]
	Io(#[from] io::Error),
	#[error("The version file format is incorrect")]
	CorruptedVersionFile,
	#[error("Future version (expected {current:?}, found {got:?})")]
	FutureVersion {
		current: Version,
		got: Version,
	},
}


impl From<Error> for io::Error {
	fn from(me: Error) -> io::Error {
		match me {
			Error::Io(e) => e,
			_ => super::other_io_error(me.to_string()),
		}
	}
}

/// Try upgrading parachain's database to the current version.
pub fn try_upgrade_db(db_path: &Path) -> Result<(), Error> {
	let is_empty = db_path.read_dir().map_or(true, |mut d| d.next().is_none());
	if !is_empty {
		let db_version = current_version(db_path)?;
		match db_version {
			CURRENT_VERSION => (),
			v => return Err(Error::FutureVersion {
				current: CURRENT_VERSION,
				got: v,
			}),
		}
	}

	update_version(db_path)
}

/// Reads current database version from the file at given path.
/// If the file does not exist, assumes version 0.
fn current_version(path: &Path) -> Result<Version, Error> {
	match fs::File::open(version_file_path(path)) {
		Err(ref err) if err.kind() == ErrorKind::NotFound => Ok(0),
		Err(err) => Err(err.into()),
		Ok(mut file) => {
			let mut s = String::new();
			let _ = file.read_to_string(&mut s)?;
			u32::from_str_radix(&s, 10).map_err(|_| Error::CorruptedVersionFile)
		},
	}
}

/// Writes current database version to the file.
/// Creates a new file if the version file does not exist yet.
fn update_version(path: &Path) -> Result<(), Error> {
	fs::create_dir_all(path)?;
	let mut file = fs::File::create(version_file_path(path))?;
	file.write_all(format!("{}", CURRENT_VERSION).as_bytes())?;
	Ok(())
}

/// Returns the version file path.
fn version_file_path(path: &Path) -> PathBuf {
	let mut file_path = path.to_owned();
	file_path.push(VERSION_FILE_NAME);
	file_path
}
