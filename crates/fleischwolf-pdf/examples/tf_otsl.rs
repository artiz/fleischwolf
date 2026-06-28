//! Verify TableFormer inference: run it on every table region of a PDF and print
//! the predicted OTSL structure. Usage: `... --example tf_otsl -- file.pdf`

use fleischwolf_pdf::layout::LayoutModel;
use fleischwolf_pdf::tableformer::TableFormer;
use fleischwolf_pdf::PdfDocument;
use image::imageops;

fn name(t: i64) -> &'static str {
    match t {
        4 => "ecel",
        5 => "fcel",
        6 => "lcel",
        7 => "ucel",
        8 => "xcel",
        9 => "nl",
        10 => "ched",
        11 => "rhed",
        12 => "srow",
        _ => "?",
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: tf_otsl <pdf>");
    let bytes = std::fs::read(&path).expect("read");
    let doc = PdfDocument::open(&bytes, None).expect("open");
    let mut layout = LayoutModel::load().expect("layout");
    let mut tf = TableFormer::load().expect("tableformer models missing");
    for (pi, page) in doc.pages.iter().enumerate() {
        let regions = layout
            .predict(&page.image, page.width, page.height)
            .expect("layout");
        // docling resizes the whole page to 1024px height, then crops the table
        // bbox out of *that*. Replicate so the model sees the same pixels.
        let sf = 1024.0 / page.image.height() as f32;
        let pw1024 = (page.image.width() as f32 * sf).round() as u32;
        let page1024 = imageops::thumbnail(&page.image, pw1024, 1024);
        for r in regions.iter().filter(|r| r.label == "table") {
            // bbox (points) → 1024px-page coords: scale*sf = 1024/page_h_pt.
            let k = 1024.0 / page.height;
            let x = (r.l * k).max(0.0) as u32;
            let y = (r.t * k).max(0.0) as u32;
            let w = ((r.r - r.l) * k) as u32;
            let h = ((r.b - r.t) * k) as u32;
            let crop = imageops::crop_imm(&page1024, x, y, w, h).to_image();
            let otsl = tf.predict_otsl(&crop).expect("predict");
            let rows = otsl.iter().filter(|&&t| t == 9).count();
            let cols = otsl.iter().take_while(|&&t| t != 9).count();
            println!(
                "page {} table {}x{}px -> {} tokens, {} rows x {} cols",
                pi + 1,
                w,
                h,
                otsl.len(),
                rows,
                cols
            );
            println!(
                "  {}",
                otsl.iter().map(|&t| name(t)).collect::<Vec<_>>().join(" ")
            );
        }
    }
}
