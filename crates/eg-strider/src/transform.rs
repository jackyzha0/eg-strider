use crate::operation::OperationKind;
use crate::oplog::{OpLog, OperationRunIndex};
use crate::replay::TransformedOperations;
use std::ops::Range;
use strider_graph::{EventRunIndex, EventRuns};

/// Concurrent ranges with more runs go to the CRDT records, since the run sets grow with the square
/// of the count: 256 KB here.
const MAX_RUNS: usize = 1024;

/// Beyond this many edited ranges, scanning them for every operation costs more than the CRDT
/// records would.
const MAX_EDITED_RANGES: usize = 64;

/// A run's index within its concurrent range.
pub(crate) type RunInRange = u32;

/// A bitset of a concurrent range's runs.
type RunSet = [u64];

/// A part of the merged document that the range's runs have edited.
pub(crate) struct EditedRange {
    start: i64,
    len: i64,
    /// The runs that edited here and are not stable, and how much each changed its length.
    members: Vec<(RunInRange, i64)>,
}

impl EditedRange {
    /// Records that `run` changed the edited range's length by `grew`.
    fn add(&mut self, run: RunInRange, grew: i64) {
        match self.members.last_mut() {
            Some((last, by)) if *last == run => *by += grew,
            _ => self.members.push((run, grew)),
        }
    }
}

fn has(set: &RunSet, run: RunInRange) -> bool {
    set[run as usize / 64] >> (run % 64) & 1 == 1
}

/// Merges a concurrent range by operational transformation, shifting each operation by the net
/// length of the unseen operations before it, unless one lands in an edited range holding
/// operations it has not seen.
///
/// Records in `transformed` where the operations go, or returns false and leaves it as it was.
/// `versions`, `stable`, `edited` and `spare` are scratch space reused across ranges.
#[allow(clippy::too_many_arguments)]
pub(crate) fn merge_by_transforming(
    log: &OpLog,
    runs: &EventRuns,
    first_op_of_run: &[OperationRunIndex],
    range: &Range<EventRunIndex>,
    transformed: &mut TransformedOperations,
    versions: &mut Vec<u64>,
    stable: &mut Vec<u64>,
    edited: &mut Vec<EditedRange>,
    spare: &mut Vec<Vec<(RunInRange, i64)>>,
) -> bool {
    if range.len() > MAX_RUNS {
        return false;
    }
    let words = fill_versions(runs, range, versions, stable);
    for mut old in edited.drain(..) {
        old.members.clear();
        spare.push(old.members);
    }

    let mark = transformed.mark();
    for (i, run) in (range.start..range.end).enumerate() {
        let (in_range, version, stable_runs) = (i as RunInRange, &versions[i * words..][..words], &stable[i * words..][..words]);
        transformed.begin(run..run + 1);
        for op in log.ops(first_op_of_run[run as usize]..first_op_of_run[run as usize + 1], runs.events(run)) {
            let at = match op.kind {
                OperationKind::Insert => place_insert(edited, spare, in_range, version, stable_runs, op.pos as i64, op.len as i64),
                OperationKind::Delete => place_removal(edited, spare, in_range, version, stable_runs, op.pos as i64, op.len as i64),
            };
            match (at, op.kind) {
                (Some(at), OperationKind::Insert) => transformed.insert(at as u32, op.len, op.text),
                (Some(at), OperationKind::Delete) => {
                    transformed.remove(at as u32, op.len);
                }
                (None, _) => {
                    transformed.undo_to(mark);
                    return false;
                }
            }
        }
    }
    true
}

/// Fills in `versions` for the range: per run, the range's runs in its version, itself included;
/// and `stable`: per run, the runs that it and every later run have seen. Returns how many words
/// each set has.
fn fill_versions(runs: &EventRuns, range: &Range<EventRunIndex>, versions: &mut Vec<u64>, stable: &mut Vec<u64>) -> usize {
    let count = range.len();
    let words = count.div_ceil(64);
    versions.clear();
    stable.clear();
    if words == 1 {
        // Most concurrent ranges are this small: each set is one word, with no slices to cut.
        for i in 0..count {
            let inside = runs.parents(range.start + i as EventRunIndex).iter().filter_map(|&p| p.checked_sub(range.start)).map(|p| p as usize);
            let version = inside.fold(1 << i, |version, parent| version | versions[parent]);
            versions.push(version);
        }
        stable.resize(count, 0);
        let mut later = u64::MAX;
        for i in (0..count).rev() {
            later &= versions[i];
            stable[i] = later;
        }
        return words;
    }
    versions.resize(count * words, 0);
    for i in 0..count {
        let (earlier, own) = versions.split_at_mut(i * words);
        let own = &mut own[..words];
        // Parents from before the range are before a critical version: all have them.
        for parent in runs.parents(range.start + i as EventRunIndex).iter().filter_map(|&p| p.checked_sub(range.start)).map(|p| p as usize) {
            for (word, theirs) in own.iter_mut().zip(&earlier[parent * words..][..words]) {
                *word |= theirs;
            }
        }
        own[i / 64] |= 1 << (i % 64);
    }
    stable.resize(count * words, u64::MAX);
    for i in (0..count).rev() {
        for word in 0..words {
            let later = if i + 1 < count { stable[(i + 1) * words + word] } else { u64::MAX };
            stable[i * words + word] = later & versions[i * words + word];
        }
    }
    words
}

/// How much longer edited range `i` is than the run with sets `version` and `stable` knows, and
/// whether a run it has not seen edited there. Stable runs are dropped from the range first; if
/// that empties it, it is removed and `None` returned, `i` then naming the next one.
fn unseen(edited: &mut Vec<EditedRange>, spare: &mut Vec<Vec<(RunInRange, i64)>>, i: usize, version: &RunSet, stable: &RunSet) -> Option<(i64, bool)> {
    let (mut longer, mut any, mut stale) = (0, false, false);
    for &(run, grew) in &edited[i].members {
        if has(stable, run) {
            stale = true;
        } else if !has(version, run) {
            longer += grew;
            any = true;
        }
    }
    // Rewriting the list only when something has to go keeps the common case to a read.
    if stale {
        edited[i].members.retain(|&(run, _)| !has(stable, run));
    }
    if edited[i].members.is_empty() {
        spare.push(edited.remove(i).members);
        return None;
    }
    Some((longer, any))
}

/// Adds an edited range at `i`, made by `run`, unless there are too many.
fn open(edited: &mut Vec<EditedRange>, spare: &mut Vec<Vec<(RunInRange, i64)>>, i: usize, start: i64, len: i64, run: RunInRange, grew: i64) -> Option<()> {
    if edited.len() == MAX_EDITED_RANGES {
        return None;
    }
    let mut members = spare.pop().unwrap_or_default();
    members.push((run, grew));
    edited.insert(i, EditedRange { start, len, members });
    Some(())
}

/// Transforms an insert of `len` characters at `pos` in `run`'s version into a position in the
/// merged document; `None` if it lands in an edited range with operations `run` has not seen.
fn place_insert(
    edited: &mut Vec<EditedRange>,
    spare: &mut Vec<Vec<(RunInRange, i64)>>,
    run: RunInRange,
    version: &RunSet,
    stable: &RunSet,
    pos: i64,
    len: i64,
) -> Option<i64> {
    let own = !has(stable, run);
    // `shift`: how much further along the merged document is than the run's.
    let (mut i, mut shift, mut inside) = (0, 0, false);
    while i < edited.len() {
        let Some((longer, any)) = unseen(edited, spare, i, version, stable) else { continue };
        // The edited range as the run knows it.
        let start = edited[i].start - shift;
        let end = start + edited[i].len - longer;
        if pos < start {
            break;
        }
        if pos <= end {
            if any {
                return None;
            }
            inside = true;
            break;
        }
        shift += longer;
        i += 1;
    }
    let at = pos + shift;
    if inside {
        edited[i].len += len;
        if own {
            edited[i].add(run, len);
        }
        i += 1;
    } else if own {
        open(edited, spare, i, at, len, run, len)?;
        i += 1;
    }
    for later in &mut edited[i..] {
        later.start += len;
    }
    Some(at)
}

/// Transforms a removal of the `len` characters at `pos` in `run`'s version into a position in the
/// merged document; `None` if it touches an edited range with operations `run` has not seen.
fn place_removal(
    edited: &mut Vec<EditedRange>,
    spare: &mut Vec<Vec<(RunInRange, i64)>>,
    run: RunInRange,
    version: &RunSet,
    stable: &RunSet,
    pos: i64,
    len: i64,
) -> Option<i64> {
    let own = !has(stable, run);
    // Edited ranges that end before the removed text, with a character in between.
    let (mut i, mut shift) = (0, 0);
    while i < edited.len() {
        let Some((longer, _)) = unseen(edited, spare, i, version, stable) else { continue };
        if edited[i].start - shift + edited[i].len - longer >= pos {
            break;
        }
        shift += longer;
        i += 1;
    }
    // Edited ranges `i..j` that the removed text reaches into or touches.
    let (mut j, mut shift_at_start, mut shift_at_end) = (i, shift, shift);
    while j < edited.len() {
        let Some((longer, any)) = unseen(edited, spare, j, version, stable) else { continue };
        let start = edited[j].start - shift_at_end;
        let end = start + edited[j].len - longer;
        if start > pos + len {
            break;
        }
        if end == pos {
            // Touches the removed text from before: all of it lies before.
            shift_at_start = shift_at_end + longer;
        } else if start < pos + len && any {
            return None;
        }
        shift_at_end += longer;
        j += 1;
    }
    let at = pos + shift_at_start;
    if i < j {
        // The removed text and everything it touches are one edited range from now on.
        let start = at.min(edited[i].start);
        let end = (pos + len + shift_at_end).max(edited[j - 1].start + edited[j - 1].len);
        for _ in i + 1..j {
            let mut absorbed = edited.remove(i + 1).members;
            edited[i].members.extend_from_slice(&absorbed);
            absorbed.clear();
            spare.push(absorbed);
        }
        edited[i].start = start;
        edited[i].len = end - start - len;
        if own {
            edited[i].add(run, -len);
        }
        i += 1;
    } else if own {
        open(edited, spare, i, at, 0, run, -len)?;
        i += 1;
    }
    for later in &mut edited[i..] {
        later.start -= len;
    }
    Some(at)
}
