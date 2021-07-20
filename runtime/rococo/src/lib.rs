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

//! The Rococo runtime for v1 parachains.

#![cfg_attr(not(feature = "std"), no_std)]
// `construct_runtime!` does a lot of recursion and requires us to increase the limit to 256.
#![recursion_limit = "256"]

use pallet_transaction_payment::CurrencyAdapter;
use sp_std::prelude::*;
use sp_std::collections::btree_map::BTreeMap;
use parity_scale_codec::{Encode, Decode, MaxEncodedLen};
use primitives::v1::{
	AccountId, AccountIndex, Balance, BlockNumber, Hash, Nonce, Signature, Moment,
	GroupRotationInfo, CoreState, Id, ValidationCode, ValidationCodeHash, CandidateEvent,
	ValidatorId, ValidatorIndex, CommittedCandidateReceipt, OccupiedCoreAssumption,
	PersistedValidationData, InboundDownwardMessage, InboundHrmpMessage,
	SessionInfo as SessionInfoData,
};
use runtime_common::{
	SlowAdjustingFeeUpdate, impls::ToAuthor, BlockHashCount, BlockWeights, BlockLength, RocksDbWeight,
};
use runtime_parachains::{
	self,
	runtime_api_impl::v1 as runtime_api_impl,
};
use frame_support::{
	construct_runtime, parameter_types,
	traits::{Filter, KeyOwnerProofSystem, Randomness, All, IsInVec},
	weights::Weight,
	PalletId
};
use sp_runtime::{
	create_runtime_str, generic, impl_opaque_keys,
	ApplyExtrinsicResult, KeyTypeId, Perbill,
	transaction_validity::{TransactionValidity, TransactionSource, TransactionPriority},
	traits::{
		self, Keccak256, BlakeTwo256, Block as BlockT, OpaqueKeys, AccountIdLookup,
		Extrinsic as ExtrinsicT, SaturatedConversion, Verify,
	},
};
use pallet_im_online::sr25519::AuthorityId as ImOnlineId;
use authority_discovery_primitives::AuthorityId as AuthorityDiscoveryId;
#[cfg(any(feature = "std", test))]
use sp_version::NativeVersion;
use sp_version::RuntimeVersion;
use pallet_transaction_payment::{FeeDetails, RuntimeDispatchInfo};
use pallet_grandpa::{AuthorityId as GrandpaId, fg_primitives};
use sp_core::{OpaqueMetadata, RuntimeDebug};
use sp_staking::SessionIndex;
use pallet_session::historical as session_historical;
use beefy_primitives::crypto::AuthorityId as BeefyId;
use beefy_primitives::mmr::MmrLeafVersion;
use pallet_mmr_primitives as mmr;
use frame_system::EnsureRoot;
use runtime_common::{paras_sudo_wrapper, paras_registrar, xcm_sender, auctions, crowdloan, slots};

use runtime_parachains::origin as parachains_origin;
use runtime_parachains::configuration as parachains_configuration;
use runtime_parachains::shared as parachains_shared;
use runtime_parachains::inclusion as parachains_inclusion;
use runtime_parachains::paras_inherent as parachains_paras_inherent;
use runtime_parachains::initializer as parachains_initializer;
use runtime_parachains::session_info as parachains_session_info;
use runtime_parachains::paras as parachains_paras;
use runtime_parachains::dmp as parachains_dmp;
use runtime_parachains::ump as parachains_ump;
use runtime_parachains::hrmp as parachains_hrmp;
use runtime_parachains::scheduler as parachains_scheduler;

use bridge_runtime_common::messages::{MessageBridge, source::estimate_message_dispatch_and_delivery_fee};

pub use pallet_balances::Call as BalancesCall;

use polkadot_parachain::primitives::Id as ParaId;

use xcm::v0::{Xcm, MultiLocation, NetworkId, BodyId};
use xcm_executor::XcmExecutor;
use xcm_builder::{
	AccountId32Aliases, ChildParachainConvertsVia, SovereignSignedViaLocation,
	CurrencyAdapter as XcmCurrencyAdapter, ChildParachainAsNative, SignedAccountId32AsNative,
	ChildSystemParachainAsSuperuser, LocationInverter, IsConcrete, FixedWeightBounds,
	BackingToPlurality, SignedToAccountId32, UsingComponents,
};
use constants::{time::*, currency::*, fee::*};
use frame_support::traits::InstanceFilter;

/// Constant values used within the runtime.
pub mod constants;
mod bridge_messages;
mod validator_manager;

// Make the WASM binary available.
#[cfg(feature = "std")]
include!(concat!(env!("OUT_DIR"), "/wasm_binary.rs"));

/// Runtime version (Rococo).
pub const VERSION: RuntimeVersion = RuntimeVersion {
	spec_name: create_runtime_str!("rococo"),
	impl_name: create_runtime_str!("parity-rococo-v1.6"),
	authoring_version: 0,
	spec_version: 9004,
	impl_version: 0,
	#[cfg(not(feature = "disable-runtime-api"))]
	apis: RUNTIME_API_VERSIONS,
	#[cfg(feature = "disable-runtime-api")]
	apis: sp_version::create_apis_vec![[]],
	transaction_version: 0,
};

/// The BABE epoch configuration at genesis.
pub const BABE_GENESIS_EPOCH_CONFIG: babe_primitives::BabeEpochConfiguration =
	babe_primitives::BabeEpochConfiguration {
		c: PRIMARY_PROBABILITY,
		allowed_slots: babe_primitives::AllowedSlots::PrimaryAndSecondaryVRFSlots
	};

/// Native version.
#[cfg(any(feature = "std", test))]
pub fn native_version() -> NativeVersion {
	NativeVersion {
		runtime_version: VERSION,
		can_author_with: Default::default(),
	}
}

/// The address format for describing accounts.
pub type Address = sp_runtime::MultiAddress<AccountId, ()>;
/// Block header type as expected by this runtime.
pub type Header = generic::Header<BlockNumber, BlakeTwo256>;
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
	frame_system::CheckMortality<Runtime>,
	frame_system::CheckNonce<Runtime>,
	frame_system::CheckWeight<Runtime>,
	pallet_transaction_payment::ChargeTransactionPayment<Runtime>,
);

/// Unchecked extrinsic type as expected by this runtime.
pub type UncheckedExtrinsic = generic::UncheckedExtrinsic<Address, Call, Signature, SignedExtra>;
/// Executive: handles dispatch to the various modules.
pub type Executive = frame_executive::Executive<
	Runtime,
	Block,
	frame_system::ChainContext<Runtime>,
	Runtime,
	AllPallets,
	GrandpaStoragePrefixMigration,
>;
/// The payload being signed in transactions.
pub type SignedPayload = generic::SignedPayload<Call, SignedExtra>;

impl_opaque_keys! {
	pub struct SessionKeys {
		pub grandpa: Grandpa,
		pub babe: Babe,
		pub im_online: ImOnline,
		pub para_validator: Initializer,
		pub para_assignment: SessionInfo,
		pub authority_discovery: AuthorityDiscovery,
		pub beefy: Beefy,
	}
}

construct_runtime! {
	pub enum Runtime where
		Block = Block,
		NodeBlock = primitives::v1::Block,
		UncheckedExtrinsic = UncheckedExtrinsic
	{
		System: frame_system::{Pallet, Call, Storage, Config, Event<T>},

		// Must be before session.
		Babe: pallet_babe::{Pallet, Call, Storage, Config, ValidateUnsigned},

		Timestamp: pallet_timestamp::{Pallet, Call, Storage, Inherent},
		Indices: pallet_indices::{Pallet, Call, Storage, Config<T>, Event<T>},
		Balances: pallet_balances::{Pallet, Call, Storage, Config<T>, Event<T>},
		TransactionPayment: pallet_transaction_payment::{Pallet, Storage},

		// Consensus support.
		Authorship: pallet_authorship::{Pallet, Call, Storage},
		Offences: pallet_offences::{Pallet, Storage, Event},
		Historical: session_historical::{Pallet},
		Session: pallet_session::{Pallet, Call, Storage, Event, Config<T>},
		Grandpa: pallet_grandpa::{Pallet, Call, Storage, Config, Event, ValidateUnsigned},
		ImOnline: pallet_im_online::{Pallet, Call, Storage, Event<T>, ValidateUnsigned, Config<T>},
		AuthorityDiscovery: pallet_authority_discovery::{Pallet, Config},

		// Parachains modules.
		ParachainsOrigin: parachains_origin::{Pallet, Origin},
		ParachainsConfiguration: parachains_configuration::{Pallet, Call, Storage, Config<T>},
		Shared: parachains_shared::{Pallet, Call, Storage},
		ParaInclusion: parachains_inclusion::{Pallet, Call, Storage, Event<T>},
		ParasInherent: parachains_paras_inherent::{Pallet, Call, Storage, Inherent},
		Scheduler: parachains_scheduler::{Pallet, Call, Storage},
		Paras: parachains_paras::{Pallet, Call, Storage, Event, Config},
		Initializer: parachains_initializer::{Pallet, Call, Storage},
		Dmp: parachains_dmp::{Pallet, Call, Storage},
		Ump: parachains_ump::{Pallet, Call, Storage, Event},
		Hrmp: parachains_hrmp::{Pallet, Call, Storage, Event, Config},
		SessionInfo: parachains_session_info::{Pallet, Call, Storage},

		// Parachain Onboarding Pallets
		Registrar: paras_registrar::{Pallet, Call, Storage, Event<T>},
		Auctions: auctions::{Pallet, Call, Storage, Event<T>},
		Crowdloan: crowdloan::{Pallet, Call, Storage, Event<T>},
		Slots: slots::{Pallet, Call, Storage, Event<T>},
		ParasSudoWrapper: paras_sudo_wrapper::{Pallet, Call},

		// Sudo
		Sudo: pallet_sudo::{Pallet, Call, Storage, Event<T>, Config<T>},

		// Bridges support.
		Mmr: pallet_mmr::{Pallet, Storage},
		Beefy: pallet_beefy::{Pallet, Config<T>, Storage},
		MmrLeaf: pallet_beefy_mmr::{Pallet, Storage},

		// It might seem strange that we add both sides of the bridge to the same runtime. We do this because this
		// runtime as shared by both the Rococo and Wococo chains. When running as Rococo we only use
		// `BridgeWococoGrandpa`, and vice versa.
		BridgeRococoGrandpa: pallet_bridge_grandpa::{Pallet, Call, Storage, Config<T>} = 40,
		BridgeWococoGrandpa: pallet_bridge_grandpa::<Instance1>::{Pallet, Call, Storage, Config<T>} = 41,

		// Validator Manager pallet.
		ValidatorManager: validator_manager::{Pallet, Call, Storage, Event<T>},

		// Bridge messages support. The same story as with the bridge grandpa pallet above ^^^ - when we're
		// running as Rococo we only use `BridgeWococoMessages`/`BridgeWococoMessagesDispatch`, and vice versa.
		BridgeRococoMessages: pallet_bridge_messages::{Pallet, Call, Storage, Event<T>, Config<T>} = 43,
		BridgeWococoMessages: pallet_bridge_messages::<Instance1>::{Pallet, Call, Storage, Event<T>, Config<T>} = 44,
		BridgeRococoMessagesDispatch: pallet_bridge_dispatch::{Pallet, Event<T>} = 45,
		BridgeWococoMessagesDispatch: pallet_bridge_dispatch::<Instance1>::{Pallet, Event<T>} = 46,

		// A "council"
		Collective: pallet_collective::{Pallet, Call, Storage, Origin<T>, Event<T>, Config<T>} = 80,
		Membership: pallet_membership::{Pallet, Call, Storage, Event<T>, Config<T>} = 81,

		Utility: pallet_utility::{Pallet, Call, Event} = 90,
		Proxy: pallet_proxy::{Pallet, Call, Storage, Event<T>} = 91,

		// Pallet for sending XCM.
		XcmPallet: pallet_xcm::{Pallet, Call, Storage, Event<T>, Origin} = 99,
	}
}

pub struct GrandpaStoragePrefixMigration;
impl frame_support::traits::OnRuntimeUpgrade for GrandpaStoragePrefixMigration {
	fn on_runtime_upgrade() -> frame_support::weights::Weight {
		use frame_support::traits::PalletInfo;
		let name = <Runtime as frame_system::Config>::PalletInfo::name::<Grandpa>()
			.expect("grandpa is part of pallets in construct_runtime, so it has a name; qed");
		pallet_grandpa::migrations::v3_1::migrate::<Runtime, Grandpa, _>(name)
	}

	#[cfg(feature = "try-runtime")]
	fn pre_upgrade() -> Result<(), &'static str> {
		use frame_support::traits::PalletInfo;
		let name = <Runtime as frame_system::Config>::PalletInfo::name::<Grandpa>()
			.expect("grandpa is part of pallets in construct_runtime, so it has a name; qed");
		pallet_grandpa::migrations::v3_1::pre_migration::<Runtime, Grandpa, _>(name);
		Ok(())
	}

	#[cfg(feature = "try-runtime")]
	fn post_upgrade() -> Result<(), &'static str> {
		pallet_grandpa::migrations::v3_1::post_migration::<Grandpa>();
		Ok(())
	}
}

pub struct BaseFilter;
impl Filter<Call> for BaseFilter {
	fn filter(_call: &Call) -> bool {
		true
	}
}

parameter_types! {
	pub const Version: RuntimeVersion = VERSION;
	pub const SS58Prefix: u8 = 42;
}

impl frame_system::Config for Runtime {
	type BaseCallFilter = BaseFilter;
	type BlockWeights = BlockWeights;
	type BlockLength = BlockLength;
	type DbWeight = RocksDbWeight;
	type Origin = Origin;
	type Call = Call;
	type Index = Nonce;
	type BlockNumber = BlockNumber;
	type Hash = Hash;
	type Hashing = BlakeTwo256;
	type AccountId = AccountId;
	type Lookup = AccountIdLookup<AccountId, ()>;
	type Header = generic::Header<BlockNumber, BlakeTwo256>;
	type Event = Event;
	type BlockHashCount = BlockHashCount;
	type Version = Version;
	type PalletInfo = PalletInfo;
	type AccountData = pallet_balances::AccountData<Balance>;
	type OnNewAccount = ();
	type OnKilledAccount = ();
	type SystemWeightInfo = ();
	type SS58Prefix = SS58Prefix;
	type OnSetCode = ();
}

parameter_types! {
	pub const ValidationUpgradeFrequency: BlockNumber = 2 * DAYS;
	pub const ValidationUpgradeDelay: BlockNumber = 8 * HOURS;
	pub const SlashPeriod: BlockNumber = 7 * DAYS;
}

/// Submits a transaction with the node's public and signature type. Adheres to the signed extension
/// format of the chain.
impl<LocalCall> frame_system::offchain::CreateSignedTransaction<LocalCall> for Runtime where
	Call: From<LocalCall>,
{
	fn create_transaction<C: frame_system::offchain::AppCrypto<Self::Public, Self::Signature>>(
		call: Call,
		public: <Signature as Verify>::Signer,
		account: AccountId,
		nonce: <Runtime as frame_system::Config>::Index,
	) -> Option<(Call, <UncheckedExtrinsic as ExtrinsicT>::SignaturePayload)> {
		use sp_runtime::traits::StaticLookup;
		// take the biggest period possible.
		let period = BlockHashCount::get()
			.checked_next_power_of_two()
			.map(|c| c / 2)
			.unwrap_or(2) as u64;

		let current_block = System::block_number()
			.saturated_into::<u64>()
			// The `System::block_number` is initialized with `n+1`,
			// so the actual block number is `n`.
			.saturating_sub(1);
		let tip = 0;
		let extra: SignedExtra = (
			frame_system::CheckSpecVersion::<Runtime>::new(),
			frame_system::CheckTxVersion::<Runtime>::new(),
			frame_system::CheckGenesis::<Runtime>::new(),
			frame_system::CheckMortality::<Runtime>::from(generic::Era::mortal(period, current_block)),
			frame_system::CheckNonce::<Runtime>::from(nonce),
			frame_system::CheckWeight::<Runtime>::new(),
			pallet_transaction_payment::ChargeTransactionPayment::<Runtime>::from(tip),
		);
		let raw_payload = SignedPayload::new(call, extra).map_err(|e| {
			log::warn!("Unable to create signed payload: {:?}", e);
		}).ok()?;
		let signature = raw_payload.using_encoded(|payload| {
			C::sign(payload, public)
		})?;
		let (call, extra, _) = raw_payload.deconstruct();
		let address = <Runtime as frame_system::Config>::Lookup::unlookup(account);
		Some((call, (address, signature, extra)))
	}
}

impl frame_system::offchain::SigningTypes for Runtime {
	type Public = <Signature as Verify>::Signer;
	type Signature = Signature;
}

/// Special `FullIdentificationOf` implementation that is returning for every input `Some(Default::default())`.
pub struct FullIdentificationOf;
impl sp_runtime::traits::Convert<AccountId, Option<()>> for FullIdentificationOf {
	fn convert(_: AccountId) -> Option<()> { Some(Default::default()) }
}

impl pallet_session::historical::Config for Runtime {
	type FullIdentification = ();
	type FullIdentificationOf = FullIdentificationOf;
}

parameter_types! {
	pub SessionDuration: BlockNumber = EpochDurationInBlocks::get() as _;
}

parameter_types! {
	pub const ImOnlineUnsignedPriority: TransactionPriority = TransactionPriority::max_value();
}

impl pallet_im_online::Config for Runtime {
	type AuthorityId = ImOnlineId;
	type Event = Event;
	type ValidatorSet = Historical;
	type NextSessionRotation = Babe;
	type ReportUnresponsiveness = Offences;
	type UnsignedPriority = ImOnlineUnsignedPriority;
	type WeightInfo = ();
}

parameter_types! {
	pub const ExistentialDeposit: Balance = 1 * CENTS;
	pub const MaxLocks: u32 = 50;
	pub const MaxReserves: u32 = 50;
}

impl pallet_balances::Config for Runtime {
	type Balance = Balance;
	type DustRemoval = ();
	type Event = Event;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type MaxLocks = MaxLocks;
	type MaxReserves = MaxReserves;
	type ReserveIdentifier = [u8; 8];
	type WeightInfo = ();
}

impl<C> frame_system::offchain::SendTransactionTypes<C> for Runtime where
	Call: From<C>,
{
	type OverarchingCall = Call;
	type Extrinsic = UncheckedExtrinsic;
}

parameter_types! {
	pub const QueueSize: usize = 2;
	pub const MaxRetries: u32 = 3;
}

impl pallet_offences::Config for Runtime {
	type Event = Event;
	type IdentificationTuple = pallet_session::historical::IdentificationTuple<Self>;
	type OnOffenceHandler = ();
}

impl pallet_authority_discovery::Config for Runtime {}

parameter_types! {
	pub const MinimumPeriod: u64 = SLOT_DURATION / 2;
}
impl pallet_timestamp::Config for Runtime {
	type Moment = u64;
	type OnTimestampSet = Babe;
	type MinimumPeriod = MinimumPeriod;
	type WeightInfo = ();
}

parameter_types! {
	pub const TransactionByteFee: Balance = 10 * MILLICENTS;
}

impl pallet_transaction_payment::Config for Runtime {
	type OnChargeTransaction = CurrencyAdapter<Balances, ToAuthor<Runtime>>;
	type TransactionByteFee = TransactionByteFee;
	type WeightToFee = WeightToFee;
	type FeeMultiplierUpdate = SlowAdjustingFeeUpdate<Self>;
}

parameter_types! {
	pub const DisabledValidatorsThreshold: Perbill = Perbill::from_percent(17);
}

/// Special `ValidatorIdOf` implementation that is just returning the input as result.
pub struct ValidatorIdOf;
impl sp_runtime::traits::Convert<AccountId, Option<AccountId>> for ValidatorIdOf {
	fn convert(a: AccountId) -> Option<AccountId> { Some(a) }
}

impl pallet_session::Config for Runtime {
	type Event = Event;
	type ValidatorId = AccountId;
	type ValidatorIdOf = ValidatorIdOf;
	type ShouldEndSession = Babe;
	type NextSessionRotation = Babe;
	type SessionManager = pallet_session::historical::NoteHistoricalRoot<Self, ValidatorManager>;
	type SessionHandler = <SessionKeys as OpaqueKeys>::KeyTypeIdProviders;
	type Keys = SessionKeys;
	type DisabledValidatorsThreshold = DisabledValidatorsThreshold;
	type WeightInfo = ();
}

parameter_types! {
	pub const ExpectedBlockTime: Moment = MILLISECS_PER_BLOCK;
	pub ReportLongevity: u64 = EpochDurationInBlocks::get() as u64 * 10;
}

impl pallet_babe::Config for Runtime {
	type EpochDuration = EpochDurationInBlocks;
	type ExpectedBlockTime = ExpectedBlockTime;

	// session module is the trigger
	type EpochChangeTrigger = pallet_babe::ExternalTrigger;

	type KeyOwnerProofSystem = Historical;

	type KeyOwnerProof = <Self::KeyOwnerProofSystem as KeyOwnerProofSystem<(
		KeyTypeId,
		pallet_babe::AuthorityId,
	)>>::Proof;

	type KeyOwnerIdentification = <Self::KeyOwnerProofSystem as KeyOwnerProofSystem<(
		KeyTypeId,
		pallet_babe::AuthorityId,
	)>>::IdentificationTuple;

	type HandleEquivocation =
		pallet_babe::EquivocationHandler<Self::KeyOwnerIdentification, Offences, ReportLongevity>;

	type WeightInfo = ();
}

parameter_types! {
	pub const IndexDeposit: Balance = 1 * DOLLARS;
}

impl pallet_indices::Config for Runtime {
	type AccountIndex = AccountIndex;
	type Currency = Balances;
	type Deposit = IndexDeposit;
	type Event = Event;
	type WeightInfo = ();
}

parameter_types! {
	pub const AttestationPeriod: BlockNumber = 50;
}

impl pallet_grandpa::Config for Runtime {
	type Event = Event;
	type Call = Call;

	type KeyOwnerProofSystem = Historical;

	type KeyOwnerProof =
		<Self::KeyOwnerProofSystem as KeyOwnerProofSystem<(KeyTypeId, GrandpaId)>>::Proof;

	type KeyOwnerIdentification = <Self::KeyOwnerProofSystem as KeyOwnerProofSystem<(
		KeyTypeId,
		GrandpaId,
	)>>::IdentificationTuple;

	type HandleEquivocation =
		pallet_grandpa::EquivocationHandler<Self::KeyOwnerIdentification, Offences, ReportLongevity>;

	type WeightInfo = ();
}

parameter_types! {
	pub const UncleGenerations: u32 = 0;
}

impl pallet_authorship::Config for Runtime {
	type FindAuthor = pallet_session::FindAccountFromAuthorIndex<Self, Babe>;
	type UncleGenerations = UncleGenerations;
	type FilterUncle = ();
	type EventHandler = ImOnline;
}

impl parachains_origin::Config for Runtime {}

impl parachains_configuration::Config for Runtime {}

impl parachains_shared::Config for Runtime {}

/// Special `RewardValidators` that does nothing ;)
pub struct RewardValidators;
impl runtime_parachains::inclusion::RewardValidators for RewardValidators {
	fn reward_backing(_: impl IntoIterator<Item=ValidatorIndex>) {}
	fn reward_bitfields(_: impl IntoIterator<Item=ValidatorIndex>) {}
}

impl parachains_inclusion::Config for Runtime {
	type Event = Event;
	type DisputesHandler = ();
	type RewardValidators = RewardValidators;
}

impl parachains_paras::Config for Runtime {
	type Origin = Origin;
	type Event = Event;
}

parameter_types! {
	pub const RocLocation: MultiLocation = MultiLocation::Null;
	pub const RococoNetwork: NetworkId = NetworkId::Polkadot;
	pub const Ancestry: MultiLocation = MultiLocation::Null;
	pub CheckAccount: AccountId = XcmPallet::check_account();
}

pub type SovereignAccountOf = (
	ChildParachainConvertsVia<ParaId, AccountId>,
	AccountId32Aliases<RococoNetwork, AccountId>,
);

pub type LocalAssetTransactor =
	XcmCurrencyAdapter<
		// Use this currency:
		Balances,
		// Use this currency when it is a fungible asset matching the given location or name:
		IsConcrete<RocLocation>,
		// We can convert the MultiLocations with our converter above:
		SovereignAccountOf,
		// Our chain's account ID type (we can't get away without mentioning it explicitly):
		AccountId,
		// It's a native asset so we keep track of the teleports to maintain total issuance.
		CheckAccount,
	>;

type LocalOriginConverter = (
	SovereignSignedViaLocation<SovereignAccountOf, Origin>,
	ChildParachainAsNative<parachains_origin::Origin, Origin>,
	SignedAccountId32AsNative<RococoNetwork, Origin>,
	ChildSystemParachainAsSuperuser<ParaId, Origin>,
);

parameter_types! {
	pub const BaseXcmWeight: Weight = 100_000;
}

/// The XCM router. When we want to send an XCM message, we use this type. It amalgamates all of our
/// individual routers.
pub type XcmRouter = (
	// Only one router so far - use DMP to communicate with child parachains.
	xcm_sender::ChildParachainRouter<Runtime>,
);

use xcm::v0::{MultiAsset, MultiAsset::AllConcreteFungible, MultiLocation::{Null, X1}, Junction::Parachain};
parameter_types! {
	pub const RococoForTick: (MultiAsset, MultiLocation) =
		(AllConcreteFungible { id: Null }, X1(Parachain(100)));
	pub const RococoForTrick: (MultiAsset, MultiLocation) =
		(AllConcreteFungible { id: Null }, X1(Parachain(110)));
	pub const RococoForTrack: (MultiAsset, MultiLocation) =
		(AllConcreteFungible { id: Null }, X1(Parachain(120)));
	pub const RococoForStatemint: (MultiAsset, MultiLocation) =
		(AllConcreteFungible { id: Null }, X1(Parachain(1001)));
}
pub type TrustedTeleporters = (
	xcm_builder::Case<RococoForTick>,
	xcm_builder::Case<RococoForTrick>,
	xcm_builder::Case<RococoForTrack>,
	xcm_builder::Case<RococoForStatemint>,
);

parameter_types! {
	pub AllowUnpaidFrom: Vec<MultiLocation> =
		vec![
			X1(Parachain(100)),
			X1(Parachain(110)),
			X1(Parachain(120)),
			X1(Parachain(1001))
		];
}

use xcm_builder::{TakeWeightCredit, AllowTopLevelPaidExecutionFrom, AllowUnpaidExecutionFrom};
pub type Barrier = (
	TakeWeightCredit,
	AllowTopLevelPaidExecutionFrom<All<MultiLocation>>,
	AllowUnpaidExecutionFrom<IsInVec<AllowUnpaidFrom>>,	// <- Trusted parachains get free execution
);

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type Call = Call;
	type XcmSender = XcmRouter;
	type AssetTransactor = LocalAssetTransactor;
	type OriginConverter = LocalOriginConverter;
	type IsReserve = ();
	type IsTeleporter = TrustedTeleporters;
	type LocationInverter = LocationInverter<Ancestry>;
	type Barrier = Barrier;
	type Weigher = FixedWeightBounds<BaseXcmWeight, Call>;
	type Trader = UsingComponents<WeightToFee, RocLocation, AccountId, Balances, ToAuthor<Runtime>>;
	type ResponseHandler = ();
}

parameter_types! {
	pub const CollectiveBodyId: BodyId = BodyId::Unit;
}

/// Type to convert an `Origin` type value into a `MultiLocation` value which represents an interior location
/// of this chain.
pub type LocalOriginToLocation = (
	// We allow an origin from the Collective pallet to be used in XCM as a corresponding Plurality of the
	// `Unit` body.
	BackingToPlurality<Origin, pallet_collective::Origin<Runtime>, CollectiveBodyId>,
	// And a usual Signed origin to be used in XCM as a corresponding AccountId32
	SignedToAccountId32<Origin, AccountId, RococoNetwork>,
);

pub struct OnlyWithdrawTeleportForAccounts;
impl frame_support::traits::Contains<(MultiLocation, Xcm<Call>)> for OnlyWithdrawTeleportForAccounts {
	fn contains((ref origin, ref msg): &(MultiLocation, Xcm<Call>)) -> bool {
		use xcm::v0::{
			Xcm::WithdrawAsset, Order::{BuyExecution, InitiateTeleport, DepositAsset},
			MultiAsset::{All, ConcreteFungible}, Junction::{AccountId32, Plurality},
		};
		match origin {
			// Root and collective are allowed to execute anything.
			Null | X1(Plurality { .. }) => true,
			X1(AccountId32 { .. }) => {
				// An account ID trying to send a message. We ensure that it's sensible.
				// This checks that it's of the form:
				// WithdrawAsset {
				//   assets: [ ConcreteFungible { id: Null } ],
				//   effects: [ BuyExecution, InitiateTeleport {
				//     assets: All,
				//     dest: Parachain,
				//     effects: [ BuyExecution, DepositAssets {
				//       assets: All,
				//       dest: AccountId32,
				//     } ]
				//   } ]
				// }
				matches!(msg, WithdrawAsset { ref assets, ref effects }
					if assets.len() == 1
					&& matches!(assets[0], ConcreteFungible { id: Null, .. })
					&& effects.len() == 2
					&& matches!(effects[0], BuyExecution { .. })
					&& matches!(effects[1], InitiateTeleport { ref assets, dest: X1(Parachain(..)), ref effects }
						if assets.len() == 1
						&& matches!(assets[0], All)
						&& effects.len() == 2
						&& matches!(effects[0], BuyExecution { .. })
						&& matches!(effects[1], DepositAsset { ref assets, dest: X1(AccountId32{..}) }
							if assets.len() == 1
							&& matches!(assets[0], All)
						)
					)
				)
			}
			// Nobody else is allowed to execute anything.
			_ => false,
		}
	}
}

impl pallet_xcm::Config for Runtime {
	type Event = Event;
	type SendXcmOrigin = xcm_builder::EnsureXcmOrigin<Origin, LocalOriginToLocation>;
	type XcmRouter = XcmRouter;
	// Anyone can execute XCM messages locally...
	type ExecuteXcmOrigin = xcm_builder::EnsureXcmOrigin<Origin, LocalOriginToLocation>;
	// ...but they must match our filter, which requires them to be a simple withdraw + teleport.
	type XcmExecuteFilter = OnlyWithdrawTeleportForAccounts;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = All<(MultiLocation, Vec<MultiAsset>)>;
	type XcmReserveTransferFilter = All<(MultiLocation, Vec<MultiAsset>)>;
	type Weigher = FixedWeightBounds<BaseXcmWeight, Call>;
}

impl parachains_session_info::Config for Runtime {}

parameter_types! {
	pub const FirstMessageFactorPercent: u64 = 100;
}

impl parachains_ump::Config for Runtime {
	type Event = Event;
	type UmpSink = crate::parachains_ump::XcmSink<XcmExecutor<XcmConfig>, Runtime>;
	type FirstMessageFactorPercent = FirstMessageFactorPercent;
}

impl parachains_dmp::Config for Runtime {}

impl parachains_hrmp::Config for Runtime {
	type Event = Event;
	type Origin = Origin;
	type Currency = Balances;
}

impl parachains_paras_inherent::Config for Runtime {}

impl parachains_scheduler::Config for Runtime {}

impl parachains_initializer::Config for Runtime {
	type Randomness = pallet_babe::RandomnessFromOneEpochAgo<Runtime>;
	type ForceOrigin = EnsureRoot<AccountId>;
}

impl paras_sudo_wrapper::Config for Runtime {}

parameter_types! {
	pub const ParaDeposit: Balance = 5 * DOLLARS;
	pub const DataDepositPerByte: Balance = deposit(0, 1);
}

impl paras_registrar::Config for Runtime {
	type Event = Event;
	type Origin = Origin;
	type Currency = Balances;
	type OnSwap = (Crowdloan, Slots);
	type ParaDeposit = ParaDeposit;
	type DataDepositPerByte = DataDepositPerByte;
	type WeightInfo = paras_registrar::TestWeightInfo;
}

/// An insecure randomness beacon that uses the parent block hash as random material.
///
/// THIS SHOULD ONLY BE USED FOR TESTING PURPOSES.
pub struct ParentHashRandomness;

impl pallet_beefy::Config for Runtime {
	type BeefyId = BeefyId;
}

impl pallet_mmr::Config for Runtime {
	const INDEXING_PREFIX: &'static [u8] = b"mmr";
	type Hashing = Keccak256;
	type Hash = <Keccak256 as traits::Hash>::Output;
	type OnNewRoot = pallet_beefy_mmr::DepositBeefyDigest<Runtime>;
	type WeightInfo = ();
	type LeafData = pallet_beefy_mmr::Pallet<Runtime>;
}

pub struct ParasProvider;
impl pallet_beefy_mmr::ParachainHeadsProvider for ParasProvider {
	fn parachain_heads() -> Vec<(u32, Vec<u8>)> {
		Paras::parachains()
			.into_iter()
			.filter_map(|id| {
				Paras::para_head(&id).map(|head| (id.into(), head.0))
			})
			.collect()
	}
}

parameter_types! {
	/// Version of the produced MMR leaf.
	///
	/// The version consists of two parts;
	/// - `major` (3 bits)
	/// - `minor` (5 bits)
	///
	/// `major` should be updated only if decoding the previous MMR Leaf format from the payload
	/// is not possible (i.e. backward incompatible change).
	/// `minor` should be updated if fields are added to the previous MMR Leaf, which given SCALE
	/// encoding does not prevent old leafs from being decoded.
	///
	/// Hence we expect `major` to be changed really rarely (think never).
	/// See [`MmrLeafVersion`] type documentation for more details.
	pub LeafVersion: MmrLeafVersion = MmrLeafVersion::new(0, 0);
}

impl pallet_beefy_mmr::Config for Runtime {
	type LeafVersion = LeafVersion;
	type BeefyAuthorityToMerkleLeaf = pallet_beefy_mmr::BeefyEcdsaToEthereum;
	type ParachainHeads = ParasProvider;
}

parameter_types! {
	/// This is a pretty unscientific cap.
	///
	/// Note that once this is hit the pallet will essentially throttle incoming requests down to one
	/// call per block.
	pub const MaxRequests: u32 = 4 * HOURS as u32;

	/// Number of headers to keep.
	///
	/// Assuming the worst case of every header being finalized, we will keep headers at least for a
	/// week.
	pub const HeadersToKeep: u32 = 7 * DAYS as u32;
}

pub type RococoGrandpaInstance = ();
impl pallet_bridge_grandpa::Config for Runtime {
	type BridgedChain = bp_rococo::Rococo;
	type MaxRequests = MaxRequests;
	type HeadersToKeep = HeadersToKeep;

	type WeightInfo = pallet_bridge_grandpa::weights::RialtoWeight<Runtime>;
}

pub type WococoGrandpaInstance = pallet_bridge_grandpa::Instance1;
impl pallet_bridge_grandpa::Config<WococoGrandpaInstance> for Runtime {
	type BridgedChain = bp_wococo::Wococo;
	type MaxRequests = MaxRequests;
	type HeadersToKeep = HeadersToKeep;

	type WeightInfo = pallet_bridge_grandpa::weights::RialtoWeight<Runtime>;
}

// Instance that is "deployed" at Wococo chain. Responsible for dispatching Rococo -> Wococo messages.
pub type AtWococoFromRococoMessagesDispatch = pallet_bridge_dispatch::DefaultInstance;
impl pallet_bridge_dispatch::Config<AtWococoFromRococoMessagesDispatch> for Runtime {
	type Event = Event;
	type MessageId = (bp_messages::LaneId, bp_messages::MessageNonce);
	type Call = Call;
	type CallFilter = frame_support::traits::AllowAll;
	type EncodedCall = bridge_messages::FromRococoEncodedCall;
	type SourceChainAccountId = bp_wococo::AccountId;
	type TargetChainAccountPublic = sp_runtime::MultiSigner;
	type TargetChainSignature = sp_runtime::MultiSignature;
	type AccountIdConverter = bp_rococo::AccountIdConverter;
}

// Instance that is "deployed" at Rococo chain. Responsible for dispatching Wococo -> Rococo messages.
pub type AtRococoFromWococoMessagesDispatch = pallet_bridge_dispatch::Instance1;
impl pallet_bridge_dispatch::Config<AtRococoFromWococoMessagesDispatch> for Runtime {
	type Event = Event;
	type MessageId = (bp_messages::LaneId, bp_messages::MessageNonce);
	type Call = Call;
	type CallFilter = frame_support::traits::AllowAll;
	type EncodedCall = bridge_messages::FromWococoEncodedCall;
	type SourceChainAccountId = bp_rococo::AccountId;
	type TargetChainAccountPublic = sp_runtime::MultiSigner;
	type TargetChainSignature = sp_runtime::MultiSignature;
	type AccountIdConverter = bp_wococo::AccountIdConverter;
}

parameter_types! {
	pub const MaxMessagesToPruneAtOnce: bp_messages::MessageNonce = 8;
	pub const MaxUnrewardedRelayerEntriesAtInboundLane: bp_messages::MessageNonce =
		bp_rococo::MAX_UNREWARDED_RELAYER_ENTRIES_AT_INBOUND_LANE;
	pub const MaxUnconfirmedMessagesAtInboundLane: bp_messages::MessageNonce =
		bp_rococo::MAX_UNCONFIRMED_MESSAGES_AT_INBOUND_LANE;
	pub const RootAccountForPayments: Option<AccountId> = None;
}

// Instance that is "deployed" at Wococo chain. Responsible for sending Wococo -> Rococo messages
// and receiving Rococo -> Wococo messages.
pub type AtWococoWithRococoMessagesInstance = pallet_bridge_messages::DefaultInstance;
impl pallet_bridge_messages::Config<AtWococoWithRococoMessagesInstance> for Runtime {
	type Event = Event;
	type WeightInfo = pallet_bridge_messages::weights::RialtoWeight<Runtime>;
	type Parameter = ();
	type MaxMessagesToPruneAtOnce = MaxMessagesToPruneAtOnce;
	type MaxUnrewardedRelayerEntriesAtInboundLane = MaxUnrewardedRelayerEntriesAtInboundLane;
	type MaxUnconfirmedMessagesAtInboundLane = MaxUnconfirmedMessagesAtInboundLane;

	type OutboundPayload = crate::bridge_messages::ToRococoMessagePayload;
	type OutboundMessageFee = bp_wococo::Balance;

	type InboundPayload = crate::bridge_messages::FromRococoMessagePayload;
	type InboundMessageFee = bp_rococo::Balance;
	type InboundRelayer = bp_rococo::AccountId;

	type AccountIdConverter = bp_wococo::AccountIdConverter;

	type TargetHeaderChain = crate::bridge_messages::RococoAtWococo;
	type LaneMessageVerifier = crate::bridge_messages::ToRococoMessageVerifier;
	type MessageDeliveryAndDispatchPayment = pallet_bridge_messages::instant_payments::InstantCurrencyPayments<
		Runtime,
		pallet_balances::Pallet<Runtime>,
		crate::bridge_messages::GetDeliveryConfirmationTransactionFee,
		RootAccountForPayments,
	>;
	type OnDeliveryConfirmed = ();

	type SourceHeaderChain = crate::bridge_messages::RococoAtWococo;
	type MessageDispatch = crate::bridge_messages::FromRococoMessageDispatch;
}

// Instance that is "deployed" at Rococo chain. Responsible for sending Rococo -> Wococo messages
// and receiving Wococo -> Rococo messages.
pub type AtRococoWithWococoMessagesInstance = pallet_bridge_messages::Instance1;
impl pallet_bridge_messages::Config<AtRococoWithWococoMessagesInstance> for Runtime {
	type Event = Event;
	type WeightInfo = pallet_bridge_messages::weights::RialtoWeight<Runtime>;
	type Parameter = ();
	type MaxMessagesToPruneAtOnce = MaxMessagesToPruneAtOnce;
	type MaxUnrewardedRelayerEntriesAtInboundLane = MaxUnrewardedRelayerEntriesAtInboundLane;
	type MaxUnconfirmedMessagesAtInboundLane = MaxUnconfirmedMessagesAtInboundLane;

	type OutboundPayload = crate::bridge_messages::ToWococoMessagePayload;
	type OutboundMessageFee = bp_rococo::Balance;

	type InboundPayload = crate::bridge_messages::FromWococoMessagePayload;
	type InboundMessageFee = bp_wococo::Balance;
	type InboundRelayer = bp_wococo::AccountId;

	type AccountIdConverter = bp_rococo::AccountIdConverter;

	type TargetHeaderChain = crate::bridge_messages::WococoAtRococo;
	type LaneMessageVerifier = crate::bridge_messages::ToWococoMessageVerifier;
	type MessageDeliveryAndDispatchPayment = pallet_bridge_messages::instant_payments::InstantCurrencyPayments<
		Runtime,
		pallet_balances::Pallet<Runtime>,
		crate::bridge_messages::GetDeliveryConfirmationTransactionFee,
		RootAccountForPayments,
	>;
	type OnDeliveryConfirmed = ();

	type SourceHeaderChain = crate::bridge_messages::WococoAtRococo;
	type MessageDispatch = crate::bridge_messages::FromWococoMessageDispatch;
}

impl Randomness<Hash, BlockNumber> for ParentHashRandomness {
	fn random(subject: &[u8]) -> (Hash, BlockNumber) {
		(
			(System::parent_hash(), subject).using_encoded(sp_io::hashing::blake2_256).into(),
			System::block_number(),
		)
	}
}

parameter_types! {
	pub const EndingPeriod: BlockNumber = 1 * HOURS;
	pub const SampleLength: BlockNumber = 1;
}

impl auctions::Config for Runtime {
	type Event = Event;
	type Leaser = Slots;
	type Registrar = Registrar;
	type EndingPeriod = EndingPeriod;
	type SampleLength = SampleLength;
	type Randomness = ParentHashRandomness;
	type InitiateOrigin = EnsureRoot<AccountId>;
	type WeightInfo = auctions::TestWeightInfo;
}

parameter_types! {
	pub const LeasePeriod: BlockNumber = 1 * DAYS;
}

impl slots::Config for Runtime {
	type Event = Event;
	type Currency = Balances;
	type Registrar = Registrar;
	type LeasePeriod = LeasePeriod;
	type WeightInfo = slots::TestWeightInfo;
}

parameter_types! {
	pub const CrowdloanId: PalletId = PalletId(*b"py/cfund");
	pub const SubmissionDeposit: Balance = 100 * DOLLARS;
	pub const MinContribution: Balance = 1 * DOLLARS;
	pub const RemoveKeysLimit: u32 = 500;
	// Allow 32 bytes for an additional memo to a crowdloan.
	pub const MaxMemoLength: u8 = 32;
}

impl crowdloan::Config for Runtime {
	type Event = Event;
	type PalletId = CrowdloanId;
	type SubmissionDeposit = SubmissionDeposit;
	type MinContribution = MinContribution;
	type RemoveKeysLimit = RemoveKeysLimit;
	type Registrar = Registrar;
	type Auctioneer = Auctions;
	type MaxMemoLength = MaxMemoLength;
	type WeightInfo = crowdloan::TestWeightInfo;
}

impl pallet_sudo::Config for Runtime {
	type Event = Event;
	type Call = Call;
}

impl validator_manager::Config for Runtime {
	type Event = Event;
	type PrivilegedOrigin = EnsureRoot<AccountId>;
}

impl pallet_utility::Config for Runtime {
	type Event = Event;
	type Call = Call;
	type WeightInfo = ();
}

parameter_types! {
	// One storage item; key size 32, value size 8; .
	pub const ProxyDepositBase: Balance = 10;
	// Additional storage item size of 33 bytes.
	pub const ProxyDepositFactor: Balance = 10;
	pub const MaxProxies: u16 = 32;
	pub const AnnouncementDepositBase: Balance = 10;
	pub const AnnouncementDepositFactor: Balance = 10;
	pub const MaxPending: u16 = 32;
}

/// The type used to represent the kinds of proxying allowed.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, RuntimeDebug, MaxEncodedLen)]
pub enum ProxyType {
	Any,
	CancelProxy,
}
impl Default for ProxyType { fn default() -> Self { Self::Any } }
impl InstanceFilter<Call> for ProxyType {
	fn filter(&self, c: &Call) -> bool {
		match self {
			ProxyType::Any => true,
			ProxyType::CancelProxy => matches!(c,
				Call::Proxy(pallet_proxy::Call::reject_announcement(..))
			)
		}
	}
	fn is_superset(&self, o: &Self) -> bool {
		match (self, o) {
			(ProxyType::Any, _) => true,
			_ => false,
		}
	}
}

impl pallet_proxy::Config for Runtime {
	type Event = Event;
	type Call = Call;
	type Currency = Balances;
	type ProxyType = ProxyType;
	type ProxyDepositBase = ProxyDepositBase;
	type ProxyDepositFactor = ProxyDepositFactor;
	type MaxProxies = MaxProxies;
	type WeightInfo = ();
	type MaxPending = MaxPending;
	type CallHasher = BlakeTwo256;
	type AnnouncementDepositBase = AnnouncementDepositBase;
	type AnnouncementDepositFactor = AnnouncementDepositFactor;
}

parameter_types! {
	pub const MotionDuration: BlockNumber = 5;
	pub const MaxProposals: u32 = 100;
	pub const MaxMembers: u32 = 100;
}

impl pallet_collective::Config for Runtime {
	type Origin = Origin;
	type Proposal = Call;
	type Event = Event;
	type MotionDuration = MotionDuration;
	type MaxProposals = MaxProposals;
	type DefaultVote = pallet_collective::PrimeDefaultVote;
	type MaxMembers = MaxMembers;
	type WeightInfo = ();
}

impl pallet_membership::Config for Runtime {
	type Event = Event;
	type AddOrigin = EnsureRoot<AccountId>;
	type RemoveOrigin = EnsureRoot<AccountId>;
	type SwapOrigin = EnsureRoot<AccountId>;
	type ResetOrigin = EnsureRoot<AccountId>;
	type PrimeOrigin = EnsureRoot<AccountId>;
	type MembershipInitialized = Collective;
	type MembershipChanged = Collective;
	type MaxMembers = MaxMembers;
	type WeightInfo = ();
}

#[cfg(not(feature = "disable-runtime-api"))]
sp_api::impl_runtime_apis! {
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

	impl block_builder_api::BlockBuilder<Block> for Runtime {
		fn apply_extrinsic(extrinsic: <Block as BlockT>::Extrinsic) -> ApplyExtrinsicResult {
			Executive::apply_extrinsic(extrinsic)
		}

		fn finalize_block() -> <Block as BlockT>::Header {
			Executive::finalize_block()
		}

		fn inherent_extrinsics(data: inherents::InherentData) -> Vec<<Block as BlockT>::Extrinsic> {
			data.create_extrinsics()
		}

		fn check_inherents(
			block: Block,
			data: inherents::InherentData,
		) -> inherents::CheckInherentsResult {
			data.check_extrinsics(&block)
		}
	}

	impl tx_pool_api::runtime_api::TaggedTransactionQueue<Block> for Runtime {
		fn validate_transaction(
			source: TransactionSource,
			tx: <Block as BlockT>::Extrinsic,
			block_hash: <Block as BlockT>::Hash,
		) -> TransactionValidity {
			Executive::validate_transaction(source, tx, block_hash)
		}
	}

	impl offchain_primitives::OffchainWorkerApi<Block> for Runtime {
		fn offchain_worker(header: &<Block as BlockT>::Header) {
			Executive::offchain_worker(header)
		}
	}

	impl primitives::v1::ParachainHost<Block, Hash, BlockNumber> for Runtime {
		fn validators() -> Vec<ValidatorId> {
			runtime_api_impl::validators::<Runtime>()
		}

		fn validator_groups() -> (Vec<Vec<ValidatorIndex>>, GroupRotationInfo<BlockNumber>) {
			runtime_api_impl::validator_groups::<Runtime>()
		}

		fn availability_cores() -> Vec<CoreState<Hash, BlockNumber>> {
			runtime_api_impl::availability_cores::<Runtime>()
		}

		fn persisted_validation_data(para_id: Id, assumption: OccupiedCoreAssumption)
			-> Option<PersistedValidationData<Hash, BlockNumber>> {
			runtime_api_impl::persisted_validation_data::<Runtime>(para_id, assumption)
		}

		fn check_validation_outputs(
			para_id: Id,
			outputs: primitives::v1::CandidateCommitments,
		) -> bool {
			runtime_api_impl::check_validation_outputs::<Runtime>(para_id, outputs)
		}

		fn session_index_for_child() -> SessionIndex {
			runtime_api_impl::session_index_for_child::<Runtime>()
		}

		fn validation_code(para_id: Id, assumption: OccupiedCoreAssumption)
			-> Option<ValidationCode> {
			runtime_api_impl::validation_code::<Runtime>(para_id, assumption)
		}

		fn candidate_pending_availability(para_id: Id) -> Option<CommittedCandidateReceipt<Hash>> {
			runtime_api_impl::candidate_pending_availability::<Runtime>(para_id)
		}

		fn candidate_events() -> Vec<CandidateEvent<Hash>> {
			runtime_api_impl::candidate_events::<Runtime, _>(|ev| {
				match ev {
					Event::ParaInclusion(ev) => {
						Some(ev)
					}
					_ => None,
				}
			})
		}

		fn session_info(index: SessionIndex) -> Option<SessionInfoData> {
			runtime_api_impl::session_info::<Runtime>(index)
		}

		fn dmq_contents(recipient: Id) -> Vec<InboundDownwardMessage<BlockNumber>> {
			runtime_api_impl::dmq_contents::<Runtime>(recipient)
		}

		fn inbound_hrmp_channels_contents(
			recipient: Id
		) -> BTreeMap<Id, Vec<InboundHrmpMessage<BlockNumber>>> {
			runtime_api_impl::inbound_hrmp_channels_contents::<Runtime>(recipient)
		}

		fn validation_code_by_hash(hash: ValidationCodeHash) -> Option<ValidationCode> {
			runtime_api_impl::validation_code_by_hash::<Runtime>(hash)
		}
	}

	impl fg_primitives::GrandpaApi<Block> for Runtime {
		fn grandpa_authorities() -> Vec<(GrandpaId, u64)> {
			Grandpa::grandpa_authorities()
		}

		fn submit_report_equivocation_unsigned_extrinsic(
			equivocation_proof: fg_primitives::EquivocationProof<
				<Block as BlockT>::Hash,
				sp_runtime::traits::NumberFor<Block>,
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
			authority_id: fg_primitives::AuthorityId,
		) -> Option<fg_primitives::OpaqueKeyOwnershipProof> {
			use parity_scale_codec::Encode;

			Historical::prove((fg_primitives::KEY_TYPE, authority_id))
				.map(|p| p.encode())
				.map(fg_primitives::OpaqueKeyOwnershipProof::new)
		}
	}

	impl babe_primitives::BabeApi<Block> for Runtime {
		fn configuration() -> babe_primitives::BabeGenesisConfiguration {
			// The choice of `c` parameter (where `1 - c` represents the
			// probability of a slot being empty), is done in accordance to the
			// slot duration and expected target block time, for safely
			// resisting network delays of maximum two seconds.
			// <https://research.web3.foundation/en/latest/polkadot/BABE/Babe/#6-practical-results>
			babe_primitives::BabeGenesisConfiguration {
				slot_duration: Babe::slot_duration(),
				epoch_length: EpochDurationInBlocks::get().into(),
				c: BABE_GENESIS_EPOCH_CONFIG.c,
				genesis_authorities: Babe::authorities(),
				randomness: Babe::randomness(),
				allowed_slots: BABE_GENESIS_EPOCH_CONFIG.allowed_slots,
			}
		}

		fn current_epoch_start() -> babe_primitives::Slot {
			Babe::current_epoch_start()
		}

		fn current_epoch() -> babe_primitives::Epoch {
			Babe::current_epoch()
		}

		fn next_epoch() -> babe_primitives::Epoch {
			Babe::next_epoch()
		}

		fn generate_key_ownership_proof(
			_slot: babe_primitives::Slot,
			authority_id: babe_primitives::AuthorityId,
		) -> Option<babe_primitives::OpaqueKeyOwnershipProof> {
			use parity_scale_codec::Encode;

			Historical::prove((babe_primitives::KEY_TYPE, authority_id))
				.map(|p| p.encode())
				.map(babe_primitives::OpaqueKeyOwnershipProof::new)
		}

		fn submit_report_equivocation_unsigned_extrinsic(
			equivocation_proof: babe_primitives::EquivocationProof<<Block as BlockT>::Header>,
			key_owner_proof: babe_primitives::OpaqueKeyOwnershipProof,
		) -> Option<()> {
			let key_owner_proof = key_owner_proof.decode()?;

			Babe::submit_unsigned_equivocation_report(
				equivocation_proof,
				key_owner_proof,
			)
		}
	}

	impl authority_discovery_primitives::AuthorityDiscoveryApi<Block> for Runtime {
		fn authorities() -> Vec<AuthorityDiscoveryId> {
			runtime_api_impl::relevant_authority_ids::<Runtime>()
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

	impl beefy_primitives::BeefyApi<Block> for Runtime {
		fn validator_set() -> beefy_primitives::ValidatorSet<BeefyId> {
			Beefy::validator_set()
		}
	}

	impl pallet_mmr_primitives::MmrApi<Block, Hash> for Runtime {
		fn generate_proof(leaf_index: u64)
			-> Result<(mmr::EncodableOpaqueLeaf, mmr::Proof<Hash>), mmr::Error>
		{
			Mmr::generate_proof(leaf_index)
				.map(|(leaf, proof)| (mmr::EncodableOpaqueLeaf::from_leaf(&leaf), proof))
		}

		fn verify_proof(leaf: mmr::EncodableOpaqueLeaf, proof: mmr::Proof<Hash>)
			-> Result<(), mmr::Error>
		{
			pub type Leaf = <
				<Runtime as pallet_mmr::Config>::LeafData as mmr::LeafDataProvider
			>::LeafData;

			let leaf: Leaf = leaf
				.into_opaque_leaf()
				.try_decode()
				.ok_or(mmr::Error::Verify)?;
			Mmr::verify_leaf(leaf, proof)
		}

		fn verify_proof_stateless(
			root: Hash,
			leaf: mmr::EncodableOpaqueLeaf,
			proof: mmr::Proof<Hash>
		) -> Result<(), mmr::Error> {
			type MmrHashing = <Runtime as pallet_mmr::Config>::Hashing;
			let node = mmr::DataOrHash::Data(leaf.into_opaque_leaf());
			pallet_mmr::verify_leaf_proof::<MmrHashing, _>(root, node, proof)
		}
	}

	impl bp_rococo::RococoFinalityApi<Block> for Runtime {
		fn best_finalized() -> (bp_rococo::BlockNumber, bp_rococo::Hash) {
			let header = BridgeRococoGrandpa::best_finalized();
			(header.number, header.hash())
		}

		fn is_known_header(hash: bp_rococo::Hash) -> bool {
			BridgeRococoGrandpa::is_known_header(hash)
		}
	}

	impl bp_wococo::WococoFinalityApi<Block> for Runtime {
		fn best_finalized() -> (bp_wococo::BlockNumber, bp_wococo::Hash) {
			let header = BridgeWococoGrandpa::best_finalized();
			(header.number, header.hash())
		}

		fn is_known_header(hash: bp_wococo::Hash) -> bool {
			BridgeWococoGrandpa::is_known_header(hash)
		}
	}

	impl bp_rococo::ToRococoOutboundLaneApi<Block, Balance, bridge_messages::ToRococoMessagePayload> for Runtime {
		fn estimate_message_delivery_and_dispatch_fee(
			_lane_id: bp_messages::LaneId,
			payload: bridge_messages::ToWococoMessagePayload,
		) -> Option<Balance> {
			estimate_message_dispatch_and_delivery_fee::<bridge_messages::AtWococoWithRococoMessageBridge>(
				&payload,
				bridge_messages::AtWococoWithRococoMessageBridge::RELAYER_FEE_PERCENT,
			).ok()
		}

		fn message_details(
			lane: bp_messages::LaneId,
			begin: bp_messages::MessageNonce,
			end: bp_messages::MessageNonce,
		) -> Vec<bp_messages::MessageDetails<Balance>> {
			(begin..=end).filter_map(|nonce| {
				let message_data = BridgeRococoMessages::outbound_message_data(lane, nonce)?;
				let decoded_payload = bridge_messages::ToRococoMessagePayload::decode(
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
			BridgeRococoMessages::outbound_latest_received_nonce(lane)
		}

		fn latest_generated_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeRococoMessages::outbound_latest_generated_nonce(lane)
		}
	}

	impl bp_rococo::FromRococoInboundLaneApi<Block> for Runtime {
		fn latest_received_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeRococoMessages::inbound_latest_received_nonce(lane)
		}

		fn latest_confirmed_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeRococoMessages::inbound_latest_confirmed_nonce(lane)
		}

		fn unrewarded_relayers_state(lane: bp_messages::LaneId) -> bp_messages::UnrewardedRelayersState {
			BridgeRococoMessages::inbound_unrewarded_relayers_state(lane)
		}
	}

	impl bp_wococo::ToWococoOutboundLaneApi<Block, Balance, bridge_messages::ToWococoMessagePayload> for Runtime {
		fn estimate_message_delivery_and_dispatch_fee(
			_lane_id: bp_messages::LaneId,
			payload: bridge_messages::ToWococoMessagePayload,
		) -> Option<Balance> {
			estimate_message_dispatch_and_delivery_fee::<bridge_messages::AtRococoWithWococoMessageBridge>(
				&payload,
				bridge_messages::AtRococoWithWococoMessageBridge::RELAYER_FEE_PERCENT,
			).ok()
		}

		fn message_details(
			lane: bp_messages::LaneId,
			begin: bp_messages::MessageNonce,
			end: bp_messages::MessageNonce,
		) -> Vec<bp_messages::MessageDetails<Balance>> {
			(begin..=end).filter_map(|nonce| {
				let message_data = BridgeWococoMessages::outbound_message_data(lane, nonce)?;
				let decoded_payload = bridge_messages::ToWococoMessagePayload::decode(
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
			BridgeWococoMessages::outbound_latest_received_nonce(lane)
		}

		fn latest_generated_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeWococoMessages::outbound_latest_generated_nonce(lane)
		}
	}

	impl bp_wococo::FromWococoInboundLaneApi<Block> for Runtime {
		fn latest_received_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeWococoMessages::inbound_latest_received_nonce(lane)
		}

		fn latest_confirmed_nonce(lane: bp_messages::LaneId) -> bp_messages::MessageNonce {
			BridgeWococoMessages::inbound_latest_confirmed_nonce(lane)
		}

		fn unrewarded_relayers_state(lane: bp_messages::LaneId) -> bp_messages::UnrewardedRelayersState {
			BridgeWococoMessages::inbound_unrewarded_relayers_state(lane)
		}
	}

	impl frame_system_rpc_runtime_api::AccountNonceApi<Block, AccountId, Nonce> for Runtime {
		fn account_nonce(account: AccountId) -> Nonce {
			System::account_nonce(account)
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
}
