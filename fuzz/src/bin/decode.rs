use arbitrary::{Arbitrary, Unstructured};

#[derive(Debug, Arbitrary)]
struct Input {
    page_size_choice: u8,
    eytzinger: bool,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
}

fn main() {
    // Flags are skipped so that running this through `cargo fuzz run`, which passes
    // libFuzzer's own arguments, prints usage rather than looking for a file called
    // `-artifact_prefix=...`. This is a helper for reading artifacts, not a fuzz target.
    let paths: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    if paths.is_empty() {
        eprintln!("usage: decode <artifact>...");
        return;
    }
    for path in paths {
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) => {
                println!("{path}: cannot read: {e}");
                continue;
            }
        };
        let u = Unstructured::new(&data);
        match Input::arbitrary_take_rest(u) {
            Ok(input) => {
                let page_size = match input.page_size_choice % 4 {
                    0 => 128,
                    1 => 512,
                    2 => 4096,
                    _ => 40_000,
                };
                println!(
                    "{path}: page_size={page_size} eytzinger={} rows={}",
                    input.eytzinger,
                    input.rows.len()
                );
                for (i, (k, v)) in input.rows.iter().enumerate().take(12) {
                    println!("  row {i}: key {} bytes, value {} bytes", k.len(), v.len());
                }
                let total_k: usize = input.rows.iter().map(|(k, _)| k.len()).sum();
                let total_v: usize = input.rows.iter().map(|(_, v)| v.len()).sum();
                println!("  totals: keys {total_k} bytes, values {total_v} bytes");
            }
            Err(e) => println!("{path}: cannot parse: {e}"),
        }
    }
}
