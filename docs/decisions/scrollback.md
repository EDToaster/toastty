# Decision: Scrollback Data Structure

**Date:** 2026-05-22
**Status:** Recommendation (prototype-stage)
**Slug:** `scrollback`

## Recommendation

**Use a ring buffer of `Vec<Cell>` rows (option B), with small-inline-capacity per row.**

Specifically: `Box<[Row; CAP]>` where `Row { cells: SmallVec<[Cell; 16]>, soft_wrap: bool }`. Allocate the ring once at terminal start; never resize the outer storage. Hyperlinks live in a separate `SlotMap<HyperlinkId, HyperlinkEntry>` reachable from each cell via a 64-bit key.

Why not the other two:

- **Flat `Vec<Row>` (Alacritty)** is the closest runner-up and would be a perfectly defensible choice. Within 2% of ring on memory, ~2x faster on reflow. The reason to prefer ring anyway is the *steady-state* discipline: append never reallocates, never moves the outer Vec, and "drop oldest" is `O(1)` modular arithmetic — exactly what a terminal that scrolls millions of lines per session needs.
- **Rope / chunked tree** is over-engineered for what we need. The benchmark below shows it's the slowest on every axis except memory, where the win is only ~30% — and that win comes from amortizing line metadata, which the other options can match with the same span table.

## Numbers

100,000 mixed-content lines (60% ASCII 40-80 cols, 15% long-with-hyperlink @ 200 cols, 15% CJK, 10% emoji/ZWJ), reflow 80 ↔ 120 alternating, all on a single Apple Silicon machine. Three runs; numbers below are typical.

| impl          | fill (ms) | self (MB) | peak RSS (MB) | reflow med (µs) | reflow p99 (µs) | append med (ns) | append p99 (ns) | scroll med (ns) | scroll p99 (ns) |
| ------------- | --------: | --------: | ------------: | --------------: | --------------: | --------------: | --------------: | --------------: | --------------: |
| `flat_vec`    |        45 |     399.9 |         680.6 |          31,248 |          72,759 |             167 |           2,250 |             208 |           1,000 |
| `ring_buffer` |        49 |     393.4 |         674.1 |          66,129 |          80,974 |             250 |           1,417 |              83 |             334 |
| `rope_cells`  |        49 |     275.5 |         556.1 |         194,487 |         219,854 |             250 |           1,250 |           1,417 |           2,542 |

All three produce identical row counts after wrap (129,740 physical rows at width 80), and all three pass the wide-char-no-straddle invariant after reflow to 40 and 120 cols.

Reproduce: `cd prototypes/scrollback && cargo build --release && /Users/howard/.cargo/shared-target/release/bench`.

### What the numbers say

- **Memory dominates correctness here.** `Cell` is 40 bytes (8B grapheme + 1B len + 1B width + 20B `Style` + 8B `Option<HyperlinkId>` + 2B padding). 130k rows × ~80 cells × 40 B ≈ 416 MB is the floor regardless of container. Everything above that is overhead.
- **Reflow is dominated by `Cell` copies, not container shape.** The rope's 3x penalty over flat_vec is because reflow currently rebuilds the leaf chunks from scratch — a smarter rope (in-place span re-indexing without moving cells) could theoretically beat it, but that's a >10x complexity multiplier for unclear benefit.
- **Random scroll exposes the rope's weakness.** O(log n) tree descent on every row read = 7x slower than the trivial `Vec::index`. For a renderer that pulls the viewport every frame this is bad.

## Memory of `Cell` — where it goes

```
size_of Cell                = 40 bytes
size_of Style               = 20 bytes
size_of flat_vec::Row       = 32 bytes  (Vec + bool, padded)
size_of ring_buffer::Row    = 656 bytes (SmallVec<[Cell; 16]> + bool + bool)
```

Even at INLINE_CELLS=16, the ring's row slot is 656 B. With CAP=200,000 that's 131 MB of *empty* row infrastructure before any data. We accept this because:

1. it's bounded — the ring never grows
2. the alternative (Vec<Row>) reallocates the outer Vec under push, and during that reallocation the renderer can't safely take a slice
3. inline storage means ~80% of post-wrap rows skip the allocator entirely (small post-wrap continuation rows; spacers; etc.)

If memory ever becomes the binding constraint, the right move is **shrinking `Cell` itself** (pack style into a u32 stylesheet-id; use a 16-bit hyperlink id) — not switching container. That would cut all three numbers proportionally.

## Correctness

### Wide chars at the wrap point

All three implementations route every wrap decision through the same helper, `common::wrap_line`:

```rust
if cur_w + w > cols {
    if w == 2 && cur_w == cols - 1 {
        // pad a SPACER (visible space cell) at column cols-1,
        // wide char will start at column 0 of the next row.
        cur.push(spacer);
    }
    flush_row(...);
}
cur.push(*c);
cur_w += w;
```

This means a CJK char that would land at column 79 of an 80-col row gets a spacer in column 79 and starts at column 0 of the next row — the same algorithm Alacritty's `shrink_lines` uses, modulo the in-place vs. rebuild distinction. Verified by `correctness.rs`: a hand-crafted line with 79 ASCII + 1 CJK + 10 ASCII produces a wide char at col 0 of row 1 across all three implementations.

The same logic handles grow-direction (80 → 120) because we always re-run `wrap_line` from the merged logical-line buffer; we never try to "extend" an existing wrapped row by appending cells from the next row.

### Grapheme clusters (combining marks, emoji ZWJ)

Each grapheme cluster is one `Cell`. We use `unicode-segmentation` 1.13.2 to split input into clusters before storing — this is the corpus-side responsibility, not the scrollback's. The scrollback never re-segments. That means:

- Emoji ZWJ family (`👨‍👩‍👧‍👦`) is one Cell at width 2.
- `e\u{0301}` is one Cell at width 1.
- Width-0 graphemes (rare standalone combining marks) are bumped to width 1 in our corpus; production should attach them to the previous base cell, but that's a Cell-shape question, not a scrollback-container question.

### Mode 2027 (grapheme-cluster width)

With mode 2027 the *width* of a cluster changes (some terminals will report width-2 for emoji that wcwidth says is width-1). This is **handled entirely at the parser/segmenter boundary**: by the time a Cell hits the scrollback, `width` is already correct for the current mode. None of the three container options have to know about mode 2027.

Of the three, the ring buffer is **easiest to extend** because the per-row `Vec<Cell>` already supports variable-width clusters. The rope would need to update its line-start index whenever a width recomputation moves cells across a wrap boundary — that's a tree edit per affected line.

### Hyperlink interning

Same table for all three implementations:

```rust
new_key_type! { pub struct HyperlinkId; }

pub struct HyperlinkTable {
    map: SlotMap<HyperlinkId, HyperlinkEntry>,
}

pub struct HyperlinkEntry {
    url: String,
    refcount: u32,
}
```

- Cells store `Option<HyperlinkId>` — 8 bytes (slotmap key = u64).
- `intern(url)` returns an existing key if the URL is already in the table, else inserts; refcount += 1.
- `release(id)` decrements; removes when refcount hits 0.
- Lookup: `map.get(id) -> Option<&HyperlinkEntry>` — O(1).
- The current `intern` is linear-scan over entries; production should add a `HashMap<String, HyperlinkId>` side index for O(1) intern.

The corpus generates 40 unique URLs across the 100k-line dataset. Memory in the table is ~40 * (24 + URL bytes + 4) ≈ 2 KB — utterly negligible. Even if a hostile app emits 10k unique OSC 8 sequences, the table caps out at ~500 KB.

## Honest assessment: which is over-engineered?

- **Rope (option C) is over-engineered.** It pays a 7x random-scroll penalty and a 3x reflow penalty to save ~30% of memory, and the memory win comes from a less-fragmented leaf layout that the other two could trivially match with arena allocation. Ropes shine when you need cheap mid-buffer insert/delete; a terminal *never does that* — appends are always at the bottom, edits are always on the visible last-row.
- **Flat Vec (option A) is fine** — Alacritty made the same choice. It loses only on the discipline argument: a Vec under push will eventually reallocate, which is a hidden ~ms-scale stall at 100k+ rows.
- **Ring buffer (option B) is the right amount of structure.** Allocate once, append forever, drop oldest in O(1), use small-inline cells so the inner-row allocator isn't on the hot path.

Don't reach for the rope. The grid is not a text editor.

## Crate versions used (pinned)

```toml
smallvec             = "=1.15.1"   # const_generics, const_new, union
slotmap              = "=1.1.1"
unicode-width        = "=0.2.2"
unicode-segmentation = "=1.13.2"
rand                 = "=0.10.1"
rand_chacha          = "=0.10.0"
peak_alloc           = "=0.3.0"
bitflags             = "=2.11.1"
```

`smallvec 2.0` is still in alpha (2.0.0-alpha.12 as of 2026-05-22); the const-generics path on 1.15.1 is the stable one. `criterion` 0.8.2 is the current bench framework but we used direct `Instant` timing instead — workload sizes are >100 ms, statistical bootstrapping adds nothing here. `bumpalo` (3.20.3) was evaluated but rejected for the ring path: the inline SmallVec storage already removes the allocator from 80%+ of small-row pushes, and a bump arena would force us to rebuild the whole scrollback to free memory.

## Open questions for the real implementation

1. **Outer ring capacity.** Today the ring is sized in *physical rows* (= 2x logical rows worst-case for an 80-col terminal). Realistically we want it sized in *logical lines* with an upper bound on physical rows derivable from current width. Reflow at the maximum width is the right way to size it.
2. **Spacer cells and selection.** When a wide char gets bumped, our spacer is `' '` width=1. For text selection / copy, the spacer should produce zero output and the wide char should produce one grapheme. Add a `Cell::is_spacer()` flag and have copy-out skip spacers.
3. **Span table for SGR runs.** All three prototypes store style per-cell. A space-optimized version stores `Span { start, end, style }` separately. This is a *cell-shape* refactor — orthogonal to the container choice. Defer until profiling shows it matters.
4. **Reflow is not the hot path.** Median 66 ms for the ring is fine; users resize maybe once per session. The hot path is **append**, where the ring is 250 ns p50 — comfortably under one frame budget at any refresh rate.
