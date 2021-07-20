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

//! Polkadot-specific GRANDPA integration utilities.

use std::sync::Arc;

use sp_runtime::traits::{Block as BlockT, NumberFor};
use sp_runtime::generic::BlockId;
use sp_runtime::traits::Header as _;

#[cfg(feature = "full-node")]
use {
	polkadot_primitives::v1::{Hash, Block as PolkadotBlock, Header as PolkadotHeader},
	polkadot_subsystem::messages::{ApprovalVotingMessage, HighestApprovedAncestorBlock},
	prometheus_endpoint::{self, Registry},
	polkadot_overseer::Handle,
	futures::channel::oneshot,
};

/// A custom GRANDPA voting rule that acts as a diagnostic for the approval
/// voting subsystem's desired votes.
///
/// The practical effect of this voting rule is to implement a fixed delay of
/// blocks and to issue a prometheus metric on the lag behind the head that
/// approval checking would indicate.
#[cfg(feature = "full-node")]
#[derive(Clone)]
pub(crate) struct ApprovalCheckingVotingRule {
	checking_lag: Option<prometheus_endpoint::Gauge<prometheus_endpoint::U64>>,
	overseer: Handle,
}

#[cfg(feature = "full-node")]
impl ApprovalCheckingVotingRule {
	/// Create a new approval checking diagnostic voting rule.
	pub fn new(overseer: Handle, registry: Option<&Registry>)
		-> Result<Self, prometheus_endpoint::PrometheusError>
	{
		Ok(ApprovalCheckingVotingRule {
			checking_lag: if let Some(registry) = registry {
				Some(prometheus_endpoint::register(
					prometheus_endpoint::Gauge::with_opts(
						prometheus_endpoint::Opts::new(
							"parachain_approval_checking_finality_lag",
							"How far behind the head of the chain the Approval Checking protocol wants to vote",
						)
					)?,
					registry,
				)?)
			} else {
				None
			},
			overseer,
		})
	}
}

#[cfg(feature = "full-node")]
#[derive(Debug, PartialEq)]
/// Vote explicitly on the given hash.
enum ParachainVotingRuleTarget<H, N> {
	Explicit((H, N)),
	/// Vote on the current target.
	Current,
	/// Vote on the base target - the minimal possible vote.
	Base,
}

#[cfg(feature = "full-node")]
fn approval_checking_vote_to_grandpa_vote<H, N: PartialOrd>(
	approval_checking_vote: Option<(H, N)>,
	current_number: N,
) -> ParachainVotingRuleTarget<H, N> {
	match approval_checking_vote {
		Some((hash, number)) => if number > current_number {
			// respect other voting rule.
			ParachainVotingRuleTarget::Current
		} else {
			ParachainVotingRuleTarget::Explicit((hash, number))
		},
		// If approval-voting chooses 'None', that means we should vote on the base (last round estimate).
		None => ParachainVotingRuleTarget::Base,
	}
}

/// The maximum amount of unfinalized blocks we are willing to allow due to approval checking lag.
/// This is a safety net that should be removed at some point in the future.
#[cfg(feature = "full-node")]
const MAX_APPROVAL_CHECKING_FINALITY_LAG: polkadot_primitives::v1::BlockNumber = 50;

#[cfg(feature = "full-node")]
impl<B> grandpa::VotingRule<PolkadotBlock, B> for ApprovalCheckingVotingRule
	where B: sp_blockchain::HeaderBackend<PolkadotBlock> + 'static
{
	fn restrict_vote(
		&self,
		backend: Arc<B>,
		base: &PolkadotHeader,
		best_target: &PolkadotHeader,
		current_target: &PolkadotHeader,
	) -> grandpa::VotingRuleResult<PolkadotBlock> {
		// Query approval checking and issue metrics.
		let mut overseer = self.overseer.clone();
		let checking_lag = self.checking_lag.clone();

		let best_hash = best_target.hash();
		let best_number = best_target.number.clone();
		let best_header = best_target.clone();

		let current_hash = current_target.hash();
		let current_number = current_target.number.clone();

		let base_hash = base.hash();
		let base_number = base.number;

		Box::pin(async move {
			let (tx, rx) = oneshot::channel();
			let approval_checking_subsystem_vote = {
				overseer.send_msg(
					ApprovalVotingMessage::ApprovedAncestor(
						best_hash,
						base_number,
						tx,
					),
					std::any::type_name::<Self>(),
				).await;

				rx.await
					.ok()
					.and_then(|v| v)
					.map(|HighestApprovedAncestorBlock { hash, number, .. }| (hash, number))
			};

			let approval_checking_subsystem_lag = approval_checking_subsystem_vote.map_or(
				best_number - base_number,
				|(_h, n)| best_number - n,
			);

			if let Some(ref checking_lag) = checking_lag {
				checking_lag.set(approval_checking_subsystem_lag as _);
			}

			let min_vote = {
				if best_number.checked_sub(base_number + MAX_APPROVAL_CHECKING_FINALITY_LAG).is_some() {
					// Catch up to the best, with some extra lag.
					let target_number = best_number.saturating_sub(MAX_APPROVAL_CHECKING_FINALITY_LAG);
					if target_number >= current_number {
						Some((current_hash, current_number))
					} else {
						walk_backwards_to_target_block(&*backend, target_number, &best_header).ok()
					}
				} else {
					Some((base_hash, base_number))
				}
			};

			let vote = match approval_checking_vote_to_grandpa_vote(
				approval_checking_subsystem_vote,
				current_number,
			) {
				ParachainVotingRuleTarget::Explicit(vote) => {
					if min_vote.as_ref().map_or(false, |min| min.1 > vote.1) {
						min_vote
					} else {
						Some(vote)
					}
				}
				ParachainVotingRuleTarget::Current => Some((current_hash, current_number)),
				ParachainVotingRuleTarget::Base => min_vote.or(Some((base_hash, base_number))),
			};

			tracing::trace!(
				target: "parachain::approval-voting",
				?vote,
				?approval_checking_subsystem_vote,
				approval_checking_subsystem_lag,
				current_number,
				best_number,
				base_number,
				"GRANDPA: voting based on approved ancestor.",
			);

			vote
		})
	}
}

/// Returns the block hash of the block at the given `target_number` by walking
/// backwards from the given `current_header`.
pub(super) fn walk_backwards_to_target_block<Block, B>(
	backend: &B,
	target_number: NumberFor<Block>,
	current_header: &Block::Header,
) -> Result<(Block::Hash, NumberFor<Block>), sp_blockchain::Error>
where
	Block: BlockT,
	B: sp_blockchain::HeaderBackend<Block>,
{
	let mut target_hash = current_header.hash();
	let mut target_header = current_header.clone();

	loop {
		if *target_header.number() < target_number {
			unreachable!(
				"we are traversing backwards from a known block; \
				 blocks are stored contiguously; \
				 qed"
			);
		}

		if *target_header.number() == target_number {
			return Ok((target_hash, target_number));
		}

		target_hash = *target_header.parent_hash();
		target_header = backend
			.header(BlockId::Hash(target_hash))?
			.expect("Header known to exist due to the existence of one of its descendants; qed");
	}
}

/// A custom GRANDPA voting rule that "pauses" voting (i.e. keeps voting for the
/// same last finalized block) after a given block at height `N` has been
/// finalized and for a delay of `M` blocks, i.e. until the best block reaches
/// `N` + `M`, the voter will keep voting for block `N`.
#[derive(Clone)]
pub(crate) struct PauseAfterBlockFor<N>(pub(crate) N, pub(crate) N);

impl<Block, B> grandpa::VotingRule<Block, B> for PauseAfterBlockFor<NumberFor<Block>>
where
	Block: BlockT,
	B: sp_blockchain::HeaderBackend<Block>,
{
	fn restrict_vote(
		&self,
		backend: Arc<B>,
		base: &Block::Header,
		best_target: &Block::Header,
		current_target: &Block::Header,
	) -> grandpa::VotingRuleResult<Block> {
		let aux = || {
			// only restrict votes targeting a block higher than the block
			// we've set for the pause
			if *current_target.number() > self.0 {
				// if we're past the pause period (i.e. `self.0 + self.1`)
				// then we no longer need to restrict any votes
				if *best_target.number() > self.0 + self.1 {
					return None;
				}

				// if we've finalized the pause block, just keep returning it
				// until best number increases enough to pass the condition above
				if *base.number() >= self.0 {
					return Some((base.hash(), *base.number()));
				}

				// otherwise find the target header at the pause block
				// to vote on
				return walk_backwards_to_target_block(&*backend, self.0, current_target).ok();
			}

			None
		};

		let target = aux();

		Box::pin(async move { target })
	}
}

/// GRANDPA hard forks due to borked migration of session keys after a runtime
/// upgrade (at #1491596), the signaled authority set changes were invalid
/// (blank keys) and were impossible to finalize. The authorities for these
/// intermediary pending changes are replaced with a static list comprised of
/// w3f validators and randomly selected validators from the latest session (at
/// #1500988).
#[cfg(feature = "full-node")]
pub(crate) fn kusama_hard_forks() -> Vec<(
	grandpa_primitives::SetId,
	(Hash, polkadot_primitives::v1::BlockNumber),
	grandpa_primitives::AuthorityList,
)> {
	use sp_core::crypto::Ss58Codec;
	use std::str::FromStr;

	let forks = vec![
		(
			623,
			"01e94e1e7e9cf07b3b0bf4e1717fce7448e5563901c2ef2e3b8e9ecaeba088b1",
			1492283,
		),
		(
			624,
			"ddc4323c5e8966844dfaa87e0c2f74ef6b43115f17bf8e4ff38845a62d02b9a9",
			1492436,
		),
		(
			625,
			"38ba115b296663e424e32d7b1655cd795719cef4fd7d579271a6d01086cf1628",
			1492586,
		),
		(
			626,
			"f3172b6b8497c10fc772f5dada4eeb1f4c4919c97de9de2e1a439444d5a057ff",
			1492955,
		),
		(
			627,
			"b26526aea299e9d24af29fdacd5cf4751a663d24894e3d0a37833aa14c58424a",
			1493338,
		),
		(
			628,
			"3980d024327d53b8d01ef0d198a052cd058dd579508d8ed6283fe3614e0a3694",
			1493913,
		),
		(
			629,
			"31f22997a786c25ee677786373368cae6fd501fd1bc4b212b8e267235c88179d",
			1495083,
		),
		(
			630,
			"1c65eb250cf54b466c64f1a4003d1415a7ee275e49615450c0e0525179857eef",
			1497404,
		),
		(
			631,
			"9e44116467cc9d7e224e36487bf2cf571698cae16b25f54a7430f1278331fdd8",
			1498598,
		),
	];

	let authorities = vec![
		"CwjLJ1zPWK5Ao9WChAFp7rWGEgN3AyXXjTRPrqgm5WwBpoS",
		"Dp8FHpZTzvoKXztkfrUAkF6xNf6sjVU5ZLZ29NEGUazouou",
		"DtK7YfkhNWU6wEPF1dShsFdhtosVAuJPLkoGhKhG1r5LjKq",
		"FLnHYBuoyThzqJ45tdb8P6yMLdocM7ir27Pg1AnpYoygm1K",
		"FWEfJ5UMghr52UopgYjawAg6hQg3ztbQek75pfeRtLVi8pB",
		"ECoLHAu7HKWGTB9od82HAtequYj6hvNHigkGSB9g3ApxAwB",
		"GL1Tg3Uppo8GYL9NjKj4dWKcS6tW98REop9G5hpu7HgFwTa",
		"ExnjU5LZMktrgtQBE3An6FsQfvaKG1ukxPqwhJydgdgarmY",
		"CagLpgCBu5qJqYF2tpFX6BnU4yHvMGSjc7r3Ed1jY3tMbQt",
		"DsrtmMsD4ijh3n4uodxPoiW9NZ7v7no5wVvPVj8fL1dfrWB",
		"HQB4EctrVR68ozZDyBiRJzLRAEGh1YKgCkAsFjJcegL9RQA",
		"H2YTYbXTFkDY1cGnv164ecnDT3hsD2bQXtyiDbcQuXcQZUV",
		"H5WL8jXmbkCoEcLfvqJkbLUeGrDFsJiMXkhhRWn3joct1tE",
		"DpB37GDrJDYcmg2df2eqsrPKMay1u8hyZ6sQi2FuUiUeNLu",
		"FR8yjKRA9MTjvFGK8kfzrdC23Fr6xd7rfBvZXSjAsmuxURE",
		"DxHPty3B9fpj3duu6Gc6gCSCAvsydJHJEY5G3oVYT8S5BYJ",
		"DbVKC8ZJjevrhqSnZyJMMvmPL7oPPL4ed1roxawYnHVgyin",
		"DVJV81kab2J6oTyRJ9T3NCwW2DSrysbWCssvMcE6cwZHnAd",
		"Fg4rDAyzoVzf39Zo8JFPo4W314ntNWNwm3shr4xKe8M1fJg",
		"GUaNcnAruMVxHGTs7gGpSUpigRJboQYQBBQyPohkFcP6NMH",
		"J4BMGF4W9yWiJz4pkhQW73X6QMGpKUzmPppVnqzBCqw5dQq",
		"E1cR61L1tdDEop4WdWVqcq1H1x6VqsDpSHvFyUeC41uruVJ",
		"GoWLzBsj1f23YtdDpyntnvN1LwXKhF5TEeZvBeTVxofgWGR",
		"CwHwmbogSwtRbrkajVBNubPvWmHBGU4bhMido54M9CjuKZD",
		"FLT63y9oVXJnyiWMAL4RvWxsQx21Vymw9961Z7NRFmSG7rw",
		"FoQ2y6JuHuHTG4rHFL3f2hCxfJMvtrq8wwPWdv8tsdkcyA8",
		"D7QQKqqs8ocGorRA12h4QoBSHDia1DkHeXT4eMfjWQ483QH",
		"J6z7FP35F9DiiU985bhkDTS3WxyeTBeoo9MtLdLoD3GiWPj",
		"EjapydCK25AagodRbDECavHAy8yQY1tmeRhwUXhVWx4cFPv",
		"H8admATcRkGCrF1dTDDBCjQDsYjMkuPaN9YwR2mSCj4DWMQ",
		"FtHMRU1fxsoswJjBvyCGvECepC7gP2X77QbNpyikYSqqR6k",
		"DzY5gwr45GVRUFzRMmeg8iffpqYF47nm3XbJhmjG97FijaE",
		"D3HKWAihSUmg8HrfeFrftSwNK7no261yA9RNr3LUUdsuzuJ",
		"D82DwwGJGTcSvtB3SmNrZejnSertbPzpkYvDUp3ibScL3ne",
		"FTPxLXLQvMDQYFA6VqNLGwWPKhemMYP791XVj8TmDpFuV3b",
		"FzGfKmS7N8Z1tvCBU5JH1eBXZQ9pCtRNoMUnNVv38wZNq72",
		"GDfm1MyLAQ7Rh8YPtF6FtMweV4hz91zzeDy2sSABNNqAbmg",
		"DiVQbq7sozeKp7PXPM1HLFc2m7ih8oepKLRK99oBY3QZak1",
		"HErWh7D2RzrjWWB2fTJfcAejD9MJpadeWWZM2Wnk7LiNWfG",
		"Es4DbDauYZYyRJbr6VxrhdcM1iufP9GtdBYf3YtSEvdwNyb",
		"EBgXT6FaVo4WsN2LmfnB2jnpDFf4zay3E492RGSn6v1tY99",
		"Dr9Zg4fxZurexParztL9SezFeHsPwdP8uGgULeRMbk8DDHJ",
		"JEnSTZJpLh91cSryptj57RtFxq9xXqf4U5wBH3qoP91ZZhN",
		"DqtRkrmtPANa8wrYR7Ce2LxJxk2iNFtiCxv1cXbx54uqdTN",
		"GaxmF53xbuTFKopVEseWiaCTa8fC6f99n4YfW8MGPSPYX3s",
		"EiCesgkAaighBKMpwFSAUdvwE4mRjBjNmmd5fP6d4FG8DAx",
		"HVbwWGUx7kCgUGap1Mfcs37g6JAZ5qsfsM7TsDRcSqvfxmd",
		"G45bc8Ajrd6YSXav77gQwjjGoAsR2qiGd1aLzkMy7o1RLwd",
		"Cqix2rD93Mdf7ytg8tBavAig2TvhXPgPZ2mejQvkq7qgRPq",
		"GpodE2S5dPeVjzHB4Drm8R9rEwcQPtwAspXqCVz1ooFWf5K",
		"CwfmfRmzPKLj3ntSCejuVwYmQ1F9iZWY4meQrAVoJ2G8Kce",
		"Fhp5NPvutRCJ4Gx3G8vCYGaveGcU3KgTwfrn5Zr8sLSgwVx",
		"GeYRRPkyi23wSF3cJGjq82117fKJZUbWsAGimUnzb5RPbB1",
		"DzCJ4y5oT611dfKQwbBDVbtCfENTdMCjb4KGMU3Mq6nyUMu",
	];

	let authorities = authorities
		.into_iter()
		.map(|address| {
			(
				grandpa_primitives::AuthorityId::from_ss58check(address)
					.expect("hard fork authority addresses are static and they should be carefully defined; qed."),
				1,
			)
		})
		.collect::<Vec<_>>();

	forks
		.into_iter()
		.map(|(set_id, hash, number)| {
			let hash = Hash::from_str(hash)
				.expect("hard fork hashes are static and they should be carefully defined; qed.");

			(set_id, (hash, number), authorities.clone())
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use grandpa::VotingRule;
	use polkadot_test_client::{
		TestClientBuilder, TestClientBuilderExt, DefaultTestClientBuilderExt, InitPolkadotBlockBuilder,
		ClientBlockImportExt,
	};
	use sp_blockchain::HeaderBackend;
	use sp_runtime::{generic::BlockId, traits::Header};
	use consensus_common::BlockOrigin;
	use std::sync::Arc;
	use super::{approval_checking_vote_to_grandpa_vote, ParachainVotingRuleTarget};

	#[test]
	fn grandpa_pause_voting_rule_works() {
		let _ = env_logger::try_init();

		let client = Arc::new(TestClientBuilder::new().build());

		let mut push_blocks = {
			let mut client = client.clone();

			move |n| {
				for _ in 0..n {
					let block = client.init_polkadot_block_builder().build().unwrap().block;
					futures::executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();
				}
			}
		};

		let get_header = {
			let client = client.clone();
			move |n| client.header(&BlockId::Number(n)).unwrap().unwrap()
		};

		// the rule should filter all votes after block #20
		// is finalized until block #50 is imported.
		let voting_rule = super::PauseAfterBlockFor(20, 30);

		// add 10 blocks
		push_blocks(10);
		assert_eq!(client.info().best_number, 10);

		// we have not reached the pause block
		// therefore nothing should be restricted
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&get_header(0),
				&get_header(10),
				&get_header(10)
			)),
			None,
		);

		// add 15 more blocks
		// best block: #25
		push_blocks(15);

		// we are targeting the pause block,
		// the vote should not be restricted
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&get_header(10),
				&get_header(20),
				&get_header(20)
			)),
			None,
		);

		// we are past the pause block, votes should
		// be limited to the pause block.
		let pause_block = get_header(20);
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&get_header(10),
				&get_header(21),
				&get_header(21)
			)),
			Some((pause_block.hash(), *pause_block.number())),
		);

		// we've finalized the pause block, so we'll keep
		// restricting our votes to it.
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&pause_block, // #20
				&get_header(21),
				&get_header(21),
			)),
			Some((pause_block.hash(), *pause_block.number())),
		);

		// add 30 more blocks
		// best block: #55
		push_blocks(30);

		// we're at the last block of the pause, this block
		// should still be considered in the pause period
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&pause_block, // #20
				&get_header(50),
				&get_header(50),
			)),
			Some((pause_block.hash(), *pause_block.number())),
		);

		// we're past the pause period, no votes should be filtered
		assert_eq!(
			futures::executor::block_on(voting_rule.restrict_vote(
				client.clone(),
				&pause_block, // #20
				&get_header(51),
				&get_header(51),
			)),
			None,
		);
	}

	#[test]
	fn approval_checking_to_grandpa_rules() {
		assert_eq!(
			approval_checking_vote_to_grandpa_vote::<(), _>(None, 5),
			ParachainVotingRuleTarget::Base,
		);

		assert_eq!(
			approval_checking_vote_to_grandpa_vote(Some(("2", 2)), 3),
			ParachainVotingRuleTarget::Explicit(("2", 2)),
		);

		assert_eq!(
			approval_checking_vote_to_grandpa_vote(Some(("2", 2)), 2),
			ParachainVotingRuleTarget::Explicit(("2", 2)),
		);

		assert_eq!(
			approval_checking_vote_to_grandpa_vote(Some(("2", 2)), 1),
			ParachainVotingRuleTarget::Current,
		);
	}
}
