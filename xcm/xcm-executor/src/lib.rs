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

#![cfg_attr(not(feature = "std"), no_std)]

use sp_std::{prelude::*, marker::PhantomData, convert::TryInto};
use frame_support::{ensure, dispatch::Dispatchable};
use parity_scale_codec::Decode;
use xcm::v0::{
	Xcm, Order, ExecuteXcm, ExecuteHrmp, SendXcm, Error as XcmError, Result as XcmResult,
	MultiLocation, MultiAsset, Junction,
};

pub mod traits;
mod assets;
mod config;

use traits::{TransactAsset, ConvertOrigin, FilterAssetLocation, InvertLocation};
pub use assets::{Assets, AssetId};
pub use config::Config;

pub struct XcmExecutor<Config>(PhantomData<Config>);

impl<Config: config::Config> ExecuteXcm for XcmExecutor<Config> {
	fn execute_xcm(origin: MultiLocation, msg: Xcm) -> XcmResult {
		let (mut holding, effects) = match (origin.clone(), msg) {
			(origin, Xcm::RelayedFrom { superorigin, inner }) => {
				// We ensure that it doesn't contain any `Parent` Junctions which would imply a privilege escalation.
				let mut new_origin = origin;
				for j in superorigin.into_iter() {
					ensure!(j.is_sub_consensus(), XcmError::EscalationOfPrivilege);
					new_origin.push(j).map_err(|_| XcmError::MultiLocationFull)?;
				}
				return Self::execute_xcm(
					new_origin,
					(*inner).try_into().map_err(|_| XcmError::UnhandledXcmVersion)?
				)
			}
			(origin, Xcm::WithdrawAsset { assets, effects }) => {
				// Take `assets` from the origin account (on-chain) and place in holding.
				let mut holding = Assets::default();
				for asset in assets {
					let withdrawn = Config::AssetTransactor::withdraw_asset(&asset, &origin)?;
					holding.saturating_subsume(withdrawn);
				}
				(holding, effects)
			}
			(origin, Xcm::ReserveAssetDeposit { assets, effects }) => {
				// check whether we trust origin to be our reserve location for this asset.
				if assets.iter().all(|asset| Config::IsReserve::filter_asset_location(asset, &origin)) {
					// We only trust the origin to send us assets that they identify as their
					// sovereign assets.
					(Assets::from(assets), effects)
				} else {
					Err(XcmError::UntrustedReserveLocation)?
				}
			}
			(origin, Xcm::TeleportAsset { assets, effects }) => {
				// check whether we trust origin to teleport this asset to us via config trait.
				// TODO: should de-wildcard `assets` before passing in.
				log::debug!(target: "runtime::xcm-executor", "Teleport from {:?}", origin);
				if assets.iter().all(|asset| Config::IsTeleporter::filter_asset_location(asset, &origin)) {
					// We only trust the origin to send us assets that they identify as their
					// sovereign assets.
					(Assets::from(assets), effects)
				} else {
					Err(XcmError::UntrustedTeleportLocation)?
				}
			}
			(origin, Xcm::Transact { origin_type, call }) => {
				// We assume that the Relay-chain is allowed to use transact on this parachain.

				// TODO: Weight fees should be paid.

				// TODO: allow this to be configurable in the trait.
				// TODO: allow the trait to issue filters for the relay-chain
				let message_call = Config::Call::decode(&mut &call[..]).map_err(|_| XcmError::FailedToDecode)?;
				let dispatch_origin = Config::OriginConverter::convert_origin(origin, origin_type)
					.map_err(|_| XcmError::BadOrigin)?;
				let _ok = message_call.dispatch(dispatch_origin).is_ok();
				// Not much to do with the result as it is. It's up to the parachain to ensure that the
				// message makes sense.
				return Ok(());
			}
			(origin, Xcm::HrmpInitOpenChannel { recipient, max_message_size, max_capacity } ) => {
				let sender = match origin {
					MultiLocation::X1(Junction::Parachain { id }) => id,
					_ => Err(XcmError::BadOrigin)?,
				};
				return Config::HrmpExecutor::hrmp_init_open_channel(sender, recipient, max_message_size, max_capacity)
			}
			(origin, Xcm::HrmpAcceptOpenChannel { sender } ) => {
				let recipient = match origin {
					MultiLocation::X1(Junction::Parachain { id }) => id,
					_ => Err(XcmError::BadOrigin)?,
				};
				return Config::HrmpExecutor::hrmp_accept_open_channel(recipient, sender)
			}
			(origin, Xcm::HrmpCloseChannel { sender, recipient } ) => {
				let initiator = match origin {
					MultiLocation::X1(Junction::Parachain { id }) => id,
					_ => Err(XcmError::BadOrigin)?,
				};
				return Config::HrmpExecutor::hrmp_close_channel(initiator, sender, recipient)
			}
			(origin, Xcm::RelayTo { dest: MultiLocation::X1(Junction::Parachain { id }), inner }) => {
				let msg = Xcm::RelayedFrom { superorigin: origin, inner }.into();
				return Config::XcmSender::send_xcm(Junction::Parachain { id }.into(), msg)
			},
			_ => Err(XcmError::UnhandledXcmMessage)?,	// Unhandled XCM message.
		};

		// TODO: stuff that should happen after holding is populated but before effects,
		//   including depositing fees for effects from holding account.

		for effect in effects.into_iter() {
			let _ = Self::execute_effects(&origin, &mut holding, effect)?;
		}

		// TODO: stuff that should happen after effects including refunding unused fees.

		Ok(())
	}
}

impl<Config: config::Config> XcmExecutor<Config> {
	fn reanchored(mut assets: Assets, dest: &MultiLocation) -> Vec<MultiAsset> {
		let inv_dest = Config::LocationInverter::invert_location(&dest);
		assets.reanchor(&inv_dest);
		assets.into_assets_iter().collect::<Vec<_>>()
	}

	fn execute_effects(_origin: &MultiLocation, holding: &mut Assets, effect: Order) -> XcmResult {
		match effect {
			Order::DepositAsset { assets, dest } => {
				let deposited = holding.saturating_take(assets);
				for asset in deposited.into_assets_iter() {
					Config::AssetTransactor::deposit_asset(&asset, &dest)?;
				}
				Ok(())
			},
			Order::DepositReserveAsset { assets, dest, effects } => {
				let deposited = holding.saturating_take(assets);
				for asset in deposited.assets_iter() {
					Config::AssetTransactor::deposit_asset(&asset, &dest)?;
				}
				let assets = Self::reanchored(deposited, &dest);
				Config::XcmSender::send_xcm(dest, Xcm::ReserveAssetDeposit { assets, effects })
			},
			Order::InitiateReserveWithdraw { assets, reserve, effects} => {
				let assets = Self::reanchored(holding.saturating_take(assets), &reserve);
				Config::XcmSender::send_xcm(reserve, Xcm::WithdrawAsset { assets, effects })
			}
			Order::InitiateTeleport { assets, dest, effects} => {
				let assets = Self::reanchored(holding.saturating_take(assets), &dest);
				Config::XcmSender::send_xcm(dest, Xcm::TeleportAsset { assets, effects })
			}
			Order::QueryHolding { query_id, dest, assets } => {
				let assets = Self::reanchored(holding.min(assets.iter()), &dest);
				Config::XcmSender::send_xcm(dest, Xcm::Balances { query_id, assets })
			}
			_ => Err(XcmError::UnhandledEffect)?,
		}
	}
}
