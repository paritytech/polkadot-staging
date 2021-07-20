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

use super::{
	Error,
	Registry,
	IsCollator,
	Block,
	SpawnNamed,
	Hash,
	AuthorityDiscoveryApi,
};
use std::sync::Arc;
use polkadot_network_bridge::RequestMultiplexer;
use polkadot_node_core_av_store::Config as AvailabilityConfig;
use polkadot_node_core_approval_voting::Config as ApprovalVotingConfig;
use polkadot_node_core_candidate_validation::Config as CandidateValidationConfig;
use polkadot_node_core_chain_selection::Config as ChainSelectionConfig;
use polkadot_node_core_dispute_coordinator::Config as DisputeCoordinatorConfig;
use polkadot_overseer::{AllSubsystems, BlockInfo, Overseer, Handle};
use polkadot_primitives::v1::ParachainHost;
use sc_authority_discovery::Service as AuthorityDiscoveryService;
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sc_client_api::AuxStore;
use sc_keystore::LocalKeystore;
use sp_consensus_babe::BabeApi;

pub use polkadot_availability_distribution::AvailabilityDistributionSubsystem;
pub use polkadot_node_core_av_store::AvailabilityStoreSubsystem;
pub use polkadot_availability_bitfield_distribution::BitfieldDistribution as BitfieldDistributionSubsystem;
pub use polkadot_node_core_bitfield_signing::BitfieldSigningSubsystem;
pub use polkadot_node_core_backing::CandidateBackingSubsystem;
pub use polkadot_node_core_candidate_validation::CandidateValidationSubsystem;
pub use polkadot_node_core_chain_api::ChainApiSubsystem;
pub use polkadot_node_collation_generation::CollationGenerationSubsystem;
pub use polkadot_collator_protocol::{CollatorProtocolSubsystem, ProtocolSide};
pub use polkadot_network_bridge::NetworkBridge as NetworkBridgeSubsystem;
pub use polkadot_node_core_provisioner::ProvisioningSubsystem as ProvisionerSubsystem;
pub use polkadot_node_core_runtime_api::RuntimeApiSubsystem;
pub use polkadot_statement_distribution::StatementDistribution as StatementDistributionSubsystem;
pub use polkadot_availability_recovery::AvailabilityRecoverySubsystem;
pub use polkadot_approval_distribution::ApprovalDistribution as ApprovalDistributionSubsystem;
pub use polkadot_node_core_approval_voting::ApprovalVotingSubsystem;
pub use polkadot_gossip_support::GossipSupport as GossipSupportSubsystem;
pub use polkadot_node_core_dispute_coordinator::DisputeCoordinatorSubsystem;
pub use polkadot_node_core_dispute_participation::DisputeParticipationSubsystem;
pub use polkadot_dispute_distribution::DisputeDistributionSubsystem;
pub use polkadot_node_core_chain_selection::ChainSelectionSubsystem;

/// Arguments passed for overseer construction.
pub struct OverseerGenArgs<'a, Spawner, RuntimeClient> where
	RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
	RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
	Spawner: 'static + SpawnNamed + Clone + Unpin,
{
	/// Set of initial relay chain leaves to track.
	pub leaves: Vec<BlockInfo>,
	/// The keystore to use for i.e. validator keys.
	pub keystore: Arc<LocalKeystore>,
	/// Runtime client generic, providing the `ProvieRuntimeApi` trait besides others.
	pub runtime_client: Arc<RuntimeClient>,
	/// The underlying key value store for the parachains.
	pub parachains_db: Arc<dyn kvdb::KeyValueDB>,
	/// Underlying network service implementation.
	pub network_service: Arc<sc_network::NetworkService<Block, Hash>>,
	/// Underlying authority discovery service.
	pub authority_discovery_service: AuthorityDiscoveryService,
	/// A multiplexer to arbitrate incoming `IncomingRequest`s from the network.
	pub request_multiplexer: RequestMultiplexer,
	/// Prometheus registry, commonly used for production systems, less so for test.
	pub registry: Option<&'a Registry>,
	/// Task spawner to be used throughout the overseer and the APIs it provides.
	pub spawner: Spawner,
	/// Determines the behavior of the collator.
	pub is_collator: IsCollator,
	/// Configuration for the approval voting subsystem.
	pub approval_voting_config: ApprovalVotingConfig,
	/// Configuration for the availability store subsystem.
	pub availability_config: AvailabilityConfig,
	/// Configuration for the candidate validation subsystem.
	pub candidate_validation_config: CandidateValidationConfig,
	/// Configuration for the chain selection subsystem.
	pub chain_selection_config: ChainSelectionConfig,
	/// Configuration for the dispute coordinator subsystem.
	pub dispute_coordinator_config: DisputeCoordinatorConfig,
}

/// Create a default, unaltered set of subsystems.
///
/// A convenience for usage with malus, to avoid
/// repetitive code across multiple behavior strain implementations.
pub fn create_default_subsystems<'a, Spawner, RuntimeClient>
(
	OverseerGenArgs {
		keystore,
		runtime_client,
		parachains_db,
		network_service,
		authority_discovery_service,
		request_multiplexer,
		registry,
		spawner,
		is_collator,
		approval_voting_config,
		availability_config,
		candidate_validation_config,
		chain_selection_config,
		dispute_coordinator_config,
		..
	} : OverseerGenArgs<'a, Spawner, RuntimeClient>
) -> Result<
	AllSubsystems<
	CandidateValidationSubsystem,
	CandidateBackingSubsystem<Spawner>,
	StatementDistributionSubsystem,
	AvailabilityDistributionSubsystem,
	AvailabilityRecoverySubsystem,
	BitfieldSigningSubsystem<Spawner>,
	BitfieldDistributionSubsystem,
	ProvisionerSubsystem<Spawner>,
	RuntimeApiSubsystem<RuntimeClient>,
	AvailabilityStoreSubsystem,
	NetworkBridgeSubsystem<Arc<sc_network::NetworkService<Block, Hash>>, AuthorityDiscoveryService>,
	ChainApiSubsystem<RuntimeClient>,
	CollationGenerationSubsystem,
	CollatorProtocolSubsystem,
	ApprovalDistributionSubsystem,
	ApprovalVotingSubsystem,
	GossipSupportSubsystem,
	DisputeCoordinatorSubsystem,
	DisputeParticipationSubsystem,
	DisputeDistributionSubsystem<AuthorityDiscoveryService>,
	ChainSelectionSubsystem,
>,
	Error
>
where
	RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
	RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
	Spawner: 'static + SpawnNamed + Clone + Unpin
{
	use polkadot_node_subsystem_util::metrics::Metrics;

	let all_subsystems = AllSubsystems {
		availability_distribution: AvailabilityDistributionSubsystem::new(
			keystore.clone(),
			Metrics::register(registry)?,
		),
		availability_recovery: AvailabilityRecoverySubsystem::with_chunks_only(
		),
		availability_store: AvailabilityStoreSubsystem::new(
			parachains_db.clone(),
			availability_config,
			Metrics::register(registry)?,
		),
		bitfield_distribution: BitfieldDistributionSubsystem::new(
			Metrics::register(registry)?,
		),
		bitfield_signing: BitfieldSigningSubsystem::new(
			spawner.clone(),
			keystore.clone(),
			Metrics::register(registry)?,
		),
		candidate_backing: CandidateBackingSubsystem::new(
			spawner.clone(),
			keystore.clone(),
			Metrics::register(registry)?,
		),
		candidate_validation: CandidateValidationSubsystem::with_config(
			candidate_validation_config,
			Metrics::register(registry)?,
		),
		chain_api: ChainApiSubsystem::new(
			runtime_client.clone(),
			Metrics::register(registry)?,
		),
		collation_generation: CollationGenerationSubsystem::new(
			Metrics::register(registry)?,
		),
		collator_protocol: {
			let side = match is_collator {
				IsCollator::Yes(collator_pair) => ProtocolSide::Collator(
					network_service.local_peer_id().clone(),
					collator_pair,
					Metrics::register(registry)?,
				),
				IsCollator::No => ProtocolSide::Validator {
					keystore: keystore.clone(),
					eviction_policy: Default::default(),
					metrics: Metrics::register(registry)?,
				},
			};
			CollatorProtocolSubsystem::new(
				side,
			)
		},
		network_bridge: NetworkBridgeSubsystem::new(
			network_service.clone(),
			authority_discovery_service.clone(),
			request_multiplexer,
			Box::new(network_service.clone()),
			Metrics::register(registry)?,
		),
		provisioner: ProvisionerSubsystem::new(
			spawner.clone(),
			(),
			Metrics::register(registry)?,
		),
		runtime_api: RuntimeApiSubsystem::new(
			runtime_client.clone(),
			Metrics::register(registry)?,
			spawner.clone(),
		),
		statement_distribution: StatementDistributionSubsystem::new(
			keystore.clone(),
			Metrics::register(registry)?,
		),
		approval_distribution: ApprovalDistributionSubsystem::new(
			Metrics::register(registry)?,
		),
		approval_voting: ApprovalVotingSubsystem::with_config(
			approval_voting_config,
			parachains_db.clone(),
			keystore.clone(),
			Box::new(network_service.clone()),
			Metrics::register(registry)?,
		),
		gossip_support: GossipSupportSubsystem::new(
			keystore.clone(),
		),
		dispute_coordinator: DisputeCoordinatorSubsystem::new(
			parachains_db.clone(),
			dispute_coordinator_config,
			keystore.clone(),
		),
		dispute_participation: DisputeParticipationSubsystem::new(),
		dispute_distribution: DisputeDistributionSubsystem::new(
			keystore.clone(),
			authority_discovery_service.clone(),
			Metrics::register(registry)?,
		),
		chain_selection: ChainSelectionSubsystem::new(
			chain_selection_config,
			parachains_db,
		),
	};
	Ok(all_subsystems)
}


/// Trait for the `fn` generating the overseer.
///
/// Default behavior is to create an unmodified overseer, as `RealOverseerGen`
/// would do.
pub trait OverseerGen {
	/// Overwrite the full generation of the overseer, including the subsystems.
	fn generate<'a, Spawner, RuntimeClient>(&self, args: OverseerGenArgs<'a, Spawner, RuntimeClient>) -> Result<(Overseer<Spawner, Arc<RuntimeClient>>, Handle), Error>
	where
		RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
		RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
		Spawner: 'static + SpawnNamed + Clone + Unpin {
		let gen = RealOverseerGen;
		RealOverseerGen::generate::<Spawner, RuntimeClient>(&gen, args)
	}
	// It would be nice to make `create_subsystems` part of this trait,
	// but the amount of generic arguments that would be required as
	// as consequence make this rather annoying to implement and use.
}

/// The regular set of subsystems.
pub struct RealOverseerGen;

impl OverseerGen for RealOverseerGen {
	fn generate<'a, Spawner, RuntimeClient>(&self,
		args : OverseerGenArgs<'a, Spawner, RuntimeClient>
	) -> Result<(Overseer<Spawner, Arc<RuntimeClient>>, Handle), Error>
	where
		RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
		RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
		Spawner: 'static + SpawnNamed + Clone + Unpin
	{
		let spawner = args.spawner.clone();
		let leaves = args.leaves.clone();
		let runtime_client = args.runtime_client.clone();
		let registry = args.registry.clone();

		let all_subsystems = create_default_subsystems::<Spawner, RuntimeClient>(args)?;

		Overseer::new(
			leaves,
			all_subsystems,
			registry,
			runtime_client,
			spawner,
		).map_err(|e| e.into())
	}
}
