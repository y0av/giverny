//! Regenerate `docs/options.md`: `cargo run -p giverny-core --example options`
fn main() {
    let out = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/options.md");
    std::fs::write(out, giverny_core::settings::markdown()).expect("write docs/options.md");
    println!("wrote {out}");
}
