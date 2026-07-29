//! Print the generated config.toml template: `cargo run -p giverny-core --example template`
fn main() {
    print!("{}", giverny_core::settings::template());
}
