//! Dump pdfium's raw char stream (codepoint + x + font hash) for a page.
//! Usage: `... --example dump_chars -- file.pdf`
fn main() {
    let path = std::env::args().nth(1).expect("usage: dump_chars <pdf>");
    let bytes = std::fs::read(&path).expect("read");
    let glyphs = fleischwolf_pdf::pdfium_backend::debug_glyphs(&bytes, 0);
    println!("pdfium CHAR order (ch / x / font-hash):");
    for (ch, l, font) in glyphs.iter().take(40) {
        println!(
            "  {:?} U+{:04X}  xl={:.1}  font={}",
            ch, *ch as u32, l, font
        );
    }
}
