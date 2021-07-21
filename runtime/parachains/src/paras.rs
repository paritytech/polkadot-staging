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

//! The paras pallet is responsible for storing data on parachains and parathreads.
//!
//! It tracks which paras are parachains, what their current head data is in
//! this fork of the relay chain, what their validation code is, and what their past and upcoming
//! validation code is.
//!
//! A para is not considered live until it is registered and activated in this pallet. Activation can
//! only occur at session boundaries.

use sp_std::prelude::*;
use sp_std::result;
use primitives::v1::{
	Id as ParaId, ValidationCode, ValidationCodeHash, HeadData, SessionIndex, ConsensusLog,
};
use sp_runtime::{traits::One, DispatchResult, SaturatedConversion};
use frame_system::pallet_prelude::*;
use frame_support::pallet_prelude::*;
use parity_scale_codec::{Encode, Decode};
use crate::{configuration, shared, initializer::SessionChangeNotification};
use sp_core::RuntimeDebug;

#[cfg(feature = "std")]
use serde::{Serialize, Deserialize};

pub use crate::Origin as ParachainOrigin;

pub use pallet::*;

// the two key times necessary to track for every code replacement.
#[derive(Default, Encode, Decode)]
#[cfg_attr(test, derive(Debug, Clone, PartialEq))]
pub struct ReplacementTimes<N> {
	/// The relay-chain block number that the code upgrade was expected to be activated.
	/// This is when the code change occurs from the para's perspective - after the
	/// first parablock included with a relay-parent with number >= this value.
	expected_at: N,
	/// The relay-chain block number at which the parablock activating the code upgrade was
	/// actually included. This means considered included and available, so this is the time at which
	/// that parablock enters the acceptance period in this fork of the relay-chain.
	activated_at: N,
}

/// Metadata used to track previous parachain validation code that we keep in
/// the state.
#[derive(Default, Encode, Decode)]
#[cfg_attr(test, derive(Debug, Clone, PartialEq))]
pub struct ParaPastCodeMeta<N> {
	/// Block numbers where the code was expected to be replaced and where the code
	/// was actually replaced, respectively. The first is used to do accurate lookups
	/// of historic code in historic contexts, whereas the second is used to do
	/// pruning on an accurate timeframe. These can be used as indices
	/// into the `PastCodeHash` map along with the `ParaId` to fetch the code itself.
	upgrade_times: Vec<ReplacementTimes<N>>,
	/// Tracks the highest pruned code-replacement, if any. This is the `activated_at` value,
	/// not the `expected_at` value.
	last_pruned: Option<N>,
}

#[cfg_attr(test, derive(Debug, PartialEq))]
enum UseCodeAt<N> {
	/// Use the current code.
	Current,
	/// Use the code that was replaced at the given block number.
	/// This is an inclusive endpoint - a parablock in the context of a relay-chain block on this fork
	/// with number N should use the code that is replaced at N.
	ReplacedAt(N),
}

/// The possible states of a para, to take into account delayed lifecycle changes.
///
/// If the para is in a "transition state", it is expected that the parachain is
/// queued in the `ActionsQueue` to transition it into a stable state. Its lifecycle
/// state will be used to determine the state transition to apply to the para.
#[derive(PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug)]
pub enum ParaLifecycle {
	/// Para is new and is onboarding as a Parathread or Parachain.
	Onboarding,
	/// Para is a Parathread.
	Parathread,
	/// Para is a Parachain.
	Parachain,
	/// Para is a Parathread which is upgrading to a Parachain.
	UpgradingParathread,
	/// Para is a Parachain which is downgrading to a Parathread.
	DowngradingParachain,
	/// Parathread is queued to be offboarded.
	OffboardingParathread,
	/// Parachain is queued to be offboarded.
	OffboardingParachain,
}

impl ParaLifecycle {
	/// Returns true if parachain is currently onboarding. To learn if the
	/// parachain is onboarding as a parachain or parathread, look at the
	/// `UpcomingGenesis` storage item.
	pub fn is_onboarding(&self) -> bool {
		matches!(self, ParaLifecycle::Onboarding)
	}

	/// Returns true if para is in a stable state, i.e. it is currently
	/// a parachain or parathread, and not in any transition state.
	pub fn is_stable(&self) -> bool {
		matches!(self, ParaLifecycle::Parathread | ParaLifecycle::Parachain)
	}

	/// Returns true if para is currently treated as a parachain.
	/// This also includes transitioning states, so you may want to combine
	/// this check with `is_stable` if you specifically want `Paralifecycle::Parachain`.
	pub fn is_parachain(&self) -> bool {
		matches!(self,
			ParaLifecycle::Parachain |
			ParaLifecycle::DowngradingParachain |
			ParaLifecycle::OffboardingParachain
		)
	}

	/// Returns true if para is currently treated as a parathread.
	/// This also includes transitioning states, so you may want to combine
	/// this check with `is_stable` if you specifically want `Paralifecycle::Parathread`.
	pub fn is_parathread(&self) -> bool {
		matches!(self,
			ParaLifecycle::Parathread |
			ParaLifecycle::UpgradingParathread |
			ParaLifecycle::OffboardingParathread
		)
	}

	/// Returns true if para is currently offboarding.
	pub fn is_offboarding(&self) -> bool {
		matches!(self, ParaLifecycle::OffboardingParathread | ParaLifecycle::OffboardingParachain)
	}

	/// Returns true if para is in any transitionary state.
	pub fn is_transitioning(&self) -> bool {
		!Self::is_stable(self)
	}
}

impl<N: Ord + Copy + PartialEq> ParaPastCodeMeta<N> {
	// note a replacement has occurred at a given block number.
	fn note_replacement(&mut self, expected_at: N, activated_at: N) {
		self.upgrade_times.push(ReplacementTimes { expected_at, activated_at })
	}

	// Yields an identifier that should be used for validating a
	// parablock in the context of a particular relay-chain block number in this chain.
	//
	// a return value of `None` means that there is no code we are aware of that
	// should be used to validate at the given height.
	fn code_at(&self, para_at: N) -> Option<UseCodeAt<N>> {
		// Find out
		// a) if there is a point where code was replaced in the current chain after the context
		//    we are finding out code for.
		// b) what the index of that point is.
		//
		// The reason we use `activated_at` instead of `expected_at` is that a gap may occur
		// between expectation and actual activation. Any block executed in a context from
		// `expected_at..activated_at` is expected to activate the code upgrade and therefore should
		// use the previous code.
		//
		// A block executed in the context of `activated_at` should use the new code.
		//
		// Cases where `expected_at` and `activated_at` are the same, that is, zero-delay code upgrades
		// are also handled by this rule correctly.
		let replaced_after_pos = self.upgrade_times.iter().position(|t| {
			// example: code replaced at (5, 5)
			//
			// context #4 should use old code
			// context #5 should use new code
			//
			// example: code replaced at (10, 20)
			// context #9 should use the old code
			// context #10 should use the old code
			// context #19 should use the old code
			// context #20 should use the new code
			para_at < t.activated_at
		});

		if let Some(replaced_after_pos) = replaced_after_pos {
			// The earliest stored code replacement needs to be special-cased, since we need to check
			// against the pruning state to see if this replacement represents the correct code, or
			// is simply after a replacement that actually represents the correct code, but has been pruned.
			let was_pruned = replaced_after_pos == 0
				&& self.last_pruned.map_or(false, |t| t >= para_at);

			if was_pruned {
				None
			} else {
				Some(UseCodeAt::ReplacedAt(self.upgrade_times[replaced_after_pos].expected_at))
			}
		} else {
			// No code replacements after this context.
			// This means either that the current code is valid, or `para_at` is so old that
			// we don't know the code necessary anymore. Compare against `last_pruned` to determine.
			self.last_pruned.as_ref().map_or(
				Some(UseCodeAt::Current), // nothing pruned, use current
				|earliest_activation| if &para_at < earliest_activation {
					None
				} else {
					Some(UseCodeAt::Current)
				},
			)
		}
	}

	// The block at which the most recently tracked code change occurred, from the perspective
	// of the para.
	fn most_recent_change(&self) -> Option<N> {
		self.upgrade_times.last().map(|x| x.expected_at.clone())
	}

	// prunes all code upgrade logs occurring at or before `max`.
	// note that code replaced at `x` is the code used to validate all blocks before
	// `x`. Thus, `max` should be outside of the slashing window when this is invoked.
	//
	// Since we don't want to prune anything inside the acceptance period, and the parablock only
	// enters the acceptance period after being included, we prune based on the activation height of
	// the code change, not the expected height of the code change.
	//
	// returns an iterator of block numbers at which code was replaced, where the replaced
	// code should be now pruned, in ascending order.
	fn prune_up_to(&'_ mut self, max: N) -> impl Iterator<Item=N> + '_ {
		let to_prune = self.upgrade_times.iter().take_while(|t| t.activated_at <= max).count();
		let drained = if to_prune == 0 {
			// no-op prune.
			self.upgrade_times.drain(self.upgrade_times.len()..)
		} else {
			// if we are actually pruning something, update the last_pruned member.
			self.last_pruned = Some(self.upgrade_times[to_prune - 1].activated_at);
			self.upgrade_times.drain(..to_prune)
		};

		drained.map(|times| times.expected_at)
	}
}

/// Arguments for initializing a para.
#[derive(PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
pub struct ParaGenesisArgs {
	/// The initial head data to use.
	pub genesis_head: HeadData,
	/// The initial validation code to use.
	pub validation_code: ValidationCode,
	/// True if parachain, false if parathread.
	pub parachain: bool,
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config:
		frame_system::Config +
		configuration::Config +
		shared::Config
	{
		/// The outer origin type.
		type Origin: From<Origin>
			+ From<<Self as frame_system::Config>::Origin>
			+ Into<result::Result<Origin, <Self as Config>::Origin>>;

		type Event: From<Event> + IsType<<Self as frame_system::Config>::Event>;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event {
		/// Current code has been updated for a Para. `para_id`
		CurrentCodeUpdated(ParaId),
		/// Current head has been updated for a Para. `para_id`
		CurrentHeadUpdated(ParaId),
		/// A code upgrade has been scheduled for a Para. `para_id`
		CodeUpgradeScheduled(ParaId),
		/// A new head has been noted for a Para. `para_id`
		NewHeadNoted(ParaId),
		/// A para has been queued to execute pending actions. `para_id`
		ActionQueued(ParaId, SessionIndex),
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Para is not registered in our system.
		NotRegistered,
		/// Para cannot be onboarded because it is already tracked by our system.
		CannotOnboard,
		/// Para cannot be offboarded at this time.
		CannotOffboard,
		/// Para cannot be upgraded to a parachain.
		CannotUpgrade,
		/// Para cannot be downgraded to a parathread.
		CannotDowngrade,
	}

	/// All parachains. Ordered ascending by `ParaId`. Parathreads are not included.
	#[pallet::storage]
	#[pallet::getter(fn parachains)]
	pub(super) type Parachains<T: Config> = StorageValue<_, Vec<ParaId>, ValueQuery>;

	/// The current lifecycle of a all known Para IDs.
	#[pallet::storage]
	pub(super) type ParaLifecycles<T: Config> = StorageMap<_, Twox64Concat, ParaId, ParaLifecycle>;

	/// The head-data of every registered para.
	#[pallet::storage]
	#[pallet::getter(fn para_head)]
	pub(super) type Heads<T: Config> = StorageMap<_, Twox64Concat, ParaId, HeadData>;

	/// The validation code hash of every live para.
	///
	/// Corresponding code can be retrieved with [`CodeByHash`].
	#[pallet::storage]
	pub(super) type CurrentCodeHash<T: Config> = StorageMap<_, Twox64Concat, ParaId, ValidationCodeHash>;

	/// Actual past code hash, indicated by the para id as well as the block number at which it
	/// became outdated.
	///
	/// Corresponding code can be retrieved with [`CodeByHash`].
	#[pallet::storage]
	pub(super) type PastCodeHash<T: Config> = StorageMap<
		_,
		Twox64Concat,
		(ParaId, T::BlockNumber),
		ValidationCodeHash
	>;

	/// Past code of parachains. The parachains themselves may not be registered anymore,
	/// but we also keep their code on-chain for the same amount of time as outdated code
	/// to keep it available for secondary checkers.
	#[pallet::storage]
	#[pallet::getter(fn past_code_meta)]
	pub(super) type PastCodeMeta<T: Config> = StorageMap<
		_,
		Twox64Concat,
		ParaId,
		ParaPastCodeMeta<T::BlockNumber>,
		ValueQuery
	>;

	/// Which paras have past code that needs pruning and the relay-chain block at which the code was replaced.
	/// Note that this is the actual height of the included block, not the expected height at which the
	/// code upgrade would be applied, although they may be equal.
	/// This is to ensure the entire acceptance period is covered, not an offset acceptance period starting
	/// from the time at which the parachain perceives a code upgrade as having occurred.
	/// Multiple entries for a single para are permitted. Ordered ascending by block number.
	#[pallet::storage]
	pub(super) type PastCodePruning<T: Config> = StorageValue<_, Vec<(ParaId, T::BlockNumber)>, ValueQuery>;

	/// The block number at which the planned code change is expected for a para.
	/// The change will be applied after the first parablock for this ID included which executes
	/// in the context of a relay chain block with a number >= `expected_at`.
	#[pallet::storage]
	#[pallet::getter(fn future_code_upgrade_at)]
	pub(super) type FutureCodeUpgrades<T: Config> = StorageMap<_, Twox64Concat, ParaId, T::BlockNumber>;

	/// The actual future code hash of a para.
	///
	/// Corresponding code can be retrieved with [`CodeByHash`].
	#[pallet::storage]
	pub(super) type FutureCodeHash<T: Config> = StorageMap<_, Twox64Concat, ParaId, ValidationCodeHash>;

	/// The actions to perform during the start of a specific session index.
	#[pallet::storage]
	#[pallet::getter(fn actions_queue)]
	pub(super) type ActionsQueue<T: Config> = StorageMap<_, Twox64Concat, SessionIndex, Vec<ParaId>, ValueQuery>;

	/// Upcoming paras instantiation arguments.
	#[pallet::storage]
	pub(super) type UpcomingParasGenesis<T: Config> = StorageMap<_, Twox64Concat, ParaId, ParaGenesisArgs>;

	/// The number of reference on the validation code in [`CodeByHash`] storage.
	#[pallet::storage]
	pub(super) type CodeByHashRefs<T: Config> = StorageMap<_, Identity, ValidationCodeHash, u32, ValueQuery>;

	/// Validation code stored by its hash.
	///
	/// This storage is consistent with [`FutureCodeHash`], [`CurrentCodeHash`] and
	/// [`PastCodeHash`].
	#[pallet::storage]
	#[pallet::getter(fn code_by_hash)]
	pub(super) type CodeByHash<T: Config> =  StorageMap<_, Identity, ValidationCodeHash, ValidationCode>;

	#[pallet::genesis_config]
	pub struct GenesisConfig {
		pub paras: Vec<(ParaId, ParaGenesisArgs)>,
	}

	#[cfg(feature = "std")]
	impl Default for GenesisConfig {
		fn default() -> Self {
			GenesisConfig {
				paras: Default::default(),
			}
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> GenesisBuild<T> for GenesisConfig {
		fn build(&self) {
			let mut parachains: Vec<_> = self.paras
				.iter()
				.filter(|(_, args)| args.parachain)
				.map(|&(ref id, _)| id)
				.cloned()
				.collect();

			parachains.sort();
			parachains.dedup();

			Parachains::<T>::put(&parachains);

			for (id, genesis_args) in &self.paras {
				let code_hash = genesis_args.validation_code.hash();
				<Pallet<T>>::increase_code_ref(&code_hash, &genesis_args.validation_code);
				<Pallet<T> as Store>::CurrentCodeHash::insert(&id, &code_hash);
				<Pallet<T> as Store>::Heads::insert(&id, &genesis_args.genesis_head);
				if genesis_args.parachain {
					ParaLifecycles::<T>::insert(&id, ParaLifecycle::Parachain);
				} else {
					ParaLifecycles::<T>::insert(&id, ParaLifecycle::Parathread);
				}
			}
		}
	}

	#[pallet::origin]
	pub type Origin = ParachainOrigin;

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Set the storage for the parachain validation code immediately.
		#[pallet::weight(0)]
		pub fn force_set_current_code(origin: OriginFor<T>, para: ParaId, new_code: ValidationCode) -> DispatchResult {
			ensure_root(origin)?;
			let prior_code_hash = <Self as Store>::CurrentCodeHash::get(&para).unwrap_or_default();
			let new_code_hash = new_code.hash();
			Self::increase_code_ref(&new_code_hash, &new_code);
			<Self as Store>::CurrentCodeHash::insert(&para, new_code_hash);

			let now = frame_system::Pallet::<T>::block_number();
			Self::note_past_code(para, now, now, prior_code_hash);
			Self::deposit_event(Event::CurrentCodeUpdated(para));
			Ok(())
		}

		/// Set the storage for the current parachain head data immediately.
		#[pallet::weight(0)]
		pub fn force_set_current_head(origin: OriginFor<T>, para: ParaId, new_head: HeadData) -> DispatchResult {
			ensure_root(origin)?;
			<Self as Store>::Heads::insert(&para, new_head);
			Self::deposit_event(Event::CurrentHeadUpdated(para));
			Ok(())
		}

		/// Schedule a code upgrade for block `expected_at`.
		#[pallet::weight(0)]
		pub fn force_schedule_code_upgrade(
			origin: OriginFor<T>,
			para: ParaId,
			new_code: ValidationCode,
			expected_at: T::BlockNumber
		) -> DispatchResult {
			ensure_root(origin)?;
			Self::schedule_code_upgrade(para, new_code, expected_at);
			Self::deposit_event(Event::CodeUpgradeScheduled(para));
			Ok(())
		}

		/// Note a new block head for para within the context of the current block.
		#[pallet::weight(0)]
		pub fn force_note_new_head(origin: OriginFor<T>, para: ParaId, new_head: HeadData) -> DispatchResult {
			ensure_root(origin)?;
			let now = frame_system::Pallet::<T>::block_number();
			Self::note_new_head(para, new_head, now);
			Self::deposit_event(Event::NewHeadNoted(para));
			Ok(())
		}

		/// Put a parachain directly into the next session's action queue.
		/// We can't queue it any sooner than this without going into the
		/// initializer...
		#[pallet::weight(0)]
		pub fn force_queue_action(origin: OriginFor<T>, para: ParaId) -> DispatchResult {
			ensure_root(origin)?;
			let next_session = shared::Pallet::<T>::session_index().saturating_add(One::one());
			ActionsQueue::<T>::mutate(next_session, |v| {
				if let Err(i) = v.binary_search(&para) {
					v.insert(i, para);
				}
			});
			Self::deposit_event(Event::ActionQueued(para, next_session));
			Ok(())
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Called by the initializer to initialize the configuration pallet.
	pub(crate) fn initializer_initialize(now: T::BlockNumber) -> Weight {
		Self::prune_old_code(now)
	}

	/// Called by the initializer to finalize the configuration pallet.
	pub(crate) fn initializer_finalize() { }

	/// Called by the initializer to note that a new session has started.
	///
	/// Returns the list of outgoing paras from the actions queue.
	pub(crate) fn initializer_on_new_session(notification: &SessionChangeNotification<T::BlockNumber>) -> Vec<ParaId> {
		let outgoing_paras = Self::apply_actions_queue(notification.session_index);
		outgoing_paras
	}

	/// The validation code of live para.
	pub(crate) fn current_code(para_id: &ParaId) -> Option<ValidationCode> {
		CurrentCodeHash::<T>::get(para_id).and_then(|code_hash| {
			let code = CodeByHash::<T>::get(&code_hash);
			if code.is_none() {
				log::error!(
					"Pallet paras storage is inconsistent, code not found for hash {}",
					code_hash,
				);
				debug_assert!(false, "inconsistent paras storages");
			}
			code
		})
	}

	// Apply all para actions queued for the given session index.
	//
	// The actions to take are based on the lifecycle of of the paras.
	//
	// The final state of any para after the actions queue should be as a
	// parachain, parathread, or not registered. (stable states)
	//
	// Returns the list of outgoing paras from the actions queue.
	fn apply_actions_queue(session: SessionIndex) -> Vec<ParaId> {
		let actions = ActionsQueue::<T>::take(session);
		let mut parachains = <Self as Store>::Parachains::get();
		let now = <frame_system::Pallet<T>>::block_number();
		let mut outgoing = Vec::new();

		for para in actions {
			let lifecycle = ParaLifecycles::<T>::get(&para);
			match lifecycle {
				None | Some(ParaLifecycle::Parathread) | Some(ParaLifecycle::Parachain) => { /* Nothing to do... */ },
				// Onboard a new parathread or parachain.
				Some(ParaLifecycle::Onboarding) => {
					if let Some(genesis_data) = <Self as Store>::UpcomingParasGenesis::take(&para) {
						if genesis_data.parachain {
							if let Err(i) = parachains.binary_search(&para) {
								parachains.insert(i, para);
							}
							ParaLifecycles::<T>::insert(&para, ParaLifecycle::Parachain);
						} else {
							ParaLifecycles::<T>::insert(&para, ParaLifecycle::Parathread);
						}

						let code_hash = genesis_data.validation_code.hash();
						<Self as Store>::Heads::insert(&para, genesis_data.genesis_head);
						Self::increase_code_ref(&code_hash, &genesis_data.validation_code);
						<Self as Store>::CurrentCodeHash::insert(&para, code_hash);
					}
				},
				// Upgrade a parathread to a parachain
				Some(ParaLifecycle::UpgradingParathread) => {
					if let Err(i) = parachains.binary_search(&para) {
						parachains.insert(i, para);
					}
					ParaLifecycles::<T>::insert(&para, ParaLifecycle::Parachain);
				},
				// Downgrade a parachain to a parathread
				Some(ParaLifecycle::DowngradingParachain) => {
					if let Ok(i) = parachains.binary_search(&para) {
						parachains.remove(i);
					}
					ParaLifecycles::<T>::insert(&para, ParaLifecycle::Parathread);
				},
				// Offboard a parathread or parachain from the system
				Some(ParaLifecycle::OffboardingParachain) | Some(ParaLifecycle::OffboardingParathread) => {
					if let Ok(i) = parachains.binary_search(&para) {
						parachains.remove(i);
					}

					<Self as Store>::Heads::remove(&para);
					<Self as Store>::FutureCodeUpgrades::remove(&para);
					ParaLifecycles::<T>::remove(&para);
					let removed_future_code_hash = <Self as Store>::FutureCodeHash::take(&para);
					if let Some(removed_future_code_hash) = removed_future_code_hash {
						Self::decrease_code_ref(&removed_future_code_hash);
					}

					let removed_code_hash = <Self as Store>::CurrentCodeHash::take(&para);
					if let Some(removed_code_hash) = removed_code_hash {
						Self::note_past_code(para, now, now, removed_code_hash);
					}

					outgoing.push(para);
				},
			}
		}

		// Place the new parachains set in storage.
		<Self as Store>::Parachains::set(parachains);

		return outgoing
	}

	// note replacement of the code of para with given `id`, which occured in the
	// context of the given relay-chain block number. provide the replaced code.
	//
	// `at` for para-triggered replacement is the block number of the relay-chain
	// block in whose context the parablock was executed
	// (i.e. number of `relay_parent` in the receipt)
	fn note_past_code(
		id: ParaId,
		at: T::BlockNumber,
		now: T::BlockNumber,
		old_code_hash: ValidationCodeHash,
	) -> Weight {
		<Self as Store>::PastCodeMeta::mutate(&id, |past_meta| {
			past_meta.note_replacement(at, now);
		});

		<Self as Store>::PastCodeHash::insert(&(id, at), old_code_hash);

		// Schedule pruning for this past-code to be removed as soon as it
		// exits the slashing window.
		<Self as Store>::PastCodePruning::mutate(|pruning| {
			let insert_idx = pruning.binary_search_by_key(&at, |&(_, b)| b)
				.unwrap_or_else(|idx| idx);
			pruning.insert(insert_idx, (id, now));
		});

		T::DbWeight::get().reads_writes(2, 3)
	}

	// looks at old code metadata, compares them to the current acceptance window, and prunes those
	// that are too old.
	fn prune_old_code(now: T::BlockNumber) -> Weight {
		let config = configuration::Pallet::<T>::config();
		let code_retention_period = config.code_retention_period;
		if now <= code_retention_period {
			let weight = T::DbWeight::get().reads_writes(1, 0);
			return weight;
		}

		// The height of any changes we no longer should keep around.
		let pruning_height = now - (code_retention_period + One::one());

		let pruning_tasks_done =
			<Self as Store>::PastCodePruning::mutate(|pruning_tasks: &mut Vec<(_, T::BlockNumber)>| {
				let (pruning_tasks_done, pruning_tasks_to_do) = {
					// find all past code that has just exited the pruning window.
					let up_to_idx = pruning_tasks.iter()
						.take_while(|&(_, at)| at <= &pruning_height)
						.count();
					(up_to_idx, pruning_tasks.drain(..up_to_idx))
				};

				for (para_id, _) in pruning_tasks_to_do {
					let full_deactivate = <Self as Store>::PastCodeMeta::mutate(&para_id, |meta| {
						for pruned_repl_at in meta.prune_up_to(pruning_height) {
							let removed_code_hash =
								<Self as Store>::PastCodeHash::take(&(para_id, pruned_repl_at));

							if let Some(removed_code_hash) = removed_code_hash {
								Self::decrease_code_ref(&removed_code_hash);
							} else {
								log::warn!(
									target: "runtime::paras",
									"Missing code for removed hash {:?}",
									removed_code_hash,
								);
							}
						}

						meta.most_recent_change().is_none() && Self::para_head(&para_id).is_none()
					});

					// This parachain has been removed and now the vestigial code
					// has been removed from the state. clean up meta as well.
					if full_deactivate {
						<Self as Store>::PastCodeMeta::remove(&para_id);
					}
				}

				pruning_tasks_done as u64
			});

		// 1 read for the meta for each pruning task, 1 read for the config
		// 2 writes: updating the meta and pruning the code
		T::DbWeight::get().reads_writes(1 + pruning_tasks_done, 2 * pruning_tasks_done)
	}

	/// Verify that `schedule_para_initialize` can be called successfully.
	///
	/// Returns false if para is already registered in the system.
	pub fn can_schedule_para_initialize(id: &ParaId, _: &ParaGenesisArgs) -> bool {
		let lifecycle = ParaLifecycles::<T>::get(id);
		lifecycle.is_none()
	}

	/// Schedule a para to be initialized at the start of the next session.
	///
	/// Will return error if para is already registered in the system.
	pub(crate) fn schedule_para_initialize(id: ParaId, genesis: ParaGenesisArgs) -> DispatchResult {
		let scheduled_session = Self::scheduled_session();

		// Make sure parachain isn't already in our system.
		ensure!(Self::can_schedule_para_initialize(&id, &genesis), Error::<T>::CannotOnboard);

		ParaLifecycles::<T>::insert(&id, ParaLifecycle::Onboarding);
		UpcomingParasGenesis::<T>::insert(&id, genesis);
		ActionsQueue::<T>::mutate(scheduled_session, |v| {
			if let Err(i) = v.binary_search(&id) {
				v.insert(i, id);
			}
		});

		Ok(())
	}

	/// Schedule a para to be cleaned up at the start of the next session.
	///
	/// Will return error if para is not a stable parachain or parathread.
	///
	/// No-op if para is not registered at all.
	pub(crate) fn schedule_para_cleanup(id: ParaId) -> DispatchResult {
		let lifecycle = ParaLifecycles::<T>::get(&id);
		match lifecycle {
			// If para is not registered, nothing to do!
			None => {
				return Ok(())
			},
			Some(ParaLifecycle::Parathread) => {
				ParaLifecycles::<T>::insert(&id, ParaLifecycle::OffboardingParathread);
			},
			Some(ParaLifecycle::Parachain) => {
				ParaLifecycles::<T>::insert(&id, ParaLifecycle::OffboardingParachain);
			},
			_ => return Err(Error::<T>::CannotOffboard)?,
		}

		let scheduled_session = Self::scheduled_session();
		ActionsQueue::<T>::mutate(scheduled_session, |v| {
			if let Err(i) = v.binary_search(&id) {
				v.insert(i, id);
			}
		});

		Ok(())
	}

	/// Schedule a parathread to be upgraded to a parachain.
	///
	/// Will return error if `ParaLifecycle` is not `Parathread`.
	pub(crate) fn schedule_parathread_upgrade(id: ParaId) -> DispatchResult {
		let scheduled_session = Self::scheduled_session();
		let lifecycle = ParaLifecycles::<T>::get(&id).ok_or(Error::<T>::NotRegistered)?;

		ensure!(lifecycle == ParaLifecycle::Parathread, Error::<T>::CannotUpgrade);

		ParaLifecycles::<T>::insert(&id, ParaLifecycle::UpgradingParathread);
		ActionsQueue::<T>::mutate(scheduled_session, |v| {
			if let Err(i) = v.binary_search(&id) {
				v.insert(i, id);
			}
		});

		Ok(())
	}

	/// Schedule a parachain to be downgraded to a parathread.
	///
	/// Noop if `ParaLifecycle` is not `Parachain`.
	pub(crate) fn schedule_parachain_downgrade(id: ParaId) -> DispatchResult {
		let scheduled_session = Self::scheduled_session();
		let lifecycle = ParaLifecycles::<T>::get(&id).ok_or(Error::<T>::NotRegistered)?;

		ensure!(lifecycle == ParaLifecycle::Parachain, Error::<T>::CannotDowngrade);

		ParaLifecycles::<T>::insert(&id, ParaLifecycle::DowngradingParachain);
		ActionsQueue::<T>::mutate(scheduled_session, |v| {
			if let Err(i) = v.binary_search(&id) {
				v.insert(i, id);
			}
		});

		Ok(())
	}

	/// Schedule a future code upgrade of the given parachain, to be applied after inclusion
	/// of a block of the same parachain executed in the context of a relay-chain block
	/// with number >= `expected_at`
	///
	/// If there is already a scheduled code upgrade for the para, this is a no-op.
	pub(crate) fn schedule_code_upgrade(
		id: ParaId,
		new_code: ValidationCode,
		expected_at: T::BlockNumber,
	) -> Weight {
		<Self as Store>::FutureCodeUpgrades::mutate(&id, |up| {
			if up.is_some() {
				T::DbWeight::get().reads_writes(1, 0)
			} else {
				*up = Some(expected_at);

				let new_code_hash = new_code.hash();
				let expected_at_u32 = expected_at.saturated_into();
				let log = ConsensusLog::ParaScheduleUpgradeCode(id, new_code_hash, expected_at_u32);
				<frame_system::Pallet<T>>::deposit_log(log.into());

				let (reads, writes) = Self::increase_code_ref(&new_code_hash, &new_code);
				FutureCodeHash::<T>::insert(&id, new_code_hash);
				T::DbWeight::get().reads_writes(1 + reads, 2 + writes)
			}
		})
	}

	/// Note that a para has progressed to a new head, where the new head was executed in the context
	/// of a relay-chain block with given number. This will apply pending code upgrades based
	/// on the block number provided.
	pub(crate) fn note_new_head(
		id: ParaId,
		new_head: HeadData,
		execution_context: T::BlockNumber,
	) -> Weight {
		Heads::<T>::insert(&id, new_head);

		if let Some(expected_at) = <Self as Store>::FutureCodeUpgrades::get(&id) {
			if expected_at <= execution_context {
				<Self as Store>::FutureCodeUpgrades::remove(&id);

				// Both should always be `Some` in this case, since a code upgrade is scheduled.
				let new_code_hash = FutureCodeHash::<T>::take(&id).unwrap_or_default();
				let prior_code_hash = CurrentCodeHash::<T>::get(&id).unwrap_or_default();
				CurrentCodeHash::<T>::insert(&id, &new_code_hash);

				let log = ConsensusLog::ParaUpgradeCode(id, new_code_hash);
				<frame_system::Pallet<T>>::deposit_log(log.into());

				// `now` is only used for registering pruning as part of `fn note_past_code`
				let now = <frame_system::Pallet<T>>::block_number();

				let weight = Self::note_past_code(
					id,
					expected_at,
					now,
					prior_code_hash,
				);

				// add 1 to writes due to heads update.
				weight + T::DbWeight::get().reads_writes(3, 1 + 3)
			} else {
				T::DbWeight::get().reads_writes(1, 1 + 0)
			}
		} else {
			T::DbWeight::get().reads_writes(1, 1)
		}
	}

	/// Fetches the validation code hash for the validation code to be used when validating a block
	/// in the context of the given relay-chain height. A second block number parameter may be used
	/// to tell the lookup to proceed as if an intermediate parablock has been with the given
	/// relay-chain height as its context. This may return the hash for the past, current, or
	/// (with certain choices of `assume_intermediate`) future code.
	///
	/// `assume_intermediate`, if provided, must be before `at`. This will return `None` if the validation
	/// code has been pruned.
	///
	/// To get associated code see [`Self::validation_code_at`].
	pub(crate) fn validation_code_hash_at(
		id: ParaId,
		at: T::BlockNumber,
		assume_intermediate: Option<T::BlockNumber>,
	) -> Option<ValidationCodeHash> {
		if assume_intermediate.as_ref().map_or(false, |i| &at <= i) {
			return None;
		}

		let planned_upgrade = <Self as Store>::FutureCodeUpgrades::get(&id);
		let upgrade_applied_intermediate = match assume_intermediate {
			Some(a) => planned_upgrade.as_ref().map_or(false, |u| u <= &a),
			None => false,
		};

		if upgrade_applied_intermediate {
			FutureCodeHash::<T>::get(&id)
		} else {
			match Self::past_code_meta(&id).code_at(at) {
				None => None,
				Some(UseCodeAt::Current) => CurrentCodeHash::<T>::get(&id),
				Some(UseCodeAt::ReplacedAt(replaced)) => <Self as Store>::PastCodeHash::get(&(id, replaced)),
			}
		}
	}

	/// Returns the current lifecycle state of the para.
	pub fn lifecycle(id: ParaId) -> Option<ParaLifecycle> {
		ParaLifecycles::<T>::get(&id)
	}

	/// Returns whether the given ID refers to a valid para.
	///
	/// Paras that are onboarding or offboarding are not included.
	pub fn is_valid_para(id: ParaId) -> bool {
		if let Some(state) = ParaLifecycles::<T>::get(&id) {
			!state.is_onboarding() && !state.is_offboarding()
		} else {
			false
		}
	}

	/// Whether a para ID corresponds to any live parachain.
	///
	/// Includes parachains which will downgrade to a parathread in the future.
	pub fn is_parachain(id: ParaId) -> bool {
		if let Some(state) = ParaLifecycles::<T>::get(&id) {
			state.is_parachain()
		} else {
			false
		}
	}

	/// Whether a para ID corresponds to any live parathread.
	///
	/// Includes parathreads which will upgrade to parachains in the future.
	pub fn is_parathread(id: ParaId) -> bool {
		if let Some(state) = ParaLifecycles::<T>::get(&id) {
			state.is_parathread()
		} else {
			false
		}
	}

	/// The block number of the last scheduled upgrade of the requested para. Includes future upgrades
	/// if the flag is set. This is the `expected_at` number, not the `activated_at` number.
	pub(crate) fn last_code_upgrade(id: ParaId, include_future: bool) -> Option<T::BlockNumber> {
		if include_future {
			if let Some(at) = Self::future_code_upgrade_at(id) {
				return Some(at);
			}
		}

		Self::past_code_meta(&id).most_recent_change()
	}

	/// Return the session index that should be used for any future scheduled changes.
	fn scheduled_session() -> SessionIndex {
		shared::Pallet::<T>::scheduled_session()
	}

	/// Store the validation code if not already stored, and increase the number of reference.
	///
	/// Returns the number of storage reads and number of storage writes.
	fn increase_code_ref(code_hash: &ValidationCodeHash, code: &ValidationCode) -> (u64, u64) {
		let reads = 1;
		let mut writes = 1;
		<Self as Store>::CodeByHashRefs::mutate(code_hash, |refs| {
			if *refs == 0 {
				writes += 1;
				<Self as Store>::CodeByHash::insert(code_hash, code);
			}
			*refs += 1;
		});
		(reads, writes)
	}

	/// Decrease the number of reference ofthe validation code and remove it from storage if zero
	/// is reached.
	fn decrease_code_ref(code_hash: &ValidationCodeHash) {
		let refs = <Self as Store>::CodeByHashRefs::get(code_hash);
		if refs <= 1 {
			<Self as Store>::CodeByHash::remove(code_hash);
			<Self as Store>::CodeByHashRefs::remove(code_hash);
		} else {
			<Self as Store>::CodeByHashRefs::insert(code_hash, refs - 1);
		}
	}

	/// Test function for triggering a new session in this pallet.
	#[cfg(any(feature = "std", feature = "runtime-benchmarks", test))]
	pub fn test_on_new_session() {
		Self::initializer_on_new_session(&SessionChangeNotification {
			session_index: shared::Pallet::<T>::session_index(),
			..Default::default()
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use primitives::v1::BlockNumber;
	use frame_support::assert_ok;

	use crate::mock::{new_test_ext, Paras, ParasShared, System, MockGenesisConfig};
	use crate::configuration::HostConfiguration;

	fn run_to_block(to: BlockNumber, new_session: Option<Vec<BlockNumber>>) {
		while System::block_number() < to {
			let b = System::block_number();
			Paras::initializer_finalize();
			ParasShared::initializer_finalize();
			if new_session.as_ref().map_or(false, |v| v.contains(&(b + 1))) {
				let mut session_change_notification = SessionChangeNotification::default();
				session_change_notification.session_index = ParasShared::session_index() + 1;
				ParasShared::initializer_on_new_session(
					session_change_notification.session_index,
					session_change_notification.random_seed,
					&session_change_notification.new_config,
					session_change_notification.validators.clone(),
				);
				Paras::initializer_on_new_session(&session_change_notification);
			}
			System::on_finalize(b);

			System::on_initialize(b + 1);
			System::set_block_number(b + 1);

			ParasShared::initializer_initialize(b + 1);
			Paras::initializer_initialize(b + 1);
		}
	}

	fn upgrade_at(expected_at: BlockNumber, activated_at: BlockNumber) -> ReplacementTimes<BlockNumber> {
		ReplacementTimes { expected_at, activated_at }
	}

	fn check_code_is_stored(validation_code: &ValidationCode) {
		assert!(<Paras as Store>::CodeByHashRefs::get(validation_code.hash()) != 0);
		assert!(<Paras as Store>::CodeByHash::contains_key(validation_code.hash()));
	}

	fn check_code_is_not_stored(validation_code: &ValidationCode) {
		assert!(!<Paras as Store>::CodeByHashRefs::contains_key(validation_code.hash()));
		assert!(!<Paras as Store>::CodeByHash::contains_key(validation_code.hash()));
	}

	fn fetch_validation_code_at(
		para_id: ParaId,
		at: BlockNumber,
		assume_intermediate: Option<BlockNumber>,
	) -> Option<ValidationCode> {
		Paras::validation_code_hash_at(para_id, at, assume_intermediate)
			.and_then(Paras::code_by_hash)
	}

	#[test]
	fn para_past_code_meta_gives_right_code() {
		let mut past_code = ParaPastCodeMeta::default();
		assert_eq!(past_code.code_at(0u32), Some(UseCodeAt::Current));

		past_code.note_replacement(10, 12);
		assert_eq!(past_code.code_at(0), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(10), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(11), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(12), Some(UseCodeAt::Current));

		past_code.note_replacement(20, 25);
		assert_eq!(past_code.code_at(1), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(10), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(11), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(12), Some(UseCodeAt::ReplacedAt(20)));
		assert_eq!(past_code.code_at(24), Some(UseCodeAt::ReplacedAt(20)));
		assert_eq!(past_code.code_at(25), Some(UseCodeAt::Current));

		past_code.note_replacement(30, 30);
		assert_eq!(past_code.code_at(1), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(10), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(11), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(12), Some(UseCodeAt::ReplacedAt(20)));
		assert_eq!(past_code.code_at(24), Some(UseCodeAt::ReplacedAt(20)));
		assert_eq!(past_code.code_at(25), Some(UseCodeAt::ReplacedAt(30)));
		assert_eq!(past_code.code_at(30), Some(UseCodeAt::Current));

		past_code.last_pruned = Some(5);
		assert_eq!(past_code.code_at(1), None);
		assert_eq!(past_code.code_at(5), None);
		assert_eq!(past_code.code_at(6), Some(UseCodeAt::ReplacedAt(10)));
		assert_eq!(past_code.code_at(24), Some(UseCodeAt::ReplacedAt(20)));
		assert_eq!(past_code.code_at(25), Some(UseCodeAt::ReplacedAt(30)));
		assert_eq!(past_code.code_at(30), Some(UseCodeAt::Current));

	}

	#[test]
	fn para_past_code_pruning_works_correctly() {
		let mut past_code = ParaPastCodeMeta::default();
		past_code.note_replacement(10u32, 10);
		past_code.note_replacement(20, 25);
		past_code.note_replacement(30, 35);

		let old = past_code.clone();
		assert!(past_code.prune_up_to(9).collect::<Vec<_>>().is_empty());
		assert_eq!(old, past_code);

		assert_eq!(past_code.prune_up_to(10).collect::<Vec<_>>(), vec![10]);
		assert_eq!(past_code, ParaPastCodeMeta {
			upgrade_times: vec![upgrade_at(20, 25), upgrade_at(30, 35)],
			last_pruned: Some(10),
		});

		assert!(past_code.prune_up_to(21).collect::<Vec<_>>().is_empty());

		assert_eq!(past_code.prune_up_to(26).collect::<Vec<_>>(), vec![20]);
		assert_eq!(past_code, ParaPastCodeMeta {
			upgrade_times: vec![upgrade_at(30, 35)],
			last_pruned: Some(25),
		});

		past_code.note_replacement(40, 42);
		past_code.note_replacement(50, 53);
		past_code.note_replacement(60, 66);

		assert_eq!(past_code, ParaPastCodeMeta {
			upgrade_times: vec![upgrade_at(30, 35), upgrade_at(40, 42), upgrade_at(50, 53), upgrade_at(60, 66)],
			last_pruned: Some(25),
		});

		assert_eq!(past_code.prune_up_to(60).collect::<Vec<_>>(), vec![30, 40, 50]);
		assert_eq!(past_code, ParaPastCodeMeta {
			upgrade_times: vec![upgrade_at(60, 66)],
			last_pruned: Some(53),
		});

		assert_eq!(past_code.most_recent_change(), Some(60));
		assert_eq!(past_code.prune_up_to(66).collect::<Vec<_>>(), vec![60]);

		assert_eq!(past_code, ParaPastCodeMeta {
			upgrade_times: Vec::new(),
			last_pruned: Some(66),
		});
	}

	#[test]
	fn para_past_code_pruning_in_initialize() {
		let code_retention_period = 10;
		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: Default::default(),
			}),
			(1u32.into(), ParaGenesisArgs {
				parachain: false,
				genesis_head: Default::default(),
				validation_code: Default::default(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					code_retention_period,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let id = ParaId::from(0u32);
			let at_block: BlockNumber = 10;
			let included_block: BlockNumber = 12;
			let validation_code = ValidationCode(vec![1, 2, 3]);

			Paras::increase_code_ref(&validation_code.hash(), &validation_code);
			<Paras as Store>::PastCodeHash::insert(&(id, at_block), &validation_code.hash());
			<Paras as Store>::PastCodePruning::put(&vec![(id, included_block)]);

			{
				let mut code_meta = Paras::past_code_meta(&id);
				code_meta.note_replacement(at_block, included_block);
				<Paras as Store>::PastCodeMeta::insert(&id, &code_meta);
			}

			let pruned_at: BlockNumber = included_block + code_retention_period + 1;
			assert_eq!(<Paras as Store>::PastCodeHash::get(&(id, at_block)), Some(validation_code.hash()));
			check_code_is_stored(&validation_code);

			run_to_block(pruned_at - 1, None);
			assert_eq!(<Paras as Store>::PastCodeHash::get(&(id, at_block)), Some(validation_code.hash()));
			assert_eq!(Paras::past_code_meta(&id).most_recent_change(), Some(at_block));
			check_code_is_stored(&validation_code);

			run_to_block(pruned_at, None);
			assert!(<Paras as Store>::PastCodeHash::get(&(id, at_block)).is_none());
			assert!(Paras::past_code_meta(&id).most_recent_change().is_none());
			check_code_is_not_stored(&validation_code);
		});
	}

	#[test]
	fn note_new_head_sets_head() {
		let code_retention_period = 10;
		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: Default::default(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					code_retention_period,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let id_a = ParaId::from(0u32);

			assert_eq!(Paras::para_head(&id_a), Some(Default::default()));

			Paras::note_new_head(id_a, vec![1, 2, 3].into(), 0);

			assert_eq!(Paras::para_head(&id_a), Some(vec![1, 2, 3].into()));
		});
	}

	#[test]
	fn note_past_code_sets_up_pruning_correctly() {
		let code_retention_period = 10;
		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: Default::default(),
			}),
			(1u32.into(), ParaGenesisArgs {
				parachain: false,
				genesis_head: Default::default(),
				validation_code: Default::default(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					code_retention_period,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let id_a = ParaId::from(0u32);
			let id_b = ParaId::from(1u32);

			Paras::note_past_code(id_a, 10, 12, ValidationCode(vec![1, 2, 3]).hash());
			Paras::note_past_code(id_b, 20, 23, ValidationCode(vec![4, 5, 6]).hash());

			assert_eq!(<Paras as Store>::PastCodePruning::get(), vec![(id_a, 12), (id_b, 23)]);
			assert_eq!(
				Paras::past_code_meta(&id_a),
				ParaPastCodeMeta {
					upgrade_times: vec![upgrade_at(10, 12)],
					last_pruned: None,
				}
			);
			assert_eq!(
				Paras::past_code_meta(&id_b),
				ParaPastCodeMeta {
					upgrade_times: vec![upgrade_at(20, 23)],
					last_pruned: None,
				}
			);
		});
	}

	#[test]
	fn code_upgrade_applied_after_delay() {
		let code_retention_period = 10;
		let validation_upgrade_delay = 5;

		let original_code = ValidationCode(vec![1, 2, 3]);
		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: original_code.clone(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					code_retention_period,
					validation_upgrade_delay,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			check_code_is_stored(&original_code);

			let para_id = ParaId::from(0);
			let new_code = ValidationCode(vec![4, 5, 6]);

			run_to_block(2, None);
			assert_eq!(Paras::current_code(&para_id), Some(original_code.clone()));

			let expected_at = {
				// this parablock is in the context of block 1.
				let expected_at = 1 + validation_upgrade_delay;
				Paras::schedule_code_upgrade(para_id, new_code.clone(), expected_at);
				Paras::note_new_head(para_id, Default::default(), 1);

				assert!(Paras::past_code_meta(&para_id).most_recent_change().is_none());
				assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(expected_at));
				assert_eq!(<Paras as Store>::FutureCodeHash::get(&para_id), Some(new_code.hash()));
				assert_eq!(Paras::current_code(&para_id), Some(original_code.clone()));
				check_code_is_stored(&original_code);
				check_code_is_stored(&new_code);

				expected_at
			};

			run_to_block(expected_at, None);

			// the candidate is in the context of the parent of `expected_at`,
			// thus does not trigger the code upgrade.
			{
				Paras::note_new_head(para_id, Default::default(), expected_at - 1);

				assert!(Paras::past_code_meta(&para_id).most_recent_change().is_none());
				assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(expected_at));
				assert_eq!(<Paras as Store>::FutureCodeHash::get(&para_id), Some(new_code.hash()));
				assert_eq!(Paras::current_code(&para_id), Some(original_code.clone()));
				check_code_is_stored(&original_code);
				check_code_is_stored(&new_code);
			}

			run_to_block(expected_at + 1, None);

			// the candidate is in the context of `expected_at`, and triggers
			// the upgrade.
			{
				Paras::note_new_head(para_id, Default::default(), expected_at);

				assert_eq!(
					Paras::past_code_meta(&para_id).most_recent_change(),
					Some(expected_at),
				);
				assert_eq!(
					<Paras as Store>::PastCodeHash::get(&(para_id, expected_at)),
					Some(original_code.hash()),
				);
				assert!(<Paras as Store>::FutureCodeUpgrades::get(&para_id).is_none());
				assert!(<Paras as Store>::FutureCodeHash::get(&para_id).is_none());
				assert_eq!(Paras::current_code(&para_id), Some(new_code.clone()));
				check_code_is_stored(&original_code);
				check_code_is_stored(&new_code);
			}
		});
	}

	#[test]
	fn code_upgrade_applied_after_delay_even_when_late() {
		let code_retention_period = 10;
		let validation_upgrade_delay = 5;

		let original_code = ValidationCode(vec![1, 2, 3]);
		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: original_code.clone(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					code_retention_period,
					validation_upgrade_delay,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let para_id = ParaId::from(0);
			let new_code = ValidationCode(vec![4, 5, 6]);

			run_to_block(2, None);
			assert_eq!(Paras::current_code(&para_id), Some(original_code.clone()));

			let expected_at = {
				// this parablock is in the context of block 1.
				let expected_at = 1 + validation_upgrade_delay;
				Paras::schedule_code_upgrade(para_id, new_code.clone(), expected_at);
				Paras::note_new_head(para_id, Default::default(), 1);

				assert!(Paras::past_code_meta(&para_id).most_recent_change().is_none());
				assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(expected_at));
				assert_eq!(<Paras as Store>::FutureCodeHash::get(&para_id), Some(new_code.hash()));
				assert_eq!(Paras::current_code(&para_id), Some(original_code.clone()));

				expected_at
			};

			run_to_block(expected_at + 1 + 4, None);

			// the candidate is in the context of the first descendant of `expected_at`, and triggers
			// the upgrade.
			{
				Paras::note_new_head(para_id, Default::default(), expected_at + 4);

				assert_eq!(
					Paras::past_code_meta(&para_id).most_recent_change(),
					Some(expected_at),
				);

				// Some hypothetical block which would have triggered the code change
				// should still use the old code.
				assert_eq!(
					Paras::past_code_meta(&para_id).code_at(expected_at),
					Some(UseCodeAt::ReplacedAt(expected_at)),
				);

				// Some hypothetical block at the context which actually triggered the
				// code change should still use the old code.
				assert_eq!(
					Paras::past_code_meta(&para_id).code_at(expected_at + 4),
					Some(UseCodeAt::ReplacedAt(expected_at)),
				);

				// Some hypothetical block at the context after the code was upgraded
				// should use the new code.
				assert_eq!(
					Paras::past_code_meta(&para_id).code_at(expected_at + 4 + 1),
					Some(UseCodeAt::Current),
				);

				assert_eq!(
					<Paras as Store>::PastCodeHash::get(&(para_id, expected_at)),
					Some(original_code.hash()),
				);
				assert!(<Paras as Store>::FutureCodeUpgrades::get(&para_id).is_none());
				assert!(<Paras as Store>::FutureCodeHash::get(&para_id).is_none());
				assert_eq!(Paras::current_code(&para_id), Some(new_code.clone()));
			}
		});
	}

	#[test]
	fn submit_code_change_when_not_allowed_is_err() {
		let code_retention_period = 10;

		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: vec![1, 2, 3].into(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					code_retention_period,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let para_id = ParaId::from(0);
			let new_code = ValidationCode(vec![4, 5, 6]);
			let newer_code = ValidationCode(vec![4, 5, 6, 7]);

			run_to_block(1, None);

			Paras::schedule_code_upgrade(para_id, new_code.clone(), 8);
			assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(8));
			assert_eq!(<Paras as Store>::FutureCodeHash::get(&para_id), Some(new_code.hash()));
			check_code_is_stored(&new_code);

			Paras::schedule_code_upgrade(para_id, newer_code.clone(), 10);
			assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(8));
			assert_eq!(<Paras as Store>::FutureCodeHash::get(&para_id), Some(new_code.hash()));
			check_code_is_not_stored(&newer_code);
		});
	}

	#[test]
	fn full_parachain_cleanup_storage() {
		let code_retention_period = 10;

		let original_code = ValidationCode(vec![1, 2, 3]);
		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: original_code.clone(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					code_retention_period,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			check_code_is_stored(&original_code);

			let para_id = ParaId::from(0);
			let new_code = ValidationCode(vec![4, 5, 6]);

			run_to_block(2, None);
			assert_eq!(Paras::current_code(&para_id), Some(original_code.clone()));
			check_code_is_stored(&original_code);

			let expected_at = {
				// this parablock is in the context of block 1.
				let expected_at = 1 + 5;
				Paras::schedule_code_upgrade(para_id, new_code.clone(), expected_at);
				Paras::note_new_head(para_id, Default::default(), 1);

				assert!(Paras::past_code_meta(&para_id).most_recent_change().is_none());
				assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(expected_at));
				assert_eq!(<Paras as Store>::FutureCodeHash::get(&para_id), Some(new_code.hash()));
				assert_eq!(Paras::current_code(&para_id), Some(original_code.clone()));
				check_code_is_stored(&original_code);
				check_code_is_stored(&new_code);

				expected_at
			};

			assert_ok!(Paras::schedule_para_cleanup(para_id));

			// Just scheduling cleanup shouldn't change anything.
			{
				assert_eq!(
					<Paras as Store>::ActionsQueue::get(Paras::scheduled_session()),
					vec![para_id],
				);
				assert_eq!(Paras::parachains(), vec![para_id]);

				assert!(Paras::past_code_meta(&para_id).most_recent_change().is_none());
				assert_eq!(<Paras as Store>::FutureCodeUpgrades::get(&para_id), Some(expected_at));
				assert_eq!(<Paras as Store>::FutureCodeHash::get(&para_id), Some(new_code.hash()));
				assert_eq!(Paras::current_code(&para_id), Some(original_code.clone()));
				check_code_is_stored(&original_code);
				check_code_is_stored(&new_code);

				assert_eq!(<Paras as Store>::Heads::get(&para_id), Some(Default::default()));
			}

			// run to block #4, with a 2 session changes at the end of the block 2 & 3.
			run_to_block(4, Some(vec![3,4]));

			// cleaning up the parachain should place the current parachain code
			// into the past code buffer & schedule cleanup.
			assert_eq!(Paras::past_code_meta(&para_id).most_recent_change(), Some(3));
			assert_eq!(<Paras as Store>::PastCodeHash::get(&(para_id, 3)), Some(original_code.hash()));
			assert_eq!(<Paras as Store>::PastCodePruning::get(), vec![(para_id, 3)]);
			check_code_is_stored(&original_code);

			// any future upgrades haven't been used to validate yet, so those
			// are cleaned up immediately.
			assert!(<Paras as Store>::FutureCodeUpgrades::get(&para_id).is_none());
			assert!(<Paras as Store>::FutureCodeHash::get(&para_id).is_none());
			assert!(Paras::current_code(&para_id).is_none());
			check_code_is_not_stored(&new_code);

			// run to do the final cleanup
			let cleaned_up_at = 3 + code_retention_period + 1;
			run_to_block(cleaned_up_at, None);

			// now the final cleanup: last past code cleaned up, and this triggers meta cleanup.
			assert_eq!(Paras::past_code_meta(&para_id), Default::default());
			assert!(<Paras as Store>::PastCodeHash::get(&(para_id, 3)).is_none());
			assert!(<Paras as Store>::PastCodePruning::get().is_empty());
			check_code_is_not_stored(&original_code);
		});
	}

	#[test]
	fn para_incoming_at_session() {
		new_test_ext(Default::default()).execute_with(|| {
			run_to_block(1, None);

			let b = ParaId::from(525);
			let a = ParaId::from(999);
			let c = ParaId::from(333);

			assert_ok!(Paras::schedule_para_initialize(
				b,
				ParaGenesisArgs {
					parachain: true,
					genesis_head: vec![1].into(),
					validation_code: vec![1].into(),
				},
			));

			assert_ok!(Paras::schedule_para_initialize(
				a,
				ParaGenesisArgs {
					parachain: false,
					genesis_head: vec![2].into(),
					validation_code: vec![2].into(),
				},
			));

			assert_ok!(Paras::schedule_para_initialize(
				c,
				ParaGenesisArgs {
					parachain: true,
					genesis_head: vec![3].into(),
					validation_code: vec![3].into(),
				},
			));

			assert_eq!(
				<Paras as Store>::ActionsQueue::get(Paras::scheduled_session()),
				vec![c, b, a],
			);

			// Lifecycle is tracked correctly
			assert_eq!(<Paras as Store>::ParaLifecycles::get(&a), Some(ParaLifecycle::Onboarding));
			assert_eq!(<Paras as Store>::ParaLifecycles::get(&b), Some(ParaLifecycle::Onboarding));
			assert_eq!(<Paras as Store>::ParaLifecycles::get(&c), Some(ParaLifecycle::Onboarding));

			// run to block without session change.
			run_to_block(2, None);

			assert_eq!(Paras::parachains(), Vec::new());
			assert_eq!(
				<Paras as Store>::ActionsQueue::get(Paras::scheduled_session()),
				vec![c, b, a],
			);

			// Lifecycle is tracked correctly
			assert_eq!(<Paras as Store>::ParaLifecycles::get(&a), Some(ParaLifecycle::Onboarding));
			assert_eq!(<Paras as Store>::ParaLifecycles::get(&b), Some(ParaLifecycle::Onboarding));
			assert_eq!(<Paras as Store>::ParaLifecycles::get(&c), Some(ParaLifecycle::Onboarding));


			// Two sessions pass, so action queue is triggered
			run_to_block(4, Some(vec![3,4]));

			assert_eq!(Paras::parachains(), vec![c, b]);
			assert_eq!(<Paras as Store>::ActionsQueue::get(Paras::scheduled_session()), Vec::new());

			// Lifecycle is tracked correctly
			assert_eq!(<Paras as Store>::ParaLifecycles::get(&a), Some(ParaLifecycle::Parathread));
			assert_eq!(<Paras as Store>::ParaLifecycles::get(&b), Some(ParaLifecycle::Parachain));
			assert_eq!(<Paras as Store>::ParaLifecycles::get(&c), Some(ParaLifecycle::Parachain));

			assert_eq!(Paras::current_code(&a), Some(vec![2].into()));
			assert_eq!(Paras::current_code(&b), Some(vec![1].into()));
			assert_eq!(Paras::current_code(&c), Some(vec![3].into()));
		})
	}

	#[test]
	fn code_hash_at_with_intermediate() {
		let code_retention_period = 10;

		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: vec![1, 2, 3].into(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					code_retention_period,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let para_id = ParaId::from(0);
			let old_code: ValidationCode = vec![1, 2, 3].into();
			let new_code: ValidationCode = vec![4, 5, 6].into();
			Paras::schedule_code_upgrade(para_id, new_code.clone(), 10);

			// no intermediate, falls back on current/past.
			assert_eq!(fetch_validation_code_at(para_id, 1, None), Some(old_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 10, None), Some(old_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 100, None), Some(old_code.clone()));

			// intermediate before upgrade meant to be applied, falls back on current.
			assert_eq!(fetch_validation_code_at(para_id, 9, Some(8)), Some(old_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 10, Some(9)), Some(old_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 11, Some(9)), Some(old_code.clone()));

			// intermediate at or after upgrade applied
			assert_eq!(fetch_validation_code_at(para_id, 11, Some(10)), Some(new_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 100, Some(11)), Some(new_code.clone()));

			run_to_block(code_retention_period + 5, None);

			// at <= intermediate not allowed
			assert_eq!(fetch_validation_code_at(para_id, 10, Some(10)), None);
			assert_eq!(fetch_validation_code_at(para_id, 9, Some(10)), None);
		});
	}

	#[test]
	fn code_hash_at_returns_up_to_end_of_code_retention_period() {
		let code_retention_period = 10;

		let paras = vec![
			(0u32.into(), ParaGenesisArgs {
				parachain: true,
				genesis_head: Default::default(),
				validation_code: vec![1, 2, 3].into(),
			}),
		];

		let genesis_config = MockGenesisConfig {
			paras: GenesisConfig { paras, ..Default::default() },
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					code_retention_period,
					..Default::default()
				},
				..Default::default()
			},
			..Default::default()
		};

		new_test_ext(genesis_config).execute_with(|| {
			let para_id = ParaId::from(0);
			let old_code: ValidationCode = vec![1, 2, 3].into();
			let new_code: ValidationCode = vec![4, 5, 6].into();
			Paras::schedule_code_upgrade(para_id, new_code.clone(), 2);

			run_to_block(10, None);
			Paras::note_new_head(para_id, Default::default(), 7);

			assert_eq!(
				Paras::past_code_meta(&para_id).upgrade_times,
				vec![upgrade_at(2, 10)],
			);

			assert_eq!(fetch_validation_code_at(para_id, 2, None), Some(old_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 3, None), Some(old_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 9, None), Some(old_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 10, None), Some(new_code.clone()));

			run_to_block(10 + code_retention_period, None);

			assert_eq!(fetch_validation_code_at(para_id, 2, None), Some(old_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 3, None), Some(old_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 9, None), Some(old_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 10, None), Some(new_code.clone()));

			run_to_block(10 + code_retention_period + 1, None);

			// code entry should be pruned now.

			assert_eq!(
				Paras::past_code_meta(&para_id),
				ParaPastCodeMeta {
					upgrade_times: Vec::new(),
					last_pruned: Some(10),
				},
			);

			assert_eq!(fetch_validation_code_at(para_id, 2, None), None); // pruned :(
			assert_eq!(fetch_validation_code_at(para_id, 9, None), None);
			assert_eq!(fetch_validation_code_at(para_id, 10, None), Some(new_code.clone()));
			assert_eq!(fetch_validation_code_at(para_id, 11, None), Some(new_code.clone()));
		});
	}

	#[test]
	fn code_ref_is_cleaned_correctly() {
		new_test_ext(Default::default()).execute_with(|| {
			let code: ValidationCode = vec![1, 2, 3].into();
			Paras::increase_code_ref(&code.hash(), &code);
			Paras::increase_code_ref(&code.hash(), &code);

			assert!(<Paras as Store>::CodeByHash::contains_key(code.hash()));
			assert_eq!(<Paras as Store>::CodeByHashRefs::get(code.hash()), 2);

			Paras::decrease_code_ref(&code.hash());

			assert!(<Paras as Store>::CodeByHash::contains_key(code.hash()));
			assert_eq!(<Paras as Store>::CodeByHashRefs::get(code.hash()), 1);

			Paras::decrease_code_ref(&code.hash());

			assert!(!<Paras as Store>::CodeByHash::contains_key(code.hash()));
			assert!(!<Paras as Store>::CodeByHashRefs::contains_key(code.hash()));
		});
	}
}
