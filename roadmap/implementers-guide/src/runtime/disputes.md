# Disputes Module

After a backed candidate is made available, it is included and proceeds into an acceptance period during which validators are randomly selected to do (secondary) approval checks of the parablock. Any reports disputing the validity of the candidate will cause escalation, where even more validators are requested to check the block, and so on, until either the parablock is determined to be invalid or valid. Those on the wrong side of the dispute are slashed and, if the parablock is deemed invalid, the relay chain is rolled back to a point before that block was included.

However, this isn't the end of the story. We are working in a forkful blockchain environment, which carries three important considerations:

1. For security, validators that misbehave shouldn't only be slashed on one fork, but on all possible forks. Validators that misbehave shouldn't be able to create a new fork of the chain when caught and get away with their misbehavior.
1. It is possible (and likely) that the parablock being contested has not appeared on all forks.
1. If a block author believes that there is a disputed parablock on a specific fork that will resolve to a reversion of the fork, that block author has more incentive to build on a different fork which does not include that parablock.

This means that in all likelihood, there is the possibility of disputes that are started on one fork of the relay chain, and as soon as the dispute resolution process starts to indicate that the parablock is indeed invalid, that fork of the relay chain will be abandoned and the dispute will never be fully resolved on that chain.

Even if this doesn't happen, there is the possibility that there are two disputes underway, and one resolves leading to a reversion of the chain before the other has concluded. In this case we want to both transplant the concluded dispute onto other forks of the chain as well as the unconcluded dispute.

We account for these requirements by having the disputes module handle two kinds of disputes.

1. Local disputes: those contesting the validity of the current fork by disputing a parablock included within it.
1. Remote disputes: a dispute that has partially or fully resolved on another fork which is transplanted to the local fork for completion and eventual slashing.

When a local dispute concludes negatively, the chain needs to be abandoned and reverted back to a block where the state does not contain the bad parablock. We expect that due to the [Approval Checking Protocol](../protocol-approval.md), the current executing block should not be finalized. So we do two things when a local dispute concludes negatively:
1. Freeze the state of parachains so nothing further is backed or included.
1. Issue a digest in the header of the block that signals to nodes that this branch of the chain is to be abandoned.

If, as is expected, the chain is unfinalized, the freeze will have no effect as no honest validator will attempt to build on the frozen chain. However, if the approval checking protocol has failed and the bad parablock is finalized, the freeze serves to put the chain into a governance-only mode.

The storage of this module is designed around tracking [`DisputeState`s](../types/disputes.md#disputestate), updating them with votes, and tracking blocks included by this branch of the relay chain. It also contains a `Frozen` parameter designed to freeze the state of all parachains.

## Storage

Storage Layout:

```rust
LastPrunedSession: Option<SessionIndex>,

// All ongoing or concluded disputes for the last several sessions.
Disputes: double_map (SessionIndex, CandidateHash) -> Option<DisputeState>,
// All included blocks on the chain, as well as the block number in this chain that
// should be reverted back to if the candidate is disputed and determined to be invalid.
Included: double_map (SessionIndex, CandidateHash) -> Option<BlockNumber>,
// Maps session indices to a vector indicating the number of potentially-spam disputes 
// each validator is participating in. Potentially-spam disputes are remote disputes which have
// fewer than `byzantine_threshold + 1` validators.
//
// The i'th entry of the vector corresponds to the i'th validator in the session.
SpamSlots: map SessionIndex -> Option<Vec<u32>>,
// Whether the chain is frozen or not. Starts as `None`. When this is `Some`,
// the chain will not accept any new parachain blocks for backing or inclusion,
// and its value indicates the last valid block number in the chain.
// It can only be set back to `None` by governance intervention.
Frozen: Option<BlockNumber>,
```

> `byzantine_threshold` refers to the maximum number `f` of validators which may be byzantine. The total number of validators is `n = 3f + e` where `e in { 1, 2, 3 }`.

## Session Change

1. If the current session is not greater than `config.dispute_period + 1`, nothing to do here.
1. Set `pruning_target = current_session - config.dispute_period - 1`. We add the extra `1` because we want to keep things for `config.dispute_period` _full_ sessions. 
   The stuff at the end of the most recent session has been around for a little over 0 sessions, not a little over 1.
1. If `LastPrunedSession` is `None`, then set `LastPrunedSession` to `Some(pruning_target)` and return.
1. Otherwise, clear out all disputes, included candidates, and `SpamSlots` entries in the range `last_pruned..=pruning_target` and set `LastPrunedSession` to `Some(pruning_target)`.

## Block Initialization

1. Iterate through all disputes. If any have not concluded and started more than `config.dispute_conclusion_by_timeout_period` blocks ago, set them to `Concluded` and mildly punish all validators associated, as they have failed to distribute available data. If the `Included` map does not contain the candidate and there are fewer than `byzantine_threshold + 1` participating validators, reduce `SpamSlots` for all participating validators.

## Routines

* `filter_multi_dispute_data(MultiDisputeStatementSet) -> MultiDisputeStatementSet`:
  1. Takes a `MultiDisputeStatementSet` and filters it down to a `MultiDisputeStatementSet`
    that satisfies all the criteria of `provide_multi_dispute_data`. That is, eliminating
    ancient votes, votes which overwhelm the maximum amount of spam slots, and duplicates. 
    This can be used by block authors to create the final submission in a block which is 
    guaranteed to pass the `provide_multi_dispute_data` checks.

* `provide_multi_dispute_data(MultiDisputeStatementSet) -> Vec<(SessionIndex, Hash)>`:
  1. Pass on each dispute statement set to `provide_dispute_data`, propagating failure.
  1. Return a list of all candidates who just had disputes initiated.

* `provide_dispute_data(DisputeStatementSet) -> bool`: Provide data to an ongoing dispute or initiate a dispute.
  1. All statements must be issued under the correct session for the correct candidate. 
  1. `SessionInfo` is used to check statement signatures and this function should fail if any signatures are invalid.
  1. If there is no dispute under `Disputes`, create a new `DisputeState` with blank bitfields.
  1. If `concluded_at` is `Some`, and is `concluded_at + config.post_conclusion_acceptance_period < now`, return false.
  1. If the overlap of the validators in the `DisputeStatementSet` and those already present in the `DisputeState` is fewer in number than `byzantine_threshold + 1` and the candidate is not present in the `Included` map
      1. increment `SpamSlots` for each validator in the `DisputeStatementSet` which is not already in the `DisputeState`. Initialize the `SpamSlots` to a zeroed vector first, if necessary. do not increment `SpamSlots` if the candidate is local.
      1. If the value for any spam slot exceeds `config.dispute_max_spam_slots`, return false.
  1. If the overlap of the validators in the `DisputeStatementSet` and those already present in the `DisputeState` is at least `byzantine_threshold + 1`, the `DisputeState` has fewer than `byzantine_threshold + 1` validators, and the candidate is not present in the `Included` map, then decrease `SpamSlots` by 1 for each validator in the `DisputeState`.
  1. Import all statements into the dispute. This should fail if any statements are duplicate or if the corresponding bit for the corresponding validator is set in the dispute already.
  1. If `concluded_at` is `None`, reward all statements.
  1. If `concluded_at` is `Some`, reward all statements slightly less.
  1. If either side now has supermajority and did not previously, slash the other side. This may be both sides, and we support this possibility in code, but note that this requires validators to participate on both sides which has negative expected value. Set `concluded_at` to `Some(now)` if it was `None`.
  1. If just concluded against the candidate and the `Included` map contains `(session, candidate)`: invoke `revert_and_freeze` with the stored block number.
  1. Return true if just initiated, false otherwise.

* `disputes() -> Vec<(SessionIndex, CandidateHash, DisputeState)>`: Get a list of all disputes and info about dispute state.
  1. Iterate over all disputes in `Disputes` and collect into a vector.

* `note_included(SessionIndex, CandidateHash, included_in: BlockNumber)`:
  1. Add `(SessionIndex, CandidateHash)` to the `Included` map with `included_in - 1` as the value.
  1. If there is a dispute under `(Sessionindex, CandidateHash)` with fewer than `byzantine_threshold + 1` participating validators, decrease `SpamSlots` by 1 for each validator in the `DisputeState`.
  1. If there is a dispute under `(SessionIndex, CandidateHash)` that has concluded against the candidate, invoke `revert_and_freeze` with the stored block number.

* `could_be_invalid(SessionIndex, CandidateHash) -> bool`: Returns whether a candidate has a live dispute ongoing or a dispute which has already concluded in the negative.

* `is_frozen()`: Load the value of `Frozen` from storage. Return true if `Some` and false if `None`.

* `last_valid_block()`: Load the value of `Frozen` from storage and return. None indicates that all blocks in the chain are potentially valid.

* `revert_and_freeze(BlockNumber)`:
  1. If `is_frozen()` return.
  1. Set `Frozen` to `Some(BlockNumber)` to indicate a rollback to the given block number is necessary.
