use crate::crdt_records::{CrdtRecords, DeleteTargets, RecordId};
use crate::operation::{ByteRange, OperationKind, OperationRunSlice};
use crate::oplog::{OpLog, OperationRunIndex};
use crate::transform::merge_by_transforming;
use std::ops::Range;
use strider_graph::{walk, EventIndex, EventRunIndex, HistoryRange, WalkStep};
use strider_text::PieceTable;

/// An edit in the merged document. Empty text denotes a removal; insertions carry
/// the byte range already resolved while visiting their operation run.
struct TransformedEdit {
    pos: u32,
    len: u32,
    text: ByteRange,
}

/// The paper's transformed operations for the concurrent ranges, ready to undo
/// without slicing their operation runs a second time.
#[derive(Default)]
pub(crate) struct TransformedOperations {
    edits: Vec<TransformedEdit>,
    /// Earlier edits belong to completed concurrent ranges and cannot be joined.
    coalesce_from: usize,
    /// Length of the effect version so far.
    len: u32,
    /// The first run whose events the base does not hold; earlier runs' operations are not
    /// recorded.
    first_new_run: EventRunIndex,
    /// Whether the current range of runs is of those earlier runs.
    in_base: bool,
}

impl TransformedOperations {
    /// Starts a range of runs, all or none of which the base holds.
    pub(crate) fn begin(&mut self, run_range: Range<EventRunIndex>) {
        self.in_base = run_range.end <= self.first_new_run;
        debug_assert!(self.in_base || run_range.start >= self.first_new_run, "runs with events on both sides of the base");
    }

    /// Records that an insert of `len` characters went in at `pos`.
    pub(crate) fn insert(&mut self, pos: u32, len: u32, text: ByteRange) {
        if !self.in_base {
            self.len += len;
            if self.edits.len() > self.coalesce_from {
                let last = self.edits.last_mut().expect("an edit in the current range");
                if last.text.0 != last.text.1 && last.pos + last.len == pos && last.text.1 == text.0 {
                    last.len += len;
                    last.text.1 = text.1;
                    return;
                }
            }
            self.edits.push(TransformedEdit { pos, len, text });
        }
    }

    /// Records that the current delete removed the `len` characters at `pos`.
    pub(crate) fn remove(&mut self, pos: u32, len: u32) {
        if !self.in_base {
            self.len -= len;
            if self.edits.len() > self.coalesce_from {
                let last = self.edits.last_mut().expect("an edit in the current range");
                if last.text.0 == last.text.1 && last.pos == pos {
                    last.len += len;
                    return;
                }
            }
            self.edits.push(TransformedEdit { pos, len, text: (0, 0) });
        }
    }

    /// Marks where things stand, for [`undo_to`](Self::undo_to).
    pub(crate) fn mark(&self) -> (usize, u32) {
        (self.edits.len(), self.len)
    }

    /// Forgets what was recorded since `mark`.
    pub(crate) fn undo_to(&mut self, mark: (usize, u32)) {
        self.edits.truncate(mark.0);
        self.len = mark.1;
    }
}

/// The CRDT records' result for a concurrent range, cached at its critical version so that a later
/// replay places its events without walking them.
#[derive(Default)]
struct ResolvedOrigins {
    /// How many of the range's events, from its start, `resolved` covers.
    events: u32,
    /// Records, operation run by operation run in log order. An insert: its first character's left
    /// origin, then the right origin all its characters share. A delete: per group of consecutive
    /// targets, the first and the count, with [`BACKWARD_TARGETS`] if they count down.
    resolved: Vec<RecordId>,
}

/// Flags a count of delete targets whose records count down.
const BACKWARD_TARGETS: u32 = 1 << 31;

/// The document as of the first `events` events.
#[derive(Default)]
pub(crate) struct DocumentState {
    pub events: EventIndex,
    /// The length of `text` in characters.
    pub chars: u32,
    pub text: String,
}

/// What replays of a log keep between checkouts while its events stay the same: its ranges, and the
/// CRDT records' cached results.
pub(crate) struct ReplayPlan {
    /// The number of events the plan was made for.
    events: usize,
    /// The first event of each range.
    range_starts: Vec<EventIndex>,
    ranges: Vec<HistoryRange>,
    /// Per event run, the operation run holding its first event; then the number of operation runs.
    first_op_of_run: Vec<OperationRunIndex>,
    /// Per range, the cached CRDT result: empty for linear ranges and those merged by transforming.
    resolved: Vec<ResolvedOrigins>,
    /// How many translated edits the concurrent ranges took last time, to reserve next time.
    room: usize,
}

impl ReplayPlan {
    /// Analyses the log, taking over what the `previous` plan resolved for concurrent ranges that
    /// start at the same event.
    fn new(log: &OpLog, mut previous: Option<ReplayPlan>) -> Self {
        let room = previous.as_ref().map_or(0, |previous| previous.room);
        let runs = log.graph.runs();
        let ranges = log.graph.split_at_critical_versions();
        let range_starts = ranges.iter().map(|range| runs.start(range.runs().start)).collect();
        let first_op_of_run = log.ops_containing((0..=runs.len()).map(|run| runs.start(run)));
        let resolved = ranges
            .iter()
            .map(|range| match (range, &mut previous) {
                (HistoryRange::Concurrent(concurrent), Some(previous)) => previous.take_resolved(runs.start(concurrent.start)),
                _ => ResolvedOrigins::default(),
            })
            .collect();
        ReplayPlan { events: log.len(), range_starts, ranges, first_op_of_run, resolved, room }
    }

    /// Takes the cached result for the concurrent range starting at `start`, if any.
    fn take_resolved(&mut self, start: EventIndex) -> ResolvedOrigins {
        match self.range_starts.binary_search(&start) {
            Ok(at) => std::mem::take(&mut self.resolved[at]),
            Err(_) => ResolvedOrigins::default(),
        }
    }

    /// Iterates over the operations of a range of runs, cutting any that run past either end.
    #[inline]
    fn ops<'a>(&self, log: &'a OpLog, run_range: &Range<EventRunIndex>) -> impl DoubleEndedIterator<Item = OperationRunSlice> + 'a {
        self.ops_from(log, run_range, log.graph.runs().start(run_range.start))
    }

    /// Iterates over the operations of a range of runs from event `from`, which is in it.
    #[inline]
    fn ops_from<'a>(&self, log: &'a OpLog, run_range: &Range<EventRunIndex>, from: EventIndex) -> impl DoubleEndedIterator<Item = OperationRunSlice> + 'a {
        log.ops(self.op_at(log, run_range, from)..self.first_op_of_run[run_range.end as usize], from..log.graph.runs().start(run_range.end))
    }

    /// The operation run holding event `from` of `run_range`.
    fn op_at(&self, log: &OpLog, run_range: &Range<EventRunIndex>, from: EventIndex) -> OperationRunIndex {
        if from == log.graph.runs().start(run_range.start) {
            self.first_op_of_run[run_range.start as usize]
        } else {
            log.op_containing(from)
        }
    }

    /// How many concurrent ranges have a cached CRDT result.
    pub(crate) fn ranges_using_crdt_records(&self) -> usize {
        self.resolved.iter().filter(|resolved| resolved.events > 0).count()
    }

    /// Notes that the log's event runs changed though its events did not.
    pub(crate) fn outdate(&mut self) {
        self.events = usize::MAX;
    }
}

/// Replays the events after `base` onto it, returning the text and its length in characters.
///
/// Linear ranges apply as recorded; a concurrent range is merged by transforming if it can be, else
/// with the CRDT records, whose result `plan` caches. The text is then built by undoing every
/// operation last to first. If `base` ends inside a concurrent range, an event run and an operation
/// run must start there.
pub(crate) fn replay(log: &OpLog, plan: &mut Option<ReplayPlan>, base: &DocumentState) -> (String, u32) {
    // The log only ever grows, so the same length means the same events.
    if plan.as_ref().is_none_or(|plan| plan.events != log.len()) {
        *plan = Some(ReplayPlan::new(log, plan.take()));
    }
    let plan = plan.as_mut().expect("a plan was just made");
    let mut resolved = std::mem::take(&mut plan.resolved);

    // Forwards: merge the concurrent ranges. The events of a linear range are operations already,
    // so all that is needed of them for now is what they do to the document's length.
    let runs = log.graph.runs();
    let events = |run_range: &Range<EventRunIndex>| runs.start(run_range.start)..runs.start(run_range.end);
    debug_assert!((log.graph.first_held_event()..log.graph.len()).contains(&base.events), "a base the log does not reach back to");
    let first_new_run = runs.containing(base.events);
    let mut transformed = TransformedOperations {
        edits: Vec::with_capacity(plan.room),
        coalesce_from: 0,
        len: base.chars,
        first_new_run,
        in_base: false,
    };
    // Space that transforming reuses from one concurrent range to the next, since a merge can
    // transform thousands.
    let (mut versions, mut stable, mut edited, mut spare) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut concurrent_edits = Vec::new();
    // The ranges that have events after the base, and for each the first of those.
    let after = |range: &HistoryRange| Some(events(range.runs())).filter(|events| events.end > base.events).map(|events| events.start.max(base.events));
    for (range, resolved) in plan.ranges.iter().zip(&mut resolved) {
        let Some(from) = after(range) else { continue };
        match range {
            HistoryRange::Linear(linear) => {
                // A history with no concurrency is one range, and the log keeps its count.
                let whole = plan.ranges.len() == 1 && from == runs.start(0);
                let (inserted, removed) = if whole { log.total_lengths() } else { log.lengths(plan.op_at(log, linear, from), from..runs.start(linear.end)) };
                transformed.len = transformed.len + inserted - removed;
            }
            HistoryRange::Concurrent(concurrent) => {
                let before = transformed.edits.len();
                transformed.coalesce_from = before;
                debug_assert_eq!(from, runs.start(first_new_run.max(concurrent.start)), "a base partway through a run of a concurrent range");
                // Nothing resolved: the range has not needed the CRDT records so far. See whether
                // it does now.
                let merged = resolved.events == 0
                    && merge_by_transforming(
                        log,
                        runs,
                        &plan.first_op_of_run,
                        concurrent,
                        &mut transformed,
                        &mut versions,
                        &mut stable,
                        &mut edited,
                        &mut spare,
                    );
                if !merged {
                    replay_concurrent_range(log, plan, concurrent, resolved, &mut transformed)
                }
                concurrent_edits.push(transformed.edits.len() - before);
            }
        }
    }
    // The text is built without the room that merging took.
    drop((versions, stable, edited, spare));
    plan.resolved = resolved;
    plan.room = transformed.edits.len();

    // Backwards: take every operation back, starting from the final document.
    let mut text = PieceTable::new(transformed.len, log.inserted_text());
    let bytes = |(start, end): ByteRange| start as usize..end as usize;
    let mut edits = transformed.edits.iter().rev();
    let mut concurrent_edits = concurrent_edits.into_iter().rev();
    for range in plan.ranges.iter().rev() {
        let Some(from) = after(range) else { break };
        match range {
            HistoryRange::Linear(linear) => {
                // The piece table cancels erased text in both linear and transformed streams.
                // Keep the original byte range: no character-boundary scan is needed to trim it.
                for op in plan.ops_from(log, linear, from).rev() {
                    match op.kind {
                        OperationKind::Insert => text.undo_insert(op.pos, bytes(op.text), op.len),
                        OperationKind::Delete => text.undo_remove(op.pos, op.len),
                    }
                }
            }
            HistoryRange::Concurrent(_) => {
                let count = concurrent_edits.next().expect("an edit count for each concurrent range");
                for edit in edits.by_ref().take(count) {
                    if edit.text.0 == edit.text.1 {
                        text.undo_remove(edit.pos, edit.len);
                    } else {
                        text.undo_insert(edit.pos, bytes(edit.text), edit.len);
                    }
                }
            }
        }
    }
    // What is left is the document at the base, whose text is already known.
    text.undo_rest(&base.text);
    (text.into_string(), transformed.len)
}

/// Merges a concurrent range with the CRDT records, recording in `transformed` where its operations
/// go.
fn replay_concurrent_range(
    log: &OpLog,
    plan: &ReplayPlan,
    range: &Range<EventRunIndex>,
    resolved: &mut ResolvedOrigins,
    transformed: &mut TransformedOperations,
) {
    let runs = log.graph.runs();
    let (start, end) = (runs.start(range.start), runs.start(range.end));
    let goes_before = |a: EventIndex, b: EventIndex| log.inserts_before(start + a, start + b);
    // The range's runs that the document already holds, which are its first. The document was
    // no longer when the range began than it is now plus what those events removed.
    let held_until = transformed.first_new_run.clamp(range.start, range.end);
    let mut records = CrdtRecords::new((end - start) as usize, transformed.len + (runs.start(held_until) - start));

    // Runs resolved in full by earlier replays are placed by record. Their events stay in the
    // records' prepare version, which is where the walk of the rest begins.
    let known_runs = (range.start..range.end).take_while(|&run| runs.start(run + 1) - start <= resolved.events).count();
    let known = range.start..range.start + known_runs as EventRunIndex;
    let mut origins = resolved.resolved.iter().copied();
    let mut next = || origins.next().expect("something resolved for every resolved event");
    let parted = held_until.min(known.end);
    for run_range in [known.start..parted, parted..known.end] {
        if run_range.is_empty() {
            continue;
        }
        transformed.begin(run_range.clone());
        for op in plan.ops(log, &run_range) {
            let event = op.first_event - start;
            match op.kind {
                OperationKind::Insert => {
                    let (left, right) = (next(), next());
                    let pos = records.insert_with_known_origins(event, op.len, left, right, goes_before);
                    transformed.insert(pos, op.len, op.text);
                }
                OperationKind::Delete => {
                    let mut done = 0;
                    while done < op.len {
                        let (first, count) = (next(), next());
                        let (n, backward) = ((count & !BACKWARD_TARGETS).min(op.len - done), count & BACKWARD_TARGETS != 0);
                        let record = if backward { first + 1 - n } else { first };
                        records.delete_with_known_targets(DeleteTargets { first_event: event + done, len: n, record, backward }, |pos, len| {
                            transformed.remove(pos, len)
                        });
                        done += n;
                    }
                }
            }
        }
    }
    if known_runs == range.len() {
        return;
    }

    // What the document holds is gone through before anything else, so that where the rest goes
    // is a position in that document.
    let event_kinds = |range: &Range<EventRunIndex>| {
        log.event_kinds(
            plan.first_op_of_run[range.start as usize]..plan.first_op_of_run[range.end as usize],
            runs.start(range.start)..runs.start(range.end),
        )
    };
    walk(runs, range, known_runs, (held_until - range.start) as usize, |step| match step {
        WalkStep::Retreat(run_range) => {
            for (kind, events) in event_kinds(&run_range) {
                records.retreat(kind, events.start - start..events.end - start);
            }
        }
        WalkStep::Advance(run_range) => {
            for (kind, events) in event_kinds(&run_range) {
                records.advance(kind, events.start - start..events.end - start);
            }
        }
        WalkStep::Apply(run) => {
            transformed.begin(run..run + 1);
            for op in plan.ops(log, &(run..run + 1)) {
                apply(start, op, &mut records, &goes_before, transformed);
            }
        }
    });

    // The records now know what every event of the range resolved to. Keep that.
    resolved.events = end - start;
    resolved.resolved.clear();
    for op in plan.ops(log, range) {
        let event = op.first_event - start;
        match op.kind {
            OperationKind::Insert => resolved.resolved.extend(records.origins_of(event)),
            OperationKind::Delete => {
                let mut done = 0;
                while done < op.len {
                    let targets = records.targets_of(event + done, event + op.len);
                    resolved.resolved.extend([targets.first(), targets.len | if targets.backward { BACKWARD_TARGETS } else { 0 }]);
                    done += targets.len;
                }
            }
        }
    }
    resolved.resolved.shrink_to_fit();
}

/// Applies an operation run at the CRDT records' prepare version.
fn apply(
    start: EventIndex,
    op: OperationRunSlice,
    records: &mut CrdtRecords,
    goes_before: &impl Fn(EventIndex, EventIndex) -> bool,
    transformed: &mut TransformedOperations,
) {
    let event = op.first_event - start;
    match op.kind {
        OperationKind::Insert => {
            let pos = records.insert(event, op.len, op.pos, goes_before);
            transformed.insert(pos, op.len, op.text);
        }
        OperationKind::Delete => {
            records.delete(event, op.pos, op.len, op.backward, |pos, len| transformed.remove(pos, len));
        }
    }
}
