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
use parity_scale_codec::Encode;
use primitives::v1::{
	AccountId, AccountIndex, Balance, BlockNumber, Hash, Nonce, Signature, Moment,
	GroupRotationInfo, CoreState, Id, ValidationCode, CandidateEvent,
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
	traits::{EnsureOrigin, Filter, KeyOwnerProofSystem, Randomness, BasicAccount},
	weights::Weight,
};
use sp_runtime::{
	create_runtime_str, generic, impl_opaque_keys,
	ApplyExtrinsicResult, KeyTypeId, Perbill,
	transaction_validity::{TransactionValidity, TransactionSource, TransactionPriority},
	traits::{
		BlakeTwo256, Block as BlockT, OpaqueKeys, AccountIdLookup,
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
use sp_core::OpaqueMetadata;
use sp_staking::SessionIndex;
use pallet_session::historical as session_historical;
use frame_system::{EnsureRoot, EnsureOneOf, EnsureSigned};
use runtime_common::{paras_sudo_wrapper, paras_registrar, xcm_sender};

use runtime_parachains::origin as parachains_origin;
use runtime_parachains::configuration as parachains_configuration;
use runtime_parachains::shared as parachains_shared;
use runtime_parachains::inclusion as parachains_inclusion;
use runtime_parachains::inclusion_inherent as parachains_inclusion_inherent;
use runtime_parachains::initializer as parachains_initializer;
use runtime_parachains::session_info as parachains_session_info;
use runtime_parachains::paras as parachains_paras;
use runtime_parachains::dmp as parachains_dmp;
use runtime_parachains::ump as parachains_ump;
use runtime_parachains::hrmp as parachains_hrmp;
use runtime_parachains::scheduler as parachains_scheduler;

pub use pallet_balances::Call as BalancesCall;

use polkadot_parachain::primitives::Id as ParaId;
use xcm::v0::{MultiLocation, NetworkId};
use xcm_executor::traits::IsConcrete;
use xcm_builder::{
	AccountId32Aliases, ChildParachainConvertsVia, SovereignSignedViaLocation,
	CurrencyAdapter as XcmCurrencyAdapter, ChildParachainAsNative,
	SignedAccountId32AsNative, ChildSystemParachainAsSuperuser, LocationInverter,
};
use constants::{time::*, currency::*, fee::*};

/// Constant values used within the runtime.
pub mod constants;
mod propose_parachain;

// Make the WASM binary available.
#[cfg(feature = "std")]
include!(concat!(env!("OUT_DIR"), "/wasm_binary.rs"));

/// Runtime version (Rococo).
pub const VERSION: RuntimeVersion = RuntimeVersion {
	spec_name: create_runtime_str!("rococo"),
	impl_name: create_runtime_str!("parity-rococo-v1-1"),
	authoring_version: 0,
	spec_version: 210,
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
/// BlockId type as expected by this runtime.
pub type BlockId = generic::BlockId<Block>;
/// The SignedExtension to the basic transaction logic.
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
/// Extrinsic type that has already been checked.
pub type CheckedExtrinsic = generic::CheckedExtrinsic<AccountId, Nonce, Call>;
/// Executive: handles dispatch to the various modules.
pub type Executive = frame_executive::Executive<Runtime, Block, frame_system::ChainContext<Runtime>, Runtime, AllModules>;
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
	}
}

construct_runtime! {
	pub enum Runtime where
		Block = Block,
		NodeBlock = primitives::v1::Block,
		UncheckedExtrinsic = UncheckedExtrinsic
	{
		System: frame_system::{Module, Call, Storage, Config, Event<T>},
		Accounts: pallet_accounts::{Module, Call, Storage, Event<T>},

		// Must be before session.
		Babe: pallet_babe::{Module, Call, Storage, Config, ValidateUnsigned},

		Timestamp: pallet_timestamp::{Module, Call, Storage, Inherent},
		Indices: pallet_indices::{Module, Call, Storage, Config<T>, Event<T>},
		Balances: pallet_balances::{Module, Call, Storage, Config<T>, Event<T>},
		TransactionPayment: pallet_transaction_payment::{Module, Storage},

		// Consensus support.
		Authorship: pallet_authorship::{Module, Call, Storage},
		Offences: pallet_offences::{Module, Call, Storage, Event},
		Historical: session_historical::{Module},
		Session: pallet_session::{Module, Call, Storage, Event, Config<T>},
		Grandpa: pallet_grandpa::{Module, Call, Storage, Config, Event, ValidateUnsigned},
		ImOnline: pallet_im_online::{Module, Call, Storage, Event<T>, ValidateUnsigned, Config<T>},
		AuthorityDiscovery: pallet_authority_discovery::{Module, Call, Config},

		// Parachains modules.
		ParachainsOrigin: parachains_origin::{Module, Origin},
		ParachainsConfiguration: parachains_configuration::{Module, Call, Storage, Config<T>},
		Shared: parachains_shared::{Module, Call, Storage},
		Inclusion: parachains_inclusion::{Module, Call, Storage, Event<T>},
		InclusionInherent: parachains_inclusion_inherent::{Module, Call, Storage, Inherent},
		Scheduler: parachains_scheduler::{Module, Call, Storage},
		Paras: parachains_paras::{Module, Call, Storage},
		Initializer: parachains_initializer::{Module, Call, Storage},
		Dmp: parachains_dmp::{Module, Call, Storage},
		Ump: parachains_ump::{Module, Call, Storage},
		Hrmp: parachains_hrmp::{Module, Call, Storage, Event},
		SessionInfo: parachains_session_info::{Module, Call, Storage},

		Registrar: paras_registrar::{Module, Call, Storage},
		ParasSudoWrapper: paras_sudo_wrapper::{Module, Call},

		// Sudo
		Sudo: pallet_sudo::{Module, Call, Storage, Event<T>, Config<T>},

		// Propose parachain pallet.
		ProposeParachain: propose_parachain::{Module, Call, Storage, Event<T>},
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
	type AccountStorage = Accounts;
	type SystemWeightInfo = ();
	type SS58Prefix = SS58Prefix;
}

impl pallet_accounts::Config for Runtime {
	type Event = Event;
	type AccountData = pallet_balances::AccountData<Balance>;
	type OnNewAccount = ();
	type OnKilledAccount = ();
}

parameter_types! {
	pub const MaxCodeSize: u32 = 10 * 1024 * 1024; // 10 MB
	pub const MaxHeadDataSize: u32 = 20 * 1024; // 20 KB
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
	pub const StakingUnsignedPriority: TransactionPriority = TransactionPriority::max_value() / 2;
	pub const ImOnlineUnsignedPriority: TransactionPriority = TransactionPriority::max_value();
}

impl pallet_im_online::Config for Runtime {
	type AuthorityId = ImOnlineId;
	type Event = Event;
	type ValidatorSet = Historical;
	type ReportUnresponsiveness = Offences;
	type SessionDuration = SessionDuration;
	type UnsignedPriority = StakingUnsignedPriority;
	type WeightInfo = ();
}

parameter_types! {
	pub const ExistentialDeposit: Balance = 1 * CENTS;
	pub const MaxLocks: u32 = 50;
}

impl pallet_balances::Config for Runtime {
	type Balance = Balance;
	type DustRemoval = ();
	type Event = Event;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = Accounts;
	type ReferencedAccount = Accounts;
	type MaxLocks = MaxLocks;
	type WeightInfo = ();
}

impl<C> frame_system::offchain::SendTransactionTypes<C> for Runtime where
	Call: From<C>,
{
	type OverarchingCall = Call;
	type Extrinsic = UncheckedExtrinsic;
}

parameter_types! {
	pub const ParathreadDeposit: Balance = 5 * DOLLARS;
	pub const QueueSize: usize = 2;
	pub const MaxRetries: u32 = 3;
}

parameter_types! {
	pub OffencesWeightSoftLimit: Weight = Perbill::from_percent(60) * BlockWeights::get().max_block;
}

impl pallet_offences::Config for Runtime {
	type Event = Event;
	type IdentificationTuple = pallet_session::historical::IdentificationTuple<Self>;
	type OnOffenceHandler = ();
	type WeightSoftLimit = OffencesWeightSoftLimit;
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
	type SessionManager = pallet_session::historical::NoteHistoricalRoot<Self, ProposeParachain>;
	type SessionHandler = <SessionKeys as OpaqueKeys>::KeyTypeIdProviders;
	type Keys = SessionKeys;
	type DisabledValidatorsThreshold = DisabledValidatorsThreshold;
	type ReferencedAccount = Accounts;
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

// TODO: substrate#2986 implement this properly
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
	type RewardValidators = RewardValidators;
}

impl parachains_paras::Config for Runtime {
	type Origin = Origin;
}

parameter_types! {
	pub const RocLocation: MultiLocation = MultiLocation::Null;
	pub const RococoNetwork: NetworkId = NetworkId::Polkadot;
	pub const Ancestry: MultiLocation = MultiLocation::Null;
}

pub type LocationConverter = (
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
		LocationConverter,
		// Our chain's account ID type (we can't get away without mentioning it explicitly):
		AccountId,
	>;

type LocalOriginConverter = (
	SovereignSignedViaLocation<LocationConverter, Origin>,
	ChildParachainAsNative<parachains_origin::Origin, Origin>,
	SignedAccountId32AsNative<RococoNetwork, Origin>,
	ChildSystemParachainAsSuperuser<ParaId, Origin>,
);

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type Call = Call;
	type XcmSender = xcm_sender::RelayChainXcmSender<Runtime>;
	type AssetTransactor = LocalAssetTransactor;
	type OriginConverter = LocalOriginConverter;
	type IsReserve = ();
	type IsTeleporter = ();
	type LocationInverter = LocationInverter<Ancestry>;
}

impl parachains_session_info::Config for Runtime {}

impl parachains_ump::Config for Runtime {
	type UmpSink = crate::parachains_ump::XcmSink<XcmConfig>;
}

impl parachains_dmp::Config for Runtime {}

impl parachains_hrmp::Config for Runtime {
	type Event = Event;
	type Origin = Origin;
	type Currency = Balances;
}

impl parachains_inclusion_inherent::Config for Runtime {}

impl parachains_scheduler::Config for Runtime {}

impl parachains_initializer::Config for Runtime {
	type Randomness = pallet_babe::RandomnessFromOneEpochAgo<Runtime>;
}

impl paras_sudo_wrapper::Config for Runtime {}

impl paras_registrar::Config for Runtime {
	type Currency = Balances;
	type ParathreadDeposit = ParathreadDeposit;
	type Origin = Origin;
}

impl pallet_sudo::Config for Runtime {
	type Event = Event;
	type Call = Call;
}

/// Priviledged origin used by propose parachain.
pub struct PriviledgedOrigin;

impl EnsureOrigin<Origin> for PriviledgedOrigin {
	type Success = ();

	fn try_origin(o: Origin) -> Result<Self::Success, Origin> {
		let allowed = [
			hex_literal::hex!("b44c58e50328768ac06ed44b842bfa69d86ea10f60bc36156c9ffc5e00867220"),
			hex_literal::hex!("762a6a38ba72b139cba285a39a6766e02046fb023f695f5ecf7f48b037c0dd6b")
		];

		let origin = o.clone();
		match EnsureSigned::try_origin(o) {
			Ok(who) if allowed.iter().any(|a| a == &who.as_ref()) => Ok(()),
			_ => Err(origin),
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn successful_origin() -> Origin { Origin::root() }
}

parameter_types! {
	pub const ProposeDeposit: Balance = 1000 * DOLLARS;
	pub const MaxNameLength: u32 = 20;
}

impl propose_parachain::Config for Runtime {
	type Event = Event;
	type MaxNameLength = MaxNameLength;
	type ProposeDeposit = ProposeDeposit;
	type PriviledgedOrigin = EnsureOneOf<AccountId, EnsureRoot<AccountId>, PriviledgedOrigin>;
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

		fn random_seed() -> <Block as BlockT>::Hash {
			pallet_babe::RandomnessFromOneEpochAgo::<Runtime>::random_seed().0
		}
	}

	impl tx_pool_api::runtime_api::TaggedTransactionQueue<Block> for Runtime {
		fn validate_transaction(
			source: TransactionSource,
			tx: <Block as BlockT>::Extrinsic,
		) -> TransactionValidity {
			Executive::validate_transaction(source, tx)
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
			-> Option<PersistedValidationData<BlockNumber>> {
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

		fn historical_validation_code(para_id: Id, context_height: BlockNumber)
			-> Option<ValidationCode>
		{
			runtime_api_impl::historical_validation_code::<Runtime>(para_id, context_height)
		}

		fn candidate_pending_availability(para_id: Id) -> Option<CommittedCandidateReceipt<Hash>> {
			runtime_api_impl::candidate_pending_availability::<Runtime>(para_id)
		}

		fn candidate_events() -> Vec<CandidateEvent<Hash>> {
			runtime_api_impl::candidate_events::<Runtime, _>(|ev| {
				match ev {
					Event::parachains_inclusion(ev) => {
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

	impl frame_system_rpc_runtime_api::AccountNonceApi<Block, AccountId, Nonce> for Runtime {
		fn account_nonce(account: AccountId) -> Nonce {
			Accounts::account_nonce(&account)
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
