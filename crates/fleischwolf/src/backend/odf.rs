//! OpenDocument backend (`.odt`/`.ods`/`.odp`) — a port of docling's
//! `OpenDocument*` backends. ODF is a ZIP whose `content.xml` holds the body;
//! `styles.xml` plus `content.xml`'s automatic styles define text/paragraph/list
//! styles. Paragraph styles map to Title/Subtitle/Heading; `<text:h>` maps to a
//! heading by outline level; runs (`<text:span>`) carry bold/italic/strike/sub-
//! superscript resolved through the style parent chain; lists nest by depth.

use std::collections::{HashMap, HashSet, VecDeque};

use roxmltree::{Document, Node as XmlNode};

use crate::backend::markdown::escape_text;
use crate::backend::ooxml::Package;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use fleischwolf_core::{DoclingDocument, Node, Table};

pub struct OdfBackend;

#[derive(Default, Clone, Copy, PartialEq)]
struct Fmt {
    bold: bool,
    italic: bool,
    strike: bool,
    underline: bool,
    script: u8, // 0 none, 1 sub, 2 super
}

#[derive(Default, Clone)]
struct StyleInfo {
    parent: Option<String>,
    display_name: Option<String>,
    bold: Option<bool>,
    italic: Option<bool>,
    strike: Option<bool>,
    underline: Option<bool>,
    script: Option<u8>,
}

/// One list level's rendering: bullet vs numbered, and its `start-value`.
#[derive(Default, Clone, Copy)]
struct OdfLevel {
    numbered: bool,
    start: i64,
}

/// List style name → level (1-based) → level rendering.
type ListStyles = HashMap<String, HashMap<i64, OdfLevel>>;

struct Styles {
    map: HashMap<String, StyleInfo>,
    lists: ListStyles,
}

impl DeclarativeBackend for OdfBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let mut pkg = Package::open(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("odf: not a zip".into()))?;
        let content = pkg
            .read("content.xml")
            .ok_or_else(|| ConversionError::Parse("odf: no content.xml".into()))?;
        let styles_xml = pkg.read("styles.xml").unwrap_or_default();

        let content_dom =
            Document::parse(&content).map_err(|e| ConversionError::Parse(format!("odf: {e}")))?;
        let styles_dom = Document::parse(&styles_xml).ok();
        let styles = parse_styles(&content_dom, styles_dom.as_ref());

        let mut doc = DoclingDocument::new(&source.name);
        let Some(body) = content_dom.descendants().find(|n| n.has_tag_name("body")) else {
            return Ok(doc);
        };
        for office in body.children().filter(XmlNode::is_element) {
            match office.tag_name().name() {
                "text" => walk_text(office, &styles, &mut doc),
                "spreadsheet" => walk_spreadsheet(office, &styles, &mut doc),
                "presentation" => walk_presentation(office, &styles, &mut doc),
                _ => {}
            }
        }
        Ok(doc)
    }
}

// ---------------------------------------------------------------- styles

fn parse_styles(content: &Document, styles: Option<&Document>) -> Styles {
    let mut map = HashMap::new();
    let mut lists = HashMap::new();
    for dom in [Some(content), styles].into_iter().flatten() {
        for s in dom.descendants() {
            match s.tag_name().name() {
                "style" => {
                    if let Some(name) = attr(s, "name") {
                        map.insert(name.to_string(), style_info(s));
                    }
                }
                "list-style" => {
                    if let Some(name) = attr(s, "name") {
                        let mut levels = HashMap::new();
                        for lv in s.children().filter(XmlNode::is_element) {
                            let level: i64 =
                                attr(lv, "level").and_then(|v| v.parse().ok()).unwrap_or(1);
                            let numbered = lv.tag_name().name() == "list-level-style-number";
                            let start = attr(lv, "start-value")
                                .and_then(|v| v.parse().ok())
                                .map(|n: i64| n.max(1))
                                .unwrap_or(1);
                            levels.insert(level, OdfLevel { numbered, start });
                        }
                        lists.insert(name.to_string(), levels);
                    }
                }
                _ => {}
            }
        }
    }
    Styles { map, lists }
}

fn style_info(s: XmlNode) -> StyleInfo {
    let mut info = StyleInfo {
        parent: attr(s, "parent-style-name").map(str::to_string),
        display_name: attr(s, "display-name").map(str::to_string),
        ..Default::default()
    };
    if let Some(tp) = s.children().find(|c| c.has_tag_name("text-properties")) {
        info.bold = attr(tp, "font-weight").map(is_bold);
        info.italic = attr(tp, "font-style").map(|v| v == "italic" || v == "oblique");
        info.strike = attr(tp, "text-line-through-style").map(|v| v != "none");
        info.underline = attr(tp, "text-underline-style").map(|v| v != "none");
        info.script = attr(tp, "text-position").map(|v| {
            if v.starts_with("super") {
                2
            } else if v.starts_with("sub") {
                1
            } else {
                0
            }
        });
    }
    info
}

fn is_bold(v: &str) -> bool {
    v == "bold" || v.parse::<i32>().map(|n| n >= 600).unwrap_or(false)
}

/// Resolve a text/paragraph style's formatting through its parent chain.
fn resolve_fmt(styles: &Styles, name: Option<&str>, base: Fmt) -> Fmt {
    let mut fmt = base;
    let mut chain = Vec::new();
    let mut cur = name.map(str::to_string);
    let mut seen = std::collections::HashSet::new();
    while let Some(n) = cur {
        if !seen.insert(n.clone()) {
            break;
        }
        if let Some(info) = styles.map.get(&n) {
            chain.push(info.clone());
            cur = info.parent.clone();
        } else {
            break;
        }
    }
    // Apply parent-first so the most-derived style wins.
    for info in chain.into_iter().rev() {
        if let Some(b) = info.bold {
            fmt.bold = b;
        }
        if let Some(i) = info.italic {
            fmt.italic = i;
        }
        if let Some(s) = info.strike {
            fmt.strike = s;
        }
        if let Some(u) = info.underline {
            fmt.underline = u;
        }
        if let Some(sc) = info.script {
            fmt.script = sc;
        }
    }
    fmt
}

/// The set of style names a paragraph resolves to (own, parent, display).
fn paragraph_style_names(styles: &Styles, name: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(n) = name {
        out.push(n.to_string());
        if let Some(info) = styles.map.get(n) {
            if let Some(p) = &info.parent {
                out.push(p.clone());
            }
            if let Some(d) = &info.display_name {
                out.push(d.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------- text runs

/// One formatted run of text.
struct Run {
    text: String,
    fmt: Fmt,
}

/// Collect runs from a paragraph/heading element (recursing spans).
fn collect_runs(el: XmlNode, styles: &Styles, base: Fmt, out: &mut Vec<Run>) {
    for child in el.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                out.push(Run {
                    text: t.to_string(),
                    fmt: base,
                });
            }
        } else if child.is_element() {
            match child.tag_name().name() {
                "span" => {
                    let fmt = resolve_fmt(styles, attr(child, "style-name"), base);
                    collect_runs(child, styles, fmt, out);
                }
                "line-break" => out.push(Run {
                    text: "\n".into(),
                    fmt: base,
                }),
                "tab" => out.push(Run {
                    text: "\t".into(),
                    fmt: base,
                }),
                "s" => {
                    // <text:s text:c="n"> = n spaces (default 1)
                    let n: usize = attr(child, "c").and_then(|v| v.parse().ok()).unwrap_or(1);
                    out.push(Run {
                        text: " ".repeat(n),
                        fmt: base,
                    });
                }
                "a" | "ruby" | "ruby-base" => collect_runs(child, styles, base, out),
                _ => {}
            }
        }
    }
}

/// Merge adjacent same-format runs, serialize each (markers), join with spaces —
/// docling-core's inline-group serialization (un-stripped, so spaces double up).
fn runs_to_text(mut runs: Vec<Run>) -> String {
    // merge adjacent same-fmt
    let mut merged: Vec<Run> = Vec::new();
    for r in runs.drain(..) {
        if let Some(last) = merged.last_mut() {
            if last.fmt == r.fmt {
                last.text.push_str(&r.text);
                continue;
            }
        }
        merged.push(r);
    }
    merged
        .iter()
        .map(|r| serialize_run(&r.text, r.fmt))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end()
        .to_string()
}

fn serialize_run(text: &str, fmt: Fmt) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut s = escape_text(text);
    if fmt.bold {
        s = format!("**{s}**");
    }
    if fmt.italic {
        s = format!("*{s}*");
    }
    if fmt.strike {
        s = format!("~~{s}~~");
    }
    s
}

// ---------------------------------------------------------------- text doc

fn walk_text(text: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    walk_blocks(text.children().filter(XmlNode::is_element), styles, doc);
}

/// Walk a run of sibling blocks, threading list-continuation state. Numbering
/// continues across consecutive `<text:list>` siblings when the next list opens
/// with an empty nested item (docling's `_OdfListState`); any non-list block
/// resets the continuation.
fn walk_blocks<'a, 'i: 'a>(
    els: impl Iterator<Item = XmlNode<'a, 'i>>,
    styles: &Styles,
    doc: &mut DoclingDocument,
) {
    let mut prev_state: Option<ListCont> = None;
    for el in els {
        if el.tag_name().name() == "list" {
            prev_state = add_odf_list(el, styles, doc, 0, 1, false, prev_state.take());
        } else {
            prev_state = None;
            handle_block(el, styles, doc, 0, &mut Vec::new());
        }
    }
}

fn handle_block(
    el: XmlNode,
    styles: &Styles,
    doc: &mut DoclingDocument,
    list_level: u8,
    counters: &mut Vec<u64>,
) {
    let _ = counters;
    match el.tag_name().name() {
        "h" => {
            let level = attr(el, "outline-level")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(1)
                .max(1);
            let mut runs = Vec::new();
            collect_runs(el, styles, Fmt::default(), &mut runs);
            let text = runs_to_text(runs);
            if !text.is_empty() {
                doc.push(Node::Heading {
                    level: (level + 1) as u8,
                    text,
                });
            }
        }
        "p" => {
            let names = paragraph_style_names(styles, attr(el, "style-name"));
            let mut runs = Vec::new();
            collect_runs(el, styles, Fmt::default(), &mut runs);
            let text = runs_to_text(runs);
            if text.is_empty() {
                return;
            }
            if names.iter().any(|n| n == "Title") {
                doc.push(Node::Heading { level: 1, text });
            } else if names.iter().any(|n| n == "Subtitle") {
                doc.push(Node::Heading { level: 2, text });
            } else {
                doc.push(Node::Paragraph { text });
            }
        }
        "list" => {
            add_odf_list(el, styles, doc, list_level, 1, false, None);
        }
        "table" => {
            if let Some(table) = parse_table(el, styles) {
                doc.push(Node::Table(table));
            }
        }
        _ => {}
    }
}

/// Continuation state carried across sibling `<text:list>` elements — docling's
/// `_OdfListState`.
#[derive(Clone, Copy)]
struct ListCont {
    enumerated: bool,
    counter: i64,
    has_last: bool,
}

/// A list's item elements (`<text:list-item>` / `<text:list-header>`).
fn list_items<'a, 'i>(list: XmlNode<'a, 'i>) -> impl Iterator<Item = XmlNode<'a, 'i>> {
    list.children()
        .filter(|c| c.has_tag_name("list-item") || c.has_tag_name("list-header"))
}

/// An item's rendered text (its direct paragraphs' runs, cleaned to single lines)
/// and its directly-nested `<text:list>` elements. Mirrors docling's
/// `_odf_list_item_content` with `flatten_nested_text=False`.
fn odf_item_content<'a, 'i>(
    item: XmlNode<'a, 'i>,
    styles: &Styles,
) -> (String, Vec<XmlNode<'a, 'i>>) {
    let mut parts: Vec<String> = Vec::new();
    let mut nested: Vec<XmlNode> = Vec::new();
    for child in item.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "list" => nested.push(child),
            "p" | "h" => {
                let mut runs = Vec::new();
                collect_runs(child, styles, Fmt::default(), &mut runs);
                let text = clean_lines(&runs_to_text(runs));
                if !text.is_empty() {
                    parts.push(text);
                }
            }
            _ => {}
        }
    }
    (parts.join(" "), nested)
}

/// Split on newlines, strip each line, drop the blanks, re-join with spaces —
/// docling's `_clean_odf_text_lines` joined.
fn clean_lines(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a list renders anything (any item with text, or a renderable nested list).
fn list_has_renderable(list: XmlNode, styles: &Styles) -> bool {
    list_items(list).any(|item| {
        let (text, nested) = odf_item_content(item, styles);
        !text.is_empty() || nested.iter().any(|n| list_has_renderable(*n, styles))
    })
}

/// Whether any item carries direct text (vs. only nested lists).
fn list_has_direct_text(list: XmlNode, styles: &Styles) -> bool {
    list_items(list).any(|item| !odf_item_content(item, styles).0.is_empty())
}

/// Whether the first item is empty but wraps a renderable nested list — the
/// signal that this list continues the previous one's numbering.
fn list_starts_with_empty_nested(list: XmlNode, styles: &Styles) -> bool {
    if let Some(item) = list_items(list).next() {
        let (text, nested) = odf_item_content(item, styles);
        return text.is_empty() && nested.iter().any(|n| list_has_renderable(*n, styles));
    }
    false
}

/// A list level's rendering (bullet vs numbered) from the list's own style, else
/// the inherited `fallback` — docling's `_odf_list_level_is_enumerated`.
fn level_is_enumerated(styles: &Styles, list: XmlNode, level: i64, fallback: bool) -> bool {
    attr(list, "style-name")
        .and_then(|name| styles.lists.get(name))
        .and_then(|levels| levels.get(&level))
        .map(|lv| lv.numbered)
        .unwrap_or(fallback)
}

/// A list level's `start-value` (default 1).
fn level_start(styles: &Styles, list: XmlNode, level: i64) -> i64 {
    attr(list, "style-name")
        .and_then(|name| styles.lists.get(name))
        .and_then(|levels| levels.get(&level))
        .map(|lv| lv.start)
        .unwrap_or(1)
}

/// Emit an ODF list as flat [`Node::ListItem`]s — a port of docling's
/// `_add_odf_list`. `depth` is the Markdown nesting level for items of this list;
/// `style_level` (1-based) drives style lookups; empty items collapse (their
/// nested list attaches to the previous item) and a list that opens with an empty
/// nested item continues the previous list's numbering.
fn add_odf_list(
    list: XmlNode,
    styles: &Styles,
    doc: &mut DoclingDocument,
    depth: u8,
    style_level: i64,
    enumerated_fallback: bool,
    continued: Option<ListCont>,
) -> Option<ListCont> {
    if !list_has_renderable(list, styles) {
        return None;
    }
    let style_enum = level_is_enumerated(styles, list, style_level, enumerated_fallback);
    let should_continue = continued.map(|c| c.has_last).unwrap_or(false)
        && list_starts_with_empty_nested(list, styles);

    // A list with no direct text of its own (and not continuing) is transparent:
    // its items' nested lists take its place at the same depth.
    if !should_continue && !list_has_direct_text(list, styles) {
        for item in list_items(list) {
            let (_text, nested) = odf_item_content(item, styles);
            for n in nested {
                add_odf_list(n, styles, doc, depth, style_level + 1, style_enum, None);
            }
        }
        return None;
    }

    let (mut counter, current_enum) = match (should_continue, continued) {
        (true, Some(c)) => (c.counter, c.enumerated),
        _ => (level_start(styles, list, style_level) - 1, style_enum),
    };
    let mut has_last = should_continue;

    for item in list_items(list) {
        let (text, nested) = odf_item_content(item, styles);
        let nested: Vec<XmlNode> = nested
            .into_iter()
            .filter(|n| list_has_renderable(*n, styles))
            .collect();
        if text.is_empty() && nested.is_empty() {
            continue;
        }
        if text.is_empty() {
            // Empty item: its nested list collapses under the previous item.
            for n in &nested {
                add_odf_list(
                    *n,
                    styles,
                    doc,
                    depth + 1,
                    style_level + 1,
                    style_enum,
                    None,
                );
            }
            continue;
        }
        counter += 1;
        let (ordered, number) = if current_enum {
            (true, counter.max(0) as u64)
        } else {
            (false, 0)
        };
        doc.push(Node::ListItem {
            ordered,
            number,
            first_in_list: false,
            text,
            level: depth,
        });
        has_last = true;
        for n in &nested {
            add_odf_list(
                *n,
                styles,
                doc,
                depth + 1,
                style_level + 1,
                style_enum,
                None,
            );
        }
    }

    Some(ListCont {
        enumerated: current_enum,
        counter,
        has_last,
    })
}

// ---------------------------------------------------------------- tables

fn parse_table(table: XmlNode, styles: &Styles) -> Option<Table> {
    let mut rows = Vec::new();
    for tr in table.descendants().filter(|n| n.has_tag_name("table-row")) {
        let mut cells = Vec::new();
        for tc in tr.children().filter(|c| c.has_tag_name("table-cell")) {
            let repeat: usize = attr(tc, "number-columns-repeated")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let span: usize = attr(tc, "number-columns-spanned")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let text = cell_text(tc, styles);
            for _ in 0..(repeat.max(1) * span.max(1)) {
                cells.push(text.clone());
            }
        }
        // Trailing empty repeated cells inflate rows; trim trailing blanks.
        while cells.last().map(|c| c.is_empty()).unwrap_or(false) {
            cells.pop();
        }
        if !cells.is_empty() {
            rows.push(cells);
        }
    }
    if rows.is_empty() {
        return None;
    }
    Some(Table { rows })
}

fn cell_text(tc: XmlNode, styles: &Styles) -> String {
    let mut parts = Vec::new();
    for p in tc
        .children()
        .filter(|c| c.has_tag_name("p") || c.has_tag_name("h"))
    {
        let mut runs = Vec::new();
        collect_runs(p, styles, Fmt::default(), &mut runs);
        let t = runs_to_text(runs);
        if !t.is_empty() {
            parts.push(t);
        }
    }
    parts.join(" ")
}

// ---------------------------------------------------------------- spreadsheet

fn walk_spreadsheet(sheet: XmlNode, _styles: &Styles, doc: &mut DoclingDocument) {
    for table in sheet.children().filter(|c| c.has_tag_name("table")) {
        add_ods_sheet(table, doc);
    }
}

/// Split an ODS sheet into its disconnected data regions and emit each as a
/// separate table — a port of docling's `_convert_sheet_table` /
/// `_find_data_tables_in_sheet` (strict `gap_tolerance = 0` flood fill, singleton
/// cells kept as 1×1 tables). Numeric columns right-align via the shared table
/// serializer.
fn add_ods_sheet(table: XmlNode, doc: &mut DoclingDocument) {
    // Build a sparse content grid: (row, col) → cell text, expanding
    // `number-{rows,columns}-repeated` (empty repeats only advance the index, so a
    // sheet padded to millions of empty cells stays cheap).
    let mut cells: HashMap<(usize, usize), String> = HashMap::new();
    let mut row_idx = 0usize;
    for row in table.children().filter(|c| c.has_tag_name("table-row")) {
        let rrep = repeat(row, "number-rows-repeated");
        let mut row_cells: Vec<(usize, String)> = Vec::new();
        let mut col_idx = 0usize;
        let mut row_has_content = false;
        for cell in row
            .children()
            .filter(|c| c.has_tag_name("table-cell") || c.has_tag_name("covered-table-cell"))
        {
            let crep = repeat(cell, "number-columns-repeated");
            let covered = cell.has_tag_name("covered-table-cell");
            let text = ods_cell_text(cell);
            if !text.is_empty() || covered {
                row_has_content = true;
                for c in 0..crep.min(1024) {
                    row_cells.push((col_idx + c, text.clone()));
                }
            }
            col_idx += crep;
        }
        if row_has_content {
            for r in 0..rrep.min(1024) {
                for (c, text) in &row_cells {
                    cells.insert((row_idx + r, *c), text.clone());
                }
            }
        }
        row_idx += rrep;
    }
    if cells.is_empty() {
        return;
    }

    let min_row = cells.keys().map(|(r, _)| *r).min().unwrap();
    let max_row = cells.keys().map(|(r, _)| *r).max().unwrap();
    let min_col = cells.keys().map(|(_, c)| *c).min().unwrap();
    let max_col = cells.keys().map(|(_, c)| *c).max().unwrap();

    // Flood-fill connected content cells (4-directional, immediate neighbours
    // only) in row-major scan order, so region order matches docling's.
    let mut visited: HashSet<(usize, usize)> = HashSet::new();
    for ri in min_row..=max_row {
        for ci in min_col..=max_col {
            if visited.contains(&(ri, ci)) || !cells.contains_key(&(ri, ci)) {
                continue;
            }
            let mut region: HashSet<(usize, usize)> = HashSet::new();
            let mut queue: VecDeque<(usize, usize)> = VecDeque::new();
            queue.push_back((ri, ci));
            region.insert((ri, ci));
            let (mut rmin, mut rmax, mut cmin, mut cmax) = (ri, ri, ci, ci);
            while let Some((r, c)) = queue.pop_front() {
                rmin = rmin.min(r);
                rmax = rmax.max(r);
                cmin = cmin.min(c);
                cmax = cmax.max(c);
                for (dr, dc) in [(0i64, 1i64), (0, -1), (1, 0), (-1, 0)] {
                    let nr = r as i64 + dr;
                    let nc = c as i64 + dc;
                    if nr < 0 || nc < 0 {
                        continue;
                    }
                    let key = (nr as usize, nc as usize);
                    if !region.contains(&key) && cells.contains_key(&key) {
                        region.insert(key);
                        queue.push_back(key);
                    }
                }
            }
            visited.extend(region.iter().copied());

            let rows: Vec<Vec<String>> = (rmin..=rmax)
                .map(|r| {
                    (cmin..=cmax)
                        .map(|c| cells.get(&(r, c)).cloned().unwrap_or_default())
                        .collect()
                })
                .collect();
            doc.push(Node::Table(Table { rows }));
        }
    }
}

/// A `number-*-repeated` attribute, at least 1.
fn repeat(node: XmlNode, name: &str) -> usize {
    attr(node, name)
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n >= 1)
        .unwrap_or(1)
}

/// An ODS cell's plain text — its paragraphs' text, newline-joined (github tables
/// are unescaped, matching docling's `_odf_cell_text` display).
fn ods_cell_text(cell: XmlNode) -> String {
    cell.children()
        .filter(|c| c.has_tag_name("p") || c.has_tag_name("h"))
        .map(|p| {
            p.descendants()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect::<String>()
        })
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------- presentation

fn walk_presentation(pres: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    for page in pres.children().filter(|c| c.has_tag_name("page")) {
        for frame in page.descendants().filter(|n| n.has_tag_name("frame")) {
            for tb in frame.children().filter(|c| c.has_tag_name("text-box")) {
                walk_blocks(tb.children().filter(XmlNode::is_element), styles, doc);
            }
            for table in frame.children().filter(|c| c.has_tag_name("table")) {
                if let Some(t) = parse_table(table, styles) {
                    doc.push(Node::Table(t));
                }
            }
        }
    }
}

/// Attribute by local name (ODF attributes are namespaced, e.g. `text:style-name`).
fn attr<'a>(node: XmlNode<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_resolves_through_parent_chain() {
        // P2 → P1 → Strong (bold). T1 adds italic directly.
        let content = r#"<root xmlns:style="s" xmlns:fo="f">
            <style:style style:name="Strong" style:family="text">
              <style:text-properties fo:font-weight="bold"/></style:style>
            <style:style style:name="P1" style:family="text" style:parent-style-name="Strong"/>
            <style:style style:name="P2" style:family="text" style:parent-style-name="P1"/>
            <style:style style:name="T1" style:family="text">
              <style:text-properties fo:font-style="italic"/></style:style>
          </root>"#;
        let dom = Document::parse(content).unwrap();
        let styles = parse_styles(&dom, None);
        let f = resolve_fmt(&styles, Some("P2"), Fmt::default());
        assert!(f.bold && !f.italic, "bold inherited through P2→P1→Strong");
        let t = resolve_fmt(&styles, Some("T1"), Fmt::default());
        assert!(t.italic && !t.bold);
    }

    #[test]
    fn ods_sheet_splits_into_regions() {
        // A title cell (isolated by an empty row) and a 2×2 data block become two
        // separate tables (strict gap-tolerance flood fill).
        let xml = r#"<root xmlns:table="t" xmlns:text="x">
          <table:table>
            <table:table-row><table:table-cell/>
              <table:table-cell><text:p>Title</text:p></table:table-cell></table:table-row>
            <table:table-row><table:table-cell/></table:table-row>
            <table:table-row><table:table-cell/>
              <table:table-cell><text:p>H1</text:p></table:table-cell>
              <table:table-cell><text:p>H2</text:p></table:table-cell></table:table-row>
            <table:table-row><table:table-cell/>
              <table:table-cell><text:p>1</text:p></table:table-cell>
              <table:table-cell><text:p>2</text:p></table:table-cell></table:table-row>
          </table:table></root>"#;
        let dom = Document::parse(xml).unwrap();
        let table = dom.descendants().find(|n| n.has_tag_name("table")).unwrap();
        let mut doc = DoclingDocument::new("t");
        add_ods_sheet(table, &mut doc);
        let tables: Vec<&Table> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 2, "title singleton + data region");
        assert_eq!(tables[0].rows, vec![vec!["Title".to_string()]]);
        assert_eq!(tables[1].rows.len(), 2, "header + one data row");
        assert_eq!(tables[1].rows[0], vec!["H1".to_string(), "H2".to_string()]);
    }

    #[test]
    fn list_continues_across_empty_nested_item() {
        // A numbered `<text:list>` followed by a second list that opens with an
        // empty item wrapping a nested list continues the numbering (3.) while the
        // nested bullets collapse under the previous item (level 1).
        let xml = r#"<root xmlns:text="x" xmlns:style="s">
          <style:list-style style:name="L1">
            <text:list-level-style-number text:level="1"/></style:list-style>
          <style:list-style style:name="L2">
            <text:list-level-style-bullet text:level="1"/>
            <text:list-level-style-bullet text:level="2"/></style:list-style>
          <office:body xmlns:office="o"><office:text>
            <text:list text:style-name="L1">
              <text:list-item><text:p>one</text:p></text:list-item>
              <text:list-item><text:p>two</text:p></text:list-item>
            </text:list>
            <text:list text:style-name="L2">
              <text:list-item><text:list>
                <text:list-item><text:p>bullet</text:p></text:list-item>
              </text:list></text:list-item>
              <text:list-item><text:p>three</text:p></text:list-item>
            </text:list>
          </office:text></office:body></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let body = dom.descendants().find(|n| n.has_tag_name("text")).unwrap();
        let mut doc = DoclingDocument::new("t");
        walk_text(body, &styles, &mut doc);
        let items: Vec<(u64, u8, &str)> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::ListItem {
                    number, level, text, ..
                } => Some((*number, *level, text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            vec![
                (1, 0, "one"),
                (2, 0, "two"),
                (0, 1, "bullet"),
                (3, 0, "three"),
            ]
        );
    }
}
