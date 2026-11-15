#![cfg_attr(not(feature = "builtins"), allow(dead_code))]
//! Fuel tracking.
//!
//! Fuel is a single budget for all the work a template can make the engine
//! do.  Every instruction costs one fuel, and on top of that the engine
//! charges for the data it produces and the items it visits:
//!
//! * every byte written to the output (including captured output such as
//!   `{% set %}` blocks and macro bodies),
//! * every byte of strings and every slot of lists and maps created by
//!   operators (`~`, `+`, `*`, slicing), literals and the results of filters,
//!   tests, functions and methods,
//! * every byte of string input to a filter,
//! * every item that a built-in filter or operator visits, plus the cost of
//!   comparisons for sorting.
//!
//! Internally fuel is tracked in units of [`UNITS_PER_FUEL`] so that cheap
//! operations such as individual bytes can be charged without rounding.
//!
//! Allocations whose size is known ahead of time are checked before they are
//! performed so that a single huge allocation cannot slip through.
//!
//! Once the fuel runs out, the tracker stays exhausted: every later charge
//! fails.  That way code that cannot propagate an error directly (such as an
//! iterator) can stop early and the engine fails at the next check.
use std::cell::Cell;

use crate::compiler::instructions::Instruction;
use crate::error::{Error, ErrorKind};
use crate::value::{Value, ValueRepr};

/// The internal resolution of fuel.
///
/// One fuel is worth this many units, and one byte costs one unit.  This
/// means that one fuel pays for this many bytes of data.
pub(crate) const UNITS_PER_FUEL: u64 = 64;

/// Cost of one instruction.
pub(crate) const INSTRUCTION_COST: u64 = UNITS_PER_FUEL;

/// Cost of one byte of data produced or processed.
pub(crate) const BYTE_COST: u64 = 1;

/// Cost of visiting one item in a built-in filter or operator.
pub(crate) const ITEM_COST: u64 = 32;

/// Cost of comparing two values while sorting or deduplicating.
pub(crate) const COMPARE_COST: u64 = 64;

/// Cost of one slot in a list (half that of a map entry).  A value is 24
/// bytes, the rest accounts for the growth of the list.
pub(crate) const SLOT_COST: u64 = crate::value::ops::SLOT_COST;

/// Cost of one byte of serialized output (`tojson`, `pprint` and converting
/// containers to strings).  Serializing walks the whole value which is a lot
/// slower than copying bytes.
pub(crate) const SERIALIZED_BYTE_COST: u64 = 16;

/// Cost of one format specifier in the `format` filter.
pub(crate) const FORMAT_SPEC_COST: u64 = 4 * UNITS_PER_FUEL;

/// Helper for tracking fuel consumption.
pub(crate) struct FuelTracker {
    // The initial fuel level.
    initial: u64,
    // The remaining fuel in units.
    remaining: Cell<i64>,
}

impl FuelTracker {
    /// Creates a new fuel tracker.
    pub fn new(fuel: u64) -> FuelTracker {
        FuelTracker {
            initial: fuel,
            remaining: Cell::new(
                i64::try_from(fuel.saturating_mul(UNITS_PER_FUEL)).unwrap_or(i64::MAX),
            ),
        }
    }

    /// Consumes the given number of units.
    ///
    /// Fails if this exhausts the fuel or if the fuel was already exhausted.
    #[inline]
    pub fn consume(&self, units: u64) -> Result<(), Error> {
        let remaining = self
            .remaining
            .get()
            .saturating_sub(i64::try_from(units).unwrap_or(i64::MAX));
        self.remaining.set(remaining);
        if remaining <= 0 {
            Err(Error::from(ErrorKind::OutOfFuel))
        } else {
            Ok(())
        }
    }

    /// Checks that the given number of units could still be consumed.
    ///
    /// This is used before allocating something of a known size.  It does not
    /// consume the fuel as the result is charged once it exists, but if the
    /// fuel would run out, the tracker is exhausted immediately.
    #[inline]
    pub fn ensure(&self, units: u64) -> Result<(), Error> {
        let remaining = self.remaining.get();
        if remaining <= 0 || units >= remaining as u64 {
            self.remaining.set(0);
            Err(Error::from(ErrorKind::OutOfFuel))
        } else {
            Ok(())
        }
    }

    /// Tracks an instruction.  If it runs out of fuel an error is returned.
    #[inline]
    pub fn track(&self, instr: &Instruction) -> Result<(), Error> {
        if consumes_fuel(instr) {
            self.consume(INSTRUCTION_COST)
        } else {
            Ok(())
        }
    }

    /// Returns the remaining fuel in units.
    #[inline]
    pub fn remaining_units(&self) -> u64 {
        self.remaining.get().max(0) as u64
    }

    /// Returns the remaining fuel.
    pub fn remaining(&self) -> u64 {
        self.remaining.get().max(0) as u64 / UNITS_PER_FUEL
    }

    /// Returns the consumed fuel.
    pub fn consumed(&self) -> u64 {
        self.initial.saturating_sub(self.remaining())
    }
}

/// Does an instruction consume fuel?
fn consumes_fuel(instruction: &Instruction) -> bool {
    !matches!(
        instruction,
        Instruction::BeginCapture(_)
            | Instruction::PushLoop(_)
            | Instruction::PushDidNotIterate
            | Instruction::PushWith
            | Instruction::PopFrame
            | Instruction::PopLoopFrame
            | Instruction::DupTop
            | Instruction::DiscardTop
            | Instruction::PushAutoEscape
            | Instruction::PopAutoEscape
    ) && !is_free_feature_instruction(instruction)
}

#[allow(unused_variables)]
fn is_free_feature_instruction(instruction: &Instruction) -> bool {
    #[cfg(feature = "multi_template")]
    if matches!(instruction, Instruction::ExportLocals) {
        return true;
    }
    #[cfg(feature = "multi_template")]
    if matches!(instruction, Instruction::LoadBlocks) {
        return true;
    }
    #[cfg(feature = "macros")]
    if matches!(
        instruction,
        Instruction::BuildMacro(..) | Instruction::Return
    ) {
        return true;
    }
    false
}

/// Returns the cost of `n` items of the given unit cost.
#[inline]
pub(crate) fn cost_of(n: usize, unit: u64) -> u64 {
    (n as u64).saturating_mul(unit)
}

/// Returns the cost of sorting `n` items.
pub(crate) fn sort_cost(n: usize) -> u64 {
    cost_of(n, lookup_cost(n))
}

/// Returns the cost of looking up an item among `n` sorted items.
pub(crate) fn lookup_cost(n: usize) -> u64 {
    let log = usize::BITS - n.leading_zeros();
    COMPARE_COST.saturating_mul(log as u64)
}

/// Values nested deeper than this are refused (hars patch): the engine's
/// own walks over a value (printing it, comparing it) recurse, and a
/// template could otherwise nest a list a hundred thousand levels deep
/// one `{% set a = [a] %}` at a time and exhaust the stack.
pub(crate) const MAX_VALUE_DEPTH: usize = 256;

/// A depth-first walk over a value and everything it holds, one value per
/// step: its string bytes, plus a slot for each value held by a container.
/// Each step is constant work (containers are walked through iterators,
/// never copied), so stopping a walk early stops its work too.
pub(crate) struct DeepWalk {
    first: Option<Value>,
    stack: Vec<(Box<dyn Iterator<Item = Value> + Send + Sync>, usize)>,
}

impl DeepWalk {
    pub(crate) fn new(value: &Value) -> DeepWalk {
        DeepWalk {
            first: Some(value.clone()),
            stack: Vec::new(),
        }
    }

    /// The cost of the next value, `None` when the walk is done, `u64::MAX`
    /// for a value nested too deep.
    pub(crate) fn step(&mut self) -> Option<u64> {
        let (v, depth, held) = match self.first.take() {
            Some(v) => (v, 0, 0),
            None => loop {
                let (iter, depth) = self.stack.last_mut()?;
                match iter.next() {
                    Some(v) => break (v, *depth + 1, SLOT_COST),
                    None => {
                        self.stack.pop();
                    }
                }
            },
        };
        if depth > MAX_VALUE_DEPTH {
            self.stack.clear();
            return Some(u64::MAX);
        }
        let own = match v.0 {
            ValueRepr::String(ref s, _) => cost_of(s.len(), BYTE_COST),
            ValueRepr::SmallStr(ref s) => cost_of(s.as_str().len(), BYTE_COST),
            ValueRepr::Bytes(ref b) => cost_of(b.len(), BYTE_COST),
            ValueRepr::Object(ref obj) => {
                match obj.repr() {
                    crate::value::ObjectRepr::Seq | crate::value::ObjectRepr::Iterable => {
                        if let Some(iter) = obj.try_iter() {
                            self.stack.push((iter, depth));
                        }
                    }
                    crate::value::ObjectRepr::Map => {
                        if let Some(keys) = obj.try_iter() {
                            let map = obj.clone();
                            self.stack.push((
                                Box::new(keys.flat_map(move |key| {
                                    let item = map.get_value(&key);
                                    std::iter::once(key).chain(item)
                                })),
                                depth,
                            ));
                        }
                    }
                    crate::value::ObjectRepr::Plain => {}
                }
                0
            }
            _ => 0,
        };
        Some(held.saturating_add(own))
    }
}

/// Returns the size cost of a value and of everything it holds, up to
/// `limit` (anything over it is as bad as `limit + 1`).
///
/// hars patch: the engine charges a container for its own slots only, so a
/// list that holds the same large value many times (`[s] * n`, or
/// `[b, b]` nested) costs almost nothing while operations that walk it
/// deeply (`~`, `==`, `in`, printing) do work in proportion to its full
/// size, without fuel.  Charging the deep size when a value is made keeps
/// every later walk within the budget.  The walk stops at `limit`, and a
/// value nested deeper than [`MAX_VALUE_DEPTH`] costs more than any limit.
pub(crate) fn deep_value_cost(value: &Value, limit: u64) -> u64 {
    let mut walk = DeepWalk::new(value);
    let mut total = 0u64;
    while let Some(cost) = walk.step() {
        total = total.saturating_add(cost);
        if total > limit {
            break;
        }
    }
    total
}

/// The cost of comparing two values deeply: the deep size of the smaller
/// (an equality check stops where the shorter runs out), found by walking
/// both in step so the larger is never walked further than the smaller.
///
/// hars patch: comparing walks both values every time, so charging their
/// size once, when they were made, doesn't cover comparing them in a loop.
pub(crate) fn deep_compare_cost(a: &Value, b: &Value, limit: u64) -> u64 {
    let (mut wa, mut wb) = (DeepWalk::new(a), DeepWalk::new(b));
    let (mut ca, mut cb) = (0u64, 0u64);
    loop {
        match (wa.step(), wb.step()) {
            (None, _) => return ca,
            (_, None) => return cb,
            (Some(x), Some(y)) => {
                ca = ca.saturating_add(x);
                cb = cb.saturating_add(y);
                if ca.min(cb) > limit {
                    return ca.min(cb);
                }
            }
        }
    }
}

/// An iterator adapter that charges fuel for every item.
///
/// When the fuel runs out, it yields a single invalid value that carries the
/// out of fuel error and then ends.
pub(crate) struct FuelIter<'a, I> {
    tracker: Option<&'a FuelTracker>,
    iter: I,
    done: bool,
}

impl<'a, I> FuelIter<'a, I> {
    pub fn new(tracker: Option<&'a FuelTracker>, iter: I) -> Self {
        FuelIter {
            tracker,
            iter,
            done: false,
        }
    }
}

impl<I: Iterator<Item = Value>> Iterator for FuelIter<'_, I> {
    type Item = Value;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if let Some(tracker) = self.tracker {
            if let Err(err) = tracker.consume(ITEM_COST) {
                self.done = true;
                return Some(Value::from(err));
            }
        }
        self.iter.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.done {
            (0, Some(0))
        } else {
            let (lower, upper) = self.iter.size_hint();
            (lower, upper.map(|x| x.saturating_add(1)))
        }
    }
}
