//! Re-extracts a saved page and shows what each mode keeps, scored against a truth snippet:
//! `cargo run --release -p ferrocrawl-core --example explain -- <run dir> <index>`.
use ferrocrawl::extract::extract;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (dir, i) = (std::path::Path::new(&a[1]), a[2].parse::<usize>().unwrap());
    let cases: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(dir.join("cases.json")).unwrap()).unwrap();
    let raw = std::fs::read_to_string(dir.join("raw").join(format!("{i:05}.html"))).unwrap();
    let url = cases[i]["url"].as_str().unwrap();
    let truth = cases[i]["truth"].as_str().unwrap_or("");
    for main in [true, false] {
        let ex = extract(&raw, url, main, &[]);
        let s = ferrocrawl::bench::score_overlap(&ex.markdown, truth, "");
        println!("==== only_main={main} recall={:.3} chars={}", s.recall, ex.markdown.len());
        let n = a.get(3).and_then(|n| n.parse().ok()).unwrap_or(1500);
        println!("{}", ex.markdown.chars().take(n).collect::<String>());
    }
}
