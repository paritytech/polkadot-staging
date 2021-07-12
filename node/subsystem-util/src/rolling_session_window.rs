// Copyright 2021 Parity Technologies (UK) Ltd.
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

//! A rolling window of sessions and cached session info, updated by the state of newly imported blocks.
//!
//! This is useful for consensus components which need to stay up-to-date about recent sessions but don't
//! care about the state of particular blocks.

use polkadot_primitives::v1::{Hash, Header, SessionInfo, SessionIndex};

use polkadot_node_subsystem::{
	overseer,
	messages::{RuntimeApiMessage, RuntimeApiRequest},
	errors::RuntimeApiError,
	SubsystemContext,
};
use futures::channel::oneshot;

/// Sessions unavailable in state to cache.
#[derive(Debug)]
pub enum SessionsUnavailableKind {
	/// Runtime API subsystem was unavailable.
	RuntimeApiUnavailable(oneshot::Canceled),
	/// The runtime API itself returned an error.
	RuntimeApi(RuntimeApiError),
	/// Missing session info from runtime API.
	Missing,
}

/// Information about the sessions being fetched.
#[derive(Debug)]
pub struct SessionsUnavailableInfo {
	/// The desired window start.
	pub window_start: SessionIndex,
	/// The desired window end.
	pub window_end: SessionIndex,
	/// The block hash whose state the sessions were meant to be drawn from.
	pub block_hash: Hash,
}

/// Sessions were unavailable to fetch from the state for some reason.
#[derive(Debug)]
pub struct SessionsUnavailable {
	/// The error kind.
	kind: SessionsUnavailableKind,
	/// The info about the session window, if any.
	info: Option<SessionsUnavailableInfo>,
}

/// An indicated update of the rolling session window.
#[derive(Debug, PartialEq, Clone)]
pub enum SessionWindowUpdate {
	/// The session window was just initialized to the current values.
	Initialized {
		/// The start of the window (inclusive).
		window_start: SessionIndex,
		/// The end of the window (inclusive).
		window_end: SessionIndex,
	},
	/// The session window was just advanced from one range to a new one.
	Advanced {
		/// The previous start of the window (inclusive).
		prev_window_start: SessionIndex,
		/// The previous end of the window (inclusive).
		prev_window_end: SessionIndex,
		/// The new start of the window (inclusive).
		new_window_start: SessionIndex,
		/// The new end of the window (inclusive).
		new_window_end: SessionIndex,
	},
	/// The session window was unchanged.
	Unchanged,
}

/// A rolling window of sessions and cached session info.
#[derive(Default)]
pub struct RollingSessionWindow {
	earliest_session: Option<SessionIndex>,
	session_info: Vec<SessionInfo>,
	window_size: SessionIndex,
}

impl RollingSessionWindow {
	/// Initialize a new session info cache with the given window size.
	pub fn new(window_size: SessionIndex) -> Self {
		RollingSessionWindow {
			earliest_session: None,
			session_info: Vec::new(),
			window_size,
		}
	}

	/// Initialize a new session info cache with the given window size and
	/// initial data.
	pub fn with_session_info(
		window_size: SessionIndex,
		earliest_session: SessionIndex,
		session_info: Vec<SessionInfo>,
	) -> Self {
		RollingSessionWindow {
			earliest_session: Some(earliest_session),
			session_info,
			window_size,
		}
	}

	/// Access the session info for the given session index, if stored within the window.
	pub fn session_info(&self, index: SessionIndex) -> Option<&SessionInfo> {
		self.earliest_session.and_then(|earliest| {
			if index < earliest {
				None
			} else {
				self.session_info.get((index - earliest) as usize)
			}
		})

	}

	/// Access the index of the earliest session, if the window is not empty.
	pub fn earliest_session(&self) -> Option<SessionIndex> {
		self.earliest_session.clone()
	}

	/// Access the index of the latest session, if the window is not empty.
	pub fn latest_session(&self) -> Option<SessionIndex> {
		self.earliest_session
			.map(|earliest| earliest + (self.session_info.len() as SessionIndex).saturating_sub(1))
	}

	/// When inspecting a new import notification, updates the session info cache to match
	/// the session of the imported block.
	///
	/// this only needs to be called on heads where we are directly notified about import, as sessions do
	/// not change often and import notifications are expected to be typically increasing in session number.
	///
	/// some backwards drift in session index is acceptable.
	pub async fn cache_session_info_for_head(
		&mut self,
		ctx: &mut impl overseer::SubsystemContext,
		block_hash: Hash,
		block_header: &Header,
	) -> Result<SessionWindowUpdate, SessionsUnavailable> {
		if self.window_size == 0 { return Ok(SessionWindowUpdate::Unchanged) }

		let session_index = {
			let (s_tx, s_rx) = oneshot::channel();

			// The genesis is guaranteed to be at the beginning of the session and its parent state
			// is non-existent. Therefore if we're at the genesis, we request using its state and
			// not the parent.
			ctx.send_message(RuntimeApiMessage::Request(
				if block_header.number == 0 { block_hash } else { block_header.parent_hash },
				RuntimeApiRequest::SessionIndexForChild(s_tx),
			)).await;

			match s_rx.await {
				Ok(Ok(s)) => s,
				Ok(Err(e)) => return Err(SessionsUnavailable {
					kind: SessionsUnavailableKind::RuntimeApi(e),
					info: None,
				}),
				Err(e) => return Err(SessionsUnavailable {
					kind: SessionsUnavailableKind::RuntimeApiUnavailable(e),
					info: None,
				}),
			}
		};

		match self.earliest_session {
			None => {
				// First block processed on start-up.

				let window_start = session_index.saturating_sub(self.window_size - 1);

				match load_all_sessions(ctx, block_hash, window_start, session_index).await {
					Err(kind) => {
						Err(SessionsUnavailable {
							kind,
							info: Some(SessionsUnavailableInfo {
								window_start,
								window_end: session_index,
								block_hash,
							}),
						})
					},
					Ok(s) => {
						let update = SessionWindowUpdate::Initialized {
							window_start,
							window_end: session_index,
						};

						self.earliest_session = Some(window_start);
						self.session_info = s;

						Ok(update)
					}
				}
			}
			Some(old_window_start) => {
				let latest = self.latest_session().expect("latest always exists if earliest does; qed");

				// Either cached or ancient.
				if session_index <= latest { return Ok(SessionWindowUpdate::Unchanged) }

				let old_window_end = latest;

				let window_start = session_index.saturating_sub(self.window_size - 1);

				// keep some of the old window, if applicable.
				let overlap_start = window_start.saturating_sub(old_window_start);

				let fresh_start = if latest < window_start {
					window_start
				} else {
					latest + 1
				};

				match load_all_sessions(ctx, block_hash, fresh_start, session_index).await {
					Err(kind) => {
						Err(SessionsUnavailable {
							kind,
							info: Some(SessionsUnavailableInfo {
								window_start: fresh_start,
								window_end: session_index,
								block_hash,
							}),
						})
					},
					Ok(s) => {
						let update = SessionWindowUpdate::Advanced {
							prev_window_start: old_window_start,
							prev_window_end: old_window_end,
							new_window_start: window_start,
							new_window_end: session_index,
						};

						let outdated = std::cmp::min(overlap_start as usize, self.session_info.len());
						self.session_info.drain(..outdated);
						self.session_info.extend(s);
						// we need to account for this case:
						// window_start ................................... session_index
						//              old_window_start ........... latest
						let new_earliest = std::cmp::max(window_start, old_window_start);
						self.earliest_session = Some(new_earliest);

						Ok(update)
					}
				}
			}
		}
	}
}

async fn load_all_sessions(
	ctx: &mut impl overseer::SubsystemContext,
	block_hash: Hash,
	start: SessionIndex,
	end_inclusive: SessionIndex,
) -> Result<Vec<SessionInfo>, SessionsUnavailableKind> {
	let mut v = Vec::new();
	for i in start..=end_inclusive {
		let (tx, rx)= oneshot::channel();
		ctx.send_message(RuntimeApiMessage::Request(
			block_hash,
			RuntimeApiRequest::SessionInfo(i, tx),
		)).await;

		let session_info = match rx.await {
			Ok(Ok(Some(s))) => s,
			Ok(Ok(None)) => {
				return Err(SessionsUnavailableKind::Missing);
			}
			Ok(Err(e)) => return Err(SessionsUnavailableKind::RuntimeApi(e)),
			Err(canceled) => return Err(SessionsUnavailableKind::RuntimeApiUnavailable(canceled)),
		};

		v.push(session_info);
	}

	Ok(v)
}

#[cfg(test)]
mod tests {
	use super::*;
	use polkadot_node_subsystem_test_helpers::make_subsystem_context;
	use polkadot_node_subsystem::messages::{AllMessages, AvailabilityRecoveryMessage};
	use sp_core::testing::TaskExecutor;
	use assert_matches::assert_matches;

	const TEST_WINDOW_SIZE: SessionIndex = 6;

	fn dummy_session_info(index: SessionIndex) -> SessionInfo {
		SessionInfo {
			validators: Vec::new(),
			discovery_keys: Vec::new(),
			assignment_keys: Vec::new(),
			validator_groups: Vec::new(),
			n_cores: index as _,
			zeroth_delay_tranche_width: index as _,
			relay_vrf_modulo_samples: index as _,
			n_delay_tranches: index as _,
			no_show_slots: index as _,
			needed_approvals: index as _,
		}
	}

	fn cache_session_info_test(
		expected_start_session: SessionIndex,
		session: SessionIndex,
		mut window: RollingSessionWindow,
		expect_requests_from: SessionIndex,
	) {
		let header = Header {
			digest: Default::default(),
			extrinsics_root: Default::default(),
			number: 5,
			state_root: Default::default(),
			parent_hash: Default::default(),
		};

		let pool = TaskExecutor::new();
		let (mut ctx, mut handle) = make_subsystem_context::<AvailabilityRecoveryMessage, _>(pool.clone());

		let hash = header.hash();

		let test_fut = {
			let header = header.clone();
			Box::pin(async move {
				window.cache_session_info_for_head(
					&mut ctx,
					hash,
					&header,
				).await.unwrap();

				assert_eq!(window.earliest_session, Some(expected_start_session));
				assert_eq!(
					window.session_info,
					(expected_start_session..=session).map(dummy_session_info).collect::<Vec<_>>(),
				);
			})
		};

		let aux_fut = Box::pin(async move {
			assert_matches!(
				handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					h,
					RuntimeApiRequest::SessionIndexForChild(s_tx),
				)) => {
					assert_eq!(h, header.parent_hash);
					let _ = s_tx.send(Ok(session));
				}
			);

			for i in expect_requests_from..=session {
				assert_matches!(
					handle.recv().await,
					AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						h,
						RuntimeApiRequest::SessionInfo(j, s_tx),
					)) => {
						assert_eq!(h, hash);
						assert_eq!(i, j);
						let _ = s_tx.send(Ok(Some(dummy_session_info(i))));
					}
				);
			}
		});

		futures::executor::block_on(futures::future::join(test_fut, aux_fut));
	}

	#[test]
	fn cache_session_info_first_early() {
		cache_session_info_test(
			0,
			1,
			RollingSessionWindow::new(TEST_WINDOW_SIZE),
			0,
		);
	}

	#[test]
	fn cache_session_info_does_not_underflow() {
		let window = RollingSessionWindow {
			earliest_session: Some(1),
			session_info: vec![dummy_session_info(1)],
			window_size: TEST_WINDOW_SIZE,
		};

		cache_session_info_test(
			1,
			2,
			window,
			2,
		);
	}

	#[test]
	fn cache_session_info_first_late() {
		cache_session_info_test(
			(100 as SessionIndex).saturating_sub(TEST_WINDOW_SIZE - 1),
			100,
			RollingSessionWindow::new(TEST_WINDOW_SIZE),
			(100 as SessionIndex).saturating_sub(TEST_WINDOW_SIZE - 1),
		);
	}

	#[test]
	fn cache_session_info_jump() {
		let window = RollingSessionWindow {
			earliest_session: Some(50),
			session_info: vec![dummy_session_info(50), dummy_session_info(51), dummy_session_info(52)],
			window_size: TEST_WINDOW_SIZE,
		};

		cache_session_info_test(
			(100 as SessionIndex).saturating_sub(TEST_WINDOW_SIZE - 1),
			100,
			window,
			(100 as SessionIndex).saturating_sub(TEST_WINDOW_SIZE - 1),
		);
	}

	#[test]
	fn cache_session_info_roll_full() {
		let start = 99 - (TEST_WINDOW_SIZE - 1);
		let window = RollingSessionWindow {
			earliest_session: Some(start),
			session_info: (start..=99).map(dummy_session_info).collect(),
			window_size: TEST_WINDOW_SIZE,
		};

		cache_session_info_test(
			(100 as SessionIndex).saturating_sub(TEST_WINDOW_SIZE - 1),
			100,
			window,
			100, // should only make one request.
		);
	}

	#[test]
	fn cache_session_info_roll_many_full() {
		let start = 97 - (TEST_WINDOW_SIZE - 1);
		let window = RollingSessionWindow {
			earliest_session: Some(start),
			session_info: (start..=97).map(dummy_session_info).collect(),
			window_size: TEST_WINDOW_SIZE,
		};

		cache_session_info_test(
			(100 as SessionIndex).saturating_sub(TEST_WINDOW_SIZE - 1),
			100,
			window,
			98,
		);
	}

	#[test]
	fn cache_session_info_roll_early() {
		let start = 0;
		let window = RollingSessionWindow {
			earliest_session: Some(start),
			session_info: (0..=1).map(dummy_session_info).collect(),
			window_size: TEST_WINDOW_SIZE,
		};

		cache_session_info_test(
			0,
			2,
			window,
			2, // should only make one request.
		);
	}

	#[test]
	fn cache_session_info_roll_many_early() {
		let start = 0;
		let window = RollingSessionWindow {
			earliest_session: Some(start),
			session_info: (0..=1).map(dummy_session_info).collect(),
			window_size: TEST_WINDOW_SIZE,
		};

		cache_session_info_test(
			0,
			3,
			window,
			2,
		);
	}

	#[test]
	fn any_session_unavailable_for_caching_means_no_change() {
		let session: SessionIndex = 6;
		let start_session = session.saturating_sub(TEST_WINDOW_SIZE - 1);

		let header = Header {
			digest: Default::default(),
			extrinsics_root: Default::default(),
			number: 5,
			state_root: Default::default(),
			parent_hash: Default::default(),
		};

		let pool = TaskExecutor::new();
		let (mut ctx, mut handle) = make_subsystem_context::<(), _>(pool.clone());

		let mut window = RollingSessionWindow::new(TEST_WINDOW_SIZE);
		let hash = header.hash();

		let test_fut = {
			let header = header.clone();
			Box::pin(async move {
				let res = window.cache_session_info_for_head(
					&mut ctx,
					hash,
					&header,
				).await;

				assert!(res.is_err());
			})
		};

		let aux_fut = Box::pin(async move {
			assert_matches!(
				handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					h,
					RuntimeApiRequest::SessionIndexForChild(s_tx),
				)) => {
					assert_eq!(h, header.parent_hash);
					let _ = s_tx.send(Ok(session));
				}
			);

			for i in start_session..=session {
				assert_matches!(
					handle.recv().await,
					AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						h,
						RuntimeApiRequest::SessionInfo(j, s_tx),
					)) => {
						assert_eq!(h, hash);
						assert_eq!(i, j);

						let _ = s_tx.send(Ok(if i == session {
							None
						} else {
							Some(dummy_session_info(i))
						}));
					}
				);
			}
		});

		futures::executor::block_on(futures::future::join(test_fut, aux_fut));
	}

	#[test]
	fn request_session_info_for_genesis() {
		let session: SessionIndex = 0;

		let header = Header {
			digest: Default::default(),
			extrinsics_root: Default::default(),
			number: 0,
			state_root: Default::default(),
			parent_hash: Default::default(),
		};

		let pool = TaskExecutor::new();
		let (mut ctx, mut handle) = make_subsystem_context::<(), _>(pool.clone());

		let mut window = RollingSessionWindow::new(TEST_WINDOW_SIZE);
		let hash = header.hash();

		let test_fut = {
			let header = header.clone();
			Box::pin(async move {
				window.cache_session_info_for_head(
					&mut ctx,
					hash,
					&header,
				).await.unwrap();

				assert_eq!(window.earliest_session, Some(session));
				assert_eq!(
					window.session_info,
					vec![dummy_session_info(session)],
				);
			})
		};

		let aux_fut = Box::pin(async move {
			assert_matches!(
				handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					h,
					RuntimeApiRequest::SessionIndexForChild(s_tx),
				)) => {
					assert_eq!(h, hash);
					let _ = s_tx.send(Ok(session));
				}
			);

			assert_matches!(
				handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					h,
					RuntimeApiRequest::SessionInfo(s, s_tx),
				)) => {
					assert_eq!(h, hash);
					assert_eq!(s, session);

					let _ = s_tx.send(Ok(Some(dummy_session_info(s))));
				}
			);
		});

		futures::executor::block_on(futures::future::join(test_fut, aux_fut));
	}
}
