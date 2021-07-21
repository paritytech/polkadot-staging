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
	configuration::{self, HostConfiguration},
	initializer,
};
use frame_support::pallet_prelude::*;
use sp_std::{fmt, prelude::*};
use sp_runtime::traits::{BlakeTwo256, Hash as HashT, SaturatedConversion};
use primitives::v1::{Id as ParaId, DownwardMessage, InboundDownwardMessage, Hash};
use xcm::v0::Error as XcmError;

pub use pallet::*;

/// An error sending a downward message.
#[cfg_attr(test, derive(Debug))]
pub enum QueueDownwardMessageError {
	/// The message being sent exceeds the configured max message size.
	ExceedsMaxMessageSize,
}

impl From<QueueDownwardMessageError> for XcmError {
	fn from(err: QueueDownwardMessageError) -> Self {
		match err {
			QueueDownwardMessageError::ExceedsMaxMessageSize => XcmError::ExceedsMaxMessageSize,
		}
	}
}

/// An error returned by [`check_processed_downward_messages`] that indicates an acceptance check
/// didn't pass.
pub enum ProcessedDownwardMessagesAcceptanceErr {
	/// If there are pending messages then `processed_downward_messages` should be at least 1,
	AdvancementRule,
	/// `processed_downward_messages` should not be greater than the number of pending messages.
	Underflow {
		processed_downward_messages: u32,
		dmq_length: u32,
	},
}

impl fmt::Debug for ProcessedDownwardMessagesAcceptanceErr {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
		use ProcessedDownwardMessagesAcceptanceErr::*;
		match *self {
			AdvancementRule => write!(
				fmt,
				"DMQ is not empty, but processed_downward_messages is 0",
			),
			Underflow {
				processed_downward_messages,
				dmq_length,
			} => write!(
				fmt,
				"processed_downward_messages = {}, but dmq_length is only {}",
				processed_downward_messages, dmq_length,
			),
		}
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config + configuration::Config {}

	/// The downward messages addressed for a certain para.
	#[pallet::storage]
	pub type DownwardMessageQueues<T: Config> = StorageMap<
		_,
		Twox64Concat,
		ParaId,
		Vec<InboundDownwardMessage<T::BlockNumber>>,
		ValueQuery
	>;

	/// A mapping that stores the downward message queue MQC head for each para.
	///
	/// Each link in this chain has a form:
	/// `(prev_head, B, H(M))`, where
	/// - `prev_head`: is the previous head hash or zero if none.
	/// - `B`: is the relay-chain block number in which a message was appended.
	/// - `H(M)`: is the hash of the message being appended.
	#[pallet::storage]
	pub type DownwardMessageQueueHeads<T: Config> = StorageMap<_, Twox64Concat, ParaId, Hash, ValueQuery>;

	#[pallet::call]
	impl<T: Config> Pallet<T> {}
}

/// Routines and getters related to downward message passing.
impl<T: Config> Pallet<T> {
	/// Block initialization logic, called by initializer.
	pub(crate) fn initializer_initialize(_now: T::BlockNumber) -> Weight {
		0
	}

	/// Block finalization logic, called by initializer.
	pub(crate) fn initializer_finalize() {}

	/// Called by the initializer to note that a new session has started.
	pub(crate) fn initializer_on_new_session(
		_notification: &initializer::SessionChangeNotification<T::BlockNumber>,
		outgoing_paras: &[ParaId],
	) {
		Self::perform_outgoing_para_cleanup(outgoing_paras);
	}

	/// Iterate over all paras that were noted for offboarding and remove all the data
	/// associated with them.
	fn perform_outgoing_para_cleanup(outgoing: &[ParaId]) {
		for outgoing_para in outgoing {
			Self::clean_dmp_after_outgoing(outgoing_para);
		}
	}

	/// Remove all relevant storage items for an outgoing parachain.
	fn clean_dmp_after_outgoing(outgoing_para: &ParaId) {
		<Self as Store>::DownwardMessageQueues::remove(outgoing_para);
		<Self as Store>::DownwardMessageQueueHeads::remove(outgoing_para);
	}

	/// Enqueue a downward message to a specific recipient para.
	///
	/// When encoded, the message should not exceed the `config.max_downward_message_size`.
	/// Otherwise, the message won't be sent and `Err` will be returned.
	///
	/// It is possible to send a downward message to a non-existent para. That, however, would lead
	/// to a dangling storage. If the caller cannot statically prove that the recipient exists
	/// then the caller should perform a runtime check.
	pub fn queue_downward_message(
		config: &HostConfiguration<T::BlockNumber>,
		para: ParaId,
		msg: DownwardMessage,
	) -> Result<(), QueueDownwardMessageError> {
		let serialized_len = msg.len() as u32;
		if serialized_len > config.max_downward_message_size {
			return Err(QueueDownwardMessageError::ExceedsMaxMessageSize);
		}

		let inbound = InboundDownwardMessage {
			msg,
			sent_at: <frame_system::Pallet<T>>::block_number(),
		};

		// obtain the new link in the MQC and update the head.
		<Self as Store>::DownwardMessageQueueHeads::mutate(para, |head| {
			let new_head =
				BlakeTwo256::hash_of(&(*head, inbound.sent_at, T::Hashing::hash_of(&inbound.msg)));
			*head = new_head;
		});

		<Self as Store>::DownwardMessageQueues::mutate(para, |v| {
			v.push(inbound);
		});

		Ok(())
	}

	/// Checks if the number of processed downward messages is valid.
	pub(crate) fn check_processed_downward_messages(
		para: ParaId,
		processed_downward_messages: u32,
	) -> Result<(), ProcessedDownwardMessagesAcceptanceErr> {
		let dmq_length = Self::dmq_length(para);

		if dmq_length > 0 && processed_downward_messages == 0 {
			return Err(ProcessedDownwardMessagesAcceptanceErr::AdvancementRule);
		}
		if dmq_length < processed_downward_messages {
			return Err(ProcessedDownwardMessagesAcceptanceErr::Underflow {
				processed_downward_messages,
				dmq_length,
			});
		}

		Ok(())
	}

	/// Prunes the specified number of messages from the downward message queue of the given para.
	pub(crate) fn prune_dmq(para: ParaId, processed_downward_messages: u32) -> Weight {
		<Self as Store>::DownwardMessageQueues::mutate(para, |q| {
			let processed_downward_messages = processed_downward_messages as usize;
			if processed_downward_messages > q.len() {
				// reaching this branch is unexpected due to the constraint established by
				// `check_processed_downward_messages`. But better be safe than sorry.
				q.clear();
			} else {
				*q = q.split_off(processed_downward_messages);
			}
		});
		T::DbWeight::get().reads_writes(1, 1)
	}

	/// Returns the Head of Message Queue Chain for the given para or `None` if there is none
	/// associated with it.
	#[cfg(test)]
	fn dmq_mqc_head(para: ParaId) -> Hash {
		<Self as Store>::DownwardMessageQueueHeads::get(&para)
	}

	/// Returns the number of pending downward messages addressed to the given para.
	///
	/// Returns 0 if the para doesn't have an associated downward message queue.
	pub(crate) fn dmq_length(para: ParaId) -> u32 {
		<Self as Store>::DownwardMessageQueues::decode_len(&para)
			.unwrap_or(0)
			.saturated_into::<u32>()
	}

	/// Returns the downward message queue contents for the given para.
	///
	/// The most recent messages are the latest in the vector.
	pub(crate) fn dmq_contents(recipient: ParaId) -> Vec<InboundDownwardMessage<T::BlockNumber>> {
		<Self as Store>::DownwardMessageQueues::get(&recipient)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use hex_literal::hex;
	use primitives::v1::BlockNumber;
	use parity_scale_codec::Encode;
	use crate::mock::{Configuration, new_test_ext, System, Dmp, MockGenesisConfig, Paras};

	pub(crate) fn run_to_block(to: BlockNumber, new_session: Option<Vec<BlockNumber>>) {
		while System::block_number() < to {
			let b = System::block_number();
			Paras::initializer_finalize();
			Dmp::initializer_finalize();
			if new_session.as_ref().map_or(false, |v| v.contains(&(b + 1))) {
				Dmp::initializer_on_new_session(&Default::default(), &Vec::new());
			}
			System::on_finalize(b);

			System::on_initialize(b + 1);
			System::set_block_number(b + 1);

			Paras::initializer_finalize();
			Dmp::initializer_initialize(b + 1);
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

	fn queue_downward_message(
		para_id: ParaId,
		msg: DownwardMessage,
	) -> Result<(), QueueDownwardMessageError> {
		Dmp::queue_downward_message(&Configuration::config(), para_id, msg)
	}

	#[test]
	fn clean_dmp_works() {
		let a = ParaId::from(1312);
		let b = ParaId::from(228);
		let c = ParaId::from(123);

		new_test_ext(default_genesis_config()).execute_with(|| {
			// enqueue downward messages to A, B and C.
			queue_downward_message(a, vec![1, 2, 3]).unwrap();
			queue_downward_message(b, vec![4, 5, 6]).unwrap();
			queue_downward_message(c, vec![7, 8, 9]).unwrap();

			let notification = crate::initializer::SessionChangeNotification::default();
			let outgoing_paras = vec![a, b];
			Dmp::initializer_on_new_session(&notification, &outgoing_paras);

			assert!(<Dmp as Store>::DownwardMessageQueues::get(&a).is_empty());
			assert!(<Dmp as Store>::DownwardMessageQueues::get(&b).is_empty());
			assert!(!<Dmp as Store>::DownwardMessageQueues::get(&c).is_empty());
		});
	}

	#[test]
	fn dmq_length_and_head_updated_properly() {
		let a = ParaId::from(1312);
		let b = ParaId::from(228);

		new_test_ext(default_genesis_config()).execute_with(|| {
			assert_eq!(Dmp::dmq_length(a), 0);
			assert_eq!(Dmp::dmq_length(b), 0);

			queue_downward_message(a, vec![1, 2, 3]).unwrap();

			assert_eq!(Dmp::dmq_length(a), 1);
			assert_eq!(Dmp::dmq_length(b), 0);
			assert!(!Dmp::dmq_mqc_head(a).is_zero());
			assert!(Dmp::dmq_mqc_head(b).is_zero());
		});
	}

	#[test]
	fn dmp_mqc_head_fixture() {
		let a = ParaId::from(2000);

		new_test_ext(default_genesis_config()).execute_with(|| {
			run_to_block(2, None);
			assert!(Dmp::dmq_mqc_head(a).is_zero());
			queue_downward_message(a, vec![1, 2, 3]).unwrap();

			run_to_block(3, None);
			queue_downward_message(a, vec![4, 5, 6]).unwrap();

			assert_eq!(
				Dmp::dmq_mqc_head(a),
				hex!["88dc00db8cc9d22aa62b87807705831f164387dfa49f80a8600ed1cbe1704b6b"].into(),
			);
		});
	}

	#[test]
	fn check_processed_downward_messages() {
		let a = ParaId::from(1312);

		new_test_ext(default_genesis_config()).execute_with(|| {
			// processed_downward_messages=0 is allowed when the DMQ is empty.
			assert!(Dmp::check_processed_downward_messages(a, 0).is_ok());

			queue_downward_message(a, vec![1, 2, 3]).unwrap();
			queue_downward_message(a, vec![4, 5, 6]).unwrap();
			queue_downward_message(a, vec![7, 8, 9]).unwrap();

			// 0 doesn't pass if the DMQ has msgs.
			assert!(!Dmp::check_processed_downward_messages(a, 0).is_ok());
			// a candidate can consume up to 3 messages
			assert!(Dmp::check_processed_downward_messages(a, 1).is_ok());
			assert!(Dmp::check_processed_downward_messages(a, 2).is_ok());
			assert!(Dmp::check_processed_downward_messages(a, 3).is_ok());
			// there is no 4 messages in the queue
			assert!(!Dmp::check_processed_downward_messages(a, 4).is_ok());
		});
	}

	#[test]
	fn dmq_pruning() {
		let a = ParaId::from(1312);

		new_test_ext(default_genesis_config()).execute_with(|| {
			assert_eq!(Dmp::dmq_length(a), 0);

			queue_downward_message(a, vec![1, 2, 3]).unwrap();
			queue_downward_message(a, vec![4, 5, 6]).unwrap();
			queue_downward_message(a, vec![7, 8, 9]).unwrap();
			assert_eq!(Dmp::dmq_length(a), 3);

			// pruning 0 elements shouldn't change anything.
			Dmp::prune_dmq(a, 0);
			assert_eq!(Dmp::dmq_length(a), 3);

			Dmp::prune_dmq(a, 2);
			assert_eq!(Dmp::dmq_length(a), 1);
		});
	}

	#[test]
	fn queue_downward_message_critical() {
		let a = ParaId::from(1312);

		let mut genesis = default_genesis_config();
		genesis.configuration.config.max_downward_message_size = 7;

		new_test_ext(genesis).execute_with(|| {
			let smol = [0; 3].to_vec();
			let big = [0; 8].to_vec();

			// still within limits
			assert_eq!(smol.encode().len(), 4);
			assert!(queue_downward_message(a, smol).is_ok());

			// that's too big
			assert_eq!(big.encode().len(), 9);
			assert!(queue_downward_message(a, big).is_err());
		});
	}

	#[test]
	fn verify_dmq_mqc_head_is_externally_accessible() {
		use primitives::v1::well_known_keys;
		use hex_literal::hex;

		let a = ParaId::from(2020);

		new_test_ext(default_genesis_config()).execute_with(|| {
			let head = sp_io::storage::get(&well_known_keys::dmq_mqc_head(a));
			assert_eq!(head, None);

			queue_downward_message(a, vec![1, 2, 3]).unwrap();

			let head = sp_io::storage::get(&well_known_keys::dmq_mqc_head(a));
			assert_eq!(
				head,
				Some(hex!["434f8579a2297dfea851bf6be33093c83a78b655a53ae141a7894494c0010589"].to_vec())
			);
		});
	}
}
