use std::ops::Range;
use strider_text::PieceTable;

enum Edit {
    Insert { pos: u32, bytes: Range<usize>, len: u32 },
    Delete { pos: u32, len: u32 },
}

fn next(state: &mut u64) -> usize {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (*state >> 24) as usize
}

fn main() {
    let seeds: usize = std::env::args().nth(1).unwrap_or("10000".into()).parse().unwrap();
    let alphabet = ['a', '\0', 'é', '界', '🙂', '\u{301}', '\n'];
    let mut cases = 0;
    for seed in 0..seeds {
        let mut random = seed as u64 + 1;
        let mut document = Vec::<char>::new();
        let mut inserted = String::new();
        let mut edits = Vec::new();
        let mut states = vec![String::new()];
        for step in 0..160 {
            if !document.is_empty() && (step % 32 == 31 || next(&mut random) % 3 == 0) {
                let pos = if step % 32 == 31 { 0 } else { next(&mut random) % (document.len() + 1) };
                let len = if step % 32 == 31 { document.len() } else { (next(&mut random) % 24).min(document.len() - pos) };
                document.drain(pos..pos + len);
                edits.push(Edit::Delete { pos: pos as u32, len: len as u32 });
            } else {
                let pos = next(&mut random) % (document.len() + 1);
                let len = next(&mut random) % 24;
                let text: Vec<_> = (0..len).map(|_| alphabet[next(&mut random) % alphabet.len()]).collect();
                let start = inserted.len();
                inserted.extend(text.iter());
                document.splice(pos..pos, text);
                edits.push(Edit::Insert { pos: pos as u32, bytes: start..inserted.len(), len: len as u32 });
            }
            states.push(document.iter().collect());
        }
        // Alternate a completely erased final state and a surviving Unicode suffix.
        if seed % 2 == 1 {
            let pos = document.len();
            let text: String = alphabet.iter().cycle().take(17).collect();
            let start = inserted.len();
            inserted.push_str(&text);
            document.extend(text.chars());
            edits.push(Edit::Insert { pos: pos as u32, bytes: start..inserted.len(), len: 17 });
            states.push(document.iter().collect());
        }
        let expected = states.last().unwrap();
        for cut in [0, 1, 31, 80, 159, edits.len(), next(&mut random) % edits.len()] {
            let mut table = PieceTable::new(document.len() as u32, &inserted);
            for edit in edits[cut..].iter().rev() {
                match edit {
                    Edit::Insert { pos, bytes, len } => table.undo_insert(*pos, bytes.clone(), *len),
                    Edit::Delete { pos, len } => table.undo_remove(*pos, *len),
                }
            }
            table.undo_rest(&states[cut]);
            assert_eq!(&table.into_string(), expected, "seed {seed}, cut {cut}");
            cases += 1;
        }
    }
    println!("{cases} Unicode cancellation/exhaustion/base-cut scenarios passed");
}
