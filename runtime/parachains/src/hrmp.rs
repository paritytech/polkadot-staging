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

use crate::{
	ensure_parachain,
	configuration::{self, HostConfiguration},
	initializer, paras, dmp,
};
use parity_scale_codec::{Decode, Encode};
use frame_support::{
	decl_storage, decl_module, decl_error, decl_event, ensure, traits::{Get, ReservableCurrency},
	weights::Weight, StorageMap, StorageValue, dispatch::DispatchResult,
};
use frame_system::ensure_root;
use primitives::v1::{
	Balance, Hash, HrmpChannelId, Id as ParaId, InboundHrmpMessage, OutboundHrmpMessage,
	SessionIndex,
};
use sp_runtime::traits::{UniqueSaturatedInto, AccountIdConversion, BlakeTwo256, Hash as HashT};
use sp_std::{
	mem, fmt,
	collections::{btree_map::BTreeMap, btree_set::BTreeSet},
	prelude::*,
};

/// A description of a request to open an HRMP channel.
#[derive(Encode, Decode)]
pub struct HrmpOpenChannelRequest {
	/// Indicates if this request was confirmed by the recipient.
	pub confirmed: bool,
	/// How many session boundaries ago this request was seen.
	pub age: SessionIndex,
	/// The amount that the sender supplied at the time of creation of this request.
	pub sender_deposit: Balance,
	/// The maximum message size that could be put into the channel.
	pub max_message_size: u32,
	/// The maximum number of messages that can be pending in the channel at once.
	pub max_capacity: u32,
	/// The maximum total size of the messages that can be pending in the channel at once.
	pub max_total_size: u32,
}

/// A metadata of an HRMP channel.
#[derive(Encode, Decode)]
#[cfg_attr(test, derive(Debug))]
pub struct HrmpChannel {
	// NOTE: This structure is used by parachains via merkle proofs. Therefore, this struct requires
	// special treatment.
	//
	// A parachain requested this struct can only depend on the subset of this struct. Specifically,
	// only a first few fields can be depended upon (See `AbridgedHrmpChannel`). These fields cannot
	// be changed without corresponding migration of parachains.

	/// The maximum number of messages that can be pending in the channel at once.
	pub max_capacity: u32,
	/// The maximum total size of the messages that can be pending in the channel at once.
	pub max_total_size: u32,
	/// The maximum message size that could be put into the channel.
	pub max_message_size: u32,
	/// The current number of messages pending in the channel.
	/// Invariant: should be less or equal to `max_capacity`.s`.
	pub msg_count: u32,
	/// The total size in bytes of all message payloads in the channel.
	/// Invariant: should be less or equal to `max_total_size`.
	pub total_size: u32,
	/// A head of the Message Queue Chain for this channel. Each link in this chain has a form:
	/// `(prev_head, B, H(M))`, where
	/// - `prev_head`: is the previous value of `mqc_head` or zero if none.
	/// - `B`: is the [relay-chain] block number in which a message was appended
	/// - `H(M)`: is the hash of the message being appended.
	/// This value is initialized to a special value that consists of all zeroes which indicates
	/// that no messages were previously added.
	pub mqc_head: Option<Hash>,
	/// The amount that the sender supplied as a deposit when opening this channel.
	pub sender_deposit: Balance,
	/// The amount that the recipient supplied as a deposit when accepting opening this channel.
	pub recipient_deposit: Balance,
}

/// An error returned by [`check_hrmp_watermark`] that indicates an acceptance criteria check
/// didn't pass.
pub enum HrmpWatermarkAcceptanceErr<BlockNumber> {
	AdvancementRule {
		new_watermark: BlockNumber,
		last_watermark: BlockNumber,
	},
	AheadRelayParent {
		new_watermark: BlockNumber,
		relay_chain_parent_number: BlockNumber,
	},
	LandsOnBlockWithNoMessages {
		new_watermark: BlockNumber,
	},
}

/// An error returned by [`check_outbound_hrmp`] that indicates an acceptance criteria check
/// didn't pass.
pub enum OutboundHrmpAcceptanceErr {
	MoreMessagesThanPermitted {
		sent: u32,
		permitted: u32,
	},
	NotSorted {
		idx: u32,
	},
	NoSuchChannel {
		idx: u32,
		channel_id: HrmpChannelId,
	},
	MaxMessageSizeExceeded {
		idx: u32,
		msg_size: u32,
		max_size: u32,
	},
	TotalSizeExceeded {
		idx: u32,
		total_size: u32,
		limit: u32,
	},
	CapacityExceeded {
		idx: u32,
		count: u32,
		limit: u32,
	},
}

impl<BlockNumber> fmt::Debug for HrmpWatermarkAcceptanceErr<BlockNumber>
where
	BlockNumber: fmt::Debug,
{
	fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
		use HrmpWatermarkAcceptanceErr::*;
		match self {
			AdvancementRule {
				new_watermark,
				last_watermark,
			} => write!(
				fmt,
				"the HRMP watermark is not advanced relative to the last watermark ({:?} > {:?})",
				new_watermark, last_watermark,
			),
			AheadRelayParent {
				new_watermark,
				relay_chain_parent_number,
			} => write!(
				fmt,
				"the HRMP watermark is ahead the relay-parent ({:?} > {:?})",
				new_watermark, relay_chain_parent_number
			),
			LandsOnBlockWithNoMessages { new_watermark } => write!(
				fmt,
				"the HRMP watermark ({:?}) doesn't land on a block with messages received",
				new_watermark
			),
		}
	}
}

impl fmt::Debug for OutboundHrmpAcceptanceErr {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
		use OutboundHrmpAcceptanceErr::*;
		match self {
			MoreMessagesThanPermitted { sent, permitted } => write!(
				fmt,
				"more HRMP messages than permitted by config ({} > {})",
				sent, permitted,
			),
			NotSorted { idx } => write!(
				fmt,
				"the HRMP messages are not sorted (first unsorted is at index {})",
				idx,
			),
			NoSuchChannel { idx, channel_id } => write!(
				fmt,
				"the HRMP message at index {} is sent to a non existent channel {:?}->{:?}",
				idx, channel_id.sender, channel_id.recipient,
			),
			MaxMessageSizeExceeded {
				idx,
				msg_size,
				max_size,
			} => write!(
				fmt,
				"the HRMP message at index {} exceeds the negotiated channel maximum message size ({} > {})",
				idx, msg_size, max_size,
			),
			TotalSizeExceeded {
				idx,
				total_size,
				limit,
			} => write!(
				fmt,
				"sending the HRMP message at index {} would exceed the neogitiated channel total size  ({} > {})",
				idx, total_size, limit,
			),
			CapacityExceeded { idx, count, limit } => write!(
				fmt,
				"sending the HRMP message at index {} would exceed the neogitiated channel capacity  ({} > {})",
				idx, count, limit,
			),
		}
	}
}

pub trait Config: frame_system::Config + configuration::Config + paras::Config + dmp::Config {
	/// The outer event type.
	type Event: From<Event> + Into<<Self as frame_system::Config>::Event>;

	type Origin: From<crate::Origin>
		+ From<<Self as frame_system::Config>::Origin>
		+ Into<Result<crate::Origin, <Self as Config>::Origin>>;

	/// An interface for reserving deposits for opening channels.
	///
	/// NOTE that this Currency instance will be charged with the amounts defined in the `Configuration`
	/// module. Specifically, that means that the `Balance` of the `Currency` implementation should
	/// be the same as `Balance` as used in the `Configuration`.
	type Currency: ReservableCurrency<Self::AccountId>;
}

decl_storage! {
	trait Store for Module<T: Config> as Hrmp {
		/// The set of pending HRMP open channel requests.
		///
		/// The set is accompanied by a list for iteration.
		///
		/// Invariant:
		/// - There are no channels that exists in list but not in the set and vice versa.
		HrmpOpenChannelRequests: map hasher(twox_64_concat) HrmpChannelId => Option<HrmpOpenChannelRequest>;
		HrmpOpenChannelRequestsList: Vec<HrmpChannelId>;

		/// This mapping tracks how many open channel requests are initiated by a given sender para.
		/// Invariant: `HrmpOpenChannelRequests` should contain the same number of items that has `(X, _)`
		/// as the number of `HrmpOpenChannelRequestCount` for `X`.
		HrmpOpenChannelRequestCount: map hasher(twox_64_concat) ParaId => u32;
		/// This mapping tracks how many open channel requests were accepted by a given recipient para.
		/// Invariant: `HrmpOpenChannelRequests` should contain the same number of items `(_, X)` with
		/// `confirmed` set to true, as the number of `HrmpAcceptedChannelRequestCount` for `X`.
		HrmpAcceptedChannelRequestCount: map hasher(twox_64_concat) ParaId => u32;

		/// A set of pending HRMP close channel requests that are going to be closed during the session change.
		/// Used for checking if a given channel is registered for closure.
		///
		/// The set is accompanied by a list for iteration.
		///
		/// Invariant:
		/// - There are no channels that exists in list but not in the set and vice versa.
		HrmpCloseChannelRequests: map hasher(twox_64_concat) HrmpChannelId => Option<()>;
		HrmpCloseChannelRequestsList: Vec<HrmpChannelId>;

		/// The HRMP watermark associated with each para.
		/// Invariant:
		/// - each para `P` used here as a key should satisfy `Paras::is_valid_para(P)` within a session.
		HrmpWatermarks: map hasher(twox_64_concat) ParaId => Option<T::BlockNumber>;
		/// HRMP channel data associated with each para.
		/// Invariant:
		/// - each participant in the channel should satisfy `Paras::is_valid_para(P)` within a session.
		HrmpChannels: map hasher(twox_64_concat) HrmpChannelId => Option<HrmpChannel>;
		/// Ingress/egress indexes allow to find all the senders and receivers given the opposite
		/// side. I.e.
		///
		/// (a) ingress index allows to find all the senders for a given recipient.
		/// (b) egress index allows to find all the recipients for a given sender.
		///
		/// Invariants:
		/// - for each ingress index entry for `P` each item `I` in the index should present in `HrmpChannels`
		///   as `(I, P)`.
		/// - for each egress index entry for `P` each item `E` in the index should present in `HrmpChannels`
		///   as `(P, E)`.
		/// - there should be no other dangling channels in `HrmpChannels`.
		/// - the vectors are sorted.
		HrmpIngressChannelsIndex: map hasher(twox_64_concat) ParaId => Vec<ParaId>;
		// NOTE that this field is used by parachains via merkle storage proofs, therefore changing
		// the format will require migration of parachains.
		HrmpEgressChannelsIndex: map hasher(twox_64_concat) ParaId => Vec<ParaId>;

		/// Storage for the messages for each channel.
		/// Invariant: cannot be non-empty if the corresponding channel in `HrmpChannels` is `None`.
		HrmpChannelContents: map hasher(twox_64_concat) HrmpChannelId => Vec<InboundHrmpMessage<T::BlockNumber>>;
		/// Maintains a mapping that can be used to answer the question:
		/// What paras sent a message at the given block number for a given receiver.
		/// Invariants:
		/// - The inner `Vec<ParaId>` is never empty.
		/// - The inner `Vec<ParaId>` cannot store two same `ParaId`.
		/// - The outer vector is sorted ascending by block number and cannot store two items with the same
		///   block number.
		HrmpChannelDigests: map hasher(twox_64_concat) ParaId => Vec<(T::BlockNumber, Vec<ParaId>)>;
	}
	add_extra_genesis {
		/// Preopen the given HRMP channels.
		///
		/// The values in the tuple corresponds to `(sender, recipient, max_capacity, max_message_size)`,
		/// i.e. similar to `init_open_channel`. In fact, the initialization is performed as if
		/// the `init_open_channel` and `accept_open_channel` were called with the respective parameters
		/// and the session change take place.
		///
		/// As such, each channel initializer should satisfy the same constraints, namely:
		///
		/// 1. `max_capacity` and `max_message_size` should be within the limits set by the configuration module.
		/// 2. `sender` and `recipient` must be valid paras.
		config(preopen_hrmp_channels): Vec<(ParaId, ParaId, u32, u32)>;
		build(|config| {
			initialize_storage::<T>(&config.preopen_hrmp_channels);
		})
	}
}

#[cfg(feature = "std")]
fn initialize_storage<T: Config>(preopen_hrmp_channels: &[(ParaId, ParaId, u32, u32)]) {
	let host_config = configuration::Module::<T>::config();
	for &(sender, recipient, max_capacity, max_message_size) in preopen_hrmp_channels {
		if let Err(err) = preopen_hrmp_channel::<T>(sender, recipient, max_capacity, max_message_size) {
			panic!("failed to initialize the genesis storage: {:?}", err);
		}
	}
	<Module<T>>::process_hrmp_open_channel_requests(&host_config);
}

#[cfg(feature = "std")]
fn preopen_hrmp_channel<T: Config>(
	sender: ParaId,
	recipient: ParaId,
	max_capacity: u32,
	max_message_size: u32
) -> DispatchResult {
	<Module<T>>::init_open_channel(
		sender,
		recipient,
		max_capacity,
		max_message_size,
	)?;
	<Module<T>>::accept_open_channel(recipient, sender)?;
	Ok(())
}

decl_error! {
	pub enum Error for Module<T: Config> {
		/// The sender tried to open a channel to themselves.
		OpenHrmpChannelToSelf,
		/// The recipient is not a valid para.
		OpenHrmpChannelInvalidRecipient,
		/// The requested capacity is zero.
		OpenHrmpChannelZeroCapacity,
		/// The requested capacity exceeds the global limit.
		OpenHrmpChannelCapacityExceedsLimit,
		/// The requested maximum message size is 0.
		OpenHrmpChannelZeroMessageSize,
		/// The open request requested the message size that exceeds the global limit.
		OpenHrmpChannelMessageSizeExceedsLimit,
		/// The channel already exists
		OpenHrmpChannelAlreadyExists,
		/// There is already a request to open the same channel.
		OpenHrmpChannelAlreadyRequested,
		/// The sender already has the maximum number of allowed outbound channels.
		OpenHrmpChannelLimitExceeded,
		/// The channel from the sender to the origin doesn't exist.
		AcceptHrmpChannelDoesntExist,
		/// The channel is already confirmed.
		AcceptHrmpChannelAlreadyConfirmed,
		/// The recipient already has the maximum number of allowed inbound channels.
		AcceptHrmpChannelLimitExceeded,
		/// The origin tries to close a channel where it is neither the sender nor the recipient.
		CloseHrmpChannelUnauthorized,
		/// The channel to be closed doesn't exist.
		CloseHrmpChannelDoesntExist,
		/// The channel close request is already requested.
		CloseHrmpChannelAlreadyUnderway,
	 }
}

decl_event! {
	pub enum Event {
		/// Open HRMP channel requested.
		/// `[sender, recipient, proposed_max_capacity, proposed_max_message_size]`
		OpenChannelRequested(ParaId, ParaId, u32, u32),
		/// Open HRMP channel accepted. `[sender, recipient]`
		OpenChannelAccepted(ParaId, ParaId),
		/// HRMP channel closed. `[by_parachain, channel_id]`
		ChannelClosed(ParaId, HrmpChannelId),
	}
}

decl_module! {
	/// The HRMP module.
	pub struct Module<T: Config> for enum Call where origin: <T as frame_system::Config>::Origin {
		type Error = Error<T>;

		fn deposit_event() = default;

		/// Initiate opening a channel from a parachain to a given recipient with given channel
		/// parameters.
		///
		/// - `proposed_max_capacity` - specifies how many messages can be in the channel at once.
		/// - `proposed_max_message_size` - specifies the maximum size of any of the messages.
		///
		/// These numbers are a subject to the relay-chain configuration limits.
		///
		/// The channel can be opened only after the recipient confirms it and only on a session
		/// change.
		#[weight = 0]
		pub fn hrmp_init_open_channel(
			origin,
			recipient: ParaId,
			proposed_max_capacity: u32,
			proposed_max_message_size: u32,
		) -> DispatchResult {
			let origin = ensure_parachain(<T as Config>::Origin::from(origin))?;
			Self::init_open_channel(
				origin,
				recipient,
				proposed_max_capacity,
				proposed_max_message_size
			)?;
			Self::deposit_event(Event::OpenChannelRequested(
				origin,
				recipient,
				proposed_max_capacity,
				proposed_max_message_size
			));
			Ok(())
		}

		/// Accept a pending open channel request from the given sender.
		///
		/// The channel will be opened only on the next session boundary.
		#[weight = 0]
		pub fn hrmp_accept_open_channel(origin, sender: ParaId) -> DispatchResult {
			let origin = ensure_parachain(<T as Config>::Origin::from(origin))?;
			Self::accept_open_channel(origin, sender)?;
			Self::deposit_event(Event::OpenChannelAccepted(sender, origin));
			Ok(())
		}

		/// Initiate unilateral closing of a channel. The origin must be either the sender or the
		/// recipient in the channel being closed.
		///
		/// The closure can only happen on a session change.
		#[weight = 0]
		pub fn hrmp_close_channel(origin, channel_id: HrmpChannelId) -> DispatchResult {
			let origin = ensure_parachain(<T as Config>::Origin::from(origin))?;
			Self::close_channel(origin, channel_id.clone())?;
			Self::deposit_event(Event::ChannelClosed(origin, channel_id));
			Ok(())
		}

		/// This extrinsic triggers the cleanup of all the HRMP storage items that
		/// a para may have. Normally this happens once per session, but this allows
		/// you to trigger the cleanup immediately for a specific parachain.
		///
		/// Origin must be Root.
		#[weight = 0]
		pub fn force_clean_hrmp(origin, para: ParaId) -> DispatchResult {
			ensure_root(origin)?;
			Self::clean_hrmp_after_outgoing(&para);
			Ok(())
		}

		/// Force process HRMP open channel requests.
		///
		/// If there are pending HRMP open channel requests, you can use this
		/// function process all of those requests immediately.
		#[weight = 0]
		pub fn force_process_hrmp_open(origin) -> DispatchResult {
			ensure_root(origin)?;
			let host_config = configuration::Module::<T>::config();
			Self::process_hrmp_open_channel_requests(&host_config);
			Ok(())
		}

		/// Force process HRMP close channel requests.
		///
		/// If there are pending HRMP close channel requests, you can use this
		/// function process all of those requests immediately.
		#[weight = 0]
		pub fn force_process_hrmp_close(origin) -> DispatchResult {
			ensure_root(origin)?;
			Self::process_hrmp_close_channel_requests();
			Ok(())
		}
	}
}

/// Routines and getters related to HRMP.
impl<T: Config> Module<T> {
	/// Block initialization logic, called by initializer.
	pub(crate) fn initializer_initialize(_now: T::BlockNumber) -> Weight {
		0
	}

	/// Block finalization logic, called by initializer.
	pub(crate) fn initializer_finalize() {}

	/// Called by the initializer to note that a new session has started.
	pub(crate) fn initializer_on_new_session(
		notification: &initializer::SessionChangeNotification<T::BlockNumber>,
		outgoing_paras: &[ParaId],
	) {
		Self::perform_outgoing_para_cleanup(outgoing_paras);
		Self::process_hrmp_open_channel_requests(&notification.prev_config);
		Self::process_hrmp_close_channel_requests();
	}

	/// Iterate over all paras that were noted for offboarding and remove all the data
	/// associated with them.
	fn perform_outgoing_para_cleanup(outgoing: &[ParaId]) {
		for outgoing_para in outgoing {
			Self::clean_hrmp_after_outgoing(outgoing_para);
		}
	}

	/// Remove all storage entries associated with the given para.
	fn clean_hrmp_after_outgoing(outgoing_para: &ParaId) {
		<Self as Store>::HrmpOpenChannelRequestCount::remove(outgoing_para);
		<Self as Store>::HrmpAcceptedChannelRequestCount::remove(outgoing_para);

		let ingress = <Self as Store>::HrmpIngressChannelsIndex::take(outgoing_para)
			.into_iter()
			.map(|sender| HrmpChannelId {
				sender,
				recipient: outgoing_para.clone(),
			});
		let egress = <Self as Store>::HrmpEgressChannelsIndex::take(outgoing_para)
			.into_iter()
			.map(|recipient| HrmpChannelId {
				sender: outgoing_para.clone(),
				recipient,
			});
		let mut to_close = ingress.chain(egress).collect::<Vec<_>>();
		to_close.sort();
		to_close.dedup();

		for channel in to_close {
			Self::close_hrmp_channel(&channel);
		}
	}

	/// Iterate over all open channel requests and:
	///
	/// - prune the stale requests
	/// - enact the confirmed requests
	fn process_hrmp_open_channel_requests(config: &HostConfiguration<T::BlockNumber>) {
		let mut open_req_channels = <Self as Store>::HrmpOpenChannelRequestsList::get();
		if open_req_channels.is_empty() {
			return;
		}

		// iterate the vector starting from the end making our way to the beginning. This way we
		// can leverage `swap_remove` to efficiently remove an item during iteration.
		let mut idx = open_req_channels.len();
		loop {
			// bail if we've iterated over all items.
			if idx == 0 {
				break;
			}

			idx -= 1;
			let channel_id = open_req_channels[idx].clone();
			let mut request = <Self as Store>::HrmpOpenChannelRequests::get(&channel_id).expect(
				"can't be `None` due to the invariant that the list contains the same items as the set; qed",
			);

			if request.confirmed {
				if <paras::Pallet<T>>::is_valid_para(channel_id.sender)
					&& <paras::Pallet<T>>::is_valid_para(channel_id.recipient)
				{
					<Self as Store>::HrmpChannels::insert(
						&channel_id,
						HrmpChannel {
							sender_deposit: request.sender_deposit,
							recipient_deposit: config.hrmp_recipient_deposit,
							max_capacity: request.max_capacity,
							max_total_size: request.max_total_size,
							max_message_size: request.max_message_size,
							msg_count: 0,
							total_size: 0,
							mqc_head: None,
						},
					);

					<Self as Store>::HrmpIngressChannelsIndex::mutate(&channel_id.recipient, |v| {
						if let Err(i) = v.binary_search(&channel_id.sender) {
							v.insert(i, channel_id.sender);
						}
					});
					<Self as Store>::HrmpEgressChannelsIndex::mutate(&channel_id.sender, |v| {
						if let Err(i) = v.binary_search(&channel_id.recipient) {
							v.insert(i, channel_id.recipient);
						}
					});
				}

				let new_open_channel_req_cnt =
					<Self as Store>::HrmpOpenChannelRequestCount::get(&channel_id.sender)
						.saturating_sub(1);
				if new_open_channel_req_cnt != 0 {
					<Self as Store>::HrmpOpenChannelRequestCount::insert(
						&channel_id.sender,
						new_open_channel_req_cnt,
					);
				} else {
					<Self as Store>::HrmpOpenChannelRequestCount::remove(&channel_id.sender);
				}

				let new_accepted_channel_req_cnt =
					<Self as Store>::HrmpAcceptedChannelRequestCount::get(&channel_id.recipient)
						.saturating_sub(1);
				if new_accepted_channel_req_cnt != 0 {
					<Self as Store>::HrmpAcceptedChannelRequestCount::insert(
						&channel_id.recipient,
						new_accepted_channel_req_cnt,
					);
				} else {
					<Self as Store>::HrmpAcceptedChannelRequestCount::remove(&channel_id.recipient);
				}

				let _ = open_req_channels.swap_remove(idx);
				<Self as Store>::HrmpOpenChannelRequests::remove(&channel_id);
			} else {
				request.age += 1;
				if request.age == config.hrmp_open_request_ttl {
					// got stale
					<Self as Store>::HrmpOpenChannelRequestCount::mutate(&channel_id.sender, |v| {
						*v -= 1;
					});

					let _ = open_req_channels.swap_remove(idx);
					if let Some(HrmpOpenChannelRequest { sender_deposit, .. }) =
						<Self as Store>::HrmpOpenChannelRequests::take(&channel_id)
					{
						T::Currency::unreserve(
							&channel_id.sender.into_account(),
							sender_deposit.unique_saturated_into(),
						);
					}
				} else {
					<Self as Store>::HrmpOpenChannelRequests::insert(&channel_id, request);
				}
			}
		}

		<Self as Store>::HrmpOpenChannelRequestsList::put(open_req_channels);
	}

	/// Iterate over all close channel requests unconditionally closing the channels.
	fn process_hrmp_close_channel_requests() {
		let close_reqs = <Self as Store>::HrmpCloseChannelRequestsList::take();
		for condemned_ch_id in close_reqs {
			<Self as Store>::HrmpCloseChannelRequests::remove(&condemned_ch_id);
			Self::close_hrmp_channel(&condemned_ch_id);
		}
	}

	/// Close and remove the designated HRMP channel.
	///
	/// This includes returning the deposits.
	///
	/// This function is idempotent, meaning that after the first application it should have no
	/// effect (i.e. it won't return the deposits twice).
	fn close_hrmp_channel(channel_id: &HrmpChannelId) {
		if let Some(HrmpChannel {
			sender_deposit,
			recipient_deposit,
			..
		}) = <Self as Store>::HrmpChannels::take(channel_id)
		{
			T::Currency::unreserve(
				&channel_id.sender.into_account(),
				sender_deposit.unique_saturated_into(),
			);
			T::Currency::unreserve(
				&channel_id.recipient.into_account(),
				recipient_deposit.unique_saturated_into(),
			);
		}

		<Self as Store>::HrmpChannelContents::remove(channel_id);

		<Self as Store>::HrmpEgressChannelsIndex::mutate(&channel_id.sender, |v| {
			if let Ok(i) = v.binary_search(&channel_id.recipient) {
				v.remove(i);
			}
		});
		<Self as Store>::HrmpIngressChannelsIndex::mutate(&channel_id.recipient, |v| {
			if let Ok(i) = v.binary_search(&channel_id.sender) {
				v.remove(i);
			}
		});
	}

	/// Check that the candidate of the given recipient controls the HRMP watermark properly.
	pub(crate) fn check_hrmp_watermark(
		recipient: ParaId,
		relay_chain_parent_number: T::BlockNumber,
		new_hrmp_watermark: T::BlockNumber,
	) -> Result<(), HrmpWatermarkAcceptanceErr<T::BlockNumber>> {
		// First, check where the watermark CANNOT legally land.
		//
		// (a) For ensuring that messages are eventually, a rule requires each parablock new
		//     watermark should be greater than the last one.
		//
		// (b) However, a parachain cannot read into "the future", therefore the watermark should
		//     not be greater than the relay-chain context block which the parablock refers to.
		if let Some(last_watermark) = <Self as Store>::HrmpWatermarks::get(&recipient) {
			if new_hrmp_watermark <= last_watermark {
				return Err(HrmpWatermarkAcceptanceErr::AdvancementRule {
					new_watermark: new_hrmp_watermark,
					last_watermark,
				});
			}
		}
		if new_hrmp_watermark > relay_chain_parent_number {
			return Err(HrmpWatermarkAcceptanceErr::AheadRelayParent {
				new_watermark: new_hrmp_watermark,
				relay_chain_parent_number,
			});
		}

		// Second, check where the watermark CAN land. It's one of the following:
		//
		// (a) The relay parent block number.
		// (b) A relay-chain block in which this para received at least one message.
		if new_hrmp_watermark == relay_chain_parent_number {
			Ok(())
		} else {
			let digest = <Self as Store>::HrmpChannelDigests::get(&recipient);
			if !digest
				.binary_search_by_key(&new_hrmp_watermark, |(block_no, _)| *block_no)
				.is_ok()
			{
				return Err(HrmpWatermarkAcceptanceErr::LandsOnBlockWithNoMessages {
					new_watermark: new_hrmp_watermark,
				});
			}
			Ok(())
		}
	}

	pub(crate) fn check_outbound_hrmp(
		config: &HostConfiguration<T::BlockNumber>,
		sender: ParaId,
		out_hrmp_msgs: &[OutboundHrmpMessage<ParaId>],
	) -> Result<(), OutboundHrmpAcceptanceErr> {
		if out_hrmp_msgs.len() as u32 > config.hrmp_max_message_num_per_candidate {
			return Err(OutboundHrmpAcceptanceErr::MoreMessagesThanPermitted {
				sent: out_hrmp_msgs.len() as u32,
				permitted: config.hrmp_max_message_num_per_candidate,
			});
		}

		let mut last_recipient = None::<ParaId>;

		for (idx, out_msg) in out_hrmp_msgs
			.iter()
			.enumerate()
			.map(|(idx, out_msg)| (idx as u32, out_msg))
		{
			match last_recipient {
				// the messages must be sorted in ascending order and there must be no two messages sent
				// to the same recipient. Thus we can check that every recipient is strictly greater than
				// the previous one.
				Some(last_recipient) if out_msg.recipient <= last_recipient => {
					return Err(OutboundHrmpAcceptanceErr::NotSorted { idx });
				}
				_ => last_recipient = Some(out_msg.recipient),
			}

			let channel_id = HrmpChannelId {
				sender,
				recipient: out_msg.recipient,
			};

			let channel = match <Self as Store>::HrmpChannels::get(&channel_id) {
				Some(channel) => channel,
				None => {
					return Err(OutboundHrmpAcceptanceErr::NoSuchChannel { channel_id, idx });
				}
			};

			let msg_size = out_msg.data.len() as u32;
			if msg_size > channel.max_message_size {
				return Err(OutboundHrmpAcceptanceErr::MaxMessageSizeExceeded {
					idx,
					msg_size,
					max_size: channel.max_message_size,
				});
			}

			let new_total_size = channel.total_size + out_msg.data.len() as u32;
			if new_total_size > channel.max_total_size {
				return Err(OutboundHrmpAcceptanceErr::TotalSizeExceeded {
					idx,
					total_size: new_total_size,
					limit: channel.max_total_size,
				});
			}

			let new_msg_count = channel.msg_count + 1;
			if new_msg_count > channel.max_capacity {
				return Err(OutboundHrmpAcceptanceErr::CapacityExceeded {
					idx,
					count: new_msg_count,
					limit: channel.max_capacity,
				});
			}
		}

		Ok(())
	}

	pub(crate) fn prune_hrmp(recipient: ParaId, new_hrmp_watermark: T::BlockNumber) -> Weight {
		let mut weight = 0;

		// sift through the incoming messages digest to collect the paras that sent at least one
		// message to this parachain between the old and new watermarks.
		let senders = <Self as Store>::HrmpChannelDigests::mutate(&recipient, |digest| {
			let mut senders = BTreeSet::new();
			let mut leftover = Vec::with_capacity(digest.len());
			for (block_no, paras_sent_msg) in mem::replace(digest, Vec::new()) {
				if block_no <= new_hrmp_watermark {
					senders.extend(paras_sent_msg);
				} else {
					leftover.push((block_no, paras_sent_msg));
				}
			}
			*digest = leftover;
			senders
		});
		weight += T::DbWeight::get().reads_writes(1, 1);

		// having all senders we can trivially find out the channels which we need to prune.
		let channels_to_prune = senders
			.into_iter()
			.map(|sender| HrmpChannelId { sender, recipient });
		for channel_id in channels_to_prune {
			// prune each channel up to the new watermark keeping track how many messages we removed
			// and what is the total byte size of them.
			let (mut pruned_cnt, mut pruned_size) = (0, 0);

			let contents = <Self as Store>::HrmpChannelContents::get(&channel_id);
			let mut leftover = Vec::with_capacity(contents.len());
			for msg in contents {
				if msg.sent_at <= new_hrmp_watermark {
					pruned_cnt += 1;
					pruned_size += msg.data.len();
				} else {
					leftover.push(msg);
				}
			}
			if !leftover.is_empty() {
				<Self as Store>::HrmpChannelContents::insert(&channel_id, leftover);
			} else {
				<Self as Store>::HrmpChannelContents::remove(&channel_id);
			}

			// update the channel metadata.
			<Self as Store>::HrmpChannels::mutate(&channel_id, |channel| {
				if let Some(ref mut channel) = channel {
					channel.msg_count -= pruned_cnt as u32;
					channel.total_size -= pruned_size as u32;
				}
			});

			weight += T::DbWeight::get().reads_writes(2, 2);
		}

		<Self as Store>::HrmpWatermarks::insert(&recipient, new_hrmp_watermark);
		weight += T::DbWeight::get().reads_writes(0, 1);

		weight
	}

	/// Process the outbound HRMP messages by putting them into the appropriate recipient queues.
	///
	/// Returns the amount of weight consumed.
	pub(crate) fn queue_outbound_hrmp(
		sender: ParaId,
		out_hrmp_msgs: Vec<OutboundHrmpMessage<ParaId>>,
	) -> Weight {
		let mut weight = 0;
		let now = <frame_system::Pallet<T>>::block_number();

		for out_msg in out_hrmp_msgs {
			let channel_id = HrmpChannelId {
				sender,
				recipient: out_msg.recipient,
			};

			let mut channel = match <Self as Store>::HrmpChannels::get(&channel_id) {
				Some(channel) => channel,
				None => {
					// apparently, that since acceptance of this candidate the recipient was
					// offboarded and the channel no longer exists.
					continue;
				}
			};

			let inbound = InboundHrmpMessage {
				sent_at: now,
				data: out_msg.data,
			};

			// book keeping
			channel.msg_count += 1;
			channel.total_size += inbound.data.len() as u32;

			// compute the new MQC head of the channel
			let prev_head = channel.mqc_head.clone().unwrap_or(Default::default());
			let new_head = BlakeTwo256::hash_of(&(
				prev_head,
				inbound.sent_at,
				T::Hashing::hash_of(&inbound.data),
			));
			channel.mqc_head = Some(new_head);

			<Self as Store>::HrmpChannels::insert(&channel_id, channel);
			<Self as Store>::HrmpChannelContents::append(&channel_id, inbound);

			// The digests are sorted in ascending by block number order. Assuming absence of
			// contextual execution, there are only two possible scenarios here:
			//
			// (a) It's the first time anybody sends a message to this recipient within this block.
			//     In this case, the digest vector would be empty or the block number of the latest
			//     entry  is smaller than the current.
			//
			// (b) Somebody has already sent a message within the current block. That means that
			//     the block number of the latest entry is equal to the current.
			//
			// Note that having the latest entry greater than the current block number is a logical
			// error.
			let mut recipient_digest =
				<Self as Store>::HrmpChannelDigests::get(&channel_id.recipient);
			if let Some(cur_block_digest) = recipient_digest
				.last_mut()
				.filter(|(block_no, _)| *block_no == now)
				.map(|(_, ref mut d)| d)
			{
				cur_block_digest.push(sender);
			} else {
				recipient_digest.push((now, vec![sender]));
			}
			<Self as Store>::HrmpChannelDigests::insert(&channel_id.recipient, recipient_digest);

			weight += T::DbWeight::get().reads_writes(2, 2);
		}

		weight
	}

	/// Initiate opening a channel from a parachain to a given recipient with given channel
	/// parameters.
	///
	/// Basically the same as [`hrmp_init_open_channel`](Module::hrmp_init_open_channel) but intendend for calling directly from
	/// other pallets rather than dispatched.
	pub fn init_open_channel(
		origin: ParaId,
		recipient: ParaId,
		proposed_max_capacity: u32,
		proposed_max_message_size: u32,
	) -> DispatchResult {
		ensure!(origin != recipient, Error::<T>::OpenHrmpChannelToSelf);
		ensure!(
			<paras::Pallet<T>>::is_valid_para(recipient),
			Error::<T>::OpenHrmpChannelInvalidRecipient,
		);

		let config = <configuration::Module<T>>::config();
		ensure!(
			proposed_max_capacity > 0,
			Error::<T>::OpenHrmpChannelZeroCapacity,
		);
		ensure!(
			proposed_max_capacity <= config.hrmp_channel_max_capacity,
			Error::<T>::OpenHrmpChannelCapacityExceedsLimit,
		);
		ensure!(
			proposed_max_message_size > 0,
			Error::<T>::OpenHrmpChannelZeroMessageSize,
		);
		ensure!(
			proposed_max_message_size <= config.hrmp_channel_max_message_size,
			Error::<T>::OpenHrmpChannelMessageSizeExceedsLimit,
		);

		let channel_id = HrmpChannelId {
			sender: origin,
			recipient,
		};
		ensure!(
			<Self as Store>::HrmpOpenChannelRequests::get(&channel_id).is_none(),
			Error::<T>::OpenHrmpChannelAlreadyExists,
		);
		ensure!(
			<Self as Store>::HrmpChannels::get(&channel_id).is_none(),
			Error::<T>::OpenHrmpChannelAlreadyRequested,
		);

		let egress_cnt =
			<Self as Store>::HrmpEgressChannelsIndex::decode_len(&origin).unwrap_or(0) as u32;
		let open_req_cnt = <Self as Store>::HrmpOpenChannelRequestCount::get(&origin);
		let channel_num_limit = if <paras::Pallet<T>>::is_parathread(origin) {
			config.hrmp_max_parathread_outbound_channels
		} else {
			config.hrmp_max_parachain_outbound_channels
		};
		ensure!(
			egress_cnt + open_req_cnt < channel_num_limit,
			Error::<T>::OpenHrmpChannelLimitExceeded,
		);

		T::Currency::reserve(
			&origin.into_account(),
			config.hrmp_sender_deposit.unique_saturated_into(),
		)?;

		<Self as Store>::HrmpOpenChannelRequestCount::insert(&origin, open_req_cnt + 1);
		<Self as Store>::HrmpOpenChannelRequests::insert(
			&channel_id,
			HrmpOpenChannelRequest {
				confirmed: false,
				age: 0,
				sender_deposit: config.hrmp_sender_deposit,
				max_capacity: proposed_max_capacity,
				max_message_size: proposed_max_message_size,
				max_total_size: config.hrmp_channel_max_total_size,
			},
		);
		<Self as Store>::HrmpOpenChannelRequestsList::append(channel_id);

		let notification_bytes = {
			use xcm::opaque::{v0::Xcm, VersionedXcm};
			use parity_scale_codec::Encode as _;

			VersionedXcm::from(Xcm::HrmpNewChannelOpenRequest {
				sender: u32::from(origin),
				max_capacity: proposed_max_capacity,
				max_message_size: proposed_max_message_size,
			})
			.encode()
		};
		if let Err(dmp::QueueDownwardMessageError::ExceedsMaxMessageSize) =
			<dmp::Pallet<T>>::queue_downward_message(&config, recipient, notification_bytes)
		{
			// this should never happen unless the max downward message size is configured to an
			// jokingly small number.
			debug_assert!(false);
		}

		Ok(())
	}

	/// Accept a pending open channel request from the given sender.
	///
	/// Basically the same as [`hrmp_accept_open_channel`](Module::hrmp_accept_open_channel) but
	/// intendend for calling directly from other pallets rather than dispatched.
	pub fn accept_open_channel(origin: ParaId, sender: ParaId) -> DispatchResult {
		let channel_id = HrmpChannelId {
			sender,
			recipient: origin,
		};
		let mut channel_req = <Self as Store>::HrmpOpenChannelRequests::get(&channel_id)
			.ok_or(Error::<T>::AcceptHrmpChannelDoesntExist)?;
		ensure!(
			!channel_req.confirmed,
			Error::<T>::AcceptHrmpChannelAlreadyConfirmed,
		);

		// check if by accepting this open channel request, this parachain would exceed the
		// number of inbound channels.
		let config = <configuration::Module<T>>::config();
		let channel_num_limit = if <paras::Pallet<T>>::is_parathread(origin) {
			config.hrmp_max_parathread_inbound_channels
		} else {
			config.hrmp_max_parachain_inbound_channels
		};
		let ingress_cnt =
			<Self as Store>::HrmpIngressChannelsIndex::decode_len(&origin).unwrap_or(0) as u32;
		let accepted_cnt = <Self as Store>::HrmpAcceptedChannelRequestCount::get(&origin);
		ensure!(
			ingress_cnt + accepted_cnt < channel_num_limit,
			Error::<T>::AcceptHrmpChannelLimitExceeded,
		);

		T::Currency::reserve(
			&origin.into_account(),
			config.hrmp_recipient_deposit.unique_saturated_into(),
		)?;

		// persist the updated open channel request and then increment the number of accepted
		// channels.
		channel_req.confirmed = true;
		<Self as Store>::HrmpOpenChannelRequests::insert(&channel_id, channel_req);
		<Self as Store>::HrmpAcceptedChannelRequestCount::insert(&origin, accepted_cnt + 1);

		let notification_bytes = {
			use parity_scale_codec::Encode as _;
			use xcm::opaque::{v0::Xcm, VersionedXcm};

			VersionedXcm::from(Xcm::HrmpChannelAccepted {
				recipient: u32::from(origin),
			})
			.encode()
		};
		if let Err(dmp::QueueDownwardMessageError::ExceedsMaxMessageSize) =
			<dmp::Pallet<T>>::queue_downward_message(&config, sender, notification_bytes)
		{
			// this should never happen unless the max downward message size is configured to an
			// jokingly small number.
			debug_assert!(false);
		}

		Ok(())
	}

	fn close_channel(origin: ParaId, channel_id: HrmpChannelId) -> Result<(), Error<T>> {
		// check if the origin is allowed to close the channel.
		ensure!(
			origin == channel_id.sender || origin == channel_id.recipient,
			Error::<T>::CloseHrmpChannelUnauthorized,
		);

		// check if the channel requested to close does exist.
		ensure!(
			<Self as Store>::HrmpChannels::get(&channel_id).is_some(),
			Error::<T>::CloseHrmpChannelDoesntExist,
		);

		// check that there is no outstanding close request for this channel
		ensure!(
			<Self as Store>::HrmpCloseChannelRequests::get(&channel_id).is_none(),
			Error::<T>::CloseHrmpChannelAlreadyUnderway,
		);

		<Self as Store>::HrmpCloseChannelRequests::insert(&channel_id, ());
		<Self as Store>::HrmpCloseChannelRequestsList::append(channel_id.clone());

		let config = <configuration::Module<T>>::config();
		let notification_bytes = {
			use parity_scale_codec::Encode as _;
			use xcm::opaque::{v0::Xcm, VersionedXcm};

			VersionedXcm::from(Xcm::HrmpChannelClosing {
				initiator: u32::from(origin),
				sender: u32::from(channel_id.sender),
				recipient: u32::from(channel_id.recipient),
			})
			.encode()
		};
		let opposite_party = if origin == channel_id.sender {
			channel_id.recipient
		} else {
			channel_id.sender
		};
		if let Err(dmp::QueueDownwardMessageError::ExceedsMaxMessageSize) =
			<dmp::Pallet<T>>::queue_downward_message(&config, opposite_party, notification_bytes)
		{
			// this should never happen unless the max downward message size is configured to an
			// jokingly small number.
			debug_assert!(false);
		}

		Ok(())
	}

	/// Returns the list of MQC heads for the inbound channels of the given recipient para paired
	/// with the sender para ids. This vector is sorted ascending by the para id and doesn't contain
	/// multiple entries with the same sender.
	#[cfg(test)]
	fn hrmp_mqc_heads(recipient: ParaId) -> Vec<(ParaId, Hash)> {
		let sender_set = <Self as Store>::HrmpIngressChannelsIndex::get(&recipient);

		// The ingress channels vector is sorted, thus `mqc_heads` is sorted as well.
		let mut mqc_heads = Vec::with_capacity(sender_set.len());
		for sender in sender_set {
			let channel_metadata =
				<Self as Store>::HrmpChannels::get(&HrmpChannelId { sender, recipient });
			let mqc_head = channel_metadata
				.and_then(|metadata| metadata.mqc_head)
				.unwrap_or(Hash::default());
			mqc_heads.push((sender, mqc_head));
		}

		mqc_heads
	}

	/// Returns contents of all channels addressed to the given recipient. Channels that have no
	/// messages in them are also included.
	pub(crate) fn inbound_hrmp_channels_contents(
		recipient: ParaId,
	) -> BTreeMap<ParaId, Vec<InboundHrmpMessage<T::BlockNumber>>> {
		let sender_set = <Self as Store>::HrmpIngressChannelsIndex::get(&recipient);

		let mut inbound_hrmp_channels_contents = BTreeMap::new();
		for sender in sender_set {
			let channel_contents =
				<Self as Store>::HrmpChannelContents::get(&HrmpChannelId { sender, recipient });
			inbound_hrmp_channels_contents.insert(sender, channel_contents);
		}

		inbound_hrmp_channels_contents
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::mock::{
		new_test_ext, Test, Configuration, Paras, Shared, Hrmp, System, MockGenesisConfig,
		Event as MockEvent,
	};
	use frame_support::{assert_noop, assert_ok, traits::Currency as _};
	use primitives::v1::BlockNumber;
	use std::collections::{BTreeMap, HashSet};

	fn run_to_block(to: BlockNumber, new_session: Option<Vec<BlockNumber>>) {
		use frame_support::traits::{OnFinalize as _, OnInitialize as _};

		let config = Configuration::config();
		while System::block_number() < to {
			let b = System::block_number();

			// NOTE: this is in reverse initialization order.
			Hrmp::initializer_finalize();
			Paras::initializer_finalize();
			Shared::initializer_finalize();

			if new_session.as_ref().map_or(false, |v| v.contains(&(b + 1))) {
				let notification = crate::initializer::SessionChangeNotification {
					prev_config: config.clone(),
					new_config: config.clone(),
					session_index: Shared::session_index() + 1,
					..Default::default()
				};

				// NOTE: this is in initialization order.
				Shared::initializer_on_new_session(
					notification.session_index,
					notification.random_seed,
					&notification.new_config,
					notification.validators.clone(),
				);
				let outgoing_paras = Paras::initializer_on_new_session(&notification);
				Hrmp::initializer_on_new_session(&notification, &outgoing_paras);
			}

			System::on_finalize(b);

			System::on_initialize(b + 1);
			System::set_block_number(b + 1);

			// NOTE: this is in initialization order.
			Shared::initializer_initialize(b + 1);
			Paras::initializer_initialize(b + 1);
			Hrmp::initializer_initialize(b + 1);
		}
	}

	#[derive(Debug)]
	struct GenesisConfigBuilder {
		hrmp_channel_max_capacity: u32,
		hrmp_channel_max_message_size: u32,
		hrmp_max_parathread_outbound_channels: u32,
		hrmp_max_parachain_outbound_channels: u32,
		hrmp_max_parathread_inbound_channels: u32,
		hrmp_max_parachain_inbound_channels: u32,
		hrmp_max_message_num_per_candidate: u32,
		hrmp_channel_max_total_size: u32,
		hrmp_sender_deposit: Balance,
		hrmp_recipient_deposit: Balance,
		hrmp_open_request_ttl: u32,
	}

	impl Default for GenesisConfigBuilder {
		fn default() -> Self {
			Self {
				hrmp_channel_max_capacity: 2,
				hrmp_channel_max_message_size: 8,
				hrmp_max_parathread_outbound_channels: 1,
				hrmp_max_parachain_outbound_channels: 2,
				hrmp_max_parathread_inbound_channels: 1,
				hrmp_max_parachain_inbound_channels: 2,
				hrmp_max_message_num_per_candidate: 2,
				hrmp_channel_max_total_size: 16,
				hrmp_sender_deposit: 100,
				hrmp_recipient_deposit: 100,
				hrmp_open_request_ttl: 3,
			}
		}
	}

	impl GenesisConfigBuilder {
		fn build(self) -> crate::mock::MockGenesisConfig {
			let mut genesis = default_genesis_config();
			let config = &mut genesis.configuration.config;
			config.hrmp_channel_max_capacity = self.hrmp_channel_max_capacity;
			config.hrmp_channel_max_message_size = self.hrmp_channel_max_message_size;
			config.hrmp_max_parathread_outbound_channels =
				self.hrmp_max_parathread_outbound_channels;
			config.hrmp_max_parachain_outbound_channels = self.hrmp_max_parachain_outbound_channels;
			config.hrmp_max_parathread_inbound_channels = self.hrmp_max_parathread_inbound_channels;
			config.hrmp_max_parachain_inbound_channels = self.hrmp_max_parachain_inbound_channels;
			config.hrmp_max_message_num_per_candidate = self.hrmp_max_message_num_per_candidate;
			config.hrmp_channel_max_total_size = self.hrmp_channel_max_total_size;
			config.hrmp_sender_deposit = self.hrmp_sender_deposit;
			config.hrmp_recipient_deposit = self.hrmp_recipient_deposit;
			config.hrmp_open_request_ttl = self.hrmp_open_request_ttl;
			genesis
		}
	}

	fn default_genesis_config() -> MockGenesisConfig {
		MockGenesisConfig {
			configuration: crate::configuration::GenesisConfig {
				config: crate::configuration::HostConfiguration {
					max_downward_message_size: 1024,
					..Default::default()
				},
			},
			..Default::default()
		}
	}

	fn register_parachain_with_balance(id: ParaId, balance: Balance) {
		assert_ok!(Paras::schedule_para_initialize(
			id,
			crate::paras::ParaGenesisArgs {
				parachain: true,
				genesis_head: vec![1].into(),
				validation_code: vec![1].into(),
			},
		));
		<Test as Config>::Currency::make_free_balance_be(&id.into_account(), balance);
	}

	fn register_parachain(id: ParaId) {
		register_parachain_with_balance(id, 1000);
	}

	fn deregister_parachain(id: ParaId) {
		assert_ok!(Paras::schedule_para_cleanup(id));
	}

	fn channel_exists(sender: ParaId, recipient: ParaId) -> bool {
		<Hrmp as Store>::HrmpChannels::get(&HrmpChannelId { sender, recipient }).is_some()
	}

	fn assert_storage_consistency_exhaustive() {
		use frame_support::IterableStorageMap;

		assert_eq!(
			<Hrmp as Store>::HrmpOpenChannelRequests::iter()
				.map(|(k, _)| k)
				.collect::<HashSet<_>>(),
			<Hrmp as Store>::HrmpOpenChannelRequestsList::get()
				.into_iter()
				.collect::<HashSet<_>>(),
		);

		// verify that the set of keys in `HrmpOpenChannelRequestCount` corresponds to the set
		// of _senders_ in `HrmpOpenChannelRequests`.
		//
		// having ensured that, we can go ahead and go over all counts and verify that they match.
		assert_eq!(
			<Hrmp as Store>::HrmpOpenChannelRequestCount::iter()
				.map(|(k, _)| k)
				.collect::<HashSet<_>>(),
			<Hrmp as Store>::HrmpOpenChannelRequests::iter()
				.map(|(k, _)| k.sender)
				.collect::<HashSet<_>>(),
		);
		for (open_channel_initiator, expected_num) in
			<Hrmp as Store>::HrmpOpenChannelRequestCount::iter()
		{
			let actual_num = <Hrmp as Store>::HrmpOpenChannelRequests::iter()
				.filter(|(ch, _)| ch.sender == open_channel_initiator)
				.count() as u32;
			assert_eq!(expected_num, actual_num);
		}

		// The same as above, but for accepted channel request count. Note that we are interested
		// only in confirmed open requests.
		assert_eq!(
			<Hrmp as Store>::HrmpAcceptedChannelRequestCount::iter()
				.map(|(k, _)| k)
				.collect::<HashSet<_>>(),
			<Hrmp as Store>::HrmpOpenChannelRequests::iter()
				.filter(|(_, v)| v.confirmed)
				.map(|(k, _)| k.recipient)
				.collect::<HashSet<_>>(),
		);
		for (channel_recipient, expected_num) in
			<Hrmp as Store>::HrmpAcceptedChannelRequestCount::iter()
		{
			let actual_num = <Hrmp as Store>::HrmpOpenChannelRequests::iter()
				.filter(|(ch, v)| ch.recipient == channel_recipient && v.confirmed)
				.count() as u32;
			assert_eq!(expected_num, actual_num);
		}

		assert_eq!(
			<Hrmp as Store>::HrmpCloseChannelRequests::iter()
				.map(|(k, _)| k)
				.collect::<HashSet<_>>(),
			<Hrmp as Store>::HrmpCloseChannelRequestsList::get()
				.into_iter()
				.collect::<HashSet<_>>(),
		);

		// A HRMP watermark can be None for an onboarded parachain. However, an offboarded parachain
		// cannot have an HRMP watermark: it should've been cleanup.
		assert_contains_only_onboarded(
			<Hrmp as Store>::HrmpWatermarks::iter().map(|(k, _)| k),
			"HRMP watermarks should contain only onboarded paras",
		);

		// An entry in `HrmpChannels` indicates that the channel is open. Only open channels can
		// have contents.
		for (non_empty_channel, contents) in <Hrmp as Store>::HrmpChannelContents::iter() {
			assert!(<Hrmp as Store>::HrmpChannels::contains_key(
				&non_empty_channel
			));

			// pedantic check: there should be no empty vectors in storage, those should be modeled
			// by a removed kv pair.
			assert!(!contents.is_empty());
		}

		// Senders and recipients must be onboarded. Otherwise, all channels associated with them
		// are removed.
		assert_contains_only_onboarded(
			<Hrmp as Store>::HrmpChannels::iter().flat_map(|(k, _)| vec![k.sender, k.recipient]),
			"senders and recipients in all channels should be onboarded",
		);

		// Check the docs for `HrmpIngressChannelsIndex` and `HrmpEgressChannelsIndex` in decl
		// storage to get an index what are the channel mappings indexes.
		//
		// Here, from indexes.
		//
		// ingress         egress
		//
		// a -> [x, y]     x -> [a, b]
		// b -> [x, z]     y -> [a]
		//                 z -> [b]
		//
		// we derive a list of channels they represent.
		//
		//   (a, x)         (a, x)
		//   (a, y)         (a, y)
		//   (b, x)         (b, x)
		//   (b, z)         (b, z)
		//
		// and then that we compare that to the channel list in the `HrmpChannels`.
		let channel_set_derived_from_ingress = <Hrmp as Store>::HrmpIngressChannelsIndex::iter()
			.flat_map(|(p, v)| v.into_iter().map(|i| (i, p)).collect::<Vec<_>>())
			.collect::<HashSet<_>>();
		let channel_set_derived_from_egress = <Hrmp as Store>::HrmpEgressChannelsIndex::iter()
			.flat_map(|(p, v)| v.into_iter().map(|e| (p, e)).collect::<Vec<_>>())
			.collect::<HashSet<_>>();
		let channel_set_ground_truth = <Hrmp as Store>::HrmpChannels::iter()
			.map(|(k, _)| (k.sender, k.recipient))
			.collect::<HashSet<_>>();
		assert_eq!(
			channel_set_derived_from_ingress,
			channel_set_derived_from_egress
		);
		assert_eq!(channel_set_derived_from_egress, channel_set_ground_truth);

		<Hrmp as Store>::HrmpIngressChannelsIndex::iter()
			.map(|(_, v)| v)
			.for_each(|v| assert_is_sorted(&v, "HrmpIngressChannelsIndex"));
		<Hrmp as Store>::HrmpEgressChannelsIndex::iter()
			.map(|(_, v)| v)
			.for_each(|v| assert_is_sorted(&v, "HrmpIngressChannelsIndex"));

		assert_contains_only_onboarded(
			<Hrmp as Store>::HrmpChannelDigests::iter().map(|(k, _)| k),
			"HRMP channel digests should contain only onboarded paras",
		);
		for (_digest_for_para, digest) in <Hrmp as Store>::HrmpChannelDigests::iter() {
			// Assert that items are in **strictly** ascending order. The strictness also implies
			// there are no duplicates.
			assert!(digest.windows(2).all(|xs| xs[0].0 < xs[1].0));

			for (_, mut senders) in digest {
				assert!(!senders.is_empty());

				// check for duplicates. For that we sort the vector, then perform deduplication.
				// if the vector stayed the same, there are no duplicates.
				senders.sort();
				let orig_senders = senders.clone();
				senders.dedup();
				assert_eq!(
					orig_senders, senders,
					"duplicates removed implies existence of duplicates"
				);
			}
		}

		fn assert_contains_only_onboarded(iter: impl Iterator<Item = ParaId>, cause: &str) {
			for para in iter {
				assert!(
					Paras::is_valid_para(para),
					"{}: {} para is offboarded",
					cause,
					para
				);
			}
		}
	}

	fn assert_is_sorted<T: Ord>(slice: &[T], id: &str) {
		assert!(
			slice.windows(2).all(|xs| xs[0] <= xs[1]),
			"{} supposed to be sorted",
			id
		);
	}

	#[test]
	fn empty_state_consistent_state() {
		new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
			assert_storage_consistency_exhaustive();
		});
	}

	#[test]
	fn open_channel_works() {
		let para_a = 1.into();
		let para_a_origin: crate::Origin = 1.into();
		let para_b = 3.into();
		let para_b_origin: crate::Origin = 3.into();

		new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
			// We need both A & B to be registered and alive parachains.
			register_parachain(para_a);
			register_parachain(para_b);

			run_to_block(5, Some(vec![4, 5]));
			Hrmp::hrmp_init_open_channel(para_a_origin.into(), para_b, 2, 8).unwrap();
			assert_storage_consistency_exhaustive();
			assert!(System::events().iter().any(|record|
				record.event == MockEvent::Hrmp(Event::OpenChannelRequested(para_a, para_b, 2, 8))
			));

			Hrmp::hrmp_accept_open_channel(para_b_origin.into(), para_a).unwrap();
			assert_storage_consistency_exhaustive();
			assert!(System::events().iter().any(|record|
				record.event == MockEvent::Hrmp(Event::OpenChannelAccepted(para_a, para_b))
			));

			// Advance to a block 6, but without session change. That means that the channel has
			// not been created yet.
			run_to_block(6, None);
			assert!(!channel_exists(para_a, para_b));
			assert_storage_consistency_exhaustive();

			// Now let the session change happen and thus open the channel.
			run_to_block(8, Some(vec![8]));
			assert!(channel_exists(para_a, para_b));
		});
	}

	#[test]
	fn close_channel_works() {
		let para_a = 5.into();
		let para_b = 2.into();
		let para_b_origin: crate::Origin = 2.into();

		new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
			register_parachain(para_a);
			register_parachain(para_b);

			run_to_block(5, Some(vec![4, 5]));
			Hrmp::init_open_channel(para_a, para_b, 2, 8).unwrap();
			Hrmp::accept_open_channel(para_b, para_a).unwrap();

			run_to_block(6, Some(vec![6]));
			assert!(channel_exists(para_a, para_b));

			// Close the channel. The effect is not immediate, but rather deferred to the next
			// session change.
			let channel_id = HrmpChannelId {
				sender: para_a,
				recipient: para_b,
			};
			Hrmp::hrmp_close_channel(para_b_origin.into(), channel_id.clone()).unwrap();
			assert!(channel_exists(para_a, para_b));
			assert_storage_consistency_exhaustive();

			// After the session change the channel should be closed.
			run_to_block(8, Some(vec![8]));
			assert!(!channel_exists(para_a, para_b));
			assert_storage_consistency_exhaustive();
			assert!(System::events().iter().any(|record|
				record.event == MockEvent::Hrmp(Event::ChannelClosed(para_b, channel_id.clone()))
			));
		});
	}

	#[test]
	fn send_recv_messages() {
		let para_a = 32.into();
		let para_b = 64.into();

		let mut genesis = GenesisConfigBuilder::default();
		genesis.hrmp_channel_max_message_size = 20;
		genesis.hrmp_channel_max_total_size = 20;
		new_test_ext(genesis.build()).execute_with(|| {
			register_parachain(para_a);
			register_parachain(para_b);

			run_to_block(5, Some(vec![4, 5]));
			Hrmp::init_open_channel(para_a, para_b, 2, 20).unwrap();
			Hrmp::accept_open_channel(para_b, para_a).unwrap();

			// On Block 6:
			// A sends a message to B
			run_to_block(6, Some(vec![6]));
			assert!(channel_exists(para_a, para_b));
			let msgs = vec![OutboundHrmpMessage {
				recipient: para_b,
				data: b"this is an emergency".to_vec(),
			}];
			let config = Configuration::config();
			assert!(Hrmp::check_outbound_hrmp(&config, para_a, &msgs).is_ok());
			let _ = Hrmp::queue_outbound_hrmp(para_a, msgs);
			assert_storage_consistency_exhaustive();

			// On Block 7:
			// B receives the message sent by A. B sets the watermark to 6.
			run_to_block(7, None);
			assert!(Hrmp::check_hrmp_watermark(para_b, 7, 6).is_ok());
			let _ = Hrmp::prune_hrmp(para_b, 6);
			assert_storage_consistency_exhaustive();
		});
	}

	#[test]
	fn hrmp_mqc_head_fixture() {
		let para_a = 2000.into();
		let para_b = 2024.into();

		let mut genesis = GenesisConfigBuilder::default();
		genesis.hrmp_channel_max_message_size = 20;
		genesis.hrmp_channel_max_total_size = 20;
		new_test_ext(genesis.build()).execute_with(|| {
			register_parachain(para_a);
			register_parachain(para_b);

			run_to_block(2, Some(vec![1,2]));
			Hrmp::init_open_channel(para_a, para_b, 2, 20).unwrap();
			Hrmp::accept_open_channel(para_b, para_a).unwrap();

			run_to_block(3, Some(vec![3]));
			let _ = Hrmp::queue_outbound_hrmp(para_a, vec![OutboundHrmpMessage {
				recipient: para_b,
				data: vec![1, 2, 3],
			}]);

			run_to_block(4, None);
			let _ = Hrmp::queue_outbound_hrmp(para_a, vec![OutboundHrmpMessage {
				recipient: para_b,
				data: vec![4, 5, 6],
			}]);

			assert_eq!(
				Hrmp::hrmp_mqc_heads(para_b),
				vec![
					(para_a, hex_literal::hex!["a964fd3b4f3d3ce92a0e25e576b87590d92bb5cb7031909c7f29050e1f04a375"].into()),
				],
			);
		});
	}

	#[test]
	fn accept_incoming_request_and_offboard() {
		let para_a = 32.into();
		let para_b = 64.into();

		new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
			register_parachain(para_a);
			register_parachain(para_b);

			run_to_block(5, Some(vec![4, 5]));
			Hrmp::init_open_channel(para_a, para_b, 2, 8).unwrap();
			Hrmp::accept_open_channel(para_b, para_a).unwrap();
			deregister_parachain(para_a);

			// On Block 7: 2x session change. The channel should not be created.
			run_to_block(7, Some(vec![6, 7]));
			assert!(!Paras::is_valid_para(para_a));
			assert!(!channel_exists(para_a, para_b));
			assert_storage_consistency_exhaustive();
		});
	}

	#[test]
	fn check_sent_messages() {
		let para_a = 32.into();
		let para_b = 64.into();
		let para_c = 97.into();

		new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
			register_parachain(para_a);
			register_parachain(para_b);
			register_parachain(para_c);

			run_to_block(5, Some(vec![4, 5]));

			// Open two channels to the same receiver, b:
			// a -> b, c -> b
			Hrmp::init_open_channel(para_a, para_b, 2, 8).unwrap();
			Hrmp::accept_open_channel(para_b, para_a).unwrap();
			Hrmp::init_open_channel(para_c, para_b, 2, 8).unwrap();
			Hrmp::accept_open_channel(para_b, para_c).unwrap();

			// On Block 6: session change.
			run_to_block(6, Some(vec![6]));
			assert!(Paras::is_valid_para(para_a));

			let msgs = vec![OutboundHrmpMessage {
				recipient: para_b,
				data: b"knock".to_vec(),
			}];
			let config = Configuration::config();
			assert!(Hrmp::check_outbound_hrmp(&config, para_a, &msgs).is_ok());
			let _ = Hrmp::queue_outbound_hrmp(para_a, msgs.clone());

			// Verify that the sent messages are there and that also the empty channels are present.
			let mqc_heads = Hrmp::hrmp_mqc_heads(para_b);
			let contents = Hrmp::inbound_hrmp_channels_contents(para_b);
			assert_eq!(
				contents,
				vec![
					(
						para_a,
						vec![InboundHrmpMessage {
							sent_at: 6,
							data: b"knock".to_vec(),
						}]
					),
					(para_c, vec![])
				]
				.into_iter()
				.collect::<BTreeMap::<_, _>>(),
			);
			assert_eq!(
				mqc_heads,
				vec![
					(
						para_a,
						hex_literal::hex!(
							"3bba6404e59c91f51deb2ae78f1273ebe75896850713e13f8c0eba4b0996c483"
						)
						.into()
					),
					(para_c, Default::default())
				],
			);

			assert_storage_consistency_exhaustive();
		});
	}

	#[test]
	fn verify_externally_accessible() {
		use primitives::v1::{well_known_keys, AbridgedHrmpChannel};

		let para_a = 20.into();
		let para_b = 21.into();

		new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
			// Register two parachains, wait until a session change, then initiate channel open
			// request and accept that, and finally wait until the next session.
			register_parachain(para_a);
			register_parachain(para_b);
			run_to_block(5, Some(vec![4, 5]));
			Hrmp::init_open_channel(para_a, para_b, 2, 8).unwrap();
			Hrmp::accept_open_channel(para_b, para_a).unwrap();
			run_to_block(8, Some(vec![8]));

			// Here we have a channel a->b opened.
			//
			// Try to obtain this channel from the storage and
			// decode it into the abridged version.
			assert!(channel_exists(para_a, para_b));
			let raw_hrmp_channel =
				sp_io::storage::get(&well_known_keys::hrmp_channels(HrmpChannelId {
					sender: para_a,
					recipient: para_b,
				}))
				.expect("the channel exists and we must be able to get it through well known keys");
			let abridged_hrmp_channel = AbridgedHrmpChannel::decode(&mut &raw_hrmp_channel[..])
				.expect("HrmpChannel should be decodable as AbridgedHrmpChannel");

			assert_eq!(
				abridged_hrmp_channel,
				AbridgedHrmpChannel {
					max_capacity: 2,
					max_total_size: 16,
					max_message_size: 8,
					msg_count: 0,
					total_size: 0,
					mqc_head: None,
				},
			);

			let raw_ingress_index =
				sp_io::storage::get(
					&well_known_keys::hrmp_ingress_channel_index(para_b),
				)
				.expect("the ingress index must be present for para_b");
			let ingress_index = <Vec<ParaId>>::decode(&mut &raw_ingress_index[..])
				.expect("ingress indexx should be decodable as a list of para ids");
			assert_eq!(
				ingress_index,
				vec![para_a],
			);

			// Now, verify that we can access and decode the egress index.
			let raw_egress_index =
				sp_io::storage::get(
					&well_known_keys::hrmp_egress_channel_index(para_a)
				)
				.expect("the egress index must be present for para_a");
			let egress_index = <Vec<ParaId>>::decode(&mut &raw_egress_index[..])
				.expect("egress index should be decodable as a list of para ids");
			assert_eq!(
				egress_index,
				vec![para_b],
			);
		});
	}

	#[test]
	fn charging_deposits() {
		let para_a = 32.into();
		let para_b = 64.into();

		new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
			register_parachain_with_balance(para_a, 0);
			register_parachain(para_b);
			run_to_block(5, Some(vec![4, 5]));

			assert_noop!(
				Hrmp::init_open_channel(para_a, para_b, 2, 8),
				pallet_balances::Error::<Test, _>::InsufficientBalance
			);
		});

		new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
			register_parachain(para_a);
			register_parachain_with_balance(para_b, 0);
			run_to_block(5, Some(vec![4, 5]));

			Hrmp::init_open_channel(para_a, para_b, 2, 8).unwrap();

			assert_noop!(
				Hrmp::accept_open_channel(para_b, para_a),
				pallet_balances::Error::<Test, _>::InsufficientBalance
			);
		});
	}

	#[test]
	fn refund_deposit_on_normal_closure() {
		let para_a = 32.into();
		let para_b = 64.into();

		let mut genesis = GenesisConfigBuilder::default();
		genesis.hrmp_sender_deposit = 20;
		genesis.hrmp_recipient_deposit = 15;
		new_test_ext(genesis.build()).execute_with(|| {
			// Register two parachains funded with different amounts of funds and arrange a channel.
			register_parachain_with_balance(para_a, 100);
			register_parachain_with_balance(para_b, 110);
			run_to_block(5, Some(vec![4, 5]));
			Hrmp::init_open_channel(para_a, para_b, 2, 8).unwrap();
			Hrmp::accept_open_channel(para_b, para_a).unwrap();
			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_a.into_account()),
				80
			);
			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_b.into_account()),
				95
			);
			run_to_block(8, Some(vec![8]));

			// Now, we close the channel and wait until the next session.
			Hrmp::close_channel(
				para_b,
				HrmpChannelId {
					sender: para_a,
					recipient: para_b,
				},
			)
			.unwrap();
			run_to_block(10, Some(vec![10]));
			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_a.into_account()),
				100
			);
			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_b.into_account()),
				110
			);
		});
	}

	#[test]
	fn refund_deposit_on_request_expiry() {
		let para_a = 32.into();
		let para_b = 64.into();

		let mut genesis = GenesisConfigBuilder::default();
		genesis.hrmp_sender_deposit = 20;
		genesis.hrmp_recipient_deposit = 15;
		genesis.hrmp_open_request_ttl = 2;
		new_test_ext(genesis.build()).execute_with(|| {
			// Register two parachains funded with different amounts of funds, send an open channel
			// request but do not accept it.
			register_parachain_with_balance(para_a, 100);
			register_parachain_with_balance(para_b, 110);
			run_to_block(5, Some(vec![4, 5]));
			Hrmp::init_open_channel(para_a, para_b, 2, 8).unwrap();
			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_a.into_account()),
				80
			);
			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_b.into_account()),
				110
			);

			// Request age is 1 out of 2
			run_to_block(10, Some(vec![10]));
			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_a.into_account()),
				80
			);

			// Request age is 2 out of 2. The request should expire.
			run_to_block(20, Some(vec![20]));
			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_a.into_account()),
				100
			);
		});
	}

	#[test]
	fn refund_deposit_on_offboarding() {
		let para_a = 32.into();
		let para_b = 64.into();

		let mut genesis = GenesisConfigBuilder::default();
		genesis.hrmp_sender_deposit = 20;
		genesis.hrmp_recipient_deposit = 15;
		new_test_ext(genesis.build()).execute_with(|| {
			// Register two parachains and open a channel between them.
			register_parachain_with_balance(para_a, 100);
			register_parachain_with_balance(para_b, 110);
			run_to_block(5, Some(vec![4, 5]));
			Hrmp::init_open_channel(para_a, para_b, 2, 8).unwrap();
			Hrmp::accept_open_channel(para_b, para_a).unwrap();
			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_a.into_account()),
				80
			);
			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_b.into_account()),
				95
			);
			run_to_block(8, Some(vec![8]));
			assert!(channel_exists(para_a, para_b));

			// Then deregister one parachain.
			deregister_parachain(para_a);
			run_to_block(10, Some(vec![9, 10]));

			// The channel should be removed.
			assert!(!Paras::is_valid_para(para_a));
			assert!(!channel_exists(para_a, para_b));
			assert_storage_consistency_exhaustive();

			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_a.into_account()),
				100
			);
			assert_eq!(
				<Test as Config>::Currency::free_balance(&para_b.into_account()),
				110
			);
		});
	}
}
