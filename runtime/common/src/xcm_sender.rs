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

//! XCM sender for relay chain.

use parity_scale_codec::Encode;
use sp_std::marker::PhantomData;
use xcm::opaque::{VersionedXcm, v0::{SendXcm, MultiLocation, Junction, Xcm, Result, Error}};
use runtime_parachains::{configuration, dmp};

/// XCM sender for relay chain. It only sends downward message.
pub struct ChildParachainRouter<T>(PhantomData<T>);

impl<T: configuration::Config + dmp::Config> SendXcm for ChildParachainRouter<T> {
	fn send_xcm(dest: MultiLocation, msg: Xcm) -> Result {
		match dest {
			MultiLocation::X1(Junction::Parachain(id)) => {
				// Downward message passing.
				let config = <configuration::Module<T>>::config();
				<dmp::Pallet<T>>::queue_downward_message(
					&config,
					id.into(),
					VersionedXcm::from(msg).encode(),
				).map_err(Into::<Error>::into)?;
				Ok(())
			}
			d => Err(Error::CannotReachDestination(d, msg)),
		}
	}
}
