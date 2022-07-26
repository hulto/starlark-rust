/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Summary of heap allocations and function times with stacks.

use std::cell::RefCell;
use std::collections::hash_map;
use std::collections::HashMap;
use std::io::Write;
use std::ops::AddAssign;
use std::rc::Rc;
use std::time::Instant;

use gazebo::dupe::Dupe;
use starlark_map::small_set::SmallSet;

use crate::eval::runtime::profile::flamegraph::FlameGraphWriter;
use crate::eval::runtime::small_duration::SmallDuration;
use crate::values::layout::heap::arena::ArenaVisitor;
use crate::values::layout::pointer::RawPointer;
use crate::values::Heap;
use crate::values::Value;

/// Map strings to integers 0, 1, 2, ...
#[derive(Default)]
struct StringIndex {
    strings: SmallSet<String>,
}

impl StringIndex {
    fn index(&mut self, s: &str) -> usize {
        if let Some(index) = self.strings.get_index_of(s) {
            return index;
        }

        let inserted = self.strings.insert(s.to_owned());
        assert!(inserted);
        self.strings.len() - 1
    }

    fn get_all(&self) -> Vec<&str> {
        self.strings.iter().map(|s| s.as_str()).collect()
    }
}

#[derive(Copy, Clone, Dupe, Debug, Eq, PartialEq, Hash)]
pub(crate) struct FunctionId(
    /// Index in strings index.
    pub(crate) usize,
);

/// A mapping from function Value to FunctionId, which must be continuous
#[derive(Default)]
pub(crate) struct FunctionIds {
    values: HashMap<RawPointer, FunctionId>,
    strings: StringIndex,
}

impl FunctionIds {
    pub(crate) fn get_string(&mut self, x: &str) -> FunctionId {
        FunctionId(self.strings.index(x))
    }

    fn get_value(&mut self, x: Value) -> FunctionId {
        match self.values.entry(x.ptr_value()) {
            hash_map::Entry::Occupied(v) => *v.get(),
            hash_map::Entry::Vacant(outer) => {
                let function_id = FunctionId(self.strings.index(&x.to_str()));
                outer.insert(function_id);
                function_id
            }
        }
    }

    pub(crate) fn invert(&self) -> Vec<&str> {
        self.strings.get_all()
    }
}

/// Allocations counters.
#[derive(Default, Copy, Clone, Dupe, Debug)]
pub(crate) struct AllocCounts {
    pub(crate) bytes: usize,
    pub(crate) count: usize,
}

impl AddAssign for AllocCounts {
    fn add_assign(&mut self, other: AllocCounts) {
        self.bytes += other.bytes;
        self.count += other.count;
    }
}

/// A stack frame, its caller and the functions it called, and the allocations it made itself.
pub(crate) struct StackFrameData {
    pub(crate) callees: HashMap<FunctionId, StackFrame>,
    pub(crate) allocs: HashMap<&'static str, AllocCounts>,
    /// Time spent in this frame excluding callees.
    /// Double, because enter/exit are recorded twice, in drop and non-drop heaps.
    pub(crate) time_x2: SmallDuration,
    /// How many times this function was called (with this stack).
    /// Double.
    pub(crate) calls_x2: u32,
}

#[derive(Clone, Dupe)]
pub(crate) struct StackFrame(pub(crate) Rc<RefCell<StackFrameData>>);

impl StackFrame {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(StackFrameData {
            callees: Default::default(),
            allocs: Default::default(),
            time_x2: SmallDuration::default(),
            calls_x2: 0,
        })))
    }

    /// Enter a new stack frame.
    fn push(&self, function: FunctionId) -> Self {
        let mut this = self.0.borrow_mut();

        let callee = this.callees.entry(function).or_insert_with(StackFrame::new);

        callee.dupe()
    }

    /// Write this stack frame's data to a file in a format flamegraph.pl understands
    /// (each line is: `func1:func2:func3 BYTES`).
    fn write<'a>(
        &self,
        writer: &mut FlameGraphWriter,
        stack: &'_ mut Vec<&'a str>,
        ids: &[&'a str],
    ) {
        let this = self.0.borrow();

        for (k, v) in this.allocs.iter() {
            writer.write(
                stack.iter().copied().chain(std::iter::once(*k)),
                v.bytes as u64,
            );
        }

        for (id, frame) in this.callees.iter() {
            stack.push(ids[id.0]);
            frame.write(writer, stack, ids);
            stack.pop();
        }
    }
}

/// An accumulator for stack frames that lets us visit the heap.
pub struct StackCollector {
    /// Timestamp of last call enter or exit.
    last_time: Option<Instant>,
    ids: FunctionIds,
    current: Vec<StackFrame>,
}

impl StackCollector {
    pub(crate) fn new() -> Self {
        Self {
            ids: FunctionIds::default(),
            current: vec![StackFrame::new()],
            last_time: None,
        }
    }
}

impl<'v> ArenaVisitor<'v> for StackCollector {
    fn regular_value(&mut self, value: Value<'v>) {
        let frame = match self.current.last() {
            Some(frame) => frame,
            None => return,
        };

        // Value allocated in this frame, record it!
        let typ = value.get_ref().get_type();
        let mut frame = frame.0.borrow_mut();
        let mut entry = frame.allocs.entry(typ).or_default();
        entry.bytes += value.get_ref().total_memory();
        entry.count += 1;
    }

    fn call_enter(&mut self, function: Value<'v>, time: Instant) {
        if let Some(last_time) = self.last_time {
            self.current.last_mut().unwrap().0.borrow_mut().time_x2 +=
                time.saturating_duration_since(last_time);
            self.current.last_mut().unwrap().0.borrow_mut().calls_x2 += 1;
        }

        let frame = match self.current.last() {
            Some(frame) => frame,
            None => return,
        };

        // New frame, enter it.
        let id = self.ids.get_value(function);
        let new_frame = frame.push(id);
        self.current.push(new_frame);

        self.last_time = Some(time)
    }

    fn call_exit(&mut self, time: Instant) {
        if let Some(last_time) = self.last_time {
            self.current.last_mut().unwrap().0.borrow_mut().time_x2 +=
                time.saturating_duration_since(last_time);
        }
        self.current.pop().unwrap();
        self.last_time = Some(time);
    }
}

// TODO(nga): rename to `AggregatedProfileInfo`.
pub(crate) struct Stacks {
    pub(crate) ids: FunctionIds,
    pub(crate) root: StackFrame,
}

impl Stacks {
    pub(crate) fn collect(heap: &Heap) -> Stacks {
        let mut collector = StackCollector::new();
        unsafe {
            heap.visit_arena(&mut collector);
        }
        assert_eq!(1, collector.current.len());
        Stacks {
            ids: collector.ids,
            root: collector.current.pop().unwrap(),
        }
    }

    /// Write this out recursively to a file.
    pub(crate) fn write_to(&self, file: &mut impl Write) -> anyhow::Result<()> {
        let mut writer = FlameGraphWriter::new();
        self.root
            .write(&mut writer, &mut vec![], &self.ids.invert());
        file.write_all(writer.finish().as_bytes())?;
        Ok(())
    }
}