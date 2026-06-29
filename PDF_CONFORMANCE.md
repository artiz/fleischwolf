# PDF conformance

How close the Rust PDF pipeline gets to docling's **default** Markdown, measured
byte-for-byte against the committed groundtruth (`tests/data/pdf/groundtruth/*.md`).
The groundtruth is regenerated from **live published docling**, so it agrees with
`scripts/conformance.sh pdf`.

> Measure locally with `scripts/pdf_groundtruth.sh` (diffs the checked-in
> reference; no docling install needed) or `scripts/conformance.sh pdf` (installs
> docling and diffs against it). Diff = changed lines vs the groundtruth (one
> changed line counts as 2).

## Current state

**4 / 14 groundtruth PDFs are byte-for-byte exact.**

| PDF | diff | dominant remaining blocker |
|---|---:|---|
| picture_classification | **exact** | — |
| code_and_formula | **exact** | — |
| multi_page | **exact** | — |
| 2305.03393v1-pg9 | **exact** | — (TableFormer table, cell-for-cell) |
| right_to_left_01 | 2 | RTL justified double-space |
| right_to_left_02 | 8 | RTL bidi spacing |
| amt_handbook_sample | 14 | justified spacing + figure captions |
| 2305.03393v1 | 40 | title-page reading order + author-ID run spacing |
| right_to_left_03 | 66 | RTL bidi |
| normal_4pages | 74 | reading order |
| table_mislabeled_as_picture | 110 | layout over-detects tables (survey rendered as tables) |
| 2206.01062 | 234 | TableFormer multi-row headers + title-page reading order |
| 2203.01017v2 | 247 | TableFormer structure + reading order |
| redp5110_sampled | 271 | TOC mis-classified as a picture; cover-page ordering |

The close ones (`right_to_left_01/02`, `amt_handbook`) are 1–2 fixes from exact —
the realistic path to 50% (7/14).

## How the pipeline works

pdfium extracts the glyph layer and renders each page to a bitmap; an ONNX stack
(layout detection, TableFormer, PaddleOCR) interprets it; regions are assembled in
reading order into a `DoclingDocument`. Tables use **TableFormer** (image encoder
+ autoregressive OTSL structure decoder + cell-bbox decoder, ported and exported
to ONNX in `tableformer.rs`) on a cv2-exact preprocessed crop (`resample.rs`); the
structure + matched cell text reproduce docling's padded GitHub tables (2305-pg9
is cell-for-cell exact).

### Text reconstruction: docling-parse line sanitizer (ported)

The byte-exact ceiling used to be the **text extractor** — pdfium differs from
docling's own `docling-parse` C++ parser at text-run boundaries. We closed it by
porting docling-parse's line sanitizer (`src/parse/page_item_sanitators/cells.h`
→ `dp_lines.rs`): a 3-pass corner-distance contraction (LTR → RTL → LTR-reverse)
with `merge_with` space insertion (one space when the gap exceeds
0.33×avg-char-width, plus literal space glyphs), `enforce_same_font`, ligature
recomposition (same-loose-box glyphs become one cell), and loose-box geometry
(uniform font ascent/descent), fed by pdfium glyph cells. It is the **default**
(set `DOCLING_LEGACY_LINES` to fall back to the old gap heuristic). This fixed
justified double-spacing, the space before `:`, lam-alef ordering, and fi/ffi
ligatures — and got `multi_page` to byte-exact.

Other text/serializer fixes matching docling: markdown escaping (`_`→`\_`, then
HTML-escape `&`/`<`/`>`), U+2212→`-`, `@`-glue (`mAP @0.5`), wrap dehyphenation,
paragraph-continuation merging across column/page breaks, and band-aware
two-column reading order (full-width regions break the columns into bands).

## Remaining blockers (model-level)

These yield smaller or uncertain gains than the text-layer work already shipped.

1. **TableFormer structure on complex tables.** Multi-row headers / spans on the
   big papers (2206, 2203) differ from docling's OTSL prediction; one cell-
   structure diff cascades through the padded columns into many row diffs
   (2206's ~92 table-row diffs trace to ~4 structure diffs).
2. **Layout classification.** The layout ONNX classifies redp5110's
   table-of-contents as a *picture* (docling renders it as a table) and
   table_mislabeled's survey as *tables* (docling renders lists/text) — opposite
   classifications, not a text problem.
3. **Complex title-page reading order.** Author-block / abstract interleaving on
   the academic papers (band reading-order handles the full-width title; the
   in-column author/abstract order is still off).
4. **RTL justified double-spaces.** pdfium emits zero-width space boxes, losing
   the literal space that, with the inserted one, forms a justified double
   (`right_to_left_01`). Needs space-box reconstruction that estimates width.
