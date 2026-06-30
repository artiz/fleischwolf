//! Port of docling-parse's line-cell sanitizer
//! (`src/parse/page_item_sanitators/cells.h` → `create_line_cells` /
//! `contract_cells_into_lines_v1`). It merges per-glyph char cells into line
//! cells via a 3-pass contraction — left-to-right, right-to-left, then
//! left-to-right with reverse — using corner-distance adjacency and inserting at
//! most one space per merge. This reproduces docling-parse's inter-word spacing
//! (justified double spaces, the space before a `:`, and RTL ordering) that the
//! ad-hoc `lines_from_glyphs` reconstruction can't.
//!
//! Geometry uses native PDF coordinates (y increases upward); each cell carries
//! its four transformed corners r0=bottom-left, r1=bottom-right, r2=top-right,
//! r3=top-left, exactly like `page_cell.h`.

use crate::pdfium_backend::{Glyph, TextCell};

// config.h: the factors that actually bind for line cells.
const MERGE: f64 = 1.0; // line_space_width_factor_for_merge (adjacency gate)
const MERGE_WITH_SPACE: f64 = 0.33; // line_space_width_factor_for_merge_with_space
const H_TOL: f64 = 1.0; // horizontal_cell_tolerance (ligature eps_d1 relaxation)

#[derive(Clone)]
struct Cell {
    text: String,
    rx0: f64,
    ry0: f64, // bottom-left
    rx1: f64,
    ry1: f64, // bottom-right
    rx2: f64,
    ry2: f64, // top-right
    rx3: f64,
    ry3: f64, // top-left
    ltr: bool,
    active: bool,
    lig_carry: bool, // last_merged_cell_was_ligature
    font: u64,       // hash of the PDF font name+flags (for enforce_same_font)
}

impl Cell {
    /// Length of the bottom edge (baseline advance) — `page_cell.h::length`.
    fn length(&self) -> f64 {
        ((self.rx1 - self.rx0).powi(2) + (self.ry1 - self.ry0).powi(2)).sqrt()
    }

    /// Running mean glyph advance over the whole accumulated cell.
    fn avg_char_width(&self) -> f64 {
        let n = self.text.chars().count();
        if n > 0 {
            self.length() / n as f64
        } else {
            0.0
        }
    }

    /// Distance from this cell's bottom-right corner to `other`'s bottom-left.
    fn gap(&self, other: &Cell) -> f64 {
        ((self.rx1 - other.rx0).powi(2) + (self.ry1 - other.ry0).powi(2)).sqrt()
    }

    /// `is_adjacent_to`: both the bottom-corner gap (`< eps0`) and the top-corner
    /// gap (`< eps1`) must be small. The vertical component keeps different
    /// baselines/lines from merging.
    fn adjacent(&self, other: &Cell, eps0: f64, eps1: f64) -> bool {
        let d0 = self.gap(other);
        let d1 = ((self.rx2 - other.rx3).powi(2) + (self.ry2 - other.ry3).powi(2)).sqrt();
        d0 < eps0 && d1 < eps1
    }

    /// Punctuation/space cells are bidi-neutral bridges.
    fn same_orientation(&self, other: &Cell) -> bool {
        self.ltr == other.ltr || is_punct_or_space(&self.text) || is_punct_or_space(&other.text)
    }

    /// `merge_with`: absorb `other` (which lies to this cell's right). Insert at
    /// most one separator space when the gap exceeds `delta`. RTL prepends.
    ///
    /// `euclidean` picks the gap measure: docling-parse uses the **Euclidean
    /// corner distance** `d0` (the same one `is_adjacent_to` uses). The pure-Rust
    /// parser produces clean advance boxes, so it uses `d0` to match docling
    /// byte-for-byte. pdfium's loose boxes overhang (an `f` extends left and
    /// overlaps its neighbour), which a Euclidean distance reads as a false
    /// positive gap and over-inserts spaces (`Self` → `Sel f`); that path keeps
    /// the **signed horizontal gap** instead.
    fn merge_with(&mut self, other: &Cell, delta: f64, euclidean: bool) {
        let gap = if euclidean {
            self.gap(other)
        } else {
            other.rx0 - self.rx1
        };
        if !self.ltr || !other.ltr {
            if delta < gap {
                self.text.insert(0, ' ');
            }
            self.text = format!("{}{}", other.text, self.text);
            self.ltr = false;
        } else {
            if delta < gap {
                self.text.push(' ');
            }
            self.text.push_str(&other.text);
            self.ltr = true;
        }
        // Extend the right edge to `other`.
        self.rx1 = other.rx1;
        self.ry1 = other.ry1;
        self.rx2 = other.rx2;
        self.ry2 = other.ry2;
    }
}

/// `applicable_for_merge`: both active, same font (ligatures bridge fonts), and
/// same reading orientation.
fn applicable(a: &Cell, b: &Cell) -> bool {
    if !a.active || !b.active {
        return false;
    }
    // font 0 = unknown/space (font-neutral); ligatures bridge fonts too.
    if a.font != 0
        && b.font != 0
        && a.font != b.font
        && !is_ligature(&a.text)
        && !is_ligature(&b.text)
    {
        return false;
    }
    a.same_orientation(b)
}

/// Left-to-right pass: `i` ascending accumulates cells to its right.
fn pass_ltr(cells: &mut [Cell], allow_reverse: bool, euclidean: bool) {
    for i in 0..cells.len() {
        if !cells[i].active {
            continue;
        }
        let mut j = i + 1;
        while j < cells.len() {
            if !applicable(&cells[i], &cells[j]) {
                break;
            }
            let i_lig = is_ligature(&cells[i].text) || cells[i].lig_carry;
            let j_lig = is_ligature(&cells[j].text) || cells[j].lig_carry;
            let d0 = cells[i].avg_char_width() * MERGE;
            let d1 = cells[i].avg_char_width() * MERGE_WITH_SPACE;
            let adj_d1 = d0 + if i_lig || j_lig { H_TOL } else { 0.0 };
            if cells[i].adjacent(&cells[j], d0, adj_d1) {
                let other = cells[j].clone();
                cells[i].merge_with(&other, d1, euclidean);
                cells[i].lig_carry = is_ligature(&other.text);
                cells[j].active = false;
                j += 1; // i keeps absorbing the next cell to its right
            } else if allow_reverse && cells[j].adjacent(&cells[i], d0, adj_d1) {
                let other = cells[i].clone();
                cells[j].merge_with(&other, d1, euclidean);
                cells[j].lig_carry = is_ligature(&other.text);
                cells[i].active = false;
                break; // i is consumed
            } else {
                break;
            }
        }
    }
}

/// Right-to-left pass: `i` descending; its immediate left neighbour `i-1`
/// absorbs it (then the outer loop continues leftward through the absorber).
fn pass_rtl(cells: &mut [Cell], euclidean: bool) {
    let n = cells.len();
    for k in 0..n {
        let i = n - 1 - k;
        if !cells[i].active || i == 0 {
            continue;
        }
        let j = i - 1;
        if !applicable(&cells[i], &cells[j]) {
            continue;
        }
        let i_lig = is_ligature(&cells[i].text) || cells[i].lig_carry;
        let j_lig = is_ligature(&cells[j].text) || cells[j].lig_carry;
        let d0 = cells[i].avg_char_width() * MERGE;
        let d1 = cells[i].avg_char_width() * MERGE_WITH_SPACE;
        let adj_d1 = d0 + if i_lig || j_lig { H_TOL } else { 0.0 };
        if cells[j].adjacent(&cells[i], d0, adj_d1) {
            let other = cells[i].clone();
            cells[j].merge_with(&other, d1, euclidean);
            cells[j].lig_carry = is_ligature(&other.text);
            cells[i].active = false;
        }
    }
}

fn contract(cells: &mut Vec<Cell>, euclidean: bool) {
    pass_ltr(cells, false, euclidean);
    cells.retain(|c| c.active);
    pass_rtl(cells, euclidean);
    cells.retain(|c| c.active);
    pass_ltr(cells, true, euclidean);
    cells.retain(|c| c.active);
}

/// Build line cells from a page's glyph stream via the docling-parse contraction.
pub(crate) fn line_cells(glyphs: &[Glyph], page_h: f32, euclidean: bool) -> Vec<TextCell> {
    let mut cells: Vec<Cell> = Vec::new();
    for g in glyphs {
        // Use the loose box (uniform font ascent/descent + advance) so adjacent
        // glyphs share a top edge, matching docling-parse's `compute_rect`.
        if !g.ll.is_finite() {
            continue;
        }
        // Drop *degenerate* space glyphs (zero-width loose box): pdfium's generated
        // spaces get a zero-width box at the wrong baseline that breaks the
        // corner-distance adjacency. Without them the inter-word gap drives
        // `merge_with`'s space insertion. Spaces with a real width are kept (they
        // carry justified double-space information).
        if g.ch == ' ' && (g.lr - g.ll).abs() < 0.5 {
            continue;
        }
        // Recompose a ligature: pdfium decomposes one font glyph (Latin fi/ffi,
        // Arabic lam-alef) into several chars at the *same* loose box. Append them
        // into one cell so the contraction never inserts a space inside it.
        if let Some(last) = cells.last_mut() {
            if (last.rx0 - g.ll as f64).abs() < 0.5 && (last.rx1 - g.lr as f64).abs() < 0.5 {
                last.text.push(g.ch);
                last.ltr = !is_right_to_left(&last.text);
                continue;
            }
        }
        let text = g.ch.to_string();
        let ltr = !is_right_to_left(&text);
        cells.push(Cell {
            text,
            rx0: g.ll as f64,
            ry0: g.lb as f64,
            rx1: g.lr as f64,
            ry1: g.lb as f64,
            rx2: g.lr as f64,
            ry2: g.lt as f64,
            rx3: g.ll as f64,
            ry3: g.lt as f64,
            ltr,
            active: true,
            lig_carry: false,
            font: g.font,
        });
    }
    contract(&mut cells, euclidean);
    cells
        .into_iter()
        .map(|c| {
            let l = c.rx0.min(c.rx1).min(c.rx2).min(c.rx3) as f32;
            let r = c.rx0.max(c.rx1).max(c.rx2).max(c.rx3) as f32;
            let top = c.ry0.max(c.ry1).max(c.ry2).max(c.ry3) as f32;
            let bot = c.ry0.min(c.ry1).min(c.ry2).min(c.ry3) as f32;
            TextCell {
                text: c.text,
                l,
                t: page_h - top,
                r,
                b: page_h - bot,
            }
        })
        .collect()
}

fn is_rtl_char(c: char) -> bool {
    let ch = c as u32;
    (0x0600..=0x06FF).contains(&ch)
        || (0x0750..=0x077F).contains(&ch)
        || (0x08A0..=0x08FF).contains(&ch)
        || (0xFB50..=0xFDFF).contains(&ch)
        || (0xFE70..=0xFEFF).contains(&ch)
        || (0x0590..=0x05FF).contains(&ch)
        || (0xFB1D..=0xFB4F).contains(&ch)
        || (0x0700..=0x074F).contains(&ch)
        || (0x0780..=0x07BF).contains(&ch)
        || (0x07C0..=0x07FF).contains(&ch)
}

/// All codepoints are RTL-script (matches `string.h::is_right_to_left`).
fn is_right_to_left(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_rtl_char)
}

/// A single-codepoint punctuation/space cell (matches `string.h`).
fn is_punct_or_space(s: &str) -> bool {
    let mut chars = s.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return false;
    };
    if matches!(
        c,
        ' ' | '\t'
            | '\n'
            | '\r'
            | '\u{0c}'
            | '\u{0b}'
            | '.'
            | ','
            | ';'
            | ':'
            | '!'
            | '?'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '\''
            | '"'
            | '`'
            | '\u{2018}'
            | '\u{2019}'
            | '\u{201c}'
            | '\u{201d}'
            | '-'
            | '\u{2013}'
            | '\u{2014}'
            | '_'
            | '/'
            | '\\'
            | '|'
            | '@'
            | '#'
            | '%'
            | '&'
            | '*'
            | '+'
            | '='
            | '<'
            | '>'
    ) {
        return true;
    }
    let ch = c as u32;
    (0x2000..=0x206F).contains(&ch)
        || (0x3000..=0x303F).contains(&ch)
        || (0xFE50..=0xFE6F).contains(&ch)
        || (0xFF00..=0xFF0F).contains(&ch)
        || (0xFF1A..=0xFF1F).contains(&ch)
        || (0xFF3B..=0xFF5E).contains(&ch)
}

/// Ligature glyph or its ASCII spelling (matches `string.h::is_ligature`).
fn is_ligature(s: &str) -> bool {
    matches!(s, "ff" | "fi" | "fl" | "ffi" | "ffl")
        || s.chars().any(|c| (0xFB00..=0xFB06).contains(&(c as u32)))
}
