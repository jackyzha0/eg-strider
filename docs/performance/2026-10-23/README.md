# Operation ingestion and reverse replay performance proposal

This patch reduces repeated operation decoding, tree-prefix scans and piece-list
mutations while reconstructing text. Operations whose text is subsequently
erased can cancel before materialization. It retains eg-strider's collaborative
editing rules and valid-event assumptions.

Against unchanged upstream commit `b794c18579d7ec041d6fb12f56f59e48fed81526`,
the seven original traces measured **1.264×** geometric mean merge
speedup; the reversed-order repeat measured **1.241×**.
These correspond to approximately **20.9% and 19.4% less merge time**.

The additional frozen verification after research stopped measured **1.260× and 1.268×**, also with 100 runs per trace and accepted reference controls. See [post-stop-final.json](post-stop-final.json); its complete raw records are in the archive.

## Implementation

1. Specialize single-operation ingestion and insert/delete chain normalization;
   use an adaptive UTF-8 prefix search and bounded short-payload copying.
2. Store resolved transformed edits directly and coalesce adjacent compatible
   inserts/removals within each concurrent range. Reverse replay consumes the
   resolved stream instead of decoding the operation runs a second time.
3. Traverse prepare-version changes using event spans and kinds without
   unnecessarily decoding positions or slicing text.
4. Cache the selected slot and its prefixes in the CRDT tree; compute selection
   and effect prefixes together. Mutations invalidate the relevant cached state.
5. Remove whole piece ranges within a node with one suffix compaction.
6. Coalesce restored deleted intervals during reverse replay and cancel inserts
   inside them. Once every final character is emitted, continue position,
   overflow, UTF-8 and finalization checks while avoiding obsolete piece topology
   updates. This also removes append-time trimming bookkeeping.

The production diff is confined to six existing implementation files. Benchmark
code, dependency versions, release flags and existing tests are unchanged.
The new regression test and standalone probe exercise the reverse-replay paths.
The bounded short-copy helper adds unsafe pointer copies; its destination is
reserved first, all accesses are within the supplied payload, and length is
published only after copying the complete valid UTF-8 string.

## Original workload results

The first accepted independent pair is shown below; both measurement orders
used 100 runs per trace and the original comparator's median timing.

| Trace | Baseline ms | Candidate ms | Speedup | Reversed-order speedup |
| --- | ---: | ---: | ---: | ---: |
| A1 | 6.8416 | 6.0464 | 1.132× | 1.110× |
| A2 | 15.1874 | 13.9757 | 1.087× | 1.068× |
| C1 | 19.1059 | 14.7349 | 1.297× | 1.277× |
| C2 | 22.9639 | 17.4342 | 1.317× | 1.301× |
| S1 | 2.3411 | 2.0164 | 1.161× | 1.132× |
| S2 | 3.4737 | 2.7564 | 1.260× | 1.233× |
| S3 | 3.3194 | 1.9797 | 1.677× | 1.647× |

The aggregate is the seventh root of the product of the seven baseline/candidate
median-time ratios. Each trace receives equal weight. It is not a weighted
total elapsed time or an average of percentage time reductions.

## Tradeoffs and correctness

| Trace | Merge peak heap delta | Retained heap delta | Checkpoint byte delta |
| --- | ---: | ---: | ---: |
| A1 | +261,464 B | +0 B | -2 B |
| A2 | -19,392 B | +0 B | -16 B |
| C1 | +373,648 B | +0 B | -369 B |
| C2 | +83,616 B | +0 B | -426 B |
| S1 | -96 B | +0 B | -1,307 B |
| S2 | -192 B | +0 B | -2,342 B |
| S3 | -384 B | +0 B | -2,984 B |

Reopen timings in these repetitions were approximately neutral (within about
1.2%). Incremental edits are separate metrics: for example, the first pair's
C2 edit-after-merge increased from 0.6533 ms to 0.6736 ms, while S3 decreased
from 0.3598 ms to 0.2608 ms. Raw JSON retains all current/old-version edit,
reopen, memory and checkpoint measurements. A merge improvement is not an
improvement across every metric.

The trusted qualification passed all 46 unchanged upstream tests, including
randomized differential histories, Unicode, incremental graphs, and save/load,
plus all seven full-trace diamond-types text comparisons. Four additional PR
regression tests exercise 1,792 deterministic Unicode/NUL/base-cut scenarios
and retained validation after survivor exhaustion. A standalone research probe
also checked 70,000 such scenarios; its invocation receipts are archived.

Twenty additional synthetic research histories vary content, batching,
deletions, branch length and writer count up to 16. The selected research
checkout passed all 27 original-plus-generated text comparisons; those
development measurements are in the archive. The independent 100-run headline
above covers the original seven. All workloads and the generator were available
to every researcher. This is algorithm research with open inputs, without an
unseen test set or a claim of production-wide generalization.

The omitted runtime validity checks and non-Byzantine assumption remain those
documented upstream. Other-library settings remain unchanged; this proposal
compares the candidate with upstream eg-strider rather than making a new
cross-library ranking claim.

## Measurement protocol and environment

- Same pinned Rust 1.99.0 (`b940084d7`), lockfile and upstream release profile
  (`lto=true`, `codegen-units=1`) for baseline and candidate.
- Original, unmodified compare program. Merge includes append and checkout of
  in-memory operations; JSON parsing and network transport are outside the timer.
  Memory is counted allocated heap, not process RSS.
- Fresh processes in baseline → candidate → candidate → baseline order, pinned
  to logical CPU 3 on an Intel Xeon 2.80 GHz KVM host. The host has 32 logical
  CPUs / 16 physical cores across two sockets. Other host jobs exist; this is
  a virtualized shared host, so the environment is not fully isolated.
- Both accepted independent pairs kept every trace's unchanged diamond-types
  reference within 5%. Reference timings are a drift control; the speedup is
  computed directly from upstream-eg-strider/candidate timings.
- An early apparent 1.91× result for a different submission was invalidated
  after large reference drift. Its original and superseding receipts remain in
  the audit archive; it is excluded from this patch's performance claim.
- Discovery used eight OpenCode researchers with GPT-6 Astra. Human review and
  reproduction on the maintainer's workloads are the purpose of this draft.

## Receipts

- [Independent result and source/executable hashes](confirmed.json).
- [Baseline](baseline.json), [candidate](candidate.json),
  [candidate repeat](candidate-repeat.json), [baseline repeat](baseline-repeat.json).
- [Original unchanged-suite log](upstream-tests.log); additional PR-validation
  logs are included alongside this document.
- [Complete saved receipt archive](audit-receipts.tar.gz), with a SHA-256
  inventory at `MANIFEST.json` inside the archive. It contains every submitted
  source snapshot, receipt, saved build/test log and timing record, including
  abandoned or noisy attempts and independently checked parents; baseline
  reproduction/calibration; additional generated inputs and generator;
  selected development/probe evidence; and any completed post-stop final check.
  Executables, dependency caches, provider traffic/credentials and conversations
  are outside this measurement archive.
- [Checksums for the review artifacts](SHA256SUMS).

The selected immutable source ID is `aa98473b207043a1a679d8f4ea36ff18`.
Implementation-source digest: `47ddc3d6d06441f6b7e8c03e7251471aac9803de35bbece8f02732d6570c9c6c`.
That digest covers production `.rs` files in the four crate `src` directories;
the added documentation, examples and tests do not change the measured source.

## Reproduce

Use Rust 1.99.0 and a normal C build toolchain. From this PR checkout:

```sh
git worktree add --detach ../eg-strider-baseline b794c18579d7ec041d6fb12f56f59e48fed81526
git clone https://github.com/josephg/egwalker-paper.git ../egwalker-paper
git -C ../egwalker-paper checkout 4d9bef55e4f2e3b3b8b0efe8f91cd35d34ed35a8
cargo build --release --locked -p compare --manifest-path ../eg-strider-baseline/Cargo.toml --target-dir ../eg-strider-baseline-target
cargo build --release --locked -p compare --target-dir ../eg-strider-candidate-target
cargo test --workspace --locked
cargo test --release --locked -p strider-text --test reverse_replay
cargo run --release --locked -p strider-text --example reverse_replay_probe -- 10000
python3 docs/performance/2026-10-23/reproduce.py --baseline ../eg-strider-baseline-target/release/compare --candidate ../eg-strider-candidate-target/release/compare --traces ../egwalker-paper/datasets --out ../eg-strider-measurements --cpu 3
```

Select an available idle CPU on the reproducing machine. The helper preserves
all attempts, runs the unchanged binaries, checks the text oracles and reports
both geometric means and per-trace reference drift. It exits unsuccessfully
when reference drift exceeds the requested limit; repeat in a new output
directory rather than overwriting the noisy attempt.

For the expanded suite, unpack the archive and pass its
`expanded-research/traces` directory as `--traces`. The full expanded inputs,
their generation seed/hashes and generator source are retained in the archive.
The broader five-library reproduction did not complete and is not claimed here.

## Suggested review sequence

Review ingestion/UTF-8 normalization first, then tree selection and metadata-only
walking, then the transformed stream, and finally reverse cancellation and
survivor exhaustion with the regression tests. The current source is the actual
combined measured implementation; individual experiment scores should not be
added together. Larger sparse-history/indexing prototypes remain separate
experiments and are excluded from this proposal.
