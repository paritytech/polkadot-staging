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

//! Legacy way of defining subsystems.
//!
//! In the future, everything should be set up using the generated
//! overseer builder pattern instead.

use polkadot_node_subsystem_types::errors::SubsystemError;
use polkadot_overseer_gen::{
	MapSubsystem, SubsystemContext,
	Subsystem,
	SpawnedSubsystem,
	FromOverseer,
};
use polkadot_overseer_all_subsystems_gen::AllSubsystemsGen;
use crate::OverseerSignal;
use crate::AllMessages;

/// A dummy subsystem that implements [`Subsystem`] for all
/// types of messages. Used for tests or as a placeholder.
#[derive(Clone, Copy, Debug)]
pub struct DummySubsystem;

impl<Context> Subsystem<Context, SubsystemError> for DummySubsystem
where
	Context: SubsystemContext<Signal=OverseerSignal, Error=SubsystemError, AllMessages=AllMessages>,
{
	fn start(self, mut ctx: Context) -> SpawnedSubsystem<SubsystemError> {
		let future = Box::pin(async move {
			loop {
				match ctx.recv().await {
					Err(_) => return Ok(()),
					Ok(FromOverseer::Signal(OverseerSignal::Conclude)) => return Ok(()),
					Ok(overseer_msg) => {
						tracing::debug!(
							target: "dummy-subsystem",
							"Discarding a message sent from overseer {:?}",
							overseer_msg
						);
						continue;
					}
				}
			}
		});

		SpawnedSubsystem {
			name: "dummy-subsystem",
			future,
		}
	}
}


/// This struct is passed as an argument to create a new instance of an [`Overseer`].
///
/// As any entity that satisfies the interface may act as a [`Subsystem`] this allows
/// mocking in the test code:
///
/// Each [`Subsystem`] is supposed to implement some interface that is generic over
/// message type that is specific to this [`Subsystem`]. At the moment not all
/// subsystems are implemented and the rest can be mocked with the [`DummySubsystem`].
#[derive(Debug, Clone, AllSubsystemsGen)]
pub struct AllSubsystems<
	CV = (), CB = (), SD = (), AD = (), AR = (), BS = (), BD = (), P = (),
	RA = (), AS = (), NB = (), CA = (), CG = (), CP = (), ApD = (), ApV = (),
	GS = (), DC = (), DP = (), DD = (), CS = (),
> {
	/// A candidate validation subsystem.
	pub candidate_validation: CV,
	/// A candidate backing subsystem.
	pub candidate_backing: CB,
	/// A statement distribution subsystem.
	pub statement_distribution: SD,
	/// An availability distribution subsystem.
	pub availability_distribution: AD,
	/// An availability recovery subsystem.
	pub availability_recovery: AR,
	/// A bitfield signing subsystem.
	pub bitfield_signing: BS,
	/// A bitfield distribution subsystem.
	pub bitfield_distribution: BD,
	/// A provisioner subsystem.
	pub provisioner: P,
	/// A runtime API subsystem.
	pub runtime_api: RA,
	/// An availability store subsystem.
	pub availability_store: AS,
	/// A network bridge subsystem.
	pub network_bridge: NB,
	/// A Chain API subsystem.
	pub chain_api: CA,
	/// A Collation Generation subsystem.
	pub collation_generation: CG,
	/// A Collator Protocol subsystem.
	pub collator_protocol: CP,
	/// An Approval Distribution subsystem.
	pub approval_distribution: ApD,
	/// An Approval Voting subsystem.
	pub approval_voting: ApV,
	/// A Connection Request Issuer subsystem.
	pub gossip_support: GS,
	/// A Dispute Coordinator subsystem.
	pub dispute_coordinator: DC,
	/// A Dispute Participation subsystem.
	pub dispute_participation: DP,
	/// A Dispute Distribution subsystem.
	pub dispute_distribution: DD,
	/// A Chain Selection subsystem.
	pub chain_selection: CS,
}

impl<CV, CB, SD, AD, AR, BS, BD, P, RA, AS, NB, CA, CG, CP, ApD, ApV, GS, DC, DP, DD, CS>
	AllSubsystems<CV, CB, SD, AD, AR, BS, BD, P, RA, AS, NB, CA, CG, CP, ApD, ApV, GS, DC, DP, DD, CS>
{
	/// Create a new instance of [`AllSubsystems`].
	///
	/// Each subsystem is set to [`DummySystem`].
	///
	///# Note
	///
	/// Because of a bug in rustc it is required that when calling this function,
	/// you provide a "random" type for the first generic parameter:
	///
	/// ```
	/// polkadot_overseer::AllSubsystems::<()>::dummy();
	/// ```
	pub fn dummy() -> AllSubsystems<
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
	> {
		AllSubsystems {
			candidate_validation: DummySubsystem,
			candidate_backing: DummySubsystem,
			statement_distribution: DummySubsystem,
			availability_distribution: DummySubsystem,
			availability_recovery: DummySubsystem,
			bitfield_signing: DummySubsystem,
			bitfield_distribution: DummySubsystem,
			provisioner: DummySubsystem,
			runtime_api: DummySubsystem,
			availability_store: DummySubsystem,
			network_bridge: DummySubsystem,
			chain_api: DummySubsystem,
			collation_generation: DummySubsystem,
			collator_protocol: DummySubsystem,
			approval_distribution: DummySubsystem,
			approval_voting: DummySubsystem,
			gossip_support: DummySubsystem,
			dispute_coordinator: DummySubsystem,
			dispute_participation: DummySubsystem,
			dispute_distribution: DummySubsystem,
			chain_selection: DummySubsystem,
		}
	}

	/// Reference every individual subsystem.
	pub fn as_ref(&self) -> AllSubsystems<&'_ CV, &'_ CB, &'_ SD, &'_ AD, &'_ AR, &'_ BS, &'_ BD, &'_ P, &'_ RA, &'_ AS, &'_ NB, &'_ CA, &'_ CG, &'_ CP, &'_ ApD, &'_ ApV, &'_ GS, &'_ DC, &'_ DP, &'_ DD, &'_ CS> {
		AllSubsystems {
			candidate_validation: &self.candidate_validation,
			candidate_backing: &self.candidate_backing,
			statement_distribution: &self.statement_distribution,
			availability_distribution: &self.availability_distribution,
			availability_recovery: &self.availability_recovery,
			bitfield_signing: &self.bitfield_signing,
			bitfield_distribution: &self.bitfield_distribution,
			provisioner: &self.provisioner,
			runtime_api: &self.runtime_api,
			availability_store: &self.availability_store,
			network_bridge: &self.network_bridge,
			chain_api: &self.chain_api,
			collation_generation: &self.collation_generation,
			collator_protocol: &self.collator_protocol,
			approval_distribution: &self.approval_distribution,
			approval_voting: &self.approval_voting,
			gossip_support: &self.gossip_support,
			dispute_coordinator: &self.dispute_coordinator,
			dispute_participation: &self.dispute_participation,
			dispute_distribution: &self.dispute_distribution,
			chain_selection: &self.chain_selection,
		}
	}

	/// Map each subsystem.
	pub fn map_subsystems<Mapper>(self, mapper: Mapper)
		-> AllSubsystems<
			<Mapper as MapSubsystem<CV>>::Output,
			<Mapper as MapSubsystem<CB>>::Output,
			<Mapper as MapSubsystem<SD>>::Output,
			<Mapper as MapSubsystem<AD>>::Output,
			<Mapper as MapSubsystem<AR>>::Output,
			<Mapper as MapSubsystem<BS>>::Output,
			<Mapper as MapSubsystem<BD>>::Output,
			<Mapper as MapSubsystem<P>>::Output,
			<Mapper as MapSubsystem<RA>>::Output,
			<Mapper as MapSubsystem<AS>>::Output,
			<Mapper as MapSubsystem<NB>>::Output,
			<Mapper as MapSubsystem<CA>>::Output,
			<Mapper as MapSubsystem<CG>>::Output,
			<Mapper as MapSubsystem<CP>>::Output,
			<Mapper as MapSubsystem<ApD>>::Output,
			<Mapper as MapSubsystem<ApV>>::Output,
			<Mapper as MapSubsystem<GS>>::Output,
			<Mapper as MapSubsystem<DC>>::Output,
			<Mapper as MapSubsystem<DP>>::Output,
			<Mapper as MapSubsystem<DD>>::Output,
			<Mapper as MapSubsystem<CS>>::Output,
		>
	where
		Mapper: MapSubsystem<CV>,
		Mapper: MapSubsystem<CB>,
		Mapper: MapSubsystem<SD>,
		Mapper: MapSubsystem<AD>,
		Mapper: MapSubsystem<AR>,
		Mapper: MapSubsystem<BS>,
		Mapper: MapSubsystem<BD>,
		Mapper: MapSubsystem<P>,
		Mapper: MapSubsystem<RA>,
		Mapper: MapSubsystem<AS>,
		Mapper: MapSubsystem<NB>,
		Mapper: MapSubsystem<CA>,
		Mapper: MapSubsystem<CG>,
		Mapper: MapSubsystem<CP>,
		Mapper: MapSubsystem<ApD>,
		Mapper: MapSubsystem<ApV>,
		Mapper: MapSubsystem<GS>,
		Mapper: MapSubsystem<DC>,
		Mapper: MapSubsystem<DP>,
		Mapper: MapSubsystem<DD>,
		Mapper: MapSubsystem<CS>,
	{
		AllSubsystems {
			candidate_validation: <Mapper as MapSubsystem<CV>>::map_subsystem(&mapper, self.candidate_validation),
			candidate_backing: <Mapper as MapSubsystem<CB>>::map_subsystem(&mapper, self.candidate_backing),
			statement_distribution: <Mapper as MapSubsystem<SD>>::map_subsystem(&mapper, self.statement_distribution),
			availability_distribution: <Mapper as MapSubsystem<AD>>::map_subsystem(&mapper, self.availability_distribution),
			availability_recovery: <Mapper as MapSubsystem<AR>>::map_subsystem(&mapper, self.availability_recovery),
			bitfield_signing: <Mapper as MapSubsystem<BS>>::map_subsystem(&mapper, self.bitfield_signing),
			bitfield_distribution: <Mapper as MapSubsystem<BD>>::map_subsystem(&mapper, self.bitfield_distribution),
			provisioner: <Mapper as MapSubsystem<P>>::map_subsystem(&mapper, self.provisioner),
			runtime_api: <Mapper as MapSubsystem<RA>>::map_subsystem(&mapper, self.runtime_api),
			availability_store: <Mapper as MapSubsystem<AS>>::map_subsystem(&mapper, self.availability_store),
			network_bridge: <Mapper as MapSubsystem<NB>>::map_subsystem(&mapper, self.network_bridge),
			chain_api: <Mapper as MapSubsystem<CA>>::map_subsystem(&mapper, self.chain_api),
			collation_generation: <Mapper as MapSubsystem<CG>>::map_subsystem(&mapper, self.collation_generation),
			collator_protocol: <Mapper as MapSubsystem<CP>>::map_subsystem(&mapper, self.collator_protocol),
			approval_distribution: <Mapper as MapSubsystem<ApD>>::map_subsystem(&mapper, self.approval_distribution),
			approval_voting: <Mapper as MapSubsystem<ApV>>::map_subsystem(&mapper, self.approval_voting),
			gossip_support: <Mapper as MapSubsystem<GS>>::map_subsystem(&mapper, self.gossip_support),
			dispute_coordinator: <Mapper as MapSubsystem<DC>>::map_subsystem(&mapper, self.dispute_coordinator),
			dispute_participation: <Mapper as MapSubsystem<DP>>::map_subsystem(&mapper, self.dispute_participation),
			dispute_distribution: <Mapper as MapSubsystem<DD>>::map_subsystem(&mapper, self.dispute_distribution),
			chain_selection: <Mapper as MapSubsystem<CS>>::map_subsystem(&mapper, self.chain_selection),
		}
	}
}
