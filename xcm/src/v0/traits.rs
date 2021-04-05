// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

//! Cross-Consensus Message format data structures.

use core::result;
use parity_scale_codec::{Encode, Decode};

use super::{MultiLocation, Xcm};

#[derive(Copy, Clone, Encode, Decode, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub enum Error {
	Undefined,
	Unimplemented,
	UnhandledXcmVersion,
	UnhandledXcmMessage,
	UnhandledEffect,
	EscalationOfPrivilege,
	UntrustedReserveLocation,
	UntrustedTeleportLocation,
	DestinationBufferOverflow,
	CannotReachDestination,
	MultiLocationFull,
	FailedToDecode,
	BadOrigin,
	ExceedsMaxMessageSize,
	FailedToTransactAsset(#[codec(skip)] &'static str),
}

impl From<()> for Error {
	fn from(_: ()) -> Self {
		Self::Undefined
	}
}

pub type Result = result::Result<(), Error>;

pub trait ExecuteXcm {
	fn execute_xcm(origin: MultiLocation, msg: Xcm) -> Result;
}

impl ExecuteXcm for () {
	fn execute_xcm(_origin: MultiLocation, _msg: Xcm) -> Result {
		Err(Error::Unimplemented)
	}
}

pub trait SendXcm {
	fn send_xcm(dest: MultiLocation, msg: Xcm) -> Result;
}

impl SendXcm for () {
	fn send_xcm(_dest: MultiLocation, _msg: Xcm) -> Result {
		Err(Error::Unimplemented)
	}
}

pub trait ExecuteHrmp {
	fn hrmp_init_open_channel(sender: u32, recipient: u32, max_message_size: u32, max_capacity: u32) -> Result;
	fn hrmp_accept_open_channel(recipient: u32, sender: u32) -> Result;
	fn hrmp_close_channel(initiator: u32, sender: u32, recipient: u32) -> Result;
}

impl ExecuteHrmp for () {
	fn hrmp_init_open_channel(_sender: u32, _recipient: u32, _max_message_size: u32, _max_capacity: u32) -> Result {
		Err(().into())
	}
	fn hrmp_accept_open_channel(_recipient: u32, _sender: u32) -> Result {
		Err(().into())
	}
	fn hrmp_close_channel(_initiator: u32, _sender: u32, _recipient: u32) -> Result {
		Err(().into())
	}
}