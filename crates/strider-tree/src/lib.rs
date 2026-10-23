use std::cmp::Ordering;

/// An entry's permanent id, numbered from 0 in creation order.
pub type EntryId = u32;

/// Which version a length or position is measured in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Prepare,
    Effect,
}

/// An entry's visible length in the prepare and effect versions.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Lengths {
    pub prepare: u32,
    pub effect: u32,
}

impl Lengths {
    fn of(self, view: View) -> u32 {
        match view {
            View::Prepare => self.prepare,
            View::Effect => self.effect,
        }
    }

    /// Wrapping, so that adding a negative delta from [`minus`](Self::minus) works.
    fn plus(self, other: Lengths) -> Lengths {
        Lengths { prepare: self.prepare.wrapping_add(other.prepare), effect: self.effect.wrapping_add(other.effect) }
    }

    fn minus(self, other: Lengths) -> Lengths {
        Lengths { prepare: self.prepare.wrapping_sub(other.prepare), effect: self.effect.wrapping_sub(other.effect) }
    }
}

const NO_NODE: u32 = u32::MAX;

/// An entry's place in the tree, valid until the next insertion or split.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    leaf: u32,
    slot: u32,
}

/// The last selection, including the prefix before its slot inside the leaf. The two
/// prefixes let nearby selections advance without scanning the leaf from its beginning.
#[derive(Clone, Copy)]
struct Finger {
    view: View,
    leaf: u32,
    start: u32,
    effect: u32,
    slot: usize,
    within: u32,
    effect_within: u32,
}

/// A child and its lengths (its subtree's totals in an inner node), kept together so that searching
/// a node reads one array.
#[derive(Clone, Copy)]
struct Slot {
    /// An entry id in a leaf; a node index in an inner node.
    child: u32,
    prepare_len: u32,
    effect_len: u32,
}

impl Slot {
    const EMPTY: Slot = Slot { child: NO_NODE, prepare_len: 0, effect_len: 0 };

    fn new(child: u32, lengths: Lengths) -> Self {
        Slot { child, prepare_len: lengths.prepare, effect_len: lengths.effect }
    }

    fn lengths(&self) -> Lengths {
        Lengths { prepare: self.prepare_len, effect: self.effect_len }
    }

    fn set_lengths(&mut self, lengths: Lengths) {
        (self.prepare_len, self.effect_len) = (lengths.prepare, lengths.effect);
    }
}

/// A leaf or inner node of up to `N` children.
struct Node<const N: usize> {
    len: u32,
    parent: u32,
    /// This node's index among its parent's children.
    at: u32,
    slots: [Slot; N],
}

impl<const N: usize> Node<N> {
    fn new(parent: u32) -> Self {
        Node { len: 0, parent, at: 0, slots: [Slot::EMPTY; N] }
    }

    fn total(&self) -> Lengths {
        self.slots.iter().fold(Lengths::default(), |total, slot| total.plus(slot.lengths()))
    }

    fn start(&self, view: View, child: usize) -> u32 {
        self.slots[..child].iter().map(|slot| slot.lengths().of(view)).sum()
    }

    /// The child holding position `index` of `view`, the offset into it, and its effect
    /// position. Both lengths are read together rather than scanning the prefix again.
    fn find(&self, view: View, mut index: u32) -> Option<(usize, u32, u32)> {
        let mut child = 0;
        let mut effect = 0;
        while index >= self.slots[child].lengths().of(view) {
            index -= self.slots[child].lengths().of(view);
            effect += self.slots[child].effect_len;
            child += 1;
            if child >= self.len as usize {
                return None;
            }
        }
        Some((child, index, effect))
    }

    /// The slot holding `child`.
    fn index_of(&self, child: u32) -> usize {
        if self.len <= 8 {
            // Nearly-empty nodes are the norm in small trees; a short scan beats the wide
            // one.
            return self.slots.iter().position(|slot| slot.child == child).expect("child is not in its node");
        }
        // Compare every slot with no early exit, so the loop is branch-free and
        // vectorises. Unused slots hold NO_NODE and never match.
        let mut found = 0;
        for (i, slot) in self.slots.iter().enumerate() {
            found |= if slot.child == child { i } else { 0 };
        }
        debug_assert_eq!(self.slots[found].child, child);
        found
    }

    /// Inserts a child at `at`. The node must not be full.
    fn insert(&mut self, at: usize, child: u32, lengths: Lengths) {
        let len = self.len as usize;
        if at < len {
            self.slots.copy_within(at..len, at + 1);
        }
        self.slots[at] = Slot::new(child, lengths);
        self.len += 1;
    }

    /// Moves `lengths` out of child `at` into a new child just before it if `before`, else just
    /// after. The node must not be full.
    fn split_child(&mut self, at: usize, child: u32, lengths: Lengths, before: bool) {
        let slot = &mut self.slots[at];
        slot.set_lengths(slot.lengths().minus(lengths));
        self.insert(at + !before as usize, child, lengths);
    }

    fn split_off(&mut self) -> Node<N> {
        let half = N / 2;
        let mut upper = Node::new(self.parent);
        upper.len = (N - half) as u32;
        upper.slots[..N - half].copy_from_slice(&self.slots[half..]);
        self.slots[half..].fill(Slot::EMPTY);
        self.len = half as u32;
        upper
    }
}

/// An order-statistic B+ tree over a sequence of entries, indexed by position in both the prepare
/// and effect versions.
pub struct PrepareEffectTree<const N: usize = 32> {
    leaves: Vec<Node<N>>,
    nodes: Vec<Node<N>>,
    root: u32,
    /// Levels of inner nodes, from 1.
    height: u32,
    leaf_of_entry: Vec<u32>,
    /// Where the last select ended: its view, its leaf, and where that leaf starts in that view and
    /// in the effect version. Cleared when lengths change in another leaf.
    finger: Option<Finger>,
}

impl<const N: usize> PrepareEffectTree<N> {
    /// Creates a tree of one entry.
    pub fn new(lengths: Lengths) -> Self {
        let mut tree = PrepareEffectTree { leaves: Vec::new(), nodes: Vec::new(), root: 0, height: 1, leaf_of_entry: Vec::new(), finger: None };
        tree.reset(lengths);
        tree
    }

    /// Makes this a tree of one entry again, reusing its memory.
    pub fn reset(&mut self, lengths: Lengths) {
        self.leaves.clear();
        self.nodes.clear();
        self.leaf_of_entry.clear();

        let mut leaf = Node::new(0);
        leaf.insert(0, 0, lengths);
        self.leaves.push(leaf);
        let mut root = Node::new(NO_NODE);
        root.insert(0, 0, lengths);
        self.nodes.push(root);
        self.leaf_of_entry.push(0);
        (self.root, self.height, self.finger) = (0, 1, None);
    }

    /// The number of entries, including those of length 0: entries are never removed.
    pub fn len(&self) -> usize {
        self.leaf_of_entry.len()
    }

    /// Always false: a tree holds at least one entry.
    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn first(&self) -> Cursor {
        let mut node_id = self.root;
        for _ in 1..self.height {
            node_id = self.nodes[node_id as usize].slots[0].child;
        }
        Cursor { leaf: self.nodes[node_id as usize].slots[0].child, slot: 0 }
    }

    pub fn next(&self, at: Cursor) -> Option<Cursor> {
        if at.slot + 1 < self.leaves[at.leaf as usize].len {
            return Some(Cursor { leaf: at.leaf, slot: at.slot + 1 });
        }
        // Climb to the first ancestor with a child further right, then go down its left edge.
        let leaf = &self.leaves[at.leaf as usize];
        let (mut child, mut node_id, mut levels) = (leaf.at as usize, leaf.parent, 0);
        while child + 1 == self.nodes[node_id as usize].len as usize {
            let node = &self.nodes[node_id as usize];
            if node.parent == NO_NODE {
                return None;
            }
            (child, node_id, levels) = (node.at as usize, node.parent, levels + 1);
        }
        let mut next = self.nodes[node_id as usize].slots[child + 1].child;
        for _ in 0..levels {
            next = self.nodes[next as usize].slots[0].child;
        }
        Some(Cursor { leaf: next, slot: 0 })
    }

    fn last_leaf(&self) -> u32 {
        let mut node_id = self.root;
        for _ in 1..self.height {
            let node = &self.nodes[node_id as usize];
            node_id = node.slots[node.len as usize - 1].child;
        }
        let node = &self.nodes[node_id as usize];
        node.slots[node.len as usize - 1].child
    }

    pub fn id(&self, at: Cursor) -> EntryId {
        self.leaves[at.leaf as usize].slots[at.slot as usize].child
    }

    pub fn lengths(&self, at: Cursor) -> Lengths {
        self.leaves[at.leaf as usize].slots[at.slot as usize].lengths()
    }

    pub fn cursor(&self, id: EntryId) -> Cursor {
        let leaf = self.leaf_of_entry[id as usize];
        Cursor { leaf, slot: self.leaves[leaf as usize].index_of(id) as u32 }
    }

    pub fn cmp(&self, a: Cursor, b: Cursor) -> Ordering {
        if a.leaf == b.leaf {
            return a.slot.cmp(&b.slot);
        }
        let levels = self.height as usize;
        self.path(a.leaf)[..levels].cmp(&self.path(b.leaf)[..levels])
    }

    /// The child indexes from the root down to `leaf`.
    fn path(&self, leaf: u32) -> [u8; 16] {
        let mut path = [0; 16];
        let mut level = self.height as usize - 1;
        let leaf = &self.leaves[leaf as usize];
        path[level] = leaf.at as u8;
        let mut node_id = leaf.parent;
        while level > 0 {
            level -= 1;
            let node = &self.nodes[node_id as usize];
            path[level] = node.at as u8;
            node_id = node.parent;
        }
        path
    }

    /// Selects the entry holding position `index` of `view`, and the offset into it.
    pub fn char_index_to_entry(&mut self, view: View, index: u32) -> (Cursor, u32) {
        let (at, offset, _) = self.locate(view, index);
        (at, offset)
    }

    /// Selects as [`char_index_to_entry`](Self::char_index_to_entry) does, also returning the
    /// entry's position in the effect version.
    pub fn locate(&mut self, view: View, index: u32) -> (Cursor, u32, u32) {
        if let Some(finger) = &mut self.finger {
            if finger.view == view && index >= finger.start {
                let leaf = &self.leaves[finger.leaf as usize];
                let index = index - finger.start;
                while index < finger.within {
                    finger.slot -= 1;
                    let lengths = leaf.slots[finger.slot].lengths();
                    finger.within -= lengths.of(view);
                    finger.effect_within -= lengths.effect;
                }
                while finger.slot < leaf.len as usize && index >= finger.within + leaf.slots[finger.slot].lengths().of(view) {
                    let lengths = leaf.slots[finger.slot].lengths();
                    finger.within += lengths.of(view);
                    finger.effect_within += lengths.effect;
                    finger.slot += 1;
                }
                if finger.slot < leaf.len as usize {
                    return (Cursor { leaf: finger.leaf, slot: finger.slot as u32 }, index - finger.within, finger.effect + finger.effect_within);
                }
            }
        }
        let (mut node_id, mut rest, mut effect) = (self.root, index, 0);
        for _ in 0..self.height {
            let node = &self.nodes[node_id as usize];
            let (child, offset, before) = node.find(view, rest).expect("index is past the end");
            rest = offset;
            effect += before;
            node_id = node.slots[child].child;
        }
        let leaf = &self.leaves[node_id as usize];
        let (slot, offset, within) = leaf.find(view, rest).expect("index is past the end");
        self.finger = Some(Finger { view, leaf: node_id, start: index - rest, effect, slot, within: rest - offset, effect_within: within });
        (Cursor { leaf: node_id, slot: slot as u32 }, offset, effect + within)
    }

    /// A change at or after the cached slot leaves its prefixes valid. An earlier change
    /// starts the next search at the leaf's beginning; a different leaf drops the finger.
    fn changing(&mut self, at: Cursor) {
        if let Some(finger) = &mut self.finger {
            if finger.leaf != at.leaf {
                self.finger = None;
            } else if (at.slot as usize) < finger.slot {
                (finger.slot, finger.within, finger.effect_within) = (0, 0, 0);
            }
        }
    }

    /// Ranks an entry: its position in `view`.
    pub fn entry_to_char_index(&self, view: View, at: Cursor) -> u32 {
        let leaf = &self.leaves[at.leaf as usize];
        let mut index = leaf.start(view, at.slot as usize);
        let (mut child, mut node_id) = (leaf.at as usize, leaf.parent);
        loop {
            let node = &self.nodes[node_id as usize];
            index += node.start(view, child);
            if node.parent == NO_NODE {
                return index;
            }
            (child, node_id) = (node.at as usize, node.parent);
        }
    }

    pub fn set_lengths(&mut self, at: Cursor, lengths: Lengths) {
        let delta = self.replace(at, lengths);
        self.add_to_ancestors(at.leaf, delta);
    }

    /// Adds `delta` to `leaf`'s slot in each of its ancestors.
    fn add_to_ancestors(&mut self, leaf: u32, delta: Lengths) {
        let leaf = &self.leaves[leaf as usize];
        let (mut child, mut node_id) = (leaf.at as usize, leaf.parent);
        loop {
            let node = &mut self.nodes[node_id as usize];
            let slot = &mut node.slots[child];
            slot.set_lengths(slot.lengths().plus(delta));
            if node.parent == NO_NODE {
                return;
            }
            (child, node_id) = (node.at as usize, node.parent);
        }
    }

    /// Sets an entry's lengths and ranks it in the effect version.
    pub fn update(&mut self, at: Cursor, lengths: Lengths) -> u32 {
        let delta = self.replace(at, lengths);
        self.climb(at, delta)
    }

    /// Sets an entry's lengths, returning the change.
    fn replace(&mut self, at: Cursor, lengths: Lengths) -> Lengths {
        self.changing(at);
        let slot = &mut self.leaves[at.leaf as usize].slots[at.slot as usize];
        let delta = lengths.minus(slot.lengths());
        slot.set_lengths(lengths);
        delta
    }

    /// Adds `delta` to the entry's ancestors, returning the entry's position in the effect version.
    fn climb(&mut self, at: Cursor, delta: Lengths) -> u32 {
        let leaf = &self.leaves[at.leaf as usize];
        let mut index = leaf.start(View::Effect, at.slot as usize);
        let (mut child, mut node_id) = (leaf.at as usize, leaf.parent);
        loop {
            let node = &mut self.nodes[node_id as usize];
            index += node.start(View::Effect, child);
            let slot = &mut node.slots[child];
            slot.set_lengths(slot.lengths().plus(delta));
            if node.parent == NO_NODE {
                return index;
            }
            (child, node_id) = (node.at as usize, node.parent);
        }
    }

    /// Inserts an entry before `before`, or at the end for `None`, returning its id and its
    /// position in the effect version.
    pub fn insert(&mut self, before: Option<Cursor>, lengths: Lengths) -> (EntryId, u32) {
        let (id, at) = self.place(before, lengths);
        (id, self.climb(at, lengths))
    }

    /// Inserts as [`insert`](Self::insert) does, without ranking the new entry.
    pub fn add(&mut self, before: Option<Cursor>, lengths: Lengths) -> EntryId {
        let (id, at) = self.place(before, lengths);
        self.add_to_ancestors(at.leaf, lengths);
        id
    }

    /// Inserts an entry into its leaf without updating the ancestors.
    fn place(&mut self, before: Option<Cursor>, lengths: Lengths) -> (EntryId, Cursor) {
        let before = before.unwrap_or_else(|| {
            let leaf = self.last_leaf();
            Cursor { leaf, slot: self.leaves[leaf as usize].len }
        });
        let at = self.make_room(before);
        self.changing(at);
        let id = self.new_entry(at.leaf);
        self.leaves[at.leaf as usize].insert(at.slot as usize, id, lengths);
        (id, at)
    }

    /// Moves `lengths` out of the entry at `at` into a new entry just before it if `before`, else
    /// just after, returning the new id and both entries' cursors in order.
    pub fn split_entry(&mut self, at: Cursor, lengths: Lengths, before: bool) -> (EntryId, [Cursor; 2]) {
        let at = self.make_room(at);
        self.changing(at);
        let id = self.new_entry(at.leaf);
        // The leaf's total is unchanged, so no ancestor needs updating.
        self.leaves[at.leaf as usize].split_child(at.slot as usize, id, lengths, before);
        (id, [at, Cursor { leaf: at.leaf, slot: at.slot + 1 }])
    }

    /// Hands out the id of a new entry in `leaf`.
    fn new_entry(&mut self, leaf: u32) -> EntryId {
        self.leaf_of_entry.push(leaf);
        self.leaf_of_entry.len() as EntryId - 1
    }

    /// Splits the leaf at `at` if it is full, returning where `at` is now.
    fn make_room(&mut self, at: Cursor) -> Cursor {
        if (self.leaves[at.leaf as usize].len as usize) < N {
            at
        } else {
            self.split_leaf(at)
        }
    }

    fn split_leaf(&mut self, at: Cursor) -> Cursor {
        let half = N / 2;
        let new = self.leaves.len() as u32;
        let old = &mut self.leaves[at.leaf as usize];
        let upper = old.split_off();
        let parent = old.parent;
        for slot in &upper.slots[..N - half] {
            self.leaf_of_entry[slot.child as usize] = new;
        }
        let moved = upper.total();
        self.leaves.push(upper);
        self.add_child(parent, at.leaf, new, moved, 1);

        if at.slot as usize >= half {
            Cursor { leaf: new, slot: at.slot - half as u32 }
        } else {
            at
        }
    }

    /// Adds `new` after `after` in node `node_id`, moving `lengths` from `after` to it. `level` is
    /// 1 when the children are leaves.
    fn add_child(&mut self, mut node_id: u32, after: u32, new: u32, lengths: Lengths, level: u32) {
        if self.nodes[node_id as usize].len as usize == N {
            node_id = self.split_node(node_id, after, level);
        }
        let at = self.index_in_parent(level, after) as usize;
        let len = self.nodes[node_id as usize].len as usize;
        for i in at + 1..len {
            let child = self.nodes[node_id as usize].slots[i].child;
            self.adopt(level, child, node_id, i as u32 + 1);
        }
        self.nodes[node_id as usize].split_child(at, new, lengths, false);
        self.adopt(level, new, node_id, at as u32 + 1);
    }

    /// Splits a full inner node, returning whichever half holds `child`.
    fn split_node(&mut self, node_id: u32, child: u32, level: u32) -> u32 {
        let half = N / 2;
        let new = self.nodes.len() as u32;
        let old = &mut self.nodes[node_id as usize];
        let upper = old.split_off();
        let (parent, kept, moved) = (old.parent, old.total(), upper.total());
        let children = upper.slots.map(|slot| slot.child);
        self.nodes.push(upper);
        for (i, &c) in children[..N - half].iter().enumerate() {
            self.adopt(level, c, new, i as u32);
        }

        if parent == NO_NODE {
            // Grow a new root above the two halves.
            let root = self.nodes.len() as u32;
            let mut top = Node::new(NO_NODE);
            top.insert(0, node_id, kept);
            top.insert(1, new, moved);
            self.nodes.push(top);
            self.adopt(level + 1, node_id, root, 0);
            self.adopt(level + 1, new, root, 1);
            self.root = root;
            self.height += 1;
        } else {
            self.add_child(parent, node_id, new, moved, level + 1);
        }

        if children[..N - half].contains(&child) {
            new
        } else {
            node_id
        }
    }

    /// Records that `child` is child `at` of `parent`.
    fn adopt(&mut self, level: u32, child: u32, parent: u32, at: u32) {
        let node_id = if level == 1 { &mut self.leaves[child as usize] } else { &mut self.nodes[child as usize] };
        (node_id.parent, node_id.at) = (parent, at);
    }

    fn index_in_parent(&self, level: u32, child: u32) -> u32 {
        if level == 1 {
            self.leaves[child as usize].at
        } else {
            self.nodes[child as usize].at
        }
    }
}
