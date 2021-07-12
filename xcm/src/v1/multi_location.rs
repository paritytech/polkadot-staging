// Copyright 2021 Parity Technologies (UK) Ltd.
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

use core::{convert::TryFrom, mem, result};

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
pub struct MultiLocation {
	parents: u8,
	junctions: Junctions,
}

/// Maximum number of junctions a multilocation can contain.
pub const MAX_MULTILOCATION_LENGTH: usize = 8;

impl MultiLocation {
	/// Creates a new MultiLocation, ensuring that the length of it does not exceed the maximum,
	/// otherwise returns `Err`.
	pub fn new(parents: u8, junctions: Junctions) -> result::Result<MultiLocation, ()> {
		if parents as usize + junctions.len() > MAX_MULTILOCATION_LENGTH {
			return Err(())
		}
		Ok(MultiLocation {
			parents,
			junctions,
		})
	}

	/// Return a reference to the junctions field.
	pub fn junctions(&self) -> &Junctions {
		&self.junctions
	}

	/// Return a mutable reference to the junctions field.
	pub fn junctions_mut(&mut self) -> &mut Junctions {
		&mut self.junctions
	}

	/// Returns the number of `Parent` junctions at the beginning of `self`.
	pub fn parent_count(&self) -> usize {
		self.parents as usize
	}

	/// Returns the number of parents and junctions in `self`.
	pub fn len(&self) -> usize {
		self.parent_count() + self.junctions.len()
	}

	/// Returns first junction that is not a parent, or `None` if the location is empty or
	/// contains only parents.
	pub fn first_non_parent(&self) -> Option<&Junction> {
		self.junctions.first()
	}

	/// Returns last junction, or `None` if the location is empty or contains only parents.
	pub fn last(&self) -> Option<&Junction> {
		self.junctions.last()
	}

	/// Splits off the first non-parent junction, returning the remaining suffix (first item in tuple)
	/// and the first element (second item in tuple) or `None` if it was empty.
	pub fn split_first_non_parent(self) -> (MultiLocation, Option<Junction>) {
		let MultiLocation { parents, junctions } = self;
		let (prefix, suffix) = junctions.split_first();
		let multilocation = MultiLocation {
			parents,
			junctions: prefix,
		};
		(multilocation, suffix)
	}

	/// Splits off the last junction, returning the remaining prefix (first item in tuple) and the last element
	/// (second item in tuple) or `None` if it was empty or that `self` only contains parents.
	pub fn split_last(self) -> (MultiLocation, Option<Junction>) {
		let MultiLocation { parents, junctions } = self;
		let (prefix, suffix) = junctions.split_last();
		let multilocation = MultiLocation {
			parents,
			junctions: prefix,
		};
		(multilocation, suffix)
	}
	
	/// Bumps the parent count up by 1. Returns `Err` in case of overflow.
	pub fn push_parent(&mut self) -> result::Result<(), ()> {
		if self.len() >= MAX_MULTILOCATION_LENGTH {
			return Err(())
		}
		self.parents = self.parents.saturating_add(1);
		Ok(())
	}

	/// Mutates `self`, suffixing its non-parent junctions with `new`. Returns `Err` in case of overflow.
	pub fn push_non_parent(&mut self, new: Junction) -> result::Result<(), ()> {
		let mut n = Junctions::Null;
		mem::swap(&mut self.junctions, &mut n);
		match n.pushed_with(new) {
			Ok(result) => { self.junctions = result; Ok(()) }
			Err(old) => { self.junctions = old; Err(()) }
		}
	}

	/// Mutates `self`, prefixing its non-parent junctions with `new`. Returns `Err` in case of overflow.
	pub fn push_front_non_parent(&mut self, new: Junction) -> result::Result<(), ()> {
		let mut n = Junctions::Null;
		mem::swap(&mut self.junctions, &mut n);
		match n.pushed_front_with(new) {
			Ok(result) => { self.junctions = result; Ok(()) }
			Err(old) => { self.junctions = old; Err(()) }
		}
	}

	/// Consumes `self` and returns a `MultiLocation` with its parent count incremented by 1, or
	/// an `Err` with the original value of `self` in case of overflow.
	pub fn pushed_with_parent(self) -> result::Result<Self, Self> {
		if self.len() >= MAX_MULTILOCATION_LENGTH {
			return Err(self)
		}
		Ok(MultiLocation {
			parents: self.parents.saturating_add(1),
			..self
		})
	}

	/// Consumes `self` and returns a `MultiLocation` suffixed with `new`, or an `Err` with the original value of
	/// `self` in case of overflow.
	pub fn pushed_with_non_parent(self, new: Junction) -> result::Result<Self, Self> {
		if self.len() >= MAX_MULTILOCATION_LENGTH {
			return Err(self)
		}
		Ok(MultiLocation {
			parents: self.parents,
			junctions: self.junctions.pushed_with(new).expect("length is less than max length; qed"),
		})
	}

	/// Consumes `self` and returns a `MultiLocation` prefixed with `new`, or an `Err` with the original value of
	/// `self` in case of overflow.
	pub fn pushed_front_with_non_parent(self, new: Junction) -> result::Result<Self, Self> {
		if self.len() >= MAX_MULTILOCATION_LENGTH {
			return Err(self)
		}
		Ok(MultiLocation {
			parents: self.parents,
			junctions: self.junctions.pushed_front_with(new).expect("length is less than max length; qed"),
		})
	}

	/// Returns the junction at index `i`, or `None` if the location is a parent or if the location
	/// does not contain that many elements.
	pub fn at(&self, i: usize) -> Option<&Junction> {
		let num_parents = self.parents as usize;
		if i < num_parents {
			return None
		}
		self.junctions.at(i - num_parents)
	}

	/// Returns a mutable reference to the junction at index `i`, or `None` if the location is a
	/// parent or if it doesn't contain that many elements.
	pub fn at_mut(&mut self, i: usize) -> Option<&mut Junction> {
		let num_parents = self.parents as usize;
		if i < num_parents {
			return None
		}
		self.junctions.at_mut(i - num_parents)
	}

	/// Decrement the parent count by 1.
	pub fn pop_parent(&mut self) {
		self.parents = self.parents.saturating_sub(1);
	}

	/// Removes the first non-parent element from `self`, returning it
	/// (or `None` if it was empty or if `self` contains only parents).
	pub fn take_first_non_parent(&mut self) -> Option<Junction> {
		self.junctions.take_first()
	}

	/// Removes the last element from `junctions`, returning it (or `None` if it was empty or if
	/// `self` only contains parents).
	pub fn take_last(&mut self) -> Option<Junction> {
		self.junctions.take_last()
	}

	/// Ensures that `self` has the same number of parents as `prefix`, its junctions begins with
	/// the junctions of `prefix` and that it has a single `Junction` item following.
	/// If so, returns a reference to this `Junction` item.
	///
	/// # Example
	/// ```rust
	/// # use xcm::v1::{Junctions::*, Junction::*, MultiLocation};
	/// # fn main() {
	/// let mut m = MultiLocation::new(1, X2(PalletInstance(3), OnlyChild)).unwrap();
	/// assert_eq!(
	///     m.match_and_split(&MultiLocation::new(1, X1(PalletInstance(3))).unwrap()),
	///     Some(&OnlyChild),
	/// );
	/// assert_eq!(m.match_and_split(&MultiLocation::new(1, Null).unwrap()), None);
	/// # }
	/// ```
	pub fn match_and_split(&self, prefix: &MultiLocation) -> Option<&Junction> {
		if self.parents != prefix.parents {
			return None
		}
		self.junctions.match_and_split(&prefix.junctions)
	}

	/// Mutate `self` so that it is suffixed with `suffix`. The correct normalized form is returned,
	/// removing any internal [Non-Parent, `Parent`]  combinations.
	///
	/// Does not modify `self` and returns `Err` with `suffix` in case of overflow.
	///
	/// # Example
	/// ```rust
	/// # use xcm::v1::{Junctions::*, Junction::*, MultiLocation};
	/// # fn main() {
	/// let mut m = MultiLocation::new(1, X2(Parachain(21), OnlyChild)).unwrap();
	/// assert_eq!(m.append_with(MultiLocation::new(1, X1(PalletInstance(3))).unwrap()), Ok(()));
	/// assert_eq!(m, MultiLocation::new(1, X2(Parachain(21), PalletInstance(3))).unwrap());
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

	/// Mutate `self` so that it is prefixed with `prefix`.
	///
	/// Does not modify `self` and returns `Err` with `prefix` in case of overflow.
	///
	/// # Example
	/// ```rust
	/// # use xcm::v1::{Junctions::*, Junction::*, MultiLocation};
	/// # fn main() {
	/// let mut m = MultiLocation::new(2, X1(PalletInstance(3))).unwrap();
	/// assert_eq!(m.prepend_with(MultiLocation::new(1, X2(Parachain(21), OnlyChild)).unwrap()), Ok(()));
	/// assert_eq!(m, MultiLocation::new(1, X1(PalletInstance(3))).unwrap());
	/// # }
	/// ```
	pub fn prepend_with(&mut self, mut prefix: MultiLocation) -> Result<(), MultiLocation> {
		let self_parents = self.parent_count();
		let prepend_len = (self_parents as isize - prefix.junctions.len() as isize).abs() as usize;
		if self.junctions.len() + prefix.parent_count() + prepend_len > MAX_MULTILOCATION_LENGTH {
			return Err(prefix)
		}

		let mut final_parent_count = prefix.parents;
		for _ in 0..self_parents {
			if prefix.take_last().is_none() {
				// If true, this means self parent count is greater than prefix junctions length;
				// add the resulting self parent count to final_parent_count
				final_parent_count += self.parents;
				break
			}
			self.pop_parent();
		}

		self.parents = final_parent_count;
		for j in prefix.junctions.into_iter_rev() {
			self.push_front_non_parent(j).expect(
				"self junctions len + prefix parent count + prepend len is less than max length; qed"
			);
		}
		Ok(())
	}
}

impl From<Junctions> for MultiLocation {
	fn from(junctions: Junctions) -> Self {
		MultiLocation {
			parents: 0,
			junctions,
		}
	}
}

impl From<Junction> for MultiLocation {
	fn from(x: Junction) -> Self {
		MultiLocation {
			parents: 0,
			junctions: Junctions::X1(x),
		}
	}
}

impl From<()> for MultiLocation {
	fn from(_: ()) -> Self {
		MultiLocation {
			parents: 0,
			junctions: Junctions::Null,
		}
	}
}
impl From<(Junction,)> for MultiLocation {
	fn from(x: (Junction,)) -> Self {
		MultiLocation {
			parents: 0,
			junctions: Junctions::X1(x.0),
		}
	}
}
impl From<(Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction)) -> Self {
		MultiLocation {
			parents: 0,
			junctions: Junctions::X2(x.0, x.1),
		}
	}
}
impl From<(Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction)) -> Self {
		MultiLocation {
			parents: 0,
			junctions: Junctions::X3(x.0, x.1, x.2),
		}
	}
}
impl From<(Junction, Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction, Junction)) -> Self {
		MultiLocation {
			parents: 0,
			junctions: Junctions::X4(x.0, x.1, x.2, x.3),
		}
	}
}
impl From<(Junction, Junction, Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction, Junction, Junction)) -> Self {
		MultiLocation {
			parents: 0,
			junctions: Junctions::X5(x.0, x.1, x.2, x.3, x.4),
		}
	}
}
impl From<(Junction, Junction, Junction, Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction, Junction, Junction, Junction)) -> Self {
		MultiLocation {
			parents: 0,
			junctions: Junctions::X6(x.0, x.1, x.2, x.3, x.4, x.5),
		}
	}
}
impl From<(Junction, Junction, Junction, Junction, Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction, Junction, Junction, Junction, Junction)) -> Self {
		MultiLocation {
			parents: 0,
			junctions: Junctions::X7(x.0, x.1, x.2, x.3, x.4, x.5, x.6),
		}
	}
}
impl From<(Junction, Junction, Junction, Junction, Junction, Junction, Junction, Junction)> for MultiLocation {
	fn from(x: (Junction, Junction, Junction, Junction, Junction, Junction, Junction, Junction)) -> Self {
		MultiLocation {
			parents: 0,
			junctions: Junctions::X8(x.0, x.1, x.2, x.3, x.4, x.5, x.6, x.7),
		}
	}
}

impl From<[Junction; 0]> for MultiLocation {
	fn from(_: [Junction; 0]) -> Self {
		MultiLocation {
			parents: 0,
			junctions: Junctions::Null,
		}
	}
}
impl From<[Junction; 1]> for MultiLocation {
	fn from(x: [Junction; 1]) -> Self {
		let [x0] = x;
		MultiLocation {
			parents: 0,
			junctions: Junctions::X1(x0),
		}
	}
}
impl From<[Junction; 2]> for MultiLocation {
	fn from(x: [Junction; 2]) -> Self {
		let [x0, x1] = x;
		MultiLocation {
			parents: 0,
			junctions: Junctions::X2(x0, x1),
		}
	}
}
impl From<[Junction; 3]> for MultiLocation {
	fn from(x: [Junction; 3]) -> Self {
		let [x0, x1, x2] = x;
		MultiLocation {
			parents: 0,
			junctions: Junctions::X3(x0, x1, x2),
		}
	}
}
impl From<[Junction; 4]> for MultiLocation {
	fn from(x: [Junction; 4]) -> Self {
		let [x0, x1, x2, x3] = x;
		MultiLocation {
			parents: 0,
			junctions: Junctions::X4(x0, x1, x2, x3),
		}
	}
}
impl From<[Junction; 5]> for MultiLocation {
	fn from(x: [Junction; 5]) -> Self {
		let [x0, x1, x2, x3, x4] = x;
		MultiLocation {
			parents: 0,
			junctions: Junctions::X5(x0, x1, x2, x3, x4),
		}
	}
}
impl From<[Junction; 6]> for MultiLocation {
	fn from(x: [Junction; 6]) -> Self {
		let [x0, x1, x2, x3, x4, x5] = x;
		MultiLocation {
			parents: 0,
			junctions: Junctions::X6(x0, x1, x2, x3, x4, x5),
		}
	}
}
impl From<[Junction; 7]> for MultiLocation {
	fn from(x: [Junction; 7]) -> Self {
		let [x0, x1, x2, x3, x4, x5, x6] = x;
		MultiLocation {
			parents: 0,
			junctions: Junctions::X7(x0, x1, x2, x3, x4, x5, x6),
		}
	}
}
impl From<[Junction; 8]> for MultiLocation {
	fn from(x: [Junction; 8]) -> Self {
		let [x0, x1, x2, x3, x4, x5, x6, x7] = x;
		MultiLocation {
			parents: 0,
			junctions: Junctions::X8(x0, x1, x2, x3, x4, x5, x6, x7),
		}
	}
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, Debug)]
pub enum Junctions {
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

pub struct JunctionsIterator(Junctions);
impl Iterator for JunctionsIterator {
	type Item = Junction;
	fn next(&mut self) -> Option<Junction> {
		self.0.take_first()
	}
}

pub struct JunctionsReverseIterator(Junctions);
impl Iterator for JunctionsReverseIterator {
	type Item = Junction;
	fn next(&mut self) -> Option<Junction> {
		self.0.take_last()
	}
}

pub struct JunctionsRefIterator<'a>(&'a Junctions, usize);
impl<'a> Iterator for JunctionsRefIterator<'a> {
	type Item = &'a Junction;
	fn next(&mut self) -> Option<&'a Junction> {
		let result = self.0.at(self.1);
		self.1 += 1;
		result
	}
}

pub struct JunctionsReverseRefIterator<'a>(&'a Junctions, usize);
impl<'a> Iterator for JunctionsReverseRefIterator<'a> {
	type Item = &'a Junction;
	fn next(&mut self) -> Option<&'a Junction> {
		self.1 += 1;
		self.0.at(self.0.len().checked_sub(self.1)?)
	}
}

impl Junctions {
	/// Returns first junction, or `None` if the location is empty.
	pub fn first(&self) -> Option<&Junction> {
		match &self {
			Junctions::Null => None,
			Junctions::X1(ref a) => Some(a),
			Junctions::X2(ref a, ..) => Some(a),
			Junctions::X3(ref a, ..) => Some(a),
			Junctions::X4(ref a, ..) => Some(a),
			Junctions::X5(ref a, ..) => Some(a),
			Junctions::X6(ref a, ..) => Some(a),
			Junctions::X7(ref a, ..) => Some(a),
			Junctions::X8(ref a, ..) => Some(a),
		}
	}

	/// Returns last junction, or `None` if the location is empty.
	pub fn last(&self) -> Option<&Junction> {
		match &self {
			Junctions::Null => None,
			Junctions::X1(ref a) => Some(a),
			Junctions::X2(.., ref a) => Some(a),
			Junctions::X3(.., ref a) => Some(a),
			Junctions::X4(.., ref a) => Some(a),
			Junctions::X5(.., ref a) => Some(a),
			Junctions::X6(.., ref a) => Some(a),
			Junctions::X7(.., ref a) => Some(a),
			Junctions::X8(.., ref a) => Some(a),
		}
	}

	/// Splits off the first junction, returning the remaining suffix (first item in tuple) and the first element
	/// (second item in tuple) or `None` if it was empty.
	pub fn split_first(self) -> (Junctions, Option<Junction>) {
		match self {
			Junctions::Null => (Junctions::Null, None),
			Junctions::X1(a) => (Junctions::Null, Some(a)),
			Junctions::X2(a, b) => (Junctions::X1(b), Some(a)),
			Junctions::X3(a, b, c) => (Junctions::X2(b, c), Some(a)),
			Junctions::X4(a, b, c ,d) => (Junctions::X3(b, c, d), Some(a)),
			Junctions::X5(a, b, c ,d, e) => (Junctions::X4(b, c, d, e), Some(a)),
			Junctions::X6(a, b, c ,d, e, f) => (Junctions::X5(b, c, d, e, f), Some(a)),
			Junctions::X7(a, b, c ,d, e, f, g) => (Junctions::X6(b, c, d, e, f, g), Some(a)),
			Junctions::X8(a, b, c ,d, e, f, g, h) => (Junctions::X7(b, c, d, e, f, g, h), Some(a)),
		}
	}

	/// Splits off the last junction, returning the remaining prefix (first item in tuple) and the last element
	/// (second item in tuple) or `None` if it was empty.
	pub fn split_last(self) -> (Junctions, Option<Junction>) {
		match self {
			Junctions::Null => (Junctions::Null, None),
			Junctions::X1(a) => (Junctions::Null, Some(a)),
			Junctions::X2(a, b) => (Junctions::X1(a), Some(b)),
			Junctions::X3(a, b, c) => (Junctions::X2(a, b), Some(c)),
			Junctions::X4(a, b, c ,d) => (Junctions::X3(a, b, c), Some(d)),
			Junctions::X5(a, b, c, d, e) => (Junctions::X4(a, b, c, d), Some(e)),
			Junctions::X6(a, b, c, d, e, f) => (Junctions::X5(a, b, c, d, e), Some(f)),
			Junctions::X7(a, b, c, d, e, f, g) => (Junctions::X6(a, b, c, d, e, f), Some(g)),
			Junctions::X8(a, b, c, d, e, f, g, h) => (Junctions::X7(a, b, c, d, e, f, g), Some(h)),
		}
	}

	/// Removes the first element from `self`, returning it (or `None` if it was empty).
	pub fn take_first(&mut self) -> Option<Junction> {
		let mut d = Junctions::Null;
		mem::swap(&mut *self, &mut d);
		let (tail, head) = d.split_first();
		*self = tail;
		head
	}

	/// Removes the last element from `self`, returning it (or `None` if it was empty).
	pub fn take_last(&mut self) -> Option<Junction> {
		let mut d = Junctions::Null;
		mem::swap(&mut *self, &mut d);
		let (head, tail) = d.split_last();
		*self = head;
		tail
	}

	/// Consumes `self` and returns a `Junctions` suffixed with `new`, or an `Err` with the original value of
	/// `self` in case of overflow.
	pub fn pushed_with(self, new: Junction) -> result::Result<Self, Self> {
		Ok(match self {
			Junctions::Null => Junctions::X1(new),
			Junctions::X1(a) => Junctions::X2(a, new),
			Junctions::X2(a, b) => Junctions::X3(a, b, new),
			Junctions::X3(a, b, c) => Junctions::X4(a, b, c, new),
			Junctions::X4(a, b, c, d) => Junctions::X5(a, b, c, d, new),
			Junctions::X5(a, b, c, d, e) => Junctions::X6(a, b, c, d, e, new),
			Junctions::X6(a, b, c, d, e, f) => Junctions::X7(a, b, c, d, e, f, new),
			Junctions::X7(a, b, c, d, e, f, g) => Junctions::X8(a, b, c, d, e, f, g, new),
			s => Err(s)?,
		})
	}

	/// Consumes `self` and returns a `Junctions` prefixed with `new`, or an `Err` with the original value of
	/// `self` in case of overflow.
	pub fn pushed_front_with(self, new: Junction) -> result::Result<Self, Self> {
		Ok(match self {
			Junctions::Null => Junctions::X1(new),
			Junctions::X1(a) => Junctions::X2(new, a),
			Junctions::X2(a, b) => Junctions::X3(new, a, b),
			Junctions::X3(a, b, c) => Junctions::X4(new, a, b, c),
			Junctions::X4(a, b, c, d) => Junctions::X5(new, a, b, c, d),
			Junctions::X5(a, b, c, d, e) => Junctions::X6(new, a, b, c, d, e),
			Junctions::X6(a, b, c, d, e, f) => Junctions::X7(new, a, b, c, d, e, f),
			Junctions::X7(a, b, c, d, e, f, g) => Junctions::X8(new, a, b, c, d, e, f, g),
			s => Err(s)?,
		})
	}

	/// Returns the number of junctions in `self`.
	pub fn len(&self) -> usize {
		match &self {
			Junctions::Null => 0,
			Junctions::X1(..) => 1,
			Junctions::X2(..) => 2,
			Junctions::X3(..) => 3,
			Junctions::X4(..) => 4,
			Junctions::X5(..) => 5,
			Junctions::X6(..) => 6,
			Junctions::X7(..) => 7,
			Junctions::X8(..) => 8,
		}
	}

	/// Returns the junction at index `i`, or `None` if the location doesn't contain that many elements.
	pub fn at(&self, i: usize) -> Option<&Junction> {
		Some(match (i, &self) {
			(0, Junctions::X1(ref a)) => a,
			(0, Junctions::X2(ref a, ..)) => a,
			(0, Junctions::X3(ref a, ..)) => a,
			(0, Junctions::X4(ref a, ..)) => a,
			(0, Junctions::X5(ref a, ..)) => a,
			(0, Junctions::X6(ref a, ..)) => a,
			(0, Junctions::X7(ref a, ..)) => a,
			(0, Junctions::X8(ref a, ..)) => a,
			(1, Junctions::X2(_, ref a)) => a,
			(1, Junctions::X3(_, ref a, ..)) => a,
			(1, Junctions::X4(_, ref a, ..)) => a,
			(1, Junctions::X5(_, ref a, ..)) => a,
			(1, Junctions::X6(_, ref a, ..)) => a,
			(1, Junctions::X7(_, ref a, ..)) => a,
			(1, Junctions::X8(_, ref a, ..)) => a,
			(2, Junctions::X3(_, _, ref a)) => a,
			(2, Junctions::X4(_, _, ref a, ..)) => a,
			(2, Junctions::X5(_, _, ref a, ..)) => a,
			(2, Junctions::X6(_, _, ref a, ..)) => a,
			(2, Junctions::X7(_, _, ref a, ..)) => a,
			(2, Junctions::X8(_, _, ref a, ..)) => a,
			(3, Junctions::X4(_, _, _, ref a)) => a,
			(3, Junctions::X5(_, _, _, ref a, ..)) => a,
			(3, Junctions::X6(_, _, _, ref a, ..)) => a,
			(3, Junctions::X7(_, _, _, ref a, ..)) => a,
			(3, Junctions::X8(_, _, _, ref a, ..)) => a,
			(4, Junctions::X5(_, _, _, _, ref a)) => a,
			(4, Junctions::X6(_, _, _, _, ref a, ..)) => a,
			(4, Junctions::X7(_, _, _, _, ref a, ..)) => a,
			(4, Junctions::X8(_, _, _, _, ref a, ..)) => a,
			(5, Junctions::X6(_, _, _, _, _, ref a)) => a,
			(5, Junctions::X7(_, _, _, _, _, ref a, ..)) => a,
			(5, Junctions::X8(_, _, _, _, _, ref a, ..)) => a,
			(6, Junctions::X7(_, _, _, _, _, _, ref a)) => a,
			(6, Junctions::X8(_, _, _, _, _, _, ref a, ..)) => a,
			(7, Junctions::X8(_, _, _, _, _, _, _, ref a)) => a,
			_ => return None,
		})
	}

	/// Returns a mutable reference to the junction at index `i`, or `None` if the location doesn't contain that many
	/// elements.
	pub fn at_mut(&mut self, i: usize) -> Option<&mut Junction> {
		Some(match (i, self) {
			(0, Junctions::X1(ref mut a)) => a,
			(0, Junctions::X2(ref mut a, ..)) => a,
			(0, Junctions::X3(ref mut a, ..)) => a,
			(0, Junctions::X4(ref mut a, ..)) => a,
			(0, Junctions::X5(ref mut a, ..)) => a,
			(0, Junctions::X6(ref mut a, ..)) => a,
			(0, Junctions::X7(ref mut a, ..)) => a,
			(0, Junctions::X8(ref mut a, ..)) => a,
			(1, Junctions::X2(_, ref mut a)) => a,
			(1, Junctions::X3(_, ref mut a, ..)) => a,
			(1, Junctions::X4(_, ref mut a, ..)) => a,
			(1, Junctions::X5(_, ref mut a, ..)) => a,
			(1, Junctions::X6(_, ref mut a, ..)) => a,
			(1, Junctions::X7(_, ref mut a, ..)) => a,
			(1, Junctions::X8(_, ref mut a, ..)) => a,
			(2, Junctions::X3(_, _, ref mut a)) => a,
			(2, Junctions::X4(_, _, ref mut a, ..)) => a,
			(2, Junctions::X5(_, _, ref mut a, ..)) => a,
			(2, Junctions::X6(_, _, ref mut a, ..)) => a,
			(2, Junctions::X7(_, _, ref mut a, ..)) => a,
			(2, Junctions::X8(_, _, ref mut a, ..)) => a,
			(3, Junctions::X4(_, _, _, ref mut a)) => a,
			(3, Junctions::X5(_, _, _, ref mut a, ..)) => a,
			(3, Junctions::X6(_, _, _, ref mut a, ..)) => a,
			(3, Junctions::X7(_, _, _, ref mut a, ..)) => a,
			(3, Junctions::X8(_, _, _, ref mut a, ..)) => a,
			(4, Junctions::X5(_, _, _, _, ref mut a)) => a,
			(4, Junctions::X6(_, _, _, _, ref mut a, ..)) => a,
			(4, Junctions::X7(_, _, _, _, ref mut a, ..)) => a,
			(4, Junctions::X8(_, _, _, _, ref mut a, ..)) => a,
			(5, Junctions::X6(_, _, _, _, _, ref mut a)) => a,
			(5, Junctions::X7(_, _, _, _, _, ref mut a, ..)) => a,
			(5, Junctions::X8(_, _, _, _, _, ref mut a, ..)) => a,
			(6, Junctions::X7(_, _, _, _, _, _, ref mut a)) => a,
			(6, Junctions::X8(_, _, _, _, _, _, ref mut a, ..)) => a,
			(7, Junctions::X8(_, _, _, _, _, _, _, ref mut a)) => a,
			_ => return None,
		})
	}

	/// Returns a reference iterator over the junctions.
	pub fn iter(&self) -> JunctionsRefIterator {
		JunctionsRefIterator(&self, 0)
	}

	/// Returns a reference iterator over the junctions in reverse.
	pub fn iter_rev(&self) -> JunctionsReverseRefIterator {
		JunctionsReverseRefIterator(&self, 0)
	}

	/// Consumes `self` and returns an iterator over the junctions.
	pub fn into_iter(self) -> JunctionsIterator {
		JunctionsIterator(self)
	}

	/// Consumes `self` and returns an iterator over the junctions in reverse.
	pub fn into_iter_rev(self) -> JunctionsReverseIterator {
		JunctionsReverseIterator(self)
	}

	/// Ensures that self begins with `prefix` and that it has a single `Junction` item following.
	/// If so, returns a reference to this `Junction` item.
	///
	/// # Example
	/// ```rust
	/// # use xcm::v1::{Junctions::*, Junction::*};
	/// # fn main() {
	/// let mut m = X3(Parachain(2), PalletInstance(3), OnlyChild);
	/// assert_eq!(m.match_and_split(&X2(Parachain(2), PalletInstance(3))), Some(&OnlyChild));
	/// assert_eq!(m.match_and_split(&X1(Parachain(2))), None);
	/// # }
	/// ```
	pub fn match_and_split(&self, prefix: &Junctions) -> Option<&Junction> {
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
}

impl From<MultiLocation> for VersionedMultiLocation {
	fn from(x: MultiLocation) -> Self {
		VersionedMultiLocation::V1(x)
	}
}

impl TryFrom<VersionedMultiLocation> for MultiLocation {
	type Error = ();
	fn try_from(x: VersionedMultiLocation) -> result::Result<Self, ()> {
		match x {
			VersionedMultiLocation::V1(x) => Ok(x),
			_ => Err(()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{Junctions::*, MultiLocation};
	use crate::opaque::v1::{Junction::*, NetworkId::Any};

	#[test]
	fn match_and_split_works() {
		let m = MultiLocation { parents: 1, junctions: X2(Parachain(42), AccountIndex64 { network: Any, index: 23 }) };
		assert_eq!(m.match_and_split(&MultiLocation { parents: 1, junctions: Null }), None);
		assert_eq!(
			m.match_and_split(&MultiLocation { parents: 1, junctions: X1(Parachain(42)) }),
			Some(&AccountIndex64 { network: Any, index: 23 })
		);
		assert_eq!(m.match_and_split(&m), None);
	}

	#[test]
	fn append_with_works() {
		let acc = AccountIndex64 { network: Any, index: 23 };
		let mut m = MultiLocation { parents: 1, junctions: X1(Parachain(42)) };
		assert_eq!(m.append_with(MultiLocation::from(X2(PalletInstance(3), acc.clone()))), Ok(()));
		assert_eq!(m, MultiLocation { parents: 1, junctions: X3(Parachain(42), PalletInstance(3), acc.clone()) });

		// cannot append to create overly long multilocation
		let acc = AccountIndex64 { network: Any, index: 23 };
		let mut m = MultiLocation { parents: 6, junctions: X1(Parachain(42)) };
		let suffix = MultiLocation::from(X2(PalletInstance(3), acc.clone()));
		assert_eq!(m.append_with(suffix.clone()), Err(suffix));
	}

	#[test]
	fn prepend_with_works() {
		let mut m = MultiLocation { parents: 1, junctions: X2(Parachain(42), AccountIndex64 { network: Any, index: 23 }) };
		assert_eq!(m.prepend_with(MultiLocation { parents: 1, junctions: X1(OnlyChild) }), Ok(()));
		assert_eq!(m, MultiLocation { parents: 1, junctions: X2(Parachain(42), AccountIndex64 { network: Any, index: 23 }) });

		// cannot prepend to create overly long multilocation
		let mut m = MultiLocation { parents: 6, junctions: X1(Parachain(42)) };
		let prefix = MultiLocation { parents: 2, junctions: Null };
		assert_eq!(m.prepend_with(prefix.clone()), Err(prefix));

		let prefix = MultiLocation { parents: 1, junctions: Null };
		assert_eq!(m.prepend_with(prefix), Ok(()));
		assert_eq!(m, MultiLocation { parents: 7, junctions: X1(Parachain(42)) });
	}
}
