mod pieces;

use pieces::PieceList;
use std::ops::Range;

/// Builds a document's final text by undoing its operations last to first, counting positions in
/// characters.
pub struct PieceTable<'a> {
    inserted_text: &'a str,
    pieces: PieceList,
    /// Length of the document as undone so far.
    len: u32,
    /// A restored deleted interval, not yet placed in `pieces`. Inserts undone wholly inside
    /// it can be cancelled without editing the piece list at all.
    pending_deleted: Option<(u32, u32)>,
    /// The final text, a byte per character with non-ASCII characters as [`NON_ASCII_MARK`], then
    /// [`SHORT`] spare bytes.
    final_text_bytes: Vec<u8>,
    /// Per page of [`NON_ASCII_PAGE`] positions, where its non-ASCII characters start in
    /// `non_ascii_chars`, if it has any.
    non_ascii_pages: Vec<Option<u32>>,
    /// The final text's non-ASCII characters, by position within their page.
    non_ascii_chars: Vec<char>,
    /// Final characters whose source text has not yet been reached by reverse replay.
    remaining: u32,
}

/// Stands for a non-ASCII character in `final_text_bytes`.
const NON_ASCII_MARK: u8 = 0x80;

/// The longest copy done as two 16-byte words.
const SHORT: usize = 32;

/// Copies `n` bytes of `from`, from `skip`, to `to[at..]`, which must have [`SHORT`] bytes to
/// spare.
fn copy(to: &mut [u8], at: usize, from: &[u8], skip: usize, n: usize) {
    if n > SHORT || skip + SHORT > from.len() {
        to[at..at + n].copy_from_slice(&from[skip..skip + n]);
        return;
    }
    // Most copies are a few bytes long and no two alike, and a general copy branches on the
    // length in ways that cannot be predicted. These take the same steps whatever their
    // length: two words are read from each side, and the bytes past the end are put back
    // as they were.
    let word = |bytes: &[u8]| u128::from_le_bytes(bytes[..16].try_into().expect("16 bytes"));
    for (start, n) in [(0, n.min(16)), (16, n.saturating_sub(16))] {
        let beyond = u128::MAX.checked_shl(8 * n as u32).unwrap_or(0);
        let merged = word(&to[at + start..]) & beyond | word(&from[skip + start..]) & !beyond;
        to[at + start..at + start + 16].copy_from_slice(&merged.to_le_bytes());
    }
}

/// Positions per page of non-ASCII characters; tiny in debug builds so that tests cross pages.
const NON_ASCII_PAGE: usize = if cfg!(debug_assertions) { 4 } else { 256 };

/// Places `ch` at position `at` of the final text.
#[inline]
fn place_char(final_text_bytes: &mut [u8], non_ascii_pages: &mut Vec<Option<u32>>, non_ascii_chars: &mut Vec<char>, at: usize, ch: char) {
    if ch.is_ascii() {
        final_text_bytes[at] = ch as u8;
    } else {
        final_text_bytes[at] = NON_ASCII_MARK;
        place_non_ascii(non_ascii_pages, non_ascii_chars, at, ch);
    }
}

/// Records non-ASCII `ch` at position `at`, allocating its page if it has none.
#[cold]
fn place_non_ascii(non_ascii_pages: &mut Vec<Option<u32>>, non_ascii_chars: &mut Vec<char>, at: usize, ch: char) {
    if non_ascii_pages.len() <= at / NON_ASCII_PAGE {
        non_ascii_pages.resize(at / NON_ASCII_PAGE + 1, None);
    }
    let start = *non_ascii_pages[at / NON_ASCII_PAGE].get_or_insert_with(|| {
        non_ascii_chars.resize(non_ascii_chars.len() + NON_ASCII_PAGE, '\0');
        (non_ascii_chars.len() - NON_ASCII_PAGE) as u32
    });
    non_ascii_chars[start as usize + at % NON_ASCII_PAGE] = ch;
}

fn non_ascii_at(non_ascii_pages: &[Option<u32>], non_ascii_chars: &[char], at: usize) -> char {
    let start = non_ascii_pages[at / NON_ASCII_PAGE].expect("a character was recorded on this page");
    non_ascii_chars[start as usize + at % NON_ASCII_PAGE]
}

impl<'a> PieceTable<'a> {
    /// Starts from the final document of `len` characters, with `inserted_text` holding every
    /// insert's text.
    pub fn new(len: u32, inserted_text: &'a str) -> Self {
        PieceTable {
            inserted_text,
            pieces: PieceList::new(len),
            len,
            pending_deleted: None,
            final_text_bytes: vec![0; len as usize + SHORT],
            non_ascii_pages: Vec::new(),
            non_ascii_chars: Vec::new(),
            remaining: len,
        }
    }

    /// Undoes an insert at `pos` of the `len` characters at `text` in `inserted_text`.
    pub fn undo_insert(&mut self, pos: u32, text: Range<usize>, len: u32) {
        assert!(pos.checked_add(len).is_some_and(|end| end <= self.len), "insert was not within the document");
        let (inserted_text, start) = (self.inserted_text.as_bytes(), text.start);
        let text = &self.inserted_text[text];
        // Debug builds only: counting takes as long as the text.
        debug_assert_eq!(text.chars().count(), len as usize);
        if len == 0 {
            return;
        }
        self.len -= len;
        // Once all final characters are known, earlier text can only be deleted text.
        // Keep validating the operations and tracking their lengths, but no longer
        // maintain a piece list with nothing left to emit.
        if self.remaining == 0 {
            return;
        }
        let ascii = text.len() == len as usize;
        let (mut pos, mut len, mut skipped) = (pos, len, 0);
        if let Some((at, deleted)) = self.pending_deleted {
            if pos >= at && pos - at <= deleted && len <= deleted - (pos - at) {
                self.pending_deleted = (deleted > len).then_some((at, deleted - len));
                return;
            }
            let end = pos + len;
            if pos < at && end > at && end <= at + deleted {
                // Only the insert's prefix survived the pending deletion.
                self.pending_deleted = (end < at + deleted).then_some((pos, at + deleted - end));
                len = at - pos;
            } else if pos >= at && pos < at + deleted && end > at + deleted {
                // Only its suffix survived. The undeleted prefix of the pending interval stays.
                self.pending_deleted = (pos > at).then_some((at, pos - at));
                skipped = at + deleted - pos;
                len -= skipped;
                pos = at;
            } else {
                self.flush_deleted();
            }
        }
        let (final_text_bytes, pages, chars) = (&mut self.final_text_bytes, &mut self.non_ascii_pages, &mut self.non_ascii_chars);
        let remaining = &mut self.remaining;
        if ascii {
            // All ASCII: characters and bytes coincide.
            self.pieces.remove(pos, len, |at, skip, n| {
                *remaining -= n;
                copy(final_text_bytes, at as usize, inserted_text, start + (skipped + skip) as usize, n as usize)
            });
        } else {
            // The survivors come in the order they have in `text`, so one pass over it
            // reaches them all. `passed` counts the characters gone by.
            let (mut rest, mut passed) = (text.chars(), 0);
            if skipped > 0 {
                rest.nth(skipped as usize - 1);
            }
            self.pieces.remove(pos, len, |at, skip, n| {
                *remaining -= n;
                if skip > passed {
                    rest.nth((skip - passed - 1) as usize);
                }
                for (at, ch) in (at as usize..).zip(rest.by_ref().take(n as usize)) {
                    place_char(final_text_bytes, pages, chars, at, ch);
                }
                passed = skip + n;
            });
        }
    }

    /// Undoes a removal of `len` characters at `pos`.
    pub fn undo_remove(&mut self, pos: u32, len: u32) {
        assert!(pos <= self.len, "removal was not within the document");
        if len == 0 {
            return;
        }
        self.len = self.len.checked_add(len).expect("document too long");
        if self.remaining == 0 {
            return;
        }
        if let Some((at, deleted)) = self.pending_deleted {
            if pos >= at && pos - at <= deleted {
                self.pending_deleted = Some((at, deleted + len));
                return;
            }
            self.flush_deleted();
        }
        self.pending_deleted = Some((pos, len));
    }

    fn flush_deleted(&mut self) {
        if let Some((pos, len)) = self.pending_deleted.take() {
            self.pieces.insert_deleted(pos, len);
        }
    }

    /// Undoes all remaining operations at once, given the text they produced.
    pub fn undo_rest(&mut self, text: &str) {
        if self.remaining == 0 {
            // The remaining document is entirely deleted. Retain the same traversal and
            // end check as an all-deleted piece list, without its obsolete topology.
            if text.len() != self.len as usize {
                let mut rest = text.chars();
                if self.len > 0 {
                    rest.nth(self.len as usize - 1);
                }
                assert!(rest.next().is_none(), "the text is not the document as it stands");
            }
            self.len = 0;
            return;
        }
        self.flush_deleted();
        let (final_text_bytes, pages, chars) = (&mut self.final_text_bytes, &mut self.non_ascii_pages, &mut self.non_ascii_chars);
        if text.len() == self.len as usize {
            // All ASCII: characters and bytes coincide.
            let mut from = 0;
            for (len, at) in self.pieces.iter() {
                if let Some(at) = at {
                    final_text_bytes[at as usize..(at + len) as usize].copy_from_slice(&text.as_bytes()[from..from + len as usize]);
                }
                from += len as usize;
            }
        } else {
            let mut rest = text.chars();
            for (len, at) in self.pieces.iter() {
                let Some(at) = at else {
                    rest.nth(len as usize - 1);
                    continue;
                };
                for (at, ch) in (at as usize..).zip(rest.by_ref().take(len as usize)) {
                    place_char(final_text_bytes, pages, chars, at, ch);
                }
            }
            assert!(rest.next().is_none(), "the text is not the document as it stands");
        }
        self.len = 0;
        self.remaining = 0;
    }

    /// The final text, once every operation is undone.
    pub fn into_string(mut self) -> String {
        assert_eq!(self.len, 0, "edits left to take back");
        self.final_text_bytes.truncate(self.final_text_bytes.len() - SHORT);
        if self.non_ascii_chars.is_empty() {
            return String::from_utf8(self.final_text_bytes).expect("ASCII");
        }
        // Mostly ASCII in practice: whole stretches of it are copied at once.
        let mut bytes = Vec::with_capacity(self.final_text_bytes.len() + self.non_ascii_chars.len());
        for (i, stretch) in self.final_text_bytes.chunks(16).enumerate() {
            if stretch.is_ascii() {
                bytes.extend_from_slice(stretch);
            } else {
                for (at, &byte) in (i * 16..).zip(stretch) {
                    let ch = if byte == NON_ASCII_MARK { non_ascii_at(&self.non_ascii_pages, &self.non_ascii_chars, at) } else { byte as char };
                    bytes.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
            }
        }
        String::from_utf8(bytes).expect("encoded from characters")
    }
}
