// Copyright 2020-2021 Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

//! Cross-Consensus Message format data structures.

use core::{result, mem, convert::TryFrom};

use parity_scale_codec::{self, Encode, Decode};
use super::Junction;
use crate::VersionedMultiLocation;

/// A relative path between state-bearing consensus systems.
///
/// A location in a consensus system is defined as an *isolatable state machine* held within global consensus. The
/// location in question need not have a sophisticated consensus algorithm of its own; a single account within
/// Ethereum, for example, could be considered a location.
///
/// A very-much non-exhaustive list of types of location include:
/// - A (normal, layer-1) block chain, e.g. the Bitcoin mainnet or a parachain.
/// - A layer-0 super-chain, e.g. the Polkadot Relay chain.
/// - A layer-2 smart contract, e.g. an ERC-20 on Ethereum.
/// - A logical functional component of a chain, e.g. a single instance of a pallet on a Frame-based Substrate chain.
/// - An account.
///
/// A `MultiLocation` is a *relative identifier*, meaning that it can only be used to define the relative path
/// between two locations, and cannot generally be used to refer to a location universally. It is comprised of a
/// number of *junctions*, each morphing the previous location, either diving down into one of its internal locations,
/// called a *sub-consensus*, or going up into its parent location. Correct `MultiLocation` values must have all
/// `Parent` junctions as a prefix to all *sub-consensus* junctions.
///
/// This specific `MultiLocation` implementation uses a Rust `enum` in order to make pattern matching easier.
///
/// The `MultiLocation` value of `Null` simply refers to the interpreting consensus system.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, Debug)]
pub enum MultiLocation {
	/// The interpreting consensus system.
	Null,
	/// A relative path comprising 1 junction.
	X1(Junction),
	/// A relative path comprising 2 junctions.
	X2(Junction, Junction),
	/// A relative path comprising 3 junctions.
	X3(Junction, Junction, Junction),
	/// A relative path comprising 4 junctions.
	X4(Junction, Junction, Junction, Junction),
	/// A relative path comprising 5 junctions.
	X5(Junction, Junction, Junction, Junction, Junction),
	/// A relative path comprising 6 junctions.
	X6(Junction, Junction, Junction, Junction, Junction, Junction),
	/// A relative path comprising 7 junctions.
	X7(Junction, Junction, Junction, Junction, Junction, Junction, Junction),
	/// A relative path comprising 8 junctions.
	X8(Junction, Junction, Junction, Junction, Junction, Junction, Junction, Junction),
}

/// Maximum number of junctions a multilocation can contain.
pub const MAX_MULTILOCATION_LENGTH: usize = 8;

impl From<Junction> for MultiLocation {
	fn from(x: Junction) -> Self {
		MultiLocation::X1(x)
	}
}

impl From<()> for MultiLocation {
	fn from(_: ()) -> Self {
		MultiLocation::Null
	}
}
impl From<(Junction,)> for MultiLocation {
	fn from(x: (Junction,)) -> Self {
		MultiLocation::X1(x.0)
	}
}
impl From<(Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction)) -> Self {
		MultiLocation::X2(x.0, x.1)
	}
}
impl From<(Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction)) -> Self {
		MultiLocation::X3(x.0, x.1, x.2)
	}
}
impl From<(Junction, Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction, Junction)) -> Self {
		MultiLocation::X4(x.0, x.1, x.2, x.3)
	}
}
impl From<(Junction, Junction, Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction, Junction, Junction)) -> Self {
		MultiLocation::X5(x.0, x.1, x.2, x.3, x.4)
	}
}
impl From<(Junction, Junction, Junction, Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction, Junction, Junction, Junction)) -> Self {
		MultiLocation::X6(x.0, x.1, x.2, x.3, x.4, x.5)
	}
}
impl From<(Junction, Junction, Junction, Junction, Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction, Junction, Junction, Junction, Junction)) -> Self {
		MultiLocation::X7(x.0, x.1, x.2, x.3, x.4, x.5, x.6)
	}
}
impl From<(Junction, Junction, Junction, Junction, Junction, Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction, Junction, Junction, Junction, Junction, Junction)) -> Self {
		MultiLocation::X8(x.0, x.1, x.2, x.3, x.4, x.5, x.6, x.7)
	}
}

impl From<[Junction; 0]> for MultiLocation {
	fn from(_: [Junction; 0]) -> Self {
		MultiLocation::Null
	}
}
impl From<[Junction; 1]> for MultiLocation {
	fn from(x: [Junction; 1]) -> Self {
		let [x0] = x;
		MultiLocation::X1(x0)
	}
}
impl From<[Junction; 2]> for MultiLocation {
	fn from(x: [Junction; 2]) -> Self {
		let [x0, x1] = x;
		MultiLocation::X2(x0, x1)
	}
}
impl From<[Junction; 3]> for MultiLocation {
	fn from(x: [Junction; 3]) -> Self {
		let [x0, x1, x2] = x;
		MultiLocation::X3(x0, x1, x2)
	}
}
impl From<[Junction; 4]> for MultiLocation {
	fn from(x: [Junction; 4]) -> Self {
		let [x0, x1, x2, x3] = x;
		MultiLocation::X4(x0, x1, x2, x3)
	}
}
impl From<[Junction; 5]> for MultiLocation {
	fn from(x: [Junction; 5]) -> Self {
		let [x0, x1, x2, x3, x4] = x;
		MultiLocation::X5(x0, x1, x2, x3, x4)
	}
}
impl From<[Junction; 6]> for MultiLocation {
	fn from(x: [Junction; 6]) -> Self {
		let [x0, x1, x2, x3, x4, x5] = x;
		MultiLocation::X6(x0, x1, x2, x3, x4, x5)
	}
}
impl From<[Junction; 7]> for MultiLocation {
	fn from(x: [Junction; 7]) -> Self {
		let [x0, x1, x2, x3, x4, x5, x6] = x;
		MultiLocation::X7(x0, x1, x2, x3, x4, x5, x6)
	}
}
impl From<[Junction; 8]> for MultiLocation {
	fn from(x: [Junction; 8]) -> Self {
		let [x0, x1, x2, x3, x4, x5, x6, x7] = x;
		MultiLocation::X8(x0, x1, x2, x3, x4, x5, x6, x7)
	}
}

pub struct MultiLocationIterator(MultiLocation);
impl Iterator for MultiLocationIterator {
	type Item = Junction;
	fn next(&mut self) -> Option<Junction> {
		self.0.take_first()
	}
}

pub struct MultiLocationReverseIterator(MultiLocation);
impl Iterator for MultiLocationReverseIterator {
	type Item = Junction;
	fn next(&mut self) -> Option<Junction> {
		self.0.take_last()
	}
}

pub struct MultiLocationRefIterator<'a>(&'a MultiLocation, usize);
impl<'a> Iterator for MultiLocationRefIterator<'a> {
	type Item = &'a Junction;
	fn next(&mut self) -> Option<&'a Junction> {
		let result = self.0.at(self.1);
		self.1 += 1;
		result
	}
}

pub struct MultiLocationReverseRefIterator<'a>(&'a MultiLocation, usize);
impl<'a> Iterator for MultiLocationReverseRefIterator<'a> {
	type Item = &'a Junction;
	fn next(&mut self) -> Option<&'a Junction> {
		self.1 += 1;
		self.0.at(self.0.len().checked_sub(self.1)?)
	}
}

impl MultiLocation {
	/// Returns first junction, or `None` if the location is empty.
	pub fn first(&self) -> Option<&Junction> {
		match &self {
			MultiLocation::Null => None,
			MultiLocation::X1(ref a) => Some(a),
			MultiLocation::X2(ref a, ..) => Some(a),
			MultiLocation::X3(ref a, ..) => Some(a),
			MultiLocation::X4(ref a, ..) => Some(a),
			MultiLocation::X5(ref a, ..) => Some(a),
			MultiLocation::X6(ref a, ..) => Some(a),
			MultiLocation::X7(ref a, ..) => Some(a),
			MultiLocation::X8(ref a, ..) => Some(a),
		}
	}

	/// Returns last junction, or `None` if the location is empty.
	pub fn last(&self) -> Option<&Junction> {
		match &self {
			MultiLocation::Null => None,
			MultiLocation::X1(ref a) => Some(a),
			MultiLocation::X2(.., ref a) => Some(a),
			MultiLocation::X3(.., ref a) => Some(a),
			MultiLocation::X4(.., ref a) => Some(a),
			MultiLocation::X5(.., ref a) => Some(a),
			MultiLocation::X6(.., ref a) => Some(a),
			MultiLocation::X7(.., ref a) => Some(a),
			MultiLocation::X8(.., ref a) => Some(a),
		}
	}

	/// Splits off the first junction, returning the remaining suffix (first item in tuple) and the first element
	/// (second item in tuple) or `None` if it was empty.
	pub fn split_first(self) -> (MultiLocation, Option<Junction>) {
		match self {
			MultiLocation::Null => (MultiLocation::Null, None),
			MultiLocation::X1(a) => (MultiLocation::Null, Some(a)),
			MultiLocation::X2(a, b) => (MultiLocation::X1(b), Some(a)),
			MultiLocation::X3(a, b, c) => (MultiLocation::X2(b, c), Some(a)),
			MultiLocation::X4(a, b, c ,d) => (MultiLocation::X3(b, c, d), Some(a)),
			MultiLocation::X5(a, b, c ,d, e) => (MultiLocation::X4(b, c, d, e), Some(a)),
			MultiLocation::X6(a, b, c ,d, e, f) => (MultiLocation::X5(b, c, d, e, f), Some(a)),
			MultiLocation::X7(a, b, c ,d, e, f, g) => (MultiLocation::X6(b, c, d, e, f, g), Some(a)),
			MultiLocation::X8(a, b, c ,d, e, f, g, h) => (MultiLocation::X7(b, c, d, e, f, g, h), Some(a)),
		}
	}

	/// Splits off the last junction, returning the remaining prefix (first item in tuple) and the last element
	/// (second item in tuple) or `None` if it was empty.
	pub fn split_last(self) -> (MultiLocation, Option<Junction>) {
		match self {
			MultiLocation::Null => (MultiLocation::Null, None),
			MultiLocation::X1(a) => (MultiLocation::Null, Some(a)),
			MultiLocation::X2(a, b) => (MultiLocation::X1(a), Some(b)),
			MultiLocation::X3(a, b, c) => (MultiLocation::X2(a, b), Some(c)),
			MultiLocation::X4(a, b, c ,d) => (MultiLocation::X3(a, b, c), Some(d)),
			MultiLocation::X5(a, b, c, d, e) => (MultiLocation::X4(a, b, c, d), Some(e)),
			MultiLocation::X6(a, b, c, d, e, f) => (MultiLocation::X5(a, b, c, d, e), Some(f)),
			MultiLocation::X7(a, b, c, d, e, f, g) => (MultiLocation::X6(a, b, c, d, e, f), Some(g)),
			MultiLocation::X8(a, b, c, d, e, f, g, h) => (MultiLocation::X7(a, b, c, d, e, f, g), Some(h)),
		}
	}

	/// Removes the first element from `self`, returning it (or `None` if it was empty).
	pub fn take_first(&mut self) -> Option<Junction> {
		let mut d = MultiLocation::Null;
		mem::swap(&mut *self, &mut d);
		let (tail, head) = d.split_first();
		*self = tail;
		head
	}

	/// Removes the last element from `self`, returning it (or `None` if it was empty).
	pub fn take_last(&mut self) -> Option<Junction> {
		let mut d = MultiLocation::Null;
		mem::swap(&mut *self, &mut d);
		let (head, tail) = d.split_last();
		*self = head;
		tail
	}

	/// Consumes `self` and returns a `MultiLocation` suffixed with `new`, or an `Err` with the original value of
	/// `self` in case of overflow.
	pub fn pushed_with(self, new: Junction) -> result::Result<Self, Self> {
		Ok(match self {
			MultiLocation::Null => MultiLocation::X1(new),
			MultiLocation::X1(a) => MultiLocation::X2(a, new),
			MultiLocation::X2(a, b) => MultiLocation::X3(a, b, new),
			MultiLocation::X3(a, b, c) => MultiLocation::X4(a, b, c, new),
			MultiLocation::X4(a, b, c, d) => MultiLocation::X5(a, b, c, d, new),
			MultiLocation::X5(a, b, c, d, e) => MultiLocation::X6(a, b, c, d, e, new),
			MultiLocation::X6(a, b, c, d, e, f) => MultiLocation::X7(a, b, c, d, e, f, new),
			MultiLocation::X7(a, b, c, d, e, f, g) => MultiLocation::X8(a, b, c, d, e, f, g, new),
			s => Err(s)?,
		})
	}

	/// Consumes `self` and returns a `MultiLocation` prefixed with `new`, or an `Err` with the original value of
	/// `self` in case of overflow.
	pub fn pushed_front_with(self, new: Junction) -> result::Result<Self, Self> {
		Ok(match self {
			MultiLocation::Null => MultiLocation::X1(new),
			MultiLocation::X1(a) => MultiLocation::X2(new, a),
			MultiLocation::X2(a, b) => MultiLocation::X3(new, a, b),
			MultiLocation::X3(a, b, c) => MultiLocation::X4(new, a, b, c),
			MultiLocation::X4(a, b, c, d) => MultiLocation::X5(new, a, b, c, d),
			MultiLocation::X5(a, b, c, d, e) => MultiLocation::X6(new, a, b, c, d, e),
			MultiLocation::X6(a, b, c, d, e, f) => MultiLocation::X7(new, a, b, c, d, e, f),
			MultiLocation::X7(a, b, c, d, e, f, g) => MultiLocation::X8(new, a, b, c, d, e, f, g),
			s => Err(s)?,
		})
	}

	/// Returns the number of junctions in `self`.
	pub fn len(&self) -> usize {
		match &self {
			MultiLocation::Null => 0,
			MultiLocation::X1(..) => 1,
			MultiLocation::X2(..) => 2,
			MultiLocation::X3(..) => 3,
			MultiLocation::X4(..) => 4,
			MultiLocation::X5(..) => 5,
			MultiLocation::X6(..) => 6,
			MultiLocation::X7(..) => 7,
			MultiLocation::X8(..) => 8,
		}
	}

	/// Returns the junction at index `i`, or `None` if the location doesn't contain that many elements.
	pub fn at(&self, i: usize) -> Option<&Junction> {
		Some(match (i, &self) {
			(0, MultiLocation::X1(ref a)) => a,
			(0, MultiLocation::X2(ref a, ..)) => a,
			(0, MultiLocation::X3(ref a, ..)) => a,
			(0, MultiLocation::X4(ref a, ..)) => a,
			(0, MultiLocation::X5(ref a, ..)) => a,
			(0, MultiLocation::X6(ref a, ..)) => a,
			(0, MultiLocation::X7(ref a, ..)) => a,
			(0, MultiLocation::X8(ref a, ..)) => a,
			(1, MultiLocation::X2(_, ref a)) => a,
			(1, MultiLocation::X3(_, ref a, ..)) => a,
			(1, MultiLocation::X4(_, ref a, ..)) => a,
			(1, MultiLocation::X5(_, ref a, ..)) => a,
			(1, MultiLocation::X6(_, ref a, ..)) => a,
			(1, MultiLocation::X7(_, ref a, ..)) => a,
			(1, MultiLocation::X8(_, ref a, ..)) => a,
			(2, MultiLocation::X3(_, _, ref a)) => a,
			(2, MultiLocation::X4(_, _, ref a, ..)) => a,
			(2, MultiLocation::X5(_, _, ref a, ..)) => a,
			(2, MultiLocation::X6(_, _, ref a, ..)) => a,
			(2, MultiLocation::X7(_, _, ref a, ..)) => a,
			(2, MultiLocation::X8(_, _, ref a, ..)) => a,
			(3, MultiLocation::X4(_, _, _, ref a)) => a,
			(3, MultiLocation::X5(_, _, _, ref a, ..)) => a,
			(3, MultiLocation::X6(_, _, _, ref a, ..)) => a,
			(3, MultiLocation::X7(_, _, _, ref a, ..)) => a,
			(3, MultiLocation::X8(_, _, _, ref a, ..)) => a,
			(4, MultiLocation::X5(_, _, _, _, ref a)) => a,
			(4, MultiLocation::X6(_, _, _, _, ref a, ..)) => a,
			(4, MultiLocation::X7(_, _, _, _, ref a, ..)) => a,
			(4, MultiLocation::X8(_, _, _, _, ref a, ..)) => a,
			(5, MultiLocation::X6(_, _, _, _, _, ref a)) => a,
			(5, MultiLocation::X7(_, _, _, _, _, ref a, ..)) => a,
			(5, MultiLocation::X8(_, _, _, _, _, ref a, ..)) => a,
			(6, MultiLocation::X7(_, _, _, _, _, _, ref a)) => a,
			(6, MultiLocation::X8(_, _, _, _, _, _, ref a, ..)) => a,
			(7, MultiLocation::X8(_, _, _, _, _, _, _, ref a)) => a,
			_ => return None,
		})
	}

	/// Returns a mutable reference to the junction at index `i`, or `None` if the location doesn't contain that many
	/// elements.
	pub fn at_mut(&mut self, i: usize) -> Option<&mut Junction> {
		Some(match (i, self) {
			(0, MultiLocation::X1(ref mut a)) => a,
			(0, MultiLocation::X2(ref mut a, ..)) => a,
			(0, MultiLocation::X3(ref mut a, ..)) => a,
			(0, MultiLocation::X4(ref mut a, ..)) => a,
			(0, MultiLocation::X5(ref mut a, ..)) => a,
			(0, MultiLocation::X6(ref mut a, ..)) => a,
			(0, MultiLocation::X7(ref mut a, ..)) => a,
			(0, MultiLocation::X8(ref mut a, ..)) => a,
			(1, MultiLocation::X2(_, ref mut a)) => a,
			(1, MultiLocation::X3(_, ref mut a, ..)) => a,
			(1, MultiLocation::X4(_, ref mut a, ..)) => a,
			(1, MultiLocation::X5(_, ref mut a, ..)) => a,
			(1, MultiLocation::X6(_, ref mut a, ..)) => a,
			(1, MultiLocation::X7(_, ref mut a, ..)) => a,
			(1, MultiLocation::X8(_, ref mut a, ..)) => a,
			(2, MultiLocation::X3(_, _, ref mut a)) => a,
			(2, MultiLocation::X4(_, _, ref mut a, ..)) => a,
			(2, MultiLocation::X5(_, _, ref mut a, ..)) => a,
			(2, MultiLocation::X6(_, _, ref mut a, ..)) => a,
			(2, MultiLocation::X7(_, _, ref mut a, ..)) => a,
			(2, MultiLocation::X8(_, _, ref mut a, ..)) => a,
			(3, MultiLocation::X4(_, _, _, ref mut a)) => a,
			(3, MultiLocation::X5(_, _, _, ref mut a, ..)) => a,
			(3, MultiLocation::X6(_, _, _, ref mut a, ..)) => a,
			(3, MultiLocation::X7(_, _, _, ref mut a, ..)) => a,
			(3, MultiLocation::X8(_, _, _, ref mut a, ..)) => a,
			(4, MultiLocation::X5(_, _, _, _, ref mut a)) => a,
			(4, MultiLocation::X6(_, _, _, _, ref mut a, ..)) => a,
			(4, MultiLocation::X7(_, _, _, _, ref mut a, ..)) => a,
			(4, MultiLocation::X8(_, _, _, _, ref mut a, ..)) => a,
			(5, MultiLocation::X6(_, _, _, _, _, ref mut a)) => a,
			(5, MultiLocation::X7(_, _, _, _, _, ref mut a, ..)) => a,
			(5, MultiLocation::X8(_, _, _, _, _, ref mut a, ..)) => a,
			(6, MultiLocation::X7(_, _, _, _, _, _, ref mut a)) => a,
			(6, MultiLocation::X8(_, _, _, _, _, _, ref mut a, ..)) => a,
			(7, MultiLocation::X8(_, _, _, _, _, _, _, ref mut a)) => a,
			_ => return None,
		})
	}

	/// Returns a reference iterator over the junctions.
	pub fn iter(&self) -> MultiLocationRefIterator {
		MultiLocationRefIterator(&self, 0)
	}

	/// Returns a reference iterator over the junctions in reverse.
	pub fn iter_rev(&self) -> MultiLocationReverseRefIterator {
		MultiLocationReverseRefIterator(&self, 0)
	}

	/// Consumes `self` and returns an iterator over the junctions.
	pub fn into_iter(self) -> MultiLocationIterator {
		MultiLocationIterator(self)
	}

	/// Consumes `self` and returns an iterator over the junctions in reverse.
	pub fn into_iter_rev(self) -> MultiLocationReverseIterator {
		MultiLocationReverseIterator(self)
	}

	/// Ensures that self begins with `prefix` and that it has a single `Junction` item following.
	/// If so, returns a reference to this `Junction` item.
	///
	/// # Example
	/// ```rust
	/// # use xcm::v0::{MultiLocation::*, Junction::*};
	/// # fn main() {
	/// let mut m = X3(Parent, PalletInstance(3), OnlyChild);
	/// assert_eq!(m.match_and_split(&X2(Parent, PalletInstance(3))), Some(&OnlyChild));
	/// assert_eq!(m.match_and_split(&X1(Parent)), None);
	/// # }
	/// ```
	pub fn match_and_split(&self, prefix: &MultiLocation) -> Option<&Junction> {
		if prefix.len() + 1 != self.len() {
			return None
		}
		for i in 0..prefix.len() {
			if prefix.at(i) != self.at(i) {
				return None
			}
		}
		return self.at(prefix.len())
	}

	/// Mutates `self`, suffixing it with `new`. Returns `Err` in case of overflow.
	pub fn push(&mut self, new: Junction) -> result::Result<(), ()> {
		let mut n = MultiLocation::Null;
		mem::swap(&mut *self, &mut n);
		match n.pushed_with(new) {
			Ok(result) => { *self = result; Ok(()) }
			Err(old) => { *self = old; Err(()) }
		}
	}


	/// Mutates `self`, prefixing it with `new`. Returns `Err` in case of overflow.
	pub fn push_front(&mut self, new: Junction) -> result::Result<(), ()> {
		let mut n = MultiLocation::Null;
		mem::swap(&mut *self, &mut n);
		match n.pushed_front_with(new) {
			Ok(result) => { *self = result; Ok(()) }
			Err(old) => { *self = old; Err(()) }
		}
	}

	/// Returns the number of `Parent` junctions at the beginning of `self`.
	pub fn leading_parent_count(&self) -> usize {
		use Junction::Parent;
		match self {
			MultiLocation::X8(Parent, Parent, Parent, Parent, Parent, Parent, Parent, Parent) => 8,

			MultiLocation::X8(Parent, Parent, Parent, Parent, Parent, Parent, Parent, ..) => 7,
			MultiLocation::X7(Parent, Parent, Parent, Parent, Parent, Parent, Parent) => 7,

			MultiLocation::X8(Parent, Parent, Parent, Parent, Parent, Parent, ..) => 6,
			MultiLocation::X7(Parent, Parent, Parent, Parent, Parent, Parent, ..) => 6,
			MultiLocation::X6(Parent, Parent, Parent, Parent, Parent, Parent) => 6,

			MultiLocation::X8(Parent, Parent, Parent, Parent, Parent, ..) => 5,
			MultiLocation::X7(Parent, Parent, Parent, Parent, Parent, ..) => 5,
			MultiLocation::X6(Parent, Parent, Parent, Parent, Parent, ..) => 5,
			MultiLocation::X5(Parent, Parent, Parent, Parent, Parent) => 5,

			MultiLocation::X8(Parent, Parent, Parent, Parent, ..) => 4,
			MultiLocation::X7(Parent, Parent, Parent, Parent, ..) => 4,
			MultiLocation::X6(Parent, Parent, Parent, Parent, ..) => 4,
			MultiLocation::X5(Parent, Parent, Parent, Parent, ..) => 4,
			MultiLocation::X4(Parent, Parent, Parent, Parent) => 4,

			MultiLocation::X8(Parent, Parent, Parent, ..) => 3,
			MultiLocation::X7(Parent, Parent, Parent, ..) => 3,
			MultiLocation::X6(Parent, Parent, Parent, ..) => 3,
			MultiLocation::X5(Parent, Parent, Parent, ..) => 3,
			MultiLocation::X4(Parent, Parent, Parent, ..) => 3,
			MultiLocation::X3(Parent, Parent, Parent) => 3,

			MultiLocation::X8(Parent, Parent, ..) => 2,
			MultiLocation::X7(Parent, Parent, ..) => 2,
			MultiLocation::X6(Parent, Parent, ..) => 2,
			MultiLocation::X5(Parent, Parent, ..) => 2,
			MultiLocation::X4(Parent, Parent, ..) => 2,
			MultiLocation::X3(Parent, Parent, ..) => 2,
			MultiLocation::X2(Parent, Parent) => 2,

			MultiLocation::X8(Parent, ..) => 1,
			MultiLocation::X7(Parent, ..) => 1,
			MultiLocation::X6(Parent, ..) => 1,
			MultiLocation::X5(Parent, ..) => 1,
			MultiLocation::X4(Parent, ..) => 1,
			MultiLocation::X3(Parent, ..) => 1,
			MultiLocation::X2(Parent, ..) => 1,
			MultiLocation::X1(Parent) => 1,
			_ => 0,
		}
	}

	/// This function ensures a multi-junction is in its canonicalized/normalized form, removing
	/// any internal `[Non-Parent, Parent]` combinations.
	pub fn canonicalize(&mut self) {
		let mut normalized = MultiLocation::Null;
		let mut iter = self.iter();
		// We build up the the new normalized path by taking items from the original multi-location.
		// When the next item we would add is `Parent`, we instead remove the last item assuming
		// it is non-parent.
		const EXPECT_MESSAGE: &'static str =
			"`self` is a well formed multi-location with N junctions; \
			this loop iterates over the junctions of `self`; \
			the loop can push to the new multi-location at most one time; \
			thus the size of the new multi-location is at most N junctions; \
			qed";
		while let Some(j) = iter.next() {
			if j == &Junction::Parent {
				match normalized.last() {
					None | Some(Junction::Parent) => {}
					Some(_) => {
						normalized.take_last();
						continue;
					},
				}
			}

			normalized.push(j.clone()).expect(EXPECT_MESSAGE);
		}

		core::mem::swap(self, &mut normalized);
	}


	/// Mutate `self` so that it is suffixed with `suffix`. The correct normalized form is returned,
	/// removing any internal `[Non-Parent, Parent]`  combinations.
	///
	/// In the case of overflow, `self` is unmodified and  we return `Err` with `suffix`.
	///
	/// # Example
	/// ```rust
	/// # use xcm::v0::{MultiLocation::*, Junction::*};
	/// # fn main() {
	/// let mut m = X3(Parent, Parachain(21), OnlyChild);
	/// assert_eq!(m.append_with(X2(Parent, PalletInstance(3))), Ok(()));
	/// assert_eq!(m, X3(Parent, Parachain(21), PalletInstance(3)));
	/// # }
	/// ```
	pub fn append_with(&mut self, suffix: MultiLocation) -> Result<(), MultiLocation> {
		let mut prefix = suffix;
		core::mem::swap(self, &mut prefix);
		match self.prepend_with(prefix) {
			Ok(()) => Ok(()),
			Err(prefix) => {
				let mut suffix = prefix;
				core::mem::swap(self, &mut suffix);
				Err(suffix)
			}
		}
	}

	/// Mutate `self` so that it is prefixed with `prefix`. The correct normalized form is returned,
	/// removing any internal [Non-Parent, `Parent`] combinations.
	///
	/// In the case of overflow, `self` is unmodified and  we return `Err` with `prefix`.
	///
	/// # Example
	/// ```rust
	/// # use xcm::v0::{MultiLocation::*, Junction::*, NetworkId::Any};
	/// # fn main() {
	/// let mut m = X3(Parent, Parent, PalletInstance(3));
	/// assert_eq!(m.prepend_with(X3(Parent, Parachain(21), OnlyChild)), Ok(()));
	/// assert_eq!(m, X2(Parent, PalletInstance(3)));
	/// # }
	/// ```
	pub fn prepend_with(&mut self, prefix: MultiLocation) -> Result<(), MultiLocation> {
		let mut prefix = prefix;

		// This will guarantee that all `Parent` junctions in the prefix are leading, which is
		// important for calculating the `skipped` items below.
		prefix.canonicalize();

		let self_leading_parents = self.leading_parent_count();
		// These are the number of `non-parent` items in the prefix that we can
		// potentially remove if the original location leads with parents.
		let prefix_rest = prefix.len() - prefix.leading_parent_count();
		// 2 * skipped items will be removed when performing the normalization below.
		let skipped = self_leading_parents.min(prefix_rest);

		// Pre-pending this prefix would create a multi-location with too many junctions.
		if self.len() + prefix.len() - 2 * skipped > MAX_MULTILOCATION_LENGTH {
			return Err(prefix);
		}

		// Here we cancel out `[Non-Parent, Parent]` items (normalization), where
		// the non-parent item comes from the end of the prefix, and the parent item
		// comes from the front of the original location.
		//
		// We calculated already how many of these there should be above.
		for _ in 0 .. skipped {
				let _non_parent = prefix.take_last();
				let _parent = self.take_first();
				debug_assert!(
					_non_parent.is_some() && _non_parent != Some(Junction::Parent),
					"prepend_with should always remove a non-parent from the end of the prefix",
				);
				debug_assert!(
					_parent == Some(Junction::Parent),
					"prepend_with should always remove a parent from the front of the location",
				);
		}

		for j in prefix.into_iter_rev() {
			self.push_front(j).expect("len + prefix minus 2*skipped is less than max length; qed");
		}
		Ok(())
	}

	/// Returns true iff `self` is an interior location. For this it may not contain any `Junction`s
	/// for which `Junction::is_interior` returns `false`. This is generally true, except for the
	/// `Parent` item.
	///
	/// # Example
	/// ```rust
	/// # use xcm::v0::{MultiLocation::*, Junction::*, NetworkId::Any};
	/// # fn main() {
	/// let parent = X1(Parent);
	/// assert_eq!(parent.is_interior(), false);
	/// let m = X2(PalletInstance(12), AccountIndex64 { network: Any, index: 23 });
	/// assert_eq!(m.is_interior(), true);
	/// # }
	/// ```
	pub fn is_interior(&self) -> bool {
		self.iter().all(Junction::is_interior)
	}
}

impl From<MultiLocation> for VersionedMultiLocation {
	fn from(x: MultiLocation) -> Self {
		VersionedMultiLocation::V0(x)
	}
}

impl TryFrom<VersionedMultiLocation> for MultiLocation {
	type Error = ();
	fn try_from(x: VersionedMultiLocation) -> result::Result<Self, ()> {
		match x {
			VersionedMultiLocation::V0(x) => Ok(x),
			_ => Err(()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::MultiLocation::*;
	use crate::opaque::v0::{Junction::*, NetworkId::Any};

	#[test]
	fn match_and_split_works() {
		let m = X3(Parent, Parachain(42), AccountIndex64 { network: Any, index: 23 });
		assert_eq!(m.match_and_split(&X1(Parent)), None);
		assert_eq!(
			m.match_and_split(&X2(Parent, Parachain(42))),
			Some(&AccountIndex64 { network: Any, index: 23 })
		);
		assert_eq!(m.match_and_split(&m), None);
	}

	#[test]
	fn append_with_works() {
		let acc = AccountIndex64 { network: Any, index: 23 };
		let mut m = X2(Parent, Parachain(42));
		assert_eq!(m.append_with(X2(PalletInstance(3), acc.clone())), Ok(()));
		assert_eq!(m, X4(Parent, Parachain(42), PalletInstance(3), acc.clone()));

		// cannot append to create overly long multilocation
		let acc = AccountIndex64 { network: Any, index: 23 };
		let mut m = X7(Parent, Parent, Parent, Parent, Parent, Parent, Parachain(42));
		let suffix = X2(PalletInstance(3), acc.clone());
		assert_eq!(m.append_with(suffix.clone()), Err(suffix));
	}

	#[test]
	fn prepend_with_works() {
		let mut m = X3(Parent, Parachain(42), AccountIndex64 { network: Any, index: 23 });
		assert_eq!(m.prepend_with(X2(Parent, OnlyChild)), Ok(()));
		assert_eq!(m, X3(Parent, Parachain(42), AccountIndex64 { network: Any, index: 23 }));

		// cannot prepend to create overly long multilocation
		let mut m = X7(Parent, Parent, Parent, Parent, Parent, Parent, Parachain(42));
		let prefix = X2(Parent, Parent);
		assert_eq!(m.prepend_with(prefix.clone()), Err(prefix));

		// Can handle shared prefix and resizing correctly.
		let mut m = X1(Parent);
		let prefix = X8(Parachain(100), OnlyChild, OnlyChild, OnlyChild, OnlyChild, OnlyChild, OnlyChild, Parent);
		assert_eq!(m.prepend_with(prefix.clone()), Ok(()));
		assert_eq!(m, X5(Parachain(100), OnlyChild, OnlyChild, OnlyChild, OnlyChild));

		let mut m = X1(Parent);
		let prefix = X8(Parent, Parent, Parent, Parent, Parent, Parent, Parent, Parent);
		assert_eq!(m.prepend_with(prefix.clone()), Err(prefix));

		let mut m = X1(Parent);
		let prefix = X7(Parent, Parent, Parent, Parent, Parent, Parent, Parent);
		assert_eq!(m.prepend_with(prefix.clone()), Ok(()));
		assert_eq!(m, X8(Parent, Parent, Parent, Parent, Parent, Parent, Parent, Parent));

		let mut m = X1(Parent);
		let prefix = X8(Parent, Parent, Parent, Parent, OnlyChild, Parent, Parent, Parent);
		assert_eq!(m.prepend_with(prefix.clone()), Ok(()));
		assert_eq!(m, X7(Parent, Parent, Parent, Parent, Parent, Parent, Parent));
	}

	#[test]
	fn canonicalize_works() {
		let mut m = X1(Parent);
		m.canonicalize();
		assert_eq!(m, X1(Parent));

		let mut m = X1(Parachain(1));
		m.canonicalize();
		assert_eq!(m, X1(Parachain(1)));

		let mut m = X6(Parent, Parachain(1), Parent, Parachain(2), Parent, Parachain(3));
		m.canonicalize();
		assert_eq!(m, X2(Parent, Parachain(3)));

		let mut m = X5(Parachain(1), Parent, Parachain(2), Parent, Parachain(3));
		m.canonicalize();
		assert_eq!(m, X1(Parachain(3)));

		let mut m = X6(Parachain(1), Parent, Parachain(2), Parent, Parachain(3), Parent);
		m.canonicalize();
		assert_eq!(m, Null);

		let mut m = X5(Parachain(1), Parent, Parent, Parent, Parachain(3));
		m.canonicalize();
		assert_eq!(m, X3(Parent, Parent, Parachain(3)));

		let mut m = X4(Parachain(1), Parachain(2), Parent, Parent);
		m.canonicalize();
		assert_eq!(m, Null);

		let mut m = X4( Parent, Parent, Parachain(1), Parachain(2));
		m.canonicalize();
		assert_eq!(m, X4( Parent, Parent, Parachain(1), Parachain(2)));
	}
}
