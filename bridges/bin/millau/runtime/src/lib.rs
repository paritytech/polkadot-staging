// Copyright 2019-2021 Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

//! The Millau runtime. This can be compiled with `#[no_std]`, ready for Wasm.

#![cfg_attr(not(feature = "std"), no_std)]
// `construct_runtime!` does a lot of recursion and requires us to increase the limit to 256.
#![recursion_limit = "256"]
// Runtime-generated enums
#![allow(clippy::large_enum_variant)]
// Runtime-generated DecodeLimit::decode_all_With_depth_limit
#![allow(clippy::unnecessary_mut_passed)]
// From construct_runtime macro
#![allow(clippy::from_over_into)]

// Make the WASM binary available.
#[cfg(feature = "std")]
include!(concat!(env!("OUT_DIR"), "/wasm_binary.rs"));

pub mod rialto_messages;

use crate::rialto_messages::{ToRialtoMessagePayload, WithRialtoMessageBridge};

use bridge_runtime_common::messages::{source::estimate_message_dispatch_and_delivery_fee, MessageBridge};
use codec::Decode;
use pallet_grandpa::{fg_primitives, AuthorityId as GrandpaId, AuthorityList as GrandpaAuthorityList};
use pallet_transaction_payment::{FeeDetails, RuntimeDispatchInfo};
use sp_api::impl_runtime_apis;
use sp_consensus_aura::sr25519::AuthorityId as AuraId;
use sp_core::{crypto::KeyTypeId, OpaqueMetadata};
use sp_runtime::traits::{Block as BlockT, IdentityLookup, NumberFor, OpaqueKeys};
use sp_runtime::{
	create_runtime_str, generic, impl_opaque_keys,
	transaction_validity::{TransactionSource, TransactionValidity},
	ApplyExtrinsicResult, MultiSignature, MultiSigner,
};
use sp_std::prelude::*;
#[cfg(feature = "std")]
use sp_version::NativeVersion;
use sp_version::RuntimeVersion;

// A few exports that help ease life for downstream crates.
pub use frame_support::{
	construct_runtime, parameter_types,
	traits::{Currency, ExistenceRequirement, Imbalance, KeyOwnerProofSystem},
	weights::{constants::WEIGHT_PER_SECOND, DispatchClass, IdentityFee, RuntimeDbWeight, Weight},
	StorageValue,
};

pub use frame_system::Call as SystemCall;
pub use pallet_balances::Call as BalancesCall;
pub use pallet_bridge_grandpa::Call as BridgeGrandpaRialtoCall;
pub use pallet_bridge_grandpa::Call as BridgeGrandpaWestendCall;
pub use pallet_bridge_messages::Call as MessagesCall;
pub use pallet_sudo::Call as SudoCall;
pub use pallet_timestamp::Call as TimestampCall;

#[cfg(any(feature = "std", test))]
pub use sp_runtime::BuildStorage;
pub use sp_runtime::{Perbill, Permill};

/// An index to a block.
pub type BlockNumber = bp_millau::BlockNumber;

/// Alias to 512-bit hash when used in the context of a transaction signature on the chain.
pub type Signature = bp_millau::Signature;

/// Some way of identifying an account on the chain. We intentionally make it equivalent
/// to the public key of our transaction signing scheme.
pub type AccountId = bp_millau::AccountId;

/// The type for looking up accounts. We don't expect more than 4 billion of them, but you
/// never know...
pub type AccountIndex = u32;

/// Balance of an account.
pub type Balance = bp_millau::Balance;

/// Index of a transaction in the chain.
pub type Index = u32;

/// A hash of some data used by the chain.
pub type Hash = bp_millau::Hash;

/// Hashing algorithm used by the chain.
pub type Hashing = bp_millau::Hasher;

/// Digest item type.
pub type DigestItem = generic::DigestItem<Hash>;

/// Opaque types. These are used by the CLI to instantiate machinery that don't need to know
/// the specifics of the runtime. They can then be made to be agnostic over specific formats
/// of data like extrinsics, allowing for them to continue syncing the network through upgrades
/// to even the core data structures.
pub mod opaque {
	use super::*;

	pub use sp_runtime::OpaqueExtrinsic as UncheckedExtrinsic;

	/// Opaque block header type.
	pub type Header = generic::Header<BlockNumber, Hashing>;
	/// Opaque block type.
	pub type Block = generic::Block<Header, UncheckedExtrinsic>;
	/// Opaque block identifier type.
	pub type BlockId = generic::BlockId<Block>;
}

impl_opaque_keys! {
	pub struct SessionKeys {
		pub aura: Aura,
		pub grandpa: Grandpa,
	}
}

/// This runtime version.
pub const VERSION: RuntimeVersion = RuntimeVersion {
	spec_name: create_runtime_str!("millau-runtime"),
	impl_name: create_runtime_str!("millau-runtime"),
	authoring_version: 1,
	spec_version: 1,
	impl_version: 1,
	apis: RUNTIME_API_VERSIONS,
	transaction_version: 1,
};

/// The version information used to identify this runtime when compiled natively.
#[cfg(feature = "std")]
pub fn native_version() -> NativeVersion {
	NativeVersion {
		runtime_version: VERSION,
		can_author_with: Default::default(),
	}
}

parameter_types! {
	pub const BlockHashCount: BlockNumber = 250;
	pub const Version: RuntimeVersion = VERSION;
	pub const DbWeight: RuntimeDbWeight = RuntimeDbWeight {
		read: 60_000_000, // ~0.06 ms = ~60 µs
		write: 200_000_000, // ~0.2 ms = 200 µs
	};
	pub const SS58Prefix: u8 = 60;
}

impl frame_system::Config for Runtime {
	/// The basic call filter to use in dispatchable.
	type BaseCallFilter = frame_support::traits::AllowAll;
	/// The identifier used to distinguish between accounts.
	type AccountId = AccountId;
	/// The aggregated dispatch type that is available for extrinsics.
	type Call = Call;
	/// The lookup mechanism to get account ID from whatever is passed in dispatchers.
	type Lookup = IdentityLookup<AccountId>;
	/// The index type for storing how many extrinsics an account has signed.
	type Index = Index;
	/// The index type for blocks.
	type BlockNumber = BlockNumber;
	/// The type for hashing blocks and tries.
	type Hash = Hash;
	/// The hashing algorithm used.
	type Hashing = Hashing;
	/// The header type.
	type Header = generic::Header<BlockNumber, Hashing>;
	/// The ubiquitous event type.
	type Event = Event;
	/// The ubiquitous origin type.
	type Origin = Origin;
	/// Maximum number of block number to block hash mappings to keep (oldest pruned first).
	type BlockHashCount = BlockHashCount;
	/// Version of the runtime.
	type Version = Version;
	/// Provides information about the pallet setup in the runtime.
	type PalletInfo = PalletInfo;
	/// What to do if a new account is created.
	type OnNewAccount = ();
	/// What to do if an account is fully reaped from the system.
	type OnKilledAccount = ();
	/// The data to be stored in an account.
	type AccountData = pallet_balances::AccountData<Balance>;
	// TODO: update me (https://github.com/paritytech/parity-bridges-common/issues/78)
	/// Weight information for the extrinsics of this pallet.
	type SystemWeightInfo = ();
	/// Block and extrinsics weights: base values and limits.
	type BlockWeights = bp_millau::BlockWeights;
	/// The maximum length of a block (in bytes).
	type BlockLength = bp_millau::BlockLength;
	/// The weight of database operations that the runtime can invoke.
	type DbWeight = DbWeight;
	/// The designated `SS58` prefix of this chain.
	type SS58Prefix = SS58Prefix;
	/// The set code logic, just the default since we're not a parachain.
	type OnSetCode = ();
}

impl pallet_randomness_collective_flip::Config for Runtime {}

parameter_types! {
	pub const MaxAuthorities: u32 = 10;
}

impl pallet_aura::Config for Runtime {
	type AuthorityId = AuraId;
	type MaxAuthorities = MaxAuthorities;
}
impl pallet_bridge_dispatch::Config for Runtime {
	type Event = Event;
	type MessageId = (bp_messages::LaneId, bp_messages::MessageNonce);
	type Call = Call;
	type CallFilter = frame_support::traits::AllowAll;
	type EncodedCall = crate::rialto_messages::FromRialtoEncodedCall;
	type SourceChainAccountId = bp_rialto::AccountId;
	type TargetChainAccountPublic = MultiSigner;
	type TargetChainSignature = MultiSignature;
	type AccountIdConverter = bp_millau::AccountIdConverter;
}

impl pallet_grandpa::Config for Runtime {
	type Event = Event;
	type Call = Call;
	type KeyOwnerProofSystem = ();
	type KeyOwnerProof = <Self::KeyOwnerProofSystem as KeyOwnerProofSystem<(KeyTypeId, GrandpaId)>>::Proof;
	type KeyOwnerIdentification =
		<Self::KeyOwnerProofSystem as KeyOwnerProofSystem<(KeyTypeId, GrandpaId)>>::IdentificationTuple;
	type HandleEquivocation = ();
	// TODO: update me (https://github.com/paritytech/parity-bridges-common/issues/78)
	type WeightInfo = ();
}

parameter_types! {
	pub const MinimumPeriod: u64 = bp_millau::SLOT_DURATION / 2;
}

impl pallet_timestamp::Config for Runtime {
	/// A timestamp: milliseconds since the Unix epoch.
	type Moment = u64;
	type OnTimestampSet = Aura;
	type MinimumPeriod = MinimumPeriod;
	// TODO: update me (https://github.com/paritytech/parity-bridges-common/issues/78)
	type WeightInfo = ();
}

parameter_types! {
	pub const ExistentialDeposit: bp_millau::Balance = 500;
	// For weight estimation, we assume that the most locks on an individual account will be 50.
	// This number may need to be adjusted in the future if this assumption no longer holds true.
	pub const MaxLocks: u32 = 50;
	pub const MaxReserves: u32 = 50;
}

impl pallet_balances::Config for Runtime {
	/// The type for recording an account's balance.
	type Balance = Balance;
	/// The ubiquitous event type.
	type Event = Event;
	type DustRemoval = ();
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	// TODO: update me (https://github.com/paritytech/parity-bridges-common/issues/78)
	type WeightInfo = ();
	type MaxLocks = MaxLocks;
	type MaxReserves = MaxReserves;
	type ReserveIdentifier = [u8; 8];
}

parameter_types! {
	pub const TransactionBaseFee: Balance = 0;
	pub const TransactionByteFee: Balance = 1;
}

impl pallet_transaction_payment::Config for Runtime {
	type OnChargeTransaction = pallet_transaction_payment::CurrencyAdapter<Balances, ()>;
	type TransactionByteFee = TransactionByteFee;
	type WeightToFee = IdentityFee<Balance>;
	type FeeMultiplierUpdate = ();
}

impl pallet_sudo::Config for Runtime {
	type Event = Event;
	type Call = Call;
}

parameter_types! {
	/// Authorities are changing every 5 minutes.
	pub const Period: BlockNumber = bp_millau::SESSION_LENGTH;
	pub const Offset: BlockNumber = 0;
}

impl pallet_session::Config for Runtime {
	type Event = Event;
	type ValidatorId = <Self as frame_system::Config>::AccountId;
	type ValidatorIdOf = ();
	type ShouldEndSession = pallet_session::PeriodicSessions<Period, Offset>;
	type NextSessionRotation = pallet_session::PeriodicSessions<Period, Offset>;
	type SessionManager = pallet_shift_session_manager::Pallet<Runtime>;
	type SessionHandler = <SessionKeys as OpaqueKeys>::KeyTypeIdProviders;
	type Keys = SessionKeys;
	type DisabledValidatorsThreshold = ();
	// TODO: update me (https://github.com/paritytech/parity-bridges-common/issues/78)
	type WeightInfo = ();
}

parameter_types! {
	// This is a pretty unscientific cap.
	//
	// Note that once this is hit the pallet will essentially throttle incoming requests down to one
	// call per block.
	pub const MaxRequests: u32 = 50;

	// Number of headers to keep.
	//
	// Assuming the worst case of every header being finalized, we will keep headers for at least a
	// week.
	pub const HeadersToKeep: u32 = 7 * bp_millau::DAYS as u32;
}

pub type RialtoGrandpaInstance = ();
impl pallet_bridge_grandpa::Config for Runtime {
	type BridgedChain = bp_rialto::Rialto;
	type MaxRequests = MaxRequests;
	type HeadersToKeep = HeadersToKeep;

	// TODO [#391]: Use weights generated for the Millau runtime instead of Rialto ones.
	type WeightInfo = pallet_bridge_grandpa::weights::RialtoWeight<Runtime>;
}

pub type WestendGrandpaInstance = pallet_bridge_grandpa::Instance1;
impl pallet_bridge_grandpa::Config<WestendGrandpaInstance> for Runtime {
	type BridgedChain = bp_westend::Westend;
	type MaxRequests = MaxRequests;
	type HeadersToKeep = HeadersToKeep;

	// TODO [#391]: Use weights generated for the Millau runtime instead of Rialto ones.
	type WeightInfo = pallet_bridge_grandpa::weights::RialtoWeight<Runtime>;
}

impl pallet_shift_session_manager::Config for Runtime {}

parameter_types! {
	pub const MaxMessagesToPruneAtOnce: bp_messages::MessageNonce = 8;
	pub const MaxUnrewardedRelayerEntriesAtInboundLane: bp_messages::MessageNonce =
		bp_millau::MAX_UNREWARDED_RELAYER_ENTRIES_AT_INBOUND_LANE;
	pub const MaxUnconfirmedMessagesAtInboundLane: bp_messages::MessageNonce =
		bp_millau::MAX_UNCONFIRMED_MESSAGES_AT_INBOUND_LANE;
	// `IdentityFee` is used by Millau => we may use weight directly
	pub const GetDeliveryConfirmationTransactionFee: Balance =
		bp_millau::MAX_SINGLE_MESSAGE_DELIVERY_CONFIRMATION_TX_WEIGHT as _;
	pub const RootAccountForPayments: Option<AccountId> = None;
}

/// Instance of the messages pallet used to relay messages to/from Rialto chain.
pub type WithRialtoMessagesInstance = pallet_bridge_messages::DefaultInstance;

impl pallet_bridge_messages::Config<WithRialtoMessagesInstance> for Runtime {
	type Event = Event;
	// TODO: https://github.com/paritytech/parity-bridges-common/issues/390
	type WeightInfo = pallet_bridge_messages::weights::RialtoWeight<Runtime>;
	type Parameter = rialto_messages::MillauToRialtoMessagesParameter;
	type MaxMessagesToPruneAtOnce = MaxMessagesToPruneAtOnce;
	type MaxUnrewardedRelayerEntriesAtInboundLane = MaxUnrewardedRelayerEntriesAtInboundLane;
	type MaxUnconfirmedMessagesAtInboundLane = MaxUnconfirmedMessagesAtInboundLane;

	type OutboundPayload = crate::rialto_messages::ToRialtoMessagePayload;
	type OutboundMessageFee = Balance;

	type InboundPayload = crate::rialto_messages::FromRialtoMessagePayload;
	type InboundMessageFee = bp_rialto::Balance;
	type InboundRelayer = bp_rialto::AccountId;

	type AccountIdConverter = bp_millau::AccountIdConverter;

	type TargetHeaderChain = crate::rialto_messages::Rialto;
	type LaneMessageVerifier = crate::rialto_messages::ToRialtoMessageVerifier;
	type MessageDeliveryAndDispatchPayment = pallet_bridge_messages::instant_payments::InstantCurrencyPayments<
		Runtime,
		pallet_balances::Pallet<Runtime>,
		GetDeliveryConfirmationTransactionFee,
		RootAccountForPayments,
	>;
	type OnDeliveryConfirmed = ();

	type SourceHeaderChain = crate::rialto_messages::Rialto;
	type MessageDispatch = crate::rialto_messages::FromRialtoMessageDispatch;
}

construct_runtime!(
	pub enum Runtime where
		Block = Block,
		NodeBlock = opaque::Block,
		UncheckedExtrinsic = UncheckedExtrinsic
	{
		BridgeRialtoMessages: pallet_bridge_messages::{Pallet, Call, Storage, Event<T>},
		BridgeDispatch: pallet_bridge_dispatch::{Pallet, Event<T>},
		BridgeRialtoGrandpa: pallet_bridge_grandpa::{Pallet, Call, Storage},
		BridgeWestendGrandpa: pallet_bridge_grandpa::<Instance1>::{Pallet, Call, Config<T>, Storage},
		System: frame_system::{Pallet, Call, Config, Storage, Event<T>},
		RandomnessCollectiveFlip: pallet_randomness_collective_flip::{Pallet, Storage},
		Timestamp: pallet_timestamp::{Pallet, Call, Storage, Inherent},
		Aura: pallet_aura::{Pallet, Config<T>},
		Grandpa: pallet_grandpa::{Pallet, Call, Storage, Config, Event},
		Balances: pallet_balances::{Pallet, Call, Storage, Config<T>, Event<T>},
		TransactionPayment: pallet_transaction_payment::{Pallet, Storage},
		Sudo: pallet_sudo::{Pallet, Call, Config<T>, Storage, Event<T>},
		Session: pallet_session::{Pallet, Call, Storage, Event, Config<T>},
		ShiftSessionManager: pallet_shift_session_manager::{Pallet},
	}
);

/// The address format for describing accounts.
pub type Address = AccountId;
/// Block header type as expected by this runtime.
pub type Header = generic::Header<BlockNumber, Hashing>;
/// Block type as expected by this runtime.
pub type Block = generic::Block<Header, UncheckedExtrinsic>;
/// A Block signed with a Justification
pub type SignedBlock = generic::SignedBlock<Block>;
/// `BlockId` type as expected by this runtime.
pub type BlockId = generic::BlockId<Block>;
/// The `SignedExtension` to the basic transaction logic.
pub type SignedExtra = (
	frame_system::CheckSpecVersion<Runtime>,
	frame_system::CheckTxVersion<Runtime>,
	frame_system::CheckGenesis<Runtime>,
	frame_system::CheckEra<Runtime>,
	frame_system::CheckNonce<Runtime>,
	frame_system::CheckWeight<Runtime>,
	pallet_transaction_payment::ChargeTransactionPayment<Runtime>,
);
/// The payload being signed in transactions.
pub type SignedPayload = generic::SignedPayload<Call, SignedExtra>;
/// Unchecked extrinsic type as expected by this runtime.
pub type UncheckedExtrinsic = generic::UncheckedExtrinsic<Address, Call, Signature, SignedExtra>;
/// Extrinsic type that has already been checked.
pub type CheckedExtrinsic = generic::CheckedExtrinsic<AccountId, Call, SignedExtra>;
/// Executive: handles dispatch to the various modules.
pub type Executive =
	frame_executive::Executive<Runtime, Block, frame_system::ChainContext<Runtime>, Runtime, AllPallets>;

impl_runtime_apis! {
	impl sp_api::Core<Block> for Runtime {
		fn version() -> RuntimeVersion {
			VERSION
		}

		fn execute_block(block: Block) {
			Executive::execute_block(block);
		}

		fn initialize_block(header: &<Block as BlockT>::Header) {
			Executive::initialize_block(header)
		}
	}

	impl sp_api::Metadata<Block> for Runtime {
		fn metadata() -> OpaqueMetadata {
			Runtime::metadata().into()
		}
	}

	impl sp_block_builder::BlockBuilder<Block> for Runtime {
		fn apply_extrinsic(extrinsic: <Block as BlockT>::Extrinsic) -> ApplyExtrinsicResult {
			Executive::apply_extrinsic(extrinsic)
		}

		fn finalize_block() -> <Block as BlockT>::Header {
			Executive::finalize_block()
		}

		fn inherent_extrinsics(data: sp_inherents::InherentData) -> Vec<<Block as BlockT>::Extrinsic> {
			data.create_extrinsics()
		}

		fn check_inherents(
			block: Block,
			data: sp_inherents::InherentData,
		) -> sp_inherents::CheckInherentsResult {
			data.check_extrinsics(&block)
		}
	}

	impl frame_system_rpc_runtime_api::AccountNonceApi<Block, AccountId, Index> for Runtime {
		fn account_nonce(account: AccountId) -> Index {
			System::account_nonce(account)
		}
	}

	impl sp_transaction_pool::runtime_api::TaggedTransactionQueue<Block> for Runtime {
		fn validate_transaction(
			source: TransactionSource,
			tx: <Block as BlockT>::Extrinsic,
			block_hash: <Block as BlockT>::Hash,
		) -> TransactionValidity {
			Executive::validate_transaction(source, tx, block_hash)
		}
	}

	impl sp_offchain::OffchainWorkerApi<Block> for Runtime {
		fn offchain_worker(header: &<Block as BlockT>::Header) {
			Executive::offchain_worker(header)
		}
	}

	impl sp_consensus_aura::AuraApi<Block, AuraId> for Runtime {
		fn slot_duration() -> sp_consensus_aura::SlotDuration {
			sp_consensus_aura::SlotDuration::from_millis(Aura::slot_duration())
		}

		fn authorities() -> Vec<AuraId> {
			Aura::authorities()
		}
	}

	impl pallet_transaction_payment_rpc_runtime_api::TransactionPaymentApi<
		Block,
		Balance,
	> for Runtime {
		fn query_info(uxt: <Block as BlockT>::Extrinsic, len: u32) -> RuntimeDispatchInfo<Balance> {
			TransactionPayment::query_info(uxt, len)
		}
		fn query_fee_details(uxt: <Block as BlockT>::Extrinsic, len: u32) -> FeeDetails<Balance> {
			TransactionPayment::query_fee_details(uxt, len)
		}
	}

	impl sp_session::SessionKeys<Block> for Runtime {
		fn generate_session_keys(seed: Option<Vec<u8>>) -> Vec<u8> {
			SessionKeys::generate(seed)
		}

		fn decode_session_keys(
			encoded: Vec<u8>,
		) -> Option<Vec<(Vec<u8>, sp_core::crypto::KeyTypeId)>> {
			SessionKeys::decode_into_raw_public_keys(&encoded)
		}
	}

	impl fg_primitives::GrandpaApi<Block> for Runtime {
		fn grandpa_authorities() -> GrandpaAuthorityList {
			Grandpa::grandpa_authorities()
		}

		fn submit_report_equivocation_unsigned_extrinsic(
			equivocation_proof: fg_primitives::EquivocationProof<
				<Block as BlockT>::Hash,
				NumberFor<Block>,
			>,
			key_owner_proof: fg_primitives::OpaqueKeyOwnershipProof,
		) -> Option<()> {
			let key_owner_proof = key_owner_proof.decode()?;

			Grandpa::submit_unsigned_equivocation_report(
				equivocation_proof,
				key_owner_proof,
			)
		}

		fn generate_key_ownership_proof(
			_set_id: fg_primitives::SetId,
			_authority_id: GrandpaId,
		) -> Option<fg_primitives::OpaqueKeyOwnershipProof> {
			// NOTE: this is the only implementation possible since we've
			// defined our key owner proof type as a bottom type (i.e. a type
			// with no values).
			None
		}
	}

	impl bp_rialto::RialtoFinalityApi<Block> for Runtime {
		fn best_finalized() -> (bp_rialto::BlockNumber, bp_rialto::Hash) {
			let header = BridgeRialtoGrandpa::best_finalized();
			(header.number, header.hash())
		}

		fn is_known_header(hash: bp_rialto::Hash) -> bool {
			BridgeRialtoGrandpa::is_known_header(hash)
		}
	}

	impl bp_westend::WestendFinalityApi<Block> for Runtime {
		fn best_finalized() -> (bp_westend::BlockNumber, bp_westend::Hash) {
			let header = BridgeWestendGrandpa::best_finalized();
			(header.number, header.hash())
		}

		fn is_known_header(hash: bp_westend::Hash) -> bool {
			BridgeWestendGrandpa::is_known_header(hash)
		}
	}

	impl bp_rialto::ToRialtoOutboundLaneApi<Block, Balance, ToRialtoMessagePayload> for Runtime {
		fn estimate_message_delivery_and_dispatch_fee(
			_lane_id: bp_messages::LaneId,
			payload: ToRialtoMessagePayload,
		) -> Option<Balance> {
			estimate_message_dispatch_and_delivery_fee::<WithRialtoMessageBridge>(
				&payload,
				WithRialtoMessageBridge::RELAYER_FEE_PERCENT,
			).ok()
		}

		fn message_details(
			lane: bp_messages::LaneId,
			begin: bp_messages::MessageNonce,
			end: bp_messages::MessageNonce,
		) -> Vec<bp_messages::MessageDetails<Balance>> {
			(begin..=end).filter_map(|nonce| {
				let message_data = BridgeRialtoMessages::outbound_message_data(lane, nonce)?;
				let decoded_payload = rialto_messages::ToRialtoMessagePayload::decode(
					&mut &message_data.payload[..]
				).ok()?;
				Some(bp_messages::MessageDetails {
					nonce,
					dispatch_weight: decoded_payload.weight,
					size: message_data.payload.len() as _,
					delivery_and_dispatch_fee: message_data.fee,
					dispatch_fee_payment: decoded_payload.dispatch_fee_payment,
				})
			})
			.collect()
		}

		fn latest_received_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeRialtoMessages::outbound_latest_received_nonce(lane)
		}

		fn latest_generated_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeRialtoMessages::outbound_latest_generated_nonce(lane)
		}
	}

	impl bp_rialto::FromRialtoInboundLaneApi<Block> for Runtime {
		fn latest_received_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeRialtoMessages::inbound_latest_received_nonce(lane)
		}

		fn latest_confirmed_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeRialtoMessages::inbound_latest_confirmed_nonce(lane)
		}

		fn unrewarded_relayers_state(lane: bp_messages::LaneId) -> bp_messages::UnrewardedRelayersState {
			BridgeRialtoMessages::inbound_unrewarded_relayers_state(lane)
		}
	}
}

/// Rialto account ownership digest from Millau.
///
/// The byte vector returned by this function should be signed with a Rialto account private key.
/// This way, the owner of `millau_account_id` on Millau proves that the Rialto account private key
/// is also under his control.
pub fn millau_to_rialto_account_ownership_digest<Call, AccountId, SpecVersion>(
	rialto_call: &Call,
	millau_account_id: AccountId,
	rialto_spec_version: SpecVersion,
) -> sp_std::vec::Vec<u8>
where
	Call: codec::Encode,
	AccountId: codec::Encode,
	SpecVersion: codec::Encode,
{
	pallet_bridge_dispatch::account_ownership_digest(
		rialto_call,
		millau_account_id,
		rialto_spec_version,
		bp_runtime::MILLAU_CHAIN_ID,
		bp_runtime::RIALTO_CHAIN_ID,
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use bridge_runtime_common::messages;

	#[test]
	fn ensure_millau_message_lane_weights_are_correct() {
		// TODO: https://github.com/paritytech/parity-bridges-common/issues/390
		type Weights = pallet_bridge_messages::weights::RialtoWeight<Runtime>;

		pallet_bridge_messages::ensure_weights_are_correct::<Weights>(
			bp_millau::DEFAULT_MESSAGE_DELIVERY_TX_WEIGHT,
			bp_millau::ADDITIONAL_MESSAGE_BYTE_DELIVERY_WEIGHT,
			bp_millau::MAX_SINGLE_MESSAGE_DELIVERY_CONFIRMATION_TX_WEIGHT,
			bp_millau::PAY_INBOUND_DISPATCH_FEE_WEIGHT,
		);

		let max_incoming_message_proof_size = bp_rialto::EXTRA_STORAGE_PROOF_SIZE.saturating_add(
			messages::target::maximal_incoming_message_size(bp_millau::max_extrinsic_size()),
		);
		pallet_bridge_messages::ensure_able_to_receive_message::<Weights>(
			bp_millau::max_extrinsic_size(),
			bp_millau::max_extrinsic_weight(),
			max_incoming_message_proof_size,
			messages::target::maximal_incoming_message_dispatch_weight(bp_millau::max_extrinsic_weight()),
		);

		let max_incoming_inbound_lane_data_proof_size = bp_messages::InboundLaneData::<()>::encoded_size_hint(
			bp_millau::MAXIMAL_ENCODED_ACCOUNT_ID_SIZE,
			bp_rialto::MAX_UNREWARDED_RELAYER_ENTRIES_AT_INBOUND_LANE as _,
			bp_rialto::MAX_UNCONFIRMED_MESSAGES_AT_INBOUND_LANE as _,
		)
		.unwrap_or(u32::MAX);
		pallet_bridge_messages::ensure_able_to_receive_confirmation::<Weights>(
			bp_millau::max_extrinsic_size(),
			bp_millau::max_extrinsic_weight(),
			max_incoming_inbound_lane_data_proof_size,
			bp_rialto::MAX_UNREWARDED_RELAYER_ENTRIES_AT_INBOUND_LANE,
			bp_rialto::MAX_UNCONFIRMED_MESSAGES_AT_INBOUND_LANE,
		);
	}
}
