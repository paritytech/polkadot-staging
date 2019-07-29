// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Parity Polkadot.

// Parity Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! A `CodeExecutor` specialisation which uses natively compiled runtime when the wasm to be
//! executed is equivalent to the natively compiled code.

use substrate_executor::native_executor_instance;

native_executor_instance!(
    pub Executor,
    polkadot_runtime::api::dispatch,
    polkadot_runtime::native_version,
    polkadot_runtime::WASM_BINARY
);
