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

//! The Rialto runtime. This can be compiled with `#[no_std]`, ready for Wasm.

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

pub mod exchange;

#[cfg(feature = "runtime-benchmarks")]
pub mod benches;
pub mod kovan;
pub mod millau_messages;
pub mod rialto_poa;

use crate::millau_messages::{ToMillauMessagePayload, WithMillauMessageBridge};

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
pub use pallet_bridge_currency_exchange::Call as BridgeCurrencyExchangeCall;
pub use pallet_bridge_eth_poa::Call as BridgeEthPoACall;
pub use pallet_bridge_grandpa::Call as BridgeGrandpaMillauCall;
pub use pallet_bridge_messages::Call as MessagesCall;
pub use pallet_sudo::Call as SudoCall;
pub use pallet_timestamp::Call as TimestampCall;

#[cfg(any(feature = "std", test))]
pub use sp_runtime::BuildStorage;
pub use sp_runtime::{Perbill, Permill};

/// An index to a block.
pub type BlockNumber = bp_rialto::BlockNumber;

/// Alias to 512-bit hash when used in the context of a transaction signature on the chain.
pub type Signature = bp_rialto::Signature;

/// Some way of identifying an account on the chain. We intentionally make it equivalent
/// to the public key of our transaction signing scheme.
pub type AccountId = bp_rialto::AccountId;

/// The type for looking up accounts. We don't expect more than 4 billion of them, but you
/// never know...
pub type AccountIndex = u32;

/// Balance of an account.
pub type Balance = bp_rialto::Balance;

/// Index of a transaction in the chain.
pub type Index = u32;

/// A hash of some data used by the chain.
pub type Hash = bp_rialto::Hash;

/// Hashing algorithm used by the chain.
pub type Hashing = bp_rialto::Hasher;

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
	spec_name: create_runtime_str!("rialto-runtime"),
	impl_name: create_runtime_str!("rialto-runtime"),
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
	pub const SS58Prefix: u8 = 48;
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
	type BlockWeights = bp_rialto::BlockWeights;
	/// The maximum length of a block (in bytes).
	type BlockLength = bp_rialto::BlockLength;
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

type RialtoPoA = pallet_bridge_eth_poa::Instance1;
impl pallet_bridge_eth_poa::Config<RialtoPoA> for Runtime {
	type AuraConfiguration = rialto_poa::BridgeAuraConfiguration;
	type FinalityVotesCachingInterval = rialto_poa::FinalityVotesCachingInterval;
	type ValidatorsConfiguration = rialto_poa::BridgeValidatorsConfiguration;
	type PruningStrategy = rialto_poa::PruningStrategy;
	type ChainTime = rialto_poa::ChainTime;
	type OnHeadersSubmitted = ();
}

type Kovan = pallet_bridge_eth_poa::Instance2;
impl pallet_bridge_eth_poa::Config<Kovan> for Runtime {
	type AuraConfiguration = kovan::BridgeAuraConfiguration;
	type FinalityVotesCachingInterval = kovan::FinalityVotesCachingInterval;
	type ValidatorsConfiguration = kovan::BridgeValidatorsConfiguration;
	type PruningStrategy = kovan::PruningStrategy;
	type ChainTime = kovan::ChainTime;
	type OnHeadersSubmitted = ();
}

type RialtoCurrencyExchange = pallet_bridge_currency_exchange::Instance1;
impl pallet_bridge_currency_exchange::Config<RialtoCurrencyExchange> for Runtime {
	type OnTransactionSubmitted = ();
	type PeerBlockchain = rialto_poa::RialtoBlockchain;
	type PeerMaybeLockFundsTransaction = exchange::EthTransaction;
	type RecipientsMap = bp_currency_exchange::IdentityRecipients<AccountId>;
	type Amount = Balance;
	type CurrencyConverter = bp_currency_exchange::IdentityCurrencyConverter<Balance>;
	type DepositInto = DepositInto;
}

type KovanCurrencyExchange = pallet_bridge_currency_exchange::Instance2;
impl pallet_bridge_currency_exchange::Config<KovanCurrencyExchange> for Runtime {
	type OnTransactionSubmitted = ();
	type PeerBlockchain = kovan::KovanBlockchain;
	type PeerMaybeLockFundsTransaction = exchange::EthTransaction;
	type RecipientsMap = bp_currency_exchange::IdentityRecipients<AccountId>;
	type Amount = Balance;
	type CurrencyConverter = bp_currency_exchange::IdentityCurrencyConverter<Balance>;
	type DepositInto = DepositInto;
}

impl pallet_bridge_dispatch::Config for Runtime {
	type Event = Event;
	type MessageId = (bp_messages::LaneId, bp_messages::MessageNonce);
	type Call = Call;
	type CallFilter = frame_support::traits::AllowAll;
	type EncodedCall = crate::millau_messages::FromMillauEncodedCall;
	type SourceChainAccountId = bp_millau::AccountId;
	type TargetChainAccountPublic = MultiSigner;
	type TargetChainSignature = MultiSignature;
	type AccountIdConverter = bp_rialto::AccountIdConverter;
}

pub struct DepositInto;

impl bp_currency_exchange::DepositInto for DepositInto {
	type Recipient = AccountId;
	type Amount = Balance;

	fn deposit_into(recipient: Self::Recipient, amount: Self::Amount) -> bp_currency_exchange::Result<()> {
		// let balances module make all checks for us (it won't allow depositing lower than existential
		// deposit, balance overflow, ...)
		let deposited = <pallet_balances::Pallet<Runtime> as Currency<AccountId>>::deposit_creating(&recipient, amount);

		// I'm dropping deposited here explicitly to illustrate the fact that it'll update `TotalIssuance`
		// on drop
		let deposited_amount = deposited.peek();
		drop(deposited);

		// we have 3 cases here:
		// - deposited == amount: success
		// - deposited == 0: deposit has failed and no changes to storage were made
		// - deposited != 0: (should never happen in practice) deposit has been partially completed
		match deposited_amount {
			_ if deposited_amount == amount => {
				log::trace!(
					target: "runtime",
					"Deposited {} to {:?}",
					amount,
					recipient,
				);

				Ok(())
			}
			_ if deposited_amount == 0 => {
				log::error!(
					target: "runtime",
					"Deposit of {} to {:?} has failed",
					amount,
					recipient,
				);

				Err(bp_currency_exchange::Error::DepositFailed)
			}
			_ => {
				log::error!(
					target: "runtime",
					"Deposit of {} to {:?} has partially competed. {} has been deposited",
					amount,
					recipient,
					deposited_amount,
				);

				// we can't return DepositFailed error here, because storage changes were made
				Err(bp_currency_exchange::Error::DepositPartiallyFailed)
			}
		}
	}
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
	pub const MinimumPeriod: u64 = bp_rialto::SLOT_DURATION / 2;
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
	pub const ExistentialDeposit: bp_rialto::Balance = 500;
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
	pub const Period: BlockNumber = bp_rialto::SESSION_LENGTH;
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
	/// This is a pretty unscientific cap.
	///
	/// Note that once this is hit the pallet will essentially throttle incoming requests down to one
	/// call per block.
	pub const MaxRequests: u32 = 50;
}

#[cfg(feature = "runtime-benchmarks")]
parameter_types! {
	/// Number of headers to keep in benchmarks.
	///
	/// In benchmarks we always populate with full number of `HeadersToKeep` to make sure that
	/// pruning is taken into account.
	///
	/// Note: This is lower than regular value, to speed up benchmarking setup.
	pub const HeadersToKeep: u32 = 1024;
}

#[cfg(not(feature = "runtime-benchmarks"))]
parameter_types! {
	/// Number of headers to keep.
	///
	/// Assuming the worst case of every header being finalized, we will keep headers at least for a
	/// week.
	pub const HeadersToKeep: u32 = 7 * bp_rialto::DAYS as u32;
}

pub type MillauGrandpaInstance = ();
impl pallet_bridge_grandpa::Config for Runtime {
	type BridgedChain = bp_millau::Millau;
	type MaxRequests = MaxRequests;
	type HeadersToKeep = HeadersToKeep;
	type WeightInfo = pallet_bridge_grandpa::weights::RialtoWeight<Runtime>;
}

impl pallet_shift_session_manager::Config for Runtime {}

parameter_types! {
	pub const MaxMessagesToPruneAtOnce: bp_messages::MessageNonce = 8;
	pub const MaxUnrewardedRelayerEntriesAtInboundLane: bp_messages::MessageNonce =
		bp_rialto::MAX_UNREWARDED_RELAYER_ENTRIES_AT_INBOUND_LANE;
	pub const MaxUnconfirmedMessagesAtInboundLane: bp_messages::MessageNonce =
		bp_rialto::MAX_UNCONFIRMED_MESSAGES_AT_INBOUND_LANE;
	// `IdentityFee` is used by Rialto => we may use weight directly
	pub const GetDeliveryConfirmationTransactionFee: Balance =
		bp_rialto::MAX_SINGLE_MESSAGE_DELIVERY_CONFIRMATION_TX_WEIGHT as _;
	pub const RootAccountForPayments: Option<AccountId> = None;
}

/// Instance of the messages pallet used to relay messages to/from Millau chain.
pub type WithMillauMessagesInstance = pallet_bridge_messages::DefaultInstance;

impl pallet_bridge_messages::Config<WithMillauMessagesInstance> for Runtime {
	type Event = Event;
	type WeightInfo = pallet_bridge_messages::weights::RialtoWeight<Runtime>;
	type Parameter = millau_messages::RialtoToMillauMessagesParameter;
	type MaxMessagesToPruneAtOnce = MaxMessagesToPruneAtOnce;
	type MaxUnrewardedRelayerEntriesAtInboundLane = MaxUnrewardedRelayerEntriesAtInboundLane;
	type MaxUnconfirmedMessagesAtInboundLane = MaxUnconfirmedMessagesAtInboundLane;

	type OutboundPayload = crate::millau_messages::ToMillauMessagePayload;
	type OutboundMessageFee = Balance;

	type InboundPayload = crate::millau_messages::FromMillauMessagePayload;
	type InboundMessageFee = bp_millau::Balance;
	type InboundRelayer = bp_millau::AccountId;

	type AccountIdConverter = bp_rialto::AccountIdConverter;

	type TargetHeaderChain = crate::millau_messages::Millau;
	type LaneMessageVerifier = crate::millau_messages::ToMillauMessageVerifier;
	type MessageDeliveryAndDispatchPayment = pallet_bridge_messages::instant_payments::InstantCurrencyPayments<
		Runtime,
		pallet_balances::Pallet<Runtime>,
		GetDeliveryConfirmationTransactionFee,
		RootAccountForPayments,
	>;
	type OnDeliveryConfirmed = ();

	type SourceHeaderChain = crate::millau_messages::Millau;
	type MessageDispatch = crate::millau_messages::FromMillauMessageDispatch;
}

construct_runtime!(
	pub enum Runtime where
		Block = Block,
		NodeBlock = opaque::Block,
		UncheckedExtrinsic = UncheckedExtrinsic
	{
		BridgeRialtoPoa: pallet_bridge_eth_poa::<Instance1>::{Pallet, Call, Config, Storage, ValidateUnsigned},
		BridgeKovan: pallet_bridge_eth_poa::<Instance2>::{Pallet, Call, Config, Storage, ValidateUnsigned},
		BridgeRialtoCurrencyExchange: pallet_bridge_currency_exchange::<Instance1>::{Pallet, Call},
		BridgeKovanCurrencyExchange: pallet_bridge_currency_exchange::<Instance2>::{Pallet, Call},
		BridgeMillauGrandpa: pallet_bridge_grandpa::{Pallet, Call, Storage},
		BridgeDispatch: pallet_bridge_dispatch::{Pallet, Event<T>},
		BridgeMillauMessages: pallet_bridge_messages::{Pallet, Call, Storage, Event<T>},
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

	impl bp_eth_poa::RialtoPoAHeaderApi<Block> for Runtime {
		fn best_block() -> (u64, bp_eth_poa::H256) {
			let best_block = BridgeRialtoPoa::best_block();
			(best_block.number, best_block.hash)
		}

		fn finalized_block() -> (u64, bp_eth_poa::H256) {
			let finalized_block = BridgeRialtoPoa::finalized_block();
			(finalized_block.number, finalized_block.hash)
		}

		fn is_import_requires_receipts(header: bp_eth_poa::AuraHeader) -> bool {
			BridgeRialtoPoa::is_import_requires_receipts(header)
		}

		fn is_known_block(hash: bp_eth_poa::H256) -> bool {
			BridgeRialtoPoa::is_known_block(hash)
		}
	}

	impl bp_eth_poa::KovanHeaderApi<Block> for Runtime {
		fn best_block() -> (u64, bp_eth_poa::H256) {
			let best_block = BridgeKovan::best_block();
			(best_block.number, best_block.hash)
		}

		fn finalized_block() -> (u64, bp_eth_poa::H256) {
			let finalized_block = BridgeKovan::finalized_block();
			(finalized_block.number, finalized_block.hash)
		}

		fn is_import_requires_receipts(header: bp_eth_poa::AuraHeader) -> bool {
			BridgeKovan::is_import_requires_receipts(header)
		}

		fn is_known_block(hash: bp_eth_poa::H256) -> bool {
			BridgeKovan::is_known_block(hash)
		}
	}

	impl bp_millau::MillauFinalityApi<Block> for Runtime {
		fn best_finalized() -> (bp_millau::BlockNumber, bp_millau::Hash) {
			let header = BridgeMillauGrandpa::best_finalized();
			(header.number, header.hash())
		}

		fn is_known_header(hash: bp_millau::Hash) -> bool {
			BridgeMillauGrandpa::is_known_header(hash)
		}
	}

	impl bp_currency_exchange::RialtoCurrencyExchangeApi<Block, exchange::EthereumTransactionInclusionProof> for Runtime {
		fn filter_transaction_proof(proof: exchange::EthereumTransactionInclusionProof) -> bool {
			BridgeRialtoCurrencyExchange::filter_transaction_proof(&proof)
		}
	}

	impl bp_currency_exchange::KovanCurrencyExchangeApi<Block, exchange::EthereumTransactionInclusionProof> for Runtime {
		fn filter_transaction_proof(proof: exchange::EthereumTransactionInclusionProof) -> bool {
			BridgeKovanCurrencyExchange::filter_transaction_proof(&proof)
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

	impl bp_millau::ToMillauOutboundLaneApi<Block, Balance, ToMillauMessagePayload> for Runtime {
		fn estimate_message_delivery_and_dispatch_fee(
			_lane_id: bp_messages::LaneId,
			payload: ToMillauMessagePayload,
		) -> Option<Balance> {
			estimate_message_dispatch_and_delivery_fee::<WithMillauMessageBridge>(
				&payload,
				WithMillauMessageBridge::RELAYER_FEE_PERCENT,
			).ok()
		}

		fn message_details(
			lane: bp_messages::LaneId,
			begin: bp_messages::MessageNonce,
			end: bp_messages::MessageNonce,
		) -> Vec<bp_messages::MessageDetails<Balance>> {
			(begin..=end).filter_map(|nonce| {
				let message_data = BridgeMillauMessages::outbound_message_data(lane, nonce)?;
				let decoded_payload = millau_messages::ToMillauMessagePayload::decode(
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
			BridgeMillauMessages::outbound_latest_received_nonce(lane)
		}

		fn latest_generated_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeMillauMessages::outbound_latest_generated_nonce(lane)
		}
	}

	impl bp_millau::FromMillauInboundLaneApi<Block> for Runtime {
		fn latest_received_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeMillauMessages::inbound_latest_received_nonce(lane)
		}

		fn latest_confirmed_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeMillauMessages::inbound_latest_confirmed_nonce(lane)
		}

		fn unrewarded_relayers_state(lane: bp_messages::LaneId) -> bp_messages::UnrewardedRelayersState {
			BridgeMillauMessages::inbound_unrewarded_relayers_state(lane)
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	impl frame_benchmarking::Benchmark<Block> for Runtime {
		fn dispatch_benchmark(
			config: frame_benchmarking::BenchmarkConfig,
		) -> Result<
			(Vec<frame_benchmarking::BenchmarkBatch>, Vec<frame_support::traits::StorageInfo>),
			sp_runtime::RuntimeString,
		> {
			use frame_benchmarking::{Benchmarking, BenchmarkBatch, add_benchmark, TrackedStorageKey};
			use frame_support::traits::StorageInfoTrait;

			let whitelist: Vec<TrackedStorageKey> = vec![
				// Block Number
				hex_literal::hex!("26aa394eea5630e07c48ae0c9558cef702a5c1b19ab7a04f536c519aca4983ac").to_vec().into(),
				// Execution Phase
				hex_literal::hex!("26aa394eea5630e07c48ae0c9558cef7ff553b5a9862a516939d82b3d3d8661a").to_vec().into(),
				// Event Count
				hex_literal::hex!("26aa394eea5630e07c48ae0c9558cef70a98fdbe9ce6c55837576c60c7af3850").to_vec().into(),
				// System Events
				hex_literal::hex!("26aa394eea5630e07c48ae0c9558cef780d41e5e16056765bc8461851072c9d7").to_vec().into(),
				// Caller 0 Account
				hex_literal::hex!("26aa394eea5630e07c48ae0c9558cef7b99d880ec681799c0cf30e8886371da946c154ffd9992e395af90b5b13cc6f295c77033fce8a9045824a6690bbf99c6db269502f0a8d1d2a008542d5690a0749").to_vec().into(),
			];

			let mut batches = Vec::<BenchmarkBatch>::new();
			let params = (&config, &whitelist);

			use pallet_bridge_currency_exchange::benchmarking::{
				Pallet as BridgeCurrencyExchangeBench,
				Config as BridgeCurrencyExchangeConfig,
				ProofParams as BridgeCurrencyExchangeProofParams,
			};

			impl BridgeCurrencyExchangeConfig<KovanCurrencyExchange> for Runtime {
				fn make_proof(
					proof_params: BridgeCurrencyExchangeProofParams<AccountId>,
				) -> crate::exchange::EthereumTransactionInclusionProof {
					use bp_currency_exchange::DepositInto;

					if proof_params.recipient_exists {
						<Runtime as pallet_bridge_currency_exchange::Config<KovanCurrencyExchange>>::DepositInto::deposit_into(
							proof_params.recipient.clone(),
							ExistentialDeposit::get(),
						).unwrap();
					}

					let (transaction, receipt) = crate::exchange::prepare_ethereum_transaction(
						&proof_params.recipient,
						|tx| {
							// our runtime only supports transactions where data is exactly 32 bytes long
							// (receiver key)
							// => we are ignoring `transaction_size_factor` here
							tx.value = (ExistentialDeposit::get() * 10).into();
						},
					);
					let transactions = sp_std::iter::repeat((transaction, receipt))
						.take(1 + proof_params.proof_size_factor as usize)
						.collect::<Vec<_>>();
					let block_hash = crate::exchange::prepare_environment_for_claim::<Runtime, Kovan>(&transactions);
					crate::exchange::EthereumTransactionInclusionProof {
						block: block_hash,
						index: 0,
						proof: transactions,
					}
				}
			}

			use crate::millau_messages::{ToMillauMessagePayload, WithMillauMessageBridge};
			use bp_runtime::messages::DispatchFeePayment;
			use bridge_runtime_common::messages;
			use pallet_bridge_messages::benchmarking::{
				Pallet as MessagesBench,
				Config as MessagesConfig,
				MessageDeliveryProofParams,
				MessageParams,
				MessageProofParams,
				ProofSize as MessagesProofSize,
			};

			impl MessagesConfig<WithMillauMessagesInstance> for Runtime {
				fn maximal_message_size() -> u32 {
					messages::source::maximal_message_size::<WithMillauMessageBridge>()
				}

				fn bridged_relayer_id() -> Self::InboundRelayer {
					Default::default()
				}

				fn account_balance(account: &Self::AccountId) -> Self::OutboundMessageFee {
					pallet_balances::Pallet::<Runtime>::free_balance(account)
				}

				fn endow_account(account: &Self::AccountId) {
					pallet_balances::Pallet::<Runtime>::make_free_balance_be(
						account,
						Balance::MAX / 100,
					);
				}

				fn prepare_outbound_message(
					params: MessageParams<Self::AccountId>,
				) -> (millau_messages::ToMillauMessagePayload, Balance) {
					let message_payload = vec![0; params.size as usize];
					let dispatch_origin = bp_message_dispatch::CallOrigin::SourceAccount(
						params.sender_account,
					);

					let message = ToMillauMessagePayload {
						spec_version: 0,
						weight: params.size as _,
						origin: dispatch_origin,
						call: message_payload,
						dispatch_fee_payment: DispatchFeePayment::AtSourceChain,
					};
					(message, pallet_bridge_messages::benchmarking::MESSAGE_FEE.into())
				}

				fn prepare_message_proof(
					params: MessageProofParams,
				) -> (millau_messages::FromMillauMessagesProof, Weight) {
					use crate::millau_messages::WithMillauMessageBridge;
					use bp_messages::MessageKey;
					use bridge_runtime_common::{
						messages::MessageBridge,
						messages_benchmarking::{ed25519_sign, prepare_message_proof},
					};
					use codec::Encode;
					use frame_support::weights::GetDispatchInfo;
					use pallet_bridge_messages::storage_keys;
					use sp_runtime::traits::{Header, IdentifyAccount};

					let remark = match params.size {
						MessagesProofSize::Minimal(ref size) => vec![0u8; *size as _],
						_ => vec![],
					};
					let call = Call::System(SystemCall::remark(remark));
					let call_weight = call.get_dispatch_info().weight;

					let millau_account_id: bp_millau::AccountId = Default::default();
					let (rialto_raw_public, rialto_raw_signature) = ed25519_sign(
						&call,
						&millau_account_id,
						VERSION.spec_version,
						bp_runtime::MILLAU_CHAIN_ID,
						bp_runtime::RIALTO_CHAIN_ID,
					);
					let rialto_public = MultiSigner::Ed25519(sp_core::ed25519::Public::from_raw(rialto_raw_public));
					let rialto_signature = MultiSignature::Ed25519(sp_core::ed25519::Signature::from_raw(
						rialto_raw_signature,
					));

					if params.dispatch_fee_payment == DispatchFeePayment::AtTargetChain {
						Self::endow_account(&rialto_public.clone().into_account());
					}

					let make_millau_message_key = |message_key: MessageKey| storage_keys::message_key::<
						<WithMillauMessageBridge as MessageBridge>::BridgedMessagesInstance,
					>(
						&message_key.lane_id, message_key.nonce,
					).0;
					let make_millau_outbound_lane_data_key = |lane_id| storage_keys::outbound_lane_data_key::<
						<WithMillauMessageBridge as MessageBridge>::BridgedMessagesInstance,
					>(
						&lane_id,
					).0;

					let make_millau_header = |state_root| bp_millau::Header::new(
						0,
						Default::default(),
						state_root,
						Default::default(),
						Default::default(),
					);

					let dispatch_fee_payment = params.dispatch_fee_payment.clone();
					prepare_message_proof::<WithMillauMessageBridge, bp_millau::Hasher, Runtime, (), _, _, _>(
						params,
						make_millau_message_key,
						make_millau_outbound_lane_data_key,
						make_millau_header,
						call_weight,
						bp_message_dispatch::MessagePayload {
							spec_version: VERSION.spec_version,
							weight: call_weight,
							origin: bp_message_dispatch::CallOrigin::<
								bp_millau::AccountId,
								MultiSigner,
								Signature,
							>::TargetAccount(
								millau_account_id,
								rialto_public,
								rialto_signature,
							),
							dispatch_fee_payment,
							call: call.encode(),
						}.encode(),
					)
				}

				fn prepare_message_delivery_proof(
					params: MessageDeliveryProofParams<Self::AccountId>,
				) -> millau_messages::ToMillauMessagesDeliveryProof {
					use crate::millau_messages::WithMillauMessageBridge;
					use bridge_runtime_common::{messages_benchmarking::prepare_message_delivery_proof};
					use sp_runtime::traits::Header;

					prepare_message_delivery_proof::<WithMillauMessageBridge, bp_millau::Hasher, Runtime, (), _, _>(
						params,
						|lane_id| pallet_bridge_messages::storage_keys::inbound_lane_data_key::<
							<WithMillauMessageBridge as MessageBridge>::BridgedMessagesInstance,
						>(
							&lane_id,
						).0,
						|state_root| bp_millau::Header::new(
							0,
							Default::default(),
							state_root,
							Default::default(),
							Default::default(),
						),
					)
				}

				fn is_message_dispatched(nonce: bp_messages::MessageNonce) -> bool {
					frame_system::Pallet::<Runtime>::events()
						.into_iter()
						.map(|event_record| event_record.event)
						.any(|event| matches!(
							event,
							Event::BridgeDispatch(pallet_bridge_dispatch::Event::<Runtime, _>::MessageDispatched(
								_, ([0, 0, 0, 0], nonce_from_event), _,
							)) if nonce_from_event == nonce
						))
				}
			}

			add_benchmark!(
				params,
				batches,
				pallet_bridge_currency_exchange,
				BridgeCurrencyExchangeBench::<Runtime, KovanCurrencyExchange>
			);
			add_benchmark!(
				params,
				batches,
				pallet_bridge_messages,
				MessagesBench::<Runtime, WithMillauMessagesInstance>
			);
			add_benchmark!(params, batches, pallet_bridge_grandpa, BridgeMillauGrandpa);

			if batches.is_empty() { return Err("Benchmark not found for this pallet.".into()) }
			let storage_info = AllPalletsWithSystem::storage_info();
			Ok((batches, storage_info))
		}
	}
}

/// Millau account ownership digest from Rialto.
///
/// The byte vector returned by this function should be signed with a Millau account private key.
/// This way, the owner of `rialto_account_id` on Rialto proves that the Millau account private key
/// is also under his control.
pub fn rialto_to_millau_account_ownership_digest<Call, AccountId, SpecVersion>(
	millau_call: &Call,
	rialto_account_id: AccountId,
	millau_spec_version: SpecVersion,
) -> sp_std::vec::Vec<u8>
where
	Call: codec::Encode,
	AccountId: codec::Encode,
	SpecVersion: codec::Encode,
{
	pallet_bridge_dispatch::account_ownership_digest(
		millau_call,
		rialto_account_id,
		millau_spec_version,
		bp_runtime::RIALTO_CHAIN_ID,
		bp_runtime::MILLAU_CHAIN_ID,
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use bp_currency_exchange::DepositInto;
	use bridge_runtime_common::messages;

	fn run_deposit_into_test(test: impl Fn(AccountId) -> Balance) {
		let mut ext: sp_io::TestExternalities = SystemConfig::default().build_storage::<Runtime>().unwrap().into();
		ext.execute_with(|| {
			// initially issuance is zero
			assert_eq!(
				<pallet_balances::Pallet<Runtime> as Currency<AccountId>>::total_issuance(),
				0,
			);

			// create account
			let account: AccountId = [1u8; 32].into();
			let initial_amount = ExistentialDeposit::get();
			let deposited =
				<pallet_balances::Pallet<Runtime> as Currency<AccountId>>::deposit_creating(&account, initial_amount);
			drop(deposited);
			assert_eq!(
				<pallet_balances::Pallet<Runtime> as Currency<AccountId>>::total_issuance(),
				initial_amount,
			);
			assert_eq!(
				<pallet_balances::Pallet<Runtime> as Currency<AccountId>>::free_balance(&account),
				initial_amount,
			);

			// run test
			let total_issuance_change = test(account);

			// check that total issuance has changed by `run_deposit_into_test`
			assert_eq!(
				<pallet_balances::Pallet<Runtime> as Currency<AccountId>>::total_issuance(),
				initial_amount + total_issuance_change,
			);
		});
	}

	#[test]
	fn ensure_rialto_message_lane_weights_are_correct() {
		type Weights = pallet_bridge_messages::weights::RialtoWeight<Runtime>;

		pallet_bridge_messages::ensure_weights_are_correct::<Weights>(
			bp_rialto::DEFAULT_MESSAGE_DELIVERY_TX_WEIGHT,
			bp_rialto::ADDITIONAL_MESSAGE_BYTE_DELIVERY_WEIGHT,
			bp_rialto::MAX_SINGLE_MESSAGE_DELIVERY_CONFIRMATION_TX_WEIGHT,
			bp_rialto::PAY_INBOUND_DISPATCH_FEE_WEIGHT,
		);

		let max_incoming_message_proof_size = bp_millau::EXTRA_STORAGE_PROOF_SIZE.saturating_add(
			messages::target::maximal_incoming_message_size(bp_rialto::max_extrinsic_size()),
		);
		pallet_bridge_messages::ensure_able_to_receive_message::<Weights>(
			bp_rialto::max_extrinsic_size(),
			bp_rialto::max_extrinsic_weight(),
			max_incoming_message_proof_size,
			messages::target::maximal_incoming_message_dispatch_weight(bp_rialto::max_extrinsic_weight()),
		);

		let max_incoming_inbound_lane_data_proof_size = bp_messages::InboundLaneData::<()>::encoded_size_hint(
			bp_rialto::MAXIMAL_ENCODED_ACCOUNT_ID_SIZE,
			bp_millau::MAX_UNREWARDED_RELAYER_ENTRIES_AT_INBOUND_LANE as _,
			bp_millau::MAX_UNCONFIRMED_MESSAGES_AT_INBOUND_LANE as _,
		)
		.unwrap_or(u32::MAX);
		pallet_bridge_messages::ensure_able_to_receive_confirmation::<Weights>(
			bp_rialto::max_extrinsic_size(),
			bp_rialto::max_extrinsic_weight(),
			max_incoming_inbound_lane_data_proof_size,
			bp_millau::MAX_UNREWARDED_RELAYER_ENTRIES_AT_INBOUND_LANE,
			bp_millau::MAX_UNCONFIRMED_MESSAGES_AT_INBOUND_LANE,
		);
	}

	#[test]
	fn deposit_into_existing_account_works() {
		run_deposit_into_test(|existing_account| {
			let initial_amount =
				<pallet_balances::Pallet<Runtime> as Currency<AccountId>>::free_balance(&existing_account);
			let additional_amount = 10_000;
			<Runtime as pallet_bridge_currency_exchange::Config<KovanCurrencyExchange>>::DepositInto::deposit_into(
				existing_account.clone(),
				additional_amount,
			)
			.unwrap();
			assert_eq!(
				<pallet_balances::Pallet<Runtime> as Currency<AccountId>>::free_balance(&existing_account),
				initial_amount + additional_amount,
			);
			additional_amount
		});
	}

	#[test]
	fn deposit_into_new_account_works() {
		run_deposit_into_test(|_| {
			let initial_amount = 0;
			let additional_amount = ExistentialDeposit::get() + 10_000;
			let new_account: AccountId = [42u8; 32].into();
			<Runtime as pallet_bridge_currency_exchange::Config<KovanCurrencyExchange>>::DepositInto::deposit_into(
				new_account.clone(),
				additional_amount,
			)
			.unwrap();
			assert_eq!(
				<pallet_balances::Pallet<Runtime> as Currency<AccountId>>::free_balance(&new_account),
				initial_amount + additional_amount,
			);
			additional_amount
		});
	}
}
