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
//! Autogenerated weights for pallet_proxy
//!
//! THIS FILE WAS AUTO-GENERATED USING THE SUBSTRATE BENCHMARK CLI VERSION 3.0.0
//! DATE: 2021-04-27, STEPS: `[50, ]`, REPEAT: 20, LOW RANGE: `[]`, HIGH RANGE: `[]`
//! EXECUTION: Some(Wasm), WASM-EXECUTION: Compiled, CHAIN: Some("kusama-dev"), DB CACHE: 128

// Executed Command:
// target/release/polkadot
// benchmark
// --chain=kusama-dev
// --steps=50
// --repeat=20
// --pallet=pallet_proxy
// --extrinsic=*
// --execution=wasm
// --wasm-execution=compiled
// --heap-pages=4096
// --header=./file_header.txt
// --output=./runtime/kusama/src/weights/


#![allow(unused_parens)]
#![allow(unused_imports)]

use frame_support::{traits::Get, weights::Weight};
use sp_std::marker::PhantomData;

/// Weight functions for pallet_proxy.
pub struct WeightInfo<T>(PhantomData<T>);
impl<T: frame_system::Config> pallet_proxy::WeightInfo for WeightInfo<T> {
	fn proxy(p: u32, ) -> Weight {
		(22_588_000 as Weight)
			// Standard Error: 1_000
			.saturating_add((187_000 as Weight).saturating_mul(p as Weight))
			.saturating_add(T::DbWeight::get().reads(1 as Weight))
	}
	fn proxy_announced(a: u32, p: u32, ) -> Weight {
		(52_343_000 as Weight)
			// Standard Error: 1_000
			.saturating_add((663_000 as Weight).saturating_mul(a as Weight))
			// Standard Error: 1_000
			.saturating_add((181_000 as Weight).saturating_mul(p as Weight))
			.saturating_add(T::DbWeight::get().reads(3 as Weight))
			.saturating_add(T::DbWeight::get().writes(2 as Weight))
	}
	fn remove_announcement(a: u32, p: u32, ) -> Weight {
		(36_602_000 as Weight)
			// Standard Error: 1_000
			.saturating_add((660_000 as Weight).saturating_mul(a as Weight))
			// Standard Error: 1_000
			.saturating_add((14_000 as Weight).saturating_mul(p as Weight))
			.saturating_add(T::DbWeight::get().reads(2 as Weight))
			.saturating_add(T::DbWeight::get().writes(2 as Weight))
	}
	fn reject_announcement(a: u32, p: u32, ) -> Weight {
		(36_557_000 as Weight)
			// Standard Error: 1_000
			.saturating_add((659_000 as Weight).saturating_mul(a as Weight))
			// Standard Error: 1_000
			.saturating_add((14_000 as Weight).saturating_mul(p as Weight))
			.saturating_add(T::DbWeight::get().reads(2 as Weight))
			.saturating_add(T::DbWeight::get().writes(2 as Weight))
	}
	fn announce(a: u32, p: u32, ) -> Weight {
		(49_695_000 as Weight)
			// Standard Error: 1_000
			.saturating_add((655_000 as Weight).saturating_mul(a as Weight))
			// Standard Error: 1_000
			.saturating_add((176_000 as Weight).saturating_mul(p as Weight))
			.saturating_add(T::DbWeight::get().reads(3 as Weight))
			.saturating_add(T::DbWeight::get().writes(2 as Weight))
	}
	fn add_proxy(p: u32, ) -> Weight {
		(35_155_000 as Weight)
			// Standard Error: 1_000
			.saturating_add((248_000 as Weight).saturating_mul(p as Weight))
			.saturating_add(T::DbWeight::get().reads(1 as Weight))
			.saturating_add(T::DbWeight::get().writes(1 as Weight))
	}
	fn remove_proxy(p: u32, ) -> Weight {
		(34_937_000 as Weight)
			// Standard Error: 1_000
			.saturating_add((277_000 as Weight).saturating_mul(p as Weight))
			.saturating_add(T::DbWeight::get().reads(1 as Weight))
			.saturating_add(T::DbWeight::get().writes(1 as Weight))
	}
	fn remove_proxies(p: u32, ) -> Weight {
		(34_082_000 as Weight)
			// Standard Error: 1_000
			.saturating_add((191_000 as Weight).saturating_mul(p as Weight))
			.saturating_add(T::DbWeight::get().reads(1 as Weight))
			.saturating_add(T::DbWeight::get().writes(1 as Weight))
	}
	fn anonymous(p: u32, ) -> Weight {
		(47_817_000 as Weight)
			// Standard Error: 1_000
			.saturating_add((31_000 as Weight).saturating_mul(p as Weight))
			.saturating_add(T::DbWeight::get().reads(2 as Weight))
			.saturating_add(T::DbWeight::get().writes(1 as Weight))
	}
	fn kill_anonymous(p: u32, ) -> Weight {
		(35_987_000 as Weight)
			// Standard Error: 1_000
			.saturating_add((186_000 as Weight).saturating_mul(p as Weight))
			.saturating_add(T::DbWeight::get().reads(1 as Weight))
			.saturating_add(T::DbWeight::get().writes(1 as Weight))
	}
}
