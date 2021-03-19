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

//! Polkadot Jaeger span definitions.
//!
//! ```rust
//! unimplemented!("How to use jaeger span definitions?");
//! ```

use polkadot_primitives::v1::{CandidateHash, Hash, PoV, ValidatorIndex, BlakeTwo256, HashT, Id as ParaId};
use parity_scale_codec::Encode;
use sc_network::PeerId;

use std::fmt;
use std::sync::Arc;

use super::INSTANCE;

/// A special "per leaf span".
///
/// Essentially this span wraps two spans:
///
/// 1. The span that is created per leaf in the overseer.
/// 2. Some child span of the per-leaf span.
///
/// This just works as auxiliary structure to easily store both.
#[derive(Debug)]
pub struct PerLeafSpan {
	leaf_span: Arc<Span>,
	span: Span,
}

impl PerLeafSpan {
	/// Creates a new instance.
	///
	/// Takes the `leaf_span` that is created by the overseer per leaf and a name for a child span.
	/// Both will be stored in this object, while the child span is implicitly accessible by using the
	/// [`Deref`](std::ops::Deref) implementation.
	pub fn new(leaf_span: Arc<Span>, name: &'static str) -> Self {
		let span = leaf_span.child(name);

		Self {
			span,
			leaf_span,
		}
	}

	/// Returns the leaf span.
	pub fn leaf_span(&self) -> &Arc<Span> {
		&self.leaf_span
	}
}

/// Returns a reference to the child span.
impl std::ops::Deref for PerLeafSpan {
	type Target = Span;

	fn deref(&self) -> &Span {
		&self.span
	}
}


/// A helper to annotate the stage with a numerical value
/// to ease the life of the tooling team creating viable
/// statistical metrics for which stage of the inclusion
/// pipeline drops a significant amount of candidates,
/// statistically speaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum Stage {
	CandidateSelection = 1,
	CandidateBacking = 2,
	StatementDistribution = 3,
	PoVDistribution = 4,
	AvailabilityDistribution = 5,
	AvailabilityRecovery = 6,
	BitfieldDistribution = 7,
	ApprovalChecking = 8,
	// Expand as needed, numbers should be ascending according to the stage
	// through the inclusion pipeline, or according to the descriptions
	// in [the path of a para chain block]
	// (https://polkadot.network/the-path-of-a-parachain-block/)
	// see [issue](https://github.com/paritytech/polkadot/issues/2389)
}


/// A wrapper type for a span.
///
/// Handles running with and without jaeger.
pub enum Span {
	/// Running with jaeger being enabled.
	Enabled(mick_jaeger::Span),
	/// Running with jaeger disabled.
	Disabled,
}

/// Alias for the 16 byte unique identifier used with jaeger.
pub(crate) type TraceIdentifier = u128;

/// A helper to convert the hash to the fixed size representation
/// needed for jaeger.
#[inline]
fn hash_to_identifier(hash: Hash) -> TraceIdentifier {
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&hash.as_ref()[0..16]);
    u128::from_be_bytes(buf) as TraceIdentifier
}

/// Helper to unify lazy proxy evaluation.
pub trait LazyIdent {
    /// Evaluate the type to a unique trace identifier.
	/// Called lazily on demand.
    fn eval(&self) -> TraceIdentifier;

    /// Annotate a new root item with these additional spans
	/// at construction.
    fn extra_tags(&self, _span: &mut Span) {
    }
}

impl<'a> LazyIdent for &'a [u8] {
    fn eval(&self) -> TraceIdentifier {
        hash_to_identifier(BlakeTwo256::hash_of(self))
    }
}

impl LazyIdent for PoV {
    fn eval(&self) -> TraceIdentifier {
        hash_to_identifier(self.hash())
    }

    fn extra_tags(&self, span: &mut Span) {
        span.add_pov(self)
    }
}

impl LazyIdent for Hash {
    fn eval(&self) -> TraceIdentifier {
        hash_to_identifier(*self)
    }

    fn extra_tags(&self, span: &mut Span) {
        span.add_string_fmt_debug_tag("relay-parent", self.0);
    }
}

impl LazyIdent for CandidateHash {
    fn eval(&self) -> TraceIdentifier {
        hash_to_identifier(self.0)
    }

    fn extra_tags(&self, span: &mut Span) {
        span.add_string_fmt_debug_tag("candidate-hash", &self.0);
    }
}


impl Span {
    /// Creates a new span builder based on anything that can be lazily evaluated
    /// to and identifier.
    pub fn new<I: LazyIdent>(identifier: I, span_name: &'static str) -> Span {
		let mut span = INSTANCE.read_recursive().span(
			|| { <I as LazyIdent>::eval(&identifier) },
			span_name,
		).into();
		<I as LazyIdent>::extra_tags(&identifier, &mut span);
		span
    }

    /// Creates a new span builder based on an encodable type.
    /// The encoded bytes are then used to derive the true trace identifier.
    pub fn from_encodable<I: Encode>(identifier: I, span_name: &'static str) -> Span {
		INSTANCE.read_recursive().span(
			move || {
				let bytes = identifier.encode();
				LazyIdent::eval(&bytes.as_slice())
			},
			span_name,
		).into()
	}

	/// Derive a child span from `self`.
	pub fn child(&self, name: &'static str) -> Self {
		match self {
			Self::Enabled(inner) => Self::Enabled(inner.child(name)),
			Self::Disabled => Self::Disabled,
		}
	}

	/// Derive a child span from `self` but add a candidate annotation.
	/// A shortcut for
	///
	/// ```rust,no_run
	/// # use polkadot_primitives::v1::CandidateHash;
	/// # use polkadot_node_jaeger::candidate_Span::new;
	/// # let hash = CandidateHash::default();
	/// # let span = candidate_Span::new(&hash, "foo");
	/// let _span = span.child("name").with_candidate(&hash);
	/// // equiv
	/// let _span = span.child_with_candidate("name", &hash);
	/// ```
	#[inline(always)]
	pub fn child_with_candidate(&self, name: &'static str, candidate_hash: &CandidateHash) -> Self {
		self.child(name).with_candidate(candidate_hash)
	}


	#[inline(always)]
	pub fn with_peer_id(mut self, peer: &PeerId) -> Self {
		self.add_string_tag("peer-id", &peer.to_base58());
		self
	}

	/// Attach a candidate hash to the span.
	#[inline(always)]
	pub fn with_candidate(mut self, candidate_hash: &CandidateHash) -> Self  {
		self.add_string_fmt_debug_tag("candidate-hash", &candidate_hash.0);
		self
	}

	/// Attach a para-id to the span.
	#[inline(always)]
	pub fn with_para_id(mut self, para_id: ParaId) -> Self {
		self.add_int_tag("para-id", u32::from(para_id) as i64);
		self
	}

	/// Attach a candidate stage.
	/// Should always come with a `CandidateHash`.
	#[inline(always)]
	pub fn with_stage(mut self, stage: Stage) -> Self {
		self.add_string_tag("candidate-stage", stage as u8);
		self
	}

	#[inline(always)]
	pub fn with_validator_index(mut self, validator: ValidatorIndex) -> Self {
		self.add_string_tag("validator-index", &validator.0);
		self
	}

	#[inline(always)]
	pub fn with_chunk_index(mut self, chunk_index: u32) -> Self {
		self.add_string_tag("chunk-index", chunk_index);
		self
	}

	#[inline(always)]
	pub fn with_relay_parent(mut self, relay_parent: &Hash) -> Self {
		self.add_string_fmt_debug_tag("relay-parent", relay_parent);
		self
	}

	#[inline(always)]
	pub fn with_claimed_validator_index(mut self, claimed_validator_index: ValidatorIndex) -> Self {
		self.add_string_tag(
			"claimed-validator",
			&claimed_validator_index.0,
		);
		self
	}

	#[inline(always)]
	pub fn with_pov(mut self, pov: &PoV) -> Self {
		self.add_pov(pov);
		self
	}

    /// Add meta tag proof of validity hash.
	pub(crate) fn add_pov(&mut self, pov: &PoV) {
		if self.is_enabled() {
			// avoid computing the pov hash if jaeger is not enabled
			self.add_string_fmt_debug_tag("pov", pov.hash());
		}
	}

    #[inline]
    pub fn add_string_tag<V: ToString>(&mut self, tag: &'static str, val: V) {
        match self {
			Self::Enabled(ref mut inner) => inner.add_string_tag(tag, val.to_string().as_str()),
			Self::Disabled => {},
		}
    }

    #[inline]
    pub fn add_string_fmt_debug_tag<V: fmt::Debug>(&mut self, tag: &'static str, val: V) {
        self.add_string_tag(tag, format!("{:?}", val));
    }

	/// Add an additional int tag to the span.
	///
	/// Should be used sparingly, introduction of new types is prefered.
	pub fn add_int_tag(&mut self, tag: &'static str, value: i64) {
		match self {
			Self::Enabled(ref mut inner) => inner.add_int_tag(tag, value),
			Self::Disabled => {},
		}
	}

	/// Adds the `FollowsFrom` relationship to this span with respect to the given one.
	pub fn add_follows_from(&mut self, other: &Self) {
		match (self, other) {
			(Self::Enabled(ref mut inner), Self::Enabled(ref other_inner)) => inner.add_follows_from(&other_inner),
			_ => {},
		}
	}

	/// Helper to check whether jaeger is enabled
	/// in order to avoid computational overhead.
	pub const fn is_enabled(&self) -> bool {
		match self {
			Span::Enabled(_) => true,
			_ => false,
		}
	}
}

impl std::fmt::Debug for Span {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		write!(f, "<jaeger span>")
	}
}

impl From<Option<mick_jaeger::Span>> for Span {
	fn from(src: Option<mick_jaeger::Span>) -> Self {
		if let Some(span) = src {
			Self::Enabled(span)
		} else {
			Self::Disabled
		}
	}
}

impl From<mick_jaeger::Span> for Span {
	fn from(src: mick_jaeger::Span) -> Self {
		Self::Enabled(src)
	}
}
