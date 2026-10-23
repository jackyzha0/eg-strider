mod file;

pub use file::Malformed;

use crate::operation::{ByteRange, Operation, OperationKind, OperationRun, OperationRunSlice, DELETES_END_OF_PREVIOUS_INSERT, MAX_OP_LEN};
use crate::replay::{self, DocumentState, ReplayPlan};
use std::cmp::Ordering;
use std::ops::Range;
use strider_graph::{CausalGraph, EventIndex};

/// An event's index among its replica's events; with the replica, it names the event on every
/// replica.
pub type SequenceNumber = u32;
/// A replica's index in this log, in the order the log registered it.
pub type ReplicaId = u32;

/// An operation run's position in the log.
pub(crate) type OperationRunIndex = u32;

/// Consecutive events by one replica with consecutive sequence numbers.
struct ReplicaRun {
    first_event: EventIndex,
    len: u32,
    replica: ReplicaId,
    sequence_number: SequenceNumber,
}

/// An Eg-walker (Gentle and Kleppmann, 2024) operation log: every event a replica knows of, with
/// its parents, replica and operation.
///
/// Events must be valid as the paper's Definition C.1 defines, and a chain of operations must carry
/// exactly its inserts' text; neither is checked.
#[derive(Default)]
pub struct OpLog {
    pub(crate) graph: CausalGraph,
    ops: Vec<OperationRun>,
    inserted_text: String,
    replica_names: Vec<String>,
    next_sequence_numbers: Vec<SequenceNumber>,
    replica_runs: Vec<ReplicaRun>,
    replay_plan: Option<ReplayPlan>,
    /// The document as of a prefix of history, which the next checkout replays from.
    base: DocumentState,
    /// The latest checkout, while the frontier has more than one event; the base then stays put, so
    /// that runs still being appended to are not split at every checkout.
    latest_document_state: Option<DocumentState>,
    /// The loaded file's history from before the first held event, not yet read back.
    unread_history: Option<file::UnreadHistory>,
    /// Reused buffer for the frontier as a local event's parents.
    parents: Vec<EventIndex>,
    /// Characters inserted and deleted over all events.
    totals: (u32, u32),
}

/// How many of the first `room` of `edits` continue a backspace run from a delete at `pos`, each
/// deleting the character before the last.
#[inline(never)]
fn held_backspaces(edits: &[Operation], pos: u32, room: u32) -> u32 {
    let mut held = 0;
    while held < room && matches!(edits.get(held as usize), Some(&Operation::Delete { pos: at, len: 1, .. }) if at + held + 1 == pos) {
        held += 1;
    }
    held
}

/// The length of `text`'s ASCII prefix.
fn ascii_prefix(text: &[u8]) -> usize {
    // Whole 256-byte chunks are checked a word at a time; only the chunk that fails is searched.
    let chunks = text.chunks(256).take_while(|chunk| chunk.is_ascii()).count();
    let rest = &text[(chunks * 256).min(text.len())..];
    text.len() - rest.len() + rest.iter().take_while(|byte| byte.is_ascii()).count()
}

impl OpLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of events: characters inserted plus characters deleted.
    pub fn len(&self) -> usize {
        self.graph.len() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.graph.is_empty()
    }

    pub fn get_or_create_replica(&mut self, name: &str) -> ReplicaId {
        if let Some(i) = self.replica_names.iter().position(|n| n == name) {
            return i as ReplicaId;
        }
        self.replica_names.push(name.to_string());
        self.next_sequence_numbers.push(0);
        (self.replica_names.len() - 1) as ReplicaId
    }

    pub fn replica_name(&self, replica: ReplicaId) -> Option<&str> {
        self.replica_names.get(replica as usize).map(String::as_str)
    }

    /// The frontier: the version a local edit is made at.
    pub fn frontier(&self) -> Vec<EventIndex> {
        self.graph.frontier().to_vec()
    }

    /// Merges every event and returns the document.
    pub fn checkout(&mut self) -> &str {
        let len = self.graph.len();
        if self.base.events != len && self.latest_document_state.as_ref().is_none_or(|latest| latest.events != len) {
            // An event made without all that the base holds is concurrent with some of it.
            // The merge then has to tell what the base holds from the rest.
            if self.graph.start_run_at(self.base.events) {
                self.split_op(self.base.events);
                self.replay_plan.iter_mut().for_each(ReplayPlan::outdate);
            }
            let mut plan = self.replay_plan.take();
            let (text, chars) = replay::replay(self, &mut plan, &self.base);
            self.replay_plan = plan;
            let merged = DocumentState { events: len, chars, text };
            if self.graph.frontier().len() == 1 {
                (self.base, self.latest_document_state) = (merged, None);
            } else {
                self.latest_document_state = Some(merged);
            }
        }
        &self.merged().text
    }

    /// The document as of the last checkout.
    fn merged(&self) -> &DocumentState {
        self.latest_document_state.as_ref().unwrap_or(&self.base)
    }

    /// How many events of a loaded file's history have not been read back.
    pub fn unread_events(&self) -> usize {
        self.graph.first_held_event() as usize
    }

    /// How many concurrent ranges have needed CRDT records rather than transforming alone.
    pub fn ranges_using_crdt_records(&self) -> usize {
        self.replay_plan.as_ref().map_or(0, ReplayPlan::ranges_using_crdt_records)
    }

    /// Forgets every cached replay result, reading back all saved history, so that the next
    /// checkout replays from the start.
    pub fn clear_replay_cache(&mut self) -> Result<(), Malformed> {
        self.unpack_from(0)?;
        (self.replay_plan, self.base, self.latest_document_state) = (None, DocumentState::default(), None);
        Ok(())
    }

    /// Inserts `text` at `pos` in the document at the frontier.
    pub fn add_insert(&mut self, replica: ReplicaId, pos: usize, text: &str) -> Range<EventIndex> {
        self.append_events(replica, None, None, OperationKind::Insert, false, pos, text.chars().count(), text)
    }

    /// Deletes `range` from the document at the frontier.
    pub fn add_delete(&mut self, replica: ReplicaId, range: Range<usize>) -> Range<EventIndex> {
        self.append_events(replica, None, None, OperationKind::Delete, false, range.start, range.len(), "")
    }

    /// Inserts `text` at `pos` in the document at version `parents` (ascending), reading back saved
    /// history if needed.
    pub fn add_insert_at(&mut self, replica: ReplicaId, parents: &[EventIndex], pos: usize, text: &str) -> Result<Range<EventIndex>, Malformed> {
        self.unpack_needed_history(parents)?;
        Ok(self.append_events(replica, None, Some(parents), OperationKind::Insert, false, pos, text.chars().count(), text))
    }

    /// Deletes `range` from the document at version `parents` (ascending), the last character first
    /// if `backward`, reading back saved history if needed.
    pub fn add_delete_at(&mut self, replica: ReplicaId, parents: &[EventIndex], range: Range<usize>, backward: bool) -> Result<Range<EventIndex>, Malformed> {
        self.unpack_needed_history(parents)?;
        Ok(self.append_events(replica, None, Some(parents), OperationKind::Delete, backward, range.start, range.len(), ""))
    }

    /// As [`add_insert_at`](Self::add_insert_at), for a remote event with its own sequence number.
    pub fn add_remote_insert(
        &mut self,
        replica: ReplicaId,
        sequence_number: SequenceNumber,
        parents: &[EventIndex],
        pos: usize,
        text: &str,
    ) -> Result<Range<EventIndex>, Malformed> {
        self.unpack_needed_history(parents)?;
        Ok(self.append_events(replica, Some(sequence_number), Some(parents), OperationKind::Insert, false, pos, text.chars().count(), text))
    }

    /// As [`add_delete_at`](Self::add_delete_at), for a remote event with its own sequence number.
    pub fn add_remote_delete(
        &mut self,
        replica: ReplicaId,
        sequence_number: SequenceNumber,
        parents: &[EventIndex],
        range: Range<usize>,
        backward: bool,
    ) -> Result<Range<EventIndex>, Malformed> {
        self.unpack_needed_history(parents)?;
        Ok(self.append_events(replica, Some(sequence_number), Some(parents), OperationKind::Delete, backward, range.start, range.len(), ""))
    }

    /// Appends a remote replica's chain of operations, the first at version `parents` (ascending),
    /// with `text` holding their inserts' text, reading back saved history if needed.
    pub fn add_remote_ops(
        &mut self,
        replica: ReplicaId,
        sequence_number: SequenceNumber,
        parents: &[EventIndex],
        ops: &[Operation],
        text: &str,
    ) -> Result<Range<EventIndex>, Malformed> {
        self.unpack_needed_history(parents)?;
        // One operation owns all the supplied text. No character-to-byte boundary or
        // within-chain run needs to be found, as it does for a longer chain.
        if let [op] = ops {
            let (kind, backward, pos, len) = match *op {
                Operation::Insert { pos, len } => (OperationKind::Insert, false, pos, len),
                Operation::Delete { pos, len, backward } => (OperationKind::Delete, backward, pos, len),
            };
            debug_assert_eq!(text.chars().count(), if kind == OperationKind::Insert { len as usize } else { 0 }, "the text is not what the inserts put in");
            return Ok(self.append_events(replica, Some(sequence_number), Some(parents), kind, backward, pos as usize, len as usize, text));
        }
        let event = self.graph.len();
        let total = self.append_operations(event, parents, ops, text);
        if total == 0 {
            return Ok(event..event);
        }
        let (_, events) = self.add_to_causal_graph(Some(parents), total);
        self.assign_replica(replica, Some(sequence_number), event, total);
        Ok(events)
    }

    /// Stores a chain's operations and text, returning the number of events; the caller updates the
    /// graph and replica runs. Not inlined, so that its loop keeps the op being built in registers.
    #[inline(never)]
    fn append_operations(&mut self, event: EventIndex, parents: &[EventIndex], ops: &[Operation], text: &str) -> u32 {
        // The op being built, which each edit that carries it on is joined into. It is stored
        // once one does not, so that most edits change nothing in memory.
        let mut last = if event > 0 && parents == [event - 1] { self.ops.pop() } else { None };
        let mut inserted = 0;
        let mut at = self.inserted_text.len() as u32;
        self.append_text(text);
        let end = u32::try_from(self.inserted_text.len()).expect("more inserted text than a u32 counts");
        // Characters and bytes coincide in ASCII text. `plain` is where the text stops being
        // ASCII: found again only once an insert reaches it, since such characters are few.
        let ascii_until = |from: u32| from + ascii_prefix(&text.as_bytes()[(from - (end - text.len() as u32)) as usize..]) as u32;
        let mut plain = ascii_until(at);
        let mut next = event;
        let mut i = 0;
        while let Some(edit) = ops.get(i) {
            i += 1;
            if let Operation::Insert { pos, len } = *edit {
                if len == 0 {
                    continue;
                }
                let bytes = if len <= plain.saturating_sub(at) {
                    len
                } else {
                    let bytes = self.prefix((at, end), len);
                    plain = ascii_until(at + bytes);
                    bytes
                };
                let op = OperationRun::new(next, len, OperationKind::Insert, false, false, pos, at + bytes);
                if !last.as_mut().is_some_and(|last| last.append(op)) {
                    self.ops.extend(last.replace(op));
                }
                inserted += len;
                (at, next) = (at + bytes, next.checked_add(len).expect("more events than a u32 counts"));
                continue;
            }
            let Operation::Delete { mut pos, mut len, mut backward } = *edit else { unreachable!() };
            if len == 0 {
                continue;
            }
            // Holding backspace: deletes of one character, each just before the one before,
            // often hundreds in a row. They are taken as one delete that runs backwards.
            if len == 1 && matches!(ops.get(i), Some(&Operation::Delete { pos: at, len: 1, .. }) if at + 1 == pos) {
                // Rare in most histories, and laid out away from the rest so that it costs
                // them nothing much; where it is common, a whole run is taken at once.
                std::hint::cold_path();
                let held = held_backspaces(&ops[i..], pos, MAX_OP_LEN - 1);
                if held > 0 {
                    (backward, pos, len) = (true, pos - held, len + held);
                    i += held as usize;
                }
            }
            let op = OperationRun::new(next, len, OperationKind::Delete, backward, false, pos, at);
            if !last.as_mut().is_some_and(|last| last.append(op)) {
                self.ops.extend(last.replace(op));
            }
            next = next.checked_add(len).expect("more events than a u32 counts");
        }
        self.ops.extend(last);
        // Every event either inserts or removes a character.
        self.totals = (self.totals.0 + inserted, self.totals.1 + (next - event - inserted));
        debug_assert_eq!(at, end, "the text is not what the inserts put in");
        next - event
    }

    /// Appends events forming one operation run.
    #[allow(clippy::too_many_arguments)]
    fn append_events(
        &mut self,
        replica: ReplicaId,
        sequence_number: Option<SequenceNumber>,
        parents: Option<&[EventIndex]>,
        kind: OperationKind,
        backward: bool,
        pos: usize,
        len: usize,
        text: &str,
    ) -> Range<EventIndex> {
        let event = self.graph.len();
        if len == 0 {
            return event..event;
        }
        let len = u32::try_from(len).expect("more events than a u32 counts");
        let pos = u32::try_from(pos).expect("a position past what a u32 counts");
        let (continues, events) = self.add_to_causal_graph(parents, len);
        self.assign_replica(replica, sequence_number, event, len);
        self.append_text(text);
        let end = u32::try_from(self.inserted_text.len()).expect("more inserted text than a u32 counts");
        self.append_operation_run(continues, OperationRun::new(event, len, kind, backward, false, pos, end));
        events
    }

    /// Reads back saved history if an event at `parents` has not seen all of it, from the part
    /// holding its latest parent: parts start at critical versions.
    fn unpack_needed_history(&mut self, parents: &[EventIndex]) -> Result<(), Malformed> {
        if let Some(unread) = &self.unread_history {
            let first_held = self.graph.first_held_event();
            if !parents.iter().any(|&parent| parent >= first_held) && !unread.frontier.iter().all(|event| parents.contains(event)) {
                let part = parents.iter().max().map_or(0, |&parent| unread.part_holding(parent));
                self.unpack_from(part)?;
            }
        }
        Ok(())
    }

    /// Adds a chain of `len` events at `parents`, or the frontier for `None`, returning whether
    /// they follow the last event and their indexes.
    fn add_to_causal_graph(&mut self, parents: Option<&[EventIndex]>, len: u32) -> (bool, Range<EventIndex>) {
        let event = self.graph.len();
        // Most events follow the last one, whose number is then all the graph needs.
        let continues = event > 0 && parents.map_or(self.graph.frontier() == [event - 1], |parents| parents == [event - 1]);
        if continues {
            return (true, self.graph.push(&[event - 1], len));
        }
        let events = match parents {
            Some(parents) => self.graph.push(parents, len),
            // The frontier changes as the events go in, so it is copied first.
            None => {
                let mut frontier = std::mem::take(&mut self.parents);
                frontier.clear();
                frontier.extend_from_slice(self.graph.frontier());
                let events = self.graph.push(&frontier, len);
                self.parents = frontier;
                events
            }
        };
        (false, events)
    }

    /// Records that `replica` made the `len` events from `event`, numbered from `sequence_number`
    /// or else from where its last left off.
    fn assign_replica(&mut self, replica: ReplicaId, sequence_number: Option<SequenceNumber>, event: EventIndex, len: u32) {
        let sequence_number = sequence_number.unwrap_or(self.next_sequence_numbers[replica as usize]);
        let next = &mut self.next_sequence_numbers[replica as usize];
        *next = (*next).max(sequence_number + len);
        match self.replica_runs.last_mut() {
            Some(r) if r.replica == replica && r.first_event + r.len == event && r.sequence_number + r.len == sequence_number => r.len += len,
            _ => self.replica_runs.push(ReplicaRun { first_event: event, len, replica, sequence_number }),
        }
    }

    /// Appends a complete UTF-8 payload, keeping tiny copies in the caller.
    #[inline]
    fn append_text(&mut self, text: &str) {
        let n = text.len();
        if n == 0 {
            return;
        }
        if n > 8 {
            self.inserted_text.push_str(text);
            return;
        }
        // SAFETY: reserve makes the destination writable for all n bytes. Every load and
        // store is inside those n bytes; overlapping destination words write identical
        // bytes. Only after the entire valid string has been copied is its length exposed.
        unsafe {
            let bytes = self.inserted_text.as_mut_vec();
            bytes.reserve(n);
            let old = bytes.len();
            let to = bytes.as_mut_ptr().add(old);
            let from = text.as_ptr();
            if n >= 4 {
                std::ptr::copy_nonoverlapping(from, to, 4);
                std::ptr::copy_nonoverlapping(from.add(n - 4), to.add(n - 4), 4);
            } else if n >= 2 {
                std::ptr::copy_nonoverlapping(from, to, 2);
                std::ptr::copy_nonoverlapping(from.add(n - 2), to.add(n - 2), 2);
            } else {
                *to = *from;
            }
            bytes.set_len(old + n);
        }
    }

    /// Stores an op, joining it onto the last when it `follows` it and continues it.
    fn append_operation_run(&mut self, follows: bool, op: OperationRun) {
        match op.kind() {
            OperationKind::Insert => self.totals.0 += op.len(),
            OperationKind::Delete => self.totals.1 += op.len(),
        }
        if !(follows && self.ops.last_mut().is_some_and(|last| last.append(op))) {
            self.ops.push(op);
        }
    }

    /// Makes an op start at event `at`, if one runs across it.
    fn split_op(&mut self, at: EventIndex) {
        let i = self.op_containing(at) as usize;
        let Some(op) = self.ops.get(i).filter(|op| op.first_event < at) else { return };
        let (start, end) = (op.first_event, op.first_event + op.len());
        let part = |events: Range<EventIndex>| {
            let part = self.slice(i, &events);
            OperationRun::new(part.first_event, part.len, part.kind, part.backward, false, part.pos, part.text.1)
        };
        let parts = [part(start..at), part(at..end)];
        self.ops.splice(i..=i, parts);
        // A delete only trims straight after an insert or after another delete that does.
        for after in self.ops[i + 2..].iter_mut().take_while(|after| after.deletes_end_of_previous_insert()) {
            after.len_and_flags &= !DELETES_END_OF_PREVIOUS_INSERT;
        }
    }

    /// The byte length of the first `len` characters of the text at `bytes`.
    #[cold]
    fn prefix(&self, bytes: ByteRange, len: u32) -> u32 {
        let text = self.text(bytes);
        // Consume exactly the requested characters and inspect the iterator's remaining
        // slice, rather than decoding one extra character just to obtain its byte index.
        let consumed = |text: &str, len: usize| {
            let mut chars = text.chars();
            if len > 0 {
                let last = chars.nth(len - 1);
                debug_assert!(last.is_some(), "the text is not what the inserts put in");
            }
            text.len() - chars.as_str().len()
        };
        if len < 64 {
            return consumed(text, len as usize) as u32;
        }
        let (mut at, mut remaining) = (0, len as usize);
        // Each character occupies at least one byte. Taking `remaining` bytes therefore
        // cannot pass the requested character boundary. Count this prefix in bulk, then
        // repeat for the characters still missing; every byte is counted only once.
        while remaining >= 64 {
            let mut end = (at + remaining).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == text.len() {
                debug_assert_eq!(text[at..].chars().count(), remaining, "the text is not what the inserts put in");
                return end as u32;
            }
            remaining -= text[at..end].chars().count();
            at = end;
        }
        (at + consumed(&text[at..], remaining)) as u32
    }

    /// The replica and sequence number of `event`.
    fn replica_at(&self, event: EventIndex) -> (ReplicaId, SequenceNumber) {
        let run = &self.replica_runs[self.replica_runs.partition_point(|r| r.first_event <= event) - 1];
        (run.replica, run.sequence_number + (event - run.first_event))
    }

    /// The tie-break between concurrent inserts at one position: whether `a`'s goes first, by
    /// replica name and then sequence number.
    pub(crate) fn inserts_before(&self, a: EventIndex, b: EventIndex) -> bool {
        let (replica_a, sequence_number_a) = self.replica_at(a);
        let (replica_b, sequence_number_b) = self.replica_at(b);
        match self.replica_names[replica_a as usize].cmp(&self.replica_names[replica_b as usize]) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => sequence_number_a < sequence_number_b,
        }
    }

    /// For each of the ascending `starts`, the op holding it, or the number of ops past the last
    /// event.
    pub(crate) fn ops_containing(&self, starts: impl Iterator<Item = EventIndex>) -> Vec<OperationRunIndex> {
        let mut op = 0;
        starts
            .map(|event| {
                // The terminal boundary is already known. In a long linear range this avoids
                // walking every operation merely to discover that none follow the last event.
                if event == self.graph.len() {
                    return self.ops.len() as OperationRunIndex;
                }
                while self.ops.get(op).is_some_and(|op| op.first_event + op.len() <= event) {
                    op += 1;
                }
                op as OperationRunIndex
            })
            .collect()
    }

    /// Iterates over the parts of ops inside `events`. `ops` runs from the op holding the first
    /// event to the op holding the event after the last.
    pub(crate) fn ops(&self, ops: Range<OperationRunIndex>, events: Range<EventIndex>) -> impl DoubleEndedIterator<Item = OperationRunSlice> + '_ {
        // The op containing the event after the last holds some of these too if it starts
        // before then.
        let straddles = self.ops.get(ops.end as usize).is_some_and(|op| op.first_event < events.end);
        let count = if events.is_empty() { 0 } else { (ops.end - ops.start) as usize + straddles as usize };
        (ops.start as usize..ops.start as usize + count).map(move |op| self.slice(op, &events))
    }

    /// The event spans and kinds in a range, without decoding positions or inserted text.
    /// Moving the prepare version only needs these two fields.
    pub(crate) fn event_kinds(
        &self,
        ops: Range<OperationRunIndex>,
        events: Range<EventIndex>,
    ) -> impl Iterator<Item = (OperationKind, Range<EventIndex>)> + '_ {
        let straddles = self.ops.get(ops.end as usize).is_some_and(|op| op.first_event < events.end);
        let count = if events.is_empty() { 0 } else { (ops.end - ops.start) as usize + straddles as usize };
        self.ops[ops.start as usize..ops.start as usize + count].iter().map(move |op| {
            (op.kind(), events.start.max(op.first_event)..events.end.min(op.first_event + op.len()))
        })
    }

    pub(crate) fn op_containing(&self, event: EventIndex) -> OperationRunIndex {
        self.ops.partition_point(|op| op.first_event + op.len() <= event) as OperationRunIndex
    }

    /// All inserted text, which [`OperationRunSlice::text`] points into.
    pub(crate) fn inserted_text(&self) -> &str {
        &self.inserted_text
    }

    fn text(&self, (start, end): ByteRange) -> &str {
        &self.inserted_text[start as usize..end as usize]
    }

    /// [`lengths`](Self::lengths) over every event.
    pub(crate) fn total_lengths(&self) -> (u32, u32) {
        self.totals
    }

    /// Counts the characters `events` insert and remove, applied as recorded. `first_op` is the op
    /// holding the first of them.
    pub(crate) fn lengths(&self, first_op: OperationRunIndex, events: Range<EventIndex>) -> (u32, u32) {
        let (mut inserted, mut removed) = (0, 0);
        for op in &self.ops[first_op as usize..] {
            if op.first_event.max(events.start) >= events.end {
                break;
            }
            let len = events.end.min(op.first_event + op.len()) - events.start.max(op.first_event);
            // Which kind an op is cannot be predicted, so both sums are added to without
            // branching on it.
            let insert = (op.kind() == OperationKind::Insert) as u32;
            inserted += len * insert;
            removed += len * (1 - insert);
        }
        (inserted, removed)
    }

    /// The byte range of characters `from..to` of the text at `bytes`.
    #[cold]
    fn char_bytes(&self, bytes: ByteRange, from: u32, to: u32) -> ByteRange {
        let text = self.text(bytes);
        // Where each character starts, then where the text ends.
        let mut starts = text.char_indices().map(|(byte, _)| byte as u32).chain([text.len() as u32]);
        let first = starts.nth(from as usize).expect("an op is cut within its text");
        let last = if from < to { starts.nth((to - from - 1) as usize).expect("an op is cut within its text") } else { first };
        (bytes.0 + first, bytes.0 + last)
    }

    /// The part of op `i` inside `events`.
    #[inline]
    fn slice(&self, i: usize, events: &Range<EventIndex>) -> OperationRunSlice {
        let op = &self.ops[i];
        let (len, kind, backward) = (op.len(), op.kind(), op.backward());
        let bytes = (if i == 0 { 0 } else { self.ops[i - 1].inserted_text_end }, op.inserted_text_end);
        let from = events.start.max(op.first_event) - op.first_event;
        let to = events.end.min(op.first_event + len) - op.first_event;
        // Nearly always the whole op, which needs no working out whatever its kind.
        if to - from == len {
            return OperationRunSlice {
                first_event: op.first_event,
                len,
                kind,
                backward,
                deletes_end_of_previous_insert: op.deletes_end_of_previous_insert(),
                pos: op.pos,
                text: bytes,
            };
        }
        let (pos, text) = match (kind, backward) {
            (OperationKind::Insert, _) => {
                let (start, end) = bytes;
                // Characters and bytes coincide when the text is all ASCII.
                let text = if end - start == len { (start + from, start + to) } else { self.char_bytes(bytes, from, to) };
                (op.pos + from, text)
            }
            // Each forward delete lands on the same position, so a later slice starts where
            // the op did.
            (OperationKind::Delete, false) => (op.pos, bytes),
            (OperationKind::Delete, true) => (op.pos + len - to, bytes),
        };
        OperationRunSlice { first_event: op.first_event + from, len: to - from, kind, backward, deletes_end_of_previous_insert: false, pos, text }
    }
}
