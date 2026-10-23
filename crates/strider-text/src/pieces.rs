/// A piece's `at` when its characters do not survive into the final text.
const DELETED: u32 = u32::MAX;

/// Tiny in debug builds, so that tests split and empty nodes often.
const PIECES_PER_NODE: usize = if cfg!(debug_assertions) { 4 } else { 32 };

#[derive(Clone, Copy)]
struct Piece {
    len: u32,
    /// Where the piece's first character is in the final text, or [`DELETED`].
    at: u32,
}

impl Piece {
    fn with_len(self, len: u32) -> Piece {
        Piece { len, ..self }
    }

    fn after(self, skip: u32) -> Piece {
        Piece { len: self.len - skip, at: if self.at == DELETED { DELETED } else { self.at + skip } }
    }

    /// Whether `next`, which follows this piece, can join it: both deleted, or adjacent in the
    /// final text.
    fn continues_into(self, next: Piece) -> bool {
        if self.at == DELETED || next.at == DELETED {
            self.at == next.at
        } else {
            self.at + self.len == next.at
        }
    }
}

#[derive(Clone, Copy)]
struct PieceListNode {
    len: usize,
    /// The piece found last, and its offset in the node.
    finger: (usize, u32),
    pieces: [Piece; PIECES_PER_NODE],
}

impl PieceListNode {
    const EMPTY: PieceListNode = PieceListNode { len: 0, finger: (0, 0), pieces: [Piece { len: 0, at: 0 }; PIECES_PER_NODE] };

    fn len(&self) -> usize {
        self.len
    }

    fn piece(&self, i: usize) -> Piece {
        self.pieces[i]
    }

    /// Drops the finger if it is after piece `i`, which is changing.
    fn touch(&mut self, i: usize) {
        if i < self.finger.0 {
            self.finger = (0, 0);
        }
    }

    fn set(&mut self, i: usize, piece: Piece) {
        self.touch(i);
        self.pieces[i] = piece;
    }

    /// The piece holding `offset`, and the offset into it; one past the last piece at the node's
    /// end.
    fn find(&mut self, offset: u32) -> (usize, u32) {
        let (mut i, mut start) = self.finger;
        while offset < start {
            i -= 1;
            start -= self.pieces[i].len;
        }
        while i < self.len && offset >= start + self.pieces[i].len {
            start += self.pieces[i].len;
            i += 1;
        }
        self.finger = (i, start);
        (i, offset - start)
    }

    /// Inserts a piece at `i`. The node must not be full.
    fn insert(&mut self, i: usize, piece: Piece) {
        self.touch(i);
        self.pieces.copy_within(i..self.len, i + 1);
        self.pieces[i] = piece;
        self.len += 1;
    }

    fn remove(&mut self, i: usize) {
        self.touch(i);
        self.pieces.copy_within(i + 1..self.len, i);
        self.len -= 1;
    }

    /// Removes consecutive whole pieces, compacting the remaining suffix only once.
    fn remove_range(&mut self, from: usize, to: usize) {
        self.touch(from);
        self.pieces.copy_within(to..self.len, from);
        self.len -= to - from;
    }
}

/// The document as an unrolled linked list of pieces: stretches that survive at a known place in
/// the final text, or are deleted later.
pub(crate) struct PieceList {
    nodes: Vec<PieceListNode>,
    /// Freed nodes, for reuse.
    spare: Vec<u32>,
    /// The nodes in document order, with each one's characters in `totals`.
    order: Vec<u32>,
    totals: Vec<u32>,
    /// The node used last: its place in `order`, and its start.
    finger: (usize, u32),
}

impl PieceList {
    /// The final document: `len` characters that all survive.
    pub fn new(len: u32) -> Self {
        let mut first = PieceListNode::EMPTY;
        if len > 0 {
            first.insert(0, Piece { len, at: 0 });
        }
        PieceList { nodes: vec![first], spare: Vec::new(), order: vec![0], totals: vec![len], finger: (0, 0) }
    }

    /// Puts `len` deleted characters into the document at `pos`.
    pub fn insert_deleted(&mut self, pos: u32, len: u32) {
        let deleted = Piece { len, at: DELETED };
        loop {
            let (place, start) = self.node_at(pos);
            let node = &mut self.nodes[self.order[place] as usize];
            let (i, offset) = node.find(pos - start);
            let room = PIECES_PER_NODE - node.len();
            // A deleted piece they land in, or next to, takes them in.
            let joined = if offset > 0 {
                Some(i)
            } else if i > 0 && node.piece(i - 1).at == DELETED {
                Some(i - 1)
            } else {
                Some(i).filter(|&i| i < node.len())
            };
            match joined.map(|i| (i, node.piece(i))) {
                Some((i, piece)) if piece.at == DELETED => node.set(i, piece.with_len(piece.len + len)),
                // In the middle of text that survives, which is now in two parts.
                Some((i, piece)) if offset > 0 && room >= 2 => {
                    node.set(i, piece.with_len(offset));
                    node.insert(i + 1, deleted);
                    node.insert(i + 2, piece.after(offset));
                }
                _ if offset == 0 && room >= 1 => node.insert(i, deleted),
                _ => {
                    self.split(place);
                    continue;
                }
            }
            self.totals[place] += len;
            return;
        }
    }

    /// Takes the `len` characters at `pos` out of the document, calling `survivor` for each
    /// surviving stretch with its final position, its offset into the removed characters, and its
    /// length.
    pub fn remove(&mut self, pos: u32, len: u32, mut survivor: impl FnMut(u32, u32, u32)) {
        let mut done = 0;
        while done < len {
            let (mut place, mut start) = self.node_at(pos);
            if pos == start + self.totals[place] {
                start += self.totals[place];
                place += 1;
                self.finger = (place, start);
            }
            let id = self.order[place];
            let node = &mut self.nodes[id as usize];
            let (i, offset) = node.find(pos - start);
            let piece = node.piece(i);
            let mut n = (piece.len - offset).min(len - done);
            // Surviving text with a piece taken out of its middle becomes two pieces: its
            // halves are no longer next to each other in the final text.
            let halved = offset > 0 && offset + n < piece.len && piece.at != DELETED;
            if halved && node.len() == PIECES_PER_NODE {
                self.split(place);
                continue;
            }
            if piece.at != DELETED {
                survivor(piece.at + offset, done, n);
            }
            // `seam`: the piece that now starts where the removed characters were.
            let seam = if n < len - done {
                // The removal carries on past this piece. Walk the rest of this node
                // before moving its suffix, rather than refinding and shifting it for
                // every fully covered piece.
                let from = i + usize::from(offset > 0);
                if offset > 0 {
                    node.set(i, piece.with_len(offset));
                }
                let mut next = i + 1;
                while next < node.len() && n < len - done {
                    let piece = node.piece(next);
                    let taken = piece.len.min(len - done - n);
                    if piece.at != DELETED {
                        survivor(piece.at, done + n, taken);
                    }
                    n += taken;
                    if taken < piece.len {
                        node.set(next, piece.after(taken));
                        break;
                    }
                    next += 1;
                }
                node.remove_range(from, next);
                Some(from)
            } else if offset == 0 && n == piece.len {
                node.remove(i);
                Some(i)
            } else if offset == 0 {
                node.set(i, piece.after(n));
                Some(i)
            } else if halved {
                node.set(i, piece.with_len(offset));
                node.insert(i + 1, piece.after(offset + n));
                None
            } else {
                node.set(i, piece.with_len(piece.len - n));
                (offset + n == piece.len).then_some(i + 1)
            };
            done += n;
            self.totals[place] -= n;

            // What was on either side of the removed characters may now be one piece.
            if let Some(i) = seam.filter(|&i| done == len && i > 0 && i < node.len()) {
                let (left, right) = (node.piece(i - 1), node.piece(i));
                if left.continues_into(right) {
                    node.set(i - 1, left.with_len(left.len + right.len));
                    node.remove(i);
                }
            }
            if node.len() == 0 && self.order.len() > 1 {
                self.order.remove(place);
                self.totals.remove(place);
                self.spare.push(id);
                self.finger = if place < self.order.len() { (place, start) } else { (place - 1, start - self.totals[place - 1]) };
            }
        }
    }

    /// Each piece's length, and its final position if it survives, in order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, Option<u32>)> + '_ {
        let pieces = self.order.iter().flat_map(|&id| {
            let node = &self.nodes[id as usize];
            &node.pieces[..node.len]
        });
        pieces.map(|piece| (piece.len, Some(piece.at).filter(|&at| at != DELETED)))
    }

    /// The node that `pos` is in or at either end of: its place in `order`, and its start.
    fn node_at(&mut self, pos: u32) -> (usize, u32) {
        let (mut place, mut start) = self.finger;
        while pos < start {
            place -= 1;
            start -= self.totals[place];
        }
        while pos > start + self.totals[place] {
            start += self.totals[place];
            place += 1;
        }
        self.finger = (place, start);
        (place, start)
    }

    fn split(&mut self, place: usize) {
        let left = &mut self.nodes[self.order[place] as usize];
        let half = left.len / 2;
        let mut right = PieceListNode::EMPTY;
        right.len = left.len - half;
        right.pieces[..right.len].copy_from_slice(&left.pieces[half..left.len]);
        let moved = right.pieces[..right.len].iter().map(|piece| piece.len).sum::<u32>();
        left.len = half;
        left.finger = (0, 0);

        let id = match self.spare.pop() {
            Some(id) => {
                self.nodes[id as usize] = right;
                id
            }
            None => {
                self.nodes.push(right);
                self.nodes.len() as u32 - 1
            }
        };
        self.order.insert(place + 1, id);
        self.totals.insert(place + 1, moved);
        self.totals[place] -= moved;
    }
}
