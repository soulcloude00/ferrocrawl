//! `cargo run --release --example pdfbench -- file.pdf` → time per conversion.
fn main() {
    let path = std::env::args().nth(1).expect("pdf path");
    let bytes = std::fs::read(&path).unwrap();
    println!("engine {:?}, pages {:?}", *ferrocrawl::pdf::ENGINE, ferrocrawl::pdf::page_count(&bytes));
    let mut times = Vec::new();
    let mut chars = 0;
    for _ in 0..7 {
        let t = std::time::Instant::now();
        chars = ferrocrawl::pdf::to_markdown(&bytes).unwrap().len();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    println!("median {:.1} ms (min {:.1}) · {chars} chars", times[3], times[0]);
}
