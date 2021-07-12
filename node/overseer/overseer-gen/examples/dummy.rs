//! A dummy to be used with cargo expand

use polkadot_overseer_gen::prelude::*;
use polkadot_node_network_protocol::WrongVariant;


/// Concrete subsystem implementation for `MsgStrukt` msg type.
#[derive(Default)]
pub struct AwesomeSubSys;

impl Subsystem<XxxSubsystemContext<MsgStrukt>> for  AwesomeSubSys {
	fn start(self, _ctx: XxxSubsystemContext<MsgStrukt>) -> SpawnedSubsystem {
		unimplemented!("starting yay!")
	}
}

#[derive(Default)]
pub struct GoblinTower;

impl Subsystem<XxxSubsystemContext<Plinko>> for GoblinTower {
	fn start(self, _ctx: XxxSubsystemContext<Plinko>) -> SpawnedSubsystem {
		unimplemented!("welcum")
	}
}


/// A signal sent by the overseer.
#[derive(Debug, Clone)]
pub struct SigSigSig;


/// The external event.
#[derive(Debug, Clone)]
pub struct EvX;


impl EvX {
	pub fn focus<'a, T>(&'a self) -> Result<EvX, ()> {
		unimplemented!("dispatch")
	}
}

#[derive(Debug, Clone, Copy)]
pub struct Yikes;

impl std::fmt::Display for Yikes {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		writeln!(f, "yikes!")
	}
}

impl std::error::Error for Yikes {}

impl From<polkadot_overseer_gen::OverseerError> for Yikes {
	fn from(_: polkadot_overseer_gen::OverseerError) -> Yikes {
		Yikes
	}
}

impl From<polkadot_overseer_gen::mpsc::SendError> for Yikes {
	fn from(_: polkadot_overseer_gen::mpsc::SendError) -> Yikes {
		Yikes
	}
}

#[derive(Debug, Clone)]
pub struct MsgStrukt(u8);

#[derive(Debug, Clone, Copy)]
pub struct Plinko;

impl From<NetworkMsg> for MsgStrukt {
	fn from(_event: NetworkMsg) -> Self {
		MsgStrukt(1u8)
	}
}


#[derive(Debug, Clone, Copy)]
pub enum NetworkMsg {
	A,
	B,
	C,
}


impl NetworkMsg {
	fn focus(&self) -> Result<Self, WrongVariant> {
		Ok(match self {
			Self::B => return Err(WrongVariant),
			Self::A | Self::C => self.clone()
		})
	}
}



#[overlord(signal=SigSigSig, event=EvX, error=Yikes, network=NetworkMsg, gen=AllMessages)]
struct Xxx {
	#[subsystem(MsgStrukt)]
	sub0: AwesomeSubSys,

	#[subsystem(no_dispatch, blocking, Plinko)]
	plinkos: GoblinTower,

	i_like_pi: f64,
}

#[derive(Debug, Clone)]
struct DummySpawner;

impl SpawnNamed for DummySpawner{
	fn spawn_blocking(&self, name: &'static str, _future: futures::future::BoxFuture<'static, ()>) {
		unimplemented!("spawn blocking {}", name)
	}

	fn spawn(&self, name: &'static str, _future: futures::future::BoxFuture<'static, ()>) {
		unimplemented!("spawn {}", name)
	}
}

#[derive(Debug, Clone)]
struct DummyCtx;

fn main() {
	let (overseer, _handler): (Xxx<_>, _) = Xxx::builder()
		.sub0(AwesomeSubSys::default())
		.plinkos(GoblinTower::default())
		.i_like_pi(::std::f64::consts::PI)
		.spawner(DummySpawner)
		.build()
		.unwrap();
	assert_eq!(overseer.i_like_pi.floor() as i8, 3);
}
