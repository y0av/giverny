//! Headless CPU and allocation-size probe, NOT an end-to-end GPU benchmark.
//! Run: cargo run --release -p giverny-term --example render_cost
//! Measures warm full-screen capture/build and font-size atlas retention.
use std::{hint::black_box, mem::size_of, time::Instant};

use alacritty_terminal::{
    event::{Event, EventListener},
    term::{Config, Term, cell::Cell, test::TermSize},
    vte::ansi::Processor,
};
use giverny_term::render::{
    atlas::{Atlas, PAGE_SIZE},
    mesh::{self, BuildParams, Snapshot},
    metrics::{CellMetrics, FontSet},
    theme::Theme,
};

#[derive(Clone)]
struct Noop;
impl EventListener for Noop {
    fn send_event(&self, _: Event) {}
}

fn main() -> anyhow::Result<()> {
    let ctx = egui::Context::default();
    let fonts = FontSet::load(None)?;
    let theme = Theme::monet_dark();
    let mut atlas = Atlas::default();
    println!(
        "Cell: {} bytes; filled 10k-row history at 160 columns: {:.2} MiB of cells (excludes overhead)",
        size_of::<Cell>(),
        10_000.0 * 160.0 * size_of::<Cell>() as f64 / 1048576.0
    );
    for (label, text) in [
        ("ASCII", "let result = process_terminal_output(input); "),
        ("mixed RTL", "שלום עולם terminal output 123 "),
    ] {
        let (cols, rows) = (160, 50);
        let mut term = Term::new(Config::default(), &TermSize::new(cols, rows), Noop);
        let mut parser: Processor = Processor::new();
        let line: String = text.chars().cycle().take(cols - 1).collect();
        let content = std::iter::repeat_n(line, rows)
            .collect::<Vec<_>>()
            .join("\r\n");
        parser.advance(&mut term, content.as_bytes());
        let metrics = CellMetrics::compute(fonts.primary().as_font().unwrap(), 16.0);
        let mut params = BuildParams {
            ctx: &ctx,
            fonts: &fonts,
            atlas: &mut atlas,
            metrics,
            theme: &theme,
            origin_px: egui::Vec2::ZERO,
            pixels_per_point: 1.0,
            cursor_visible: true,
            cursor_hollow: false,
        };
        let snapshot = Snapshot::capture(&term, &theme);
        let meshes = mesh::build(&snapshot, &mut params);
        let bytes: usize = std::iter::once(&meshes.bg)
            .chain(&meshes.glyphs)
            .chain(std::iter::once(&meshes.decor))
            .map(|m| {
                m.vertices.len() * size_of::<egui::epaint::Vertex>()
                    + m.indices.len() * size_of::<u32>()
            })
            .sum();
        // Drain uploads from warm-up, just as a renderer consumes deltas.
        drop(ctx.tex_manager().write().take_delta());
        let start = Instant::now();
        for _ in 0..500 {
            black_box(Snapshot::capture(&term, &theme));
        }
        let capture_us = start.elapsed().as_secs_f64() * 1e6 / 500.0;
        let start = Instant::now();
        for i in 0..500 {
            params.cursor_visible = i % 2 == 0;
            black_box(mesh::build(&snapshot, &mut params));
        }
        println!(
            "{label} {cols}x{rows}: capture {capture_us:.1} us, warm build {:.1} us; mesh payload {:.2} MiB",
            start.elapsed().as_secs_f64() * 1e6 / 500.0,
            bytes as f64 / 1048576.0
        );
        if label == "ASCII" {
            for px in 7..=32 {
                params.metrics =
                    CellMetrics::compute(fonts.primary().as_font().unwrap(), px as f32);
                black_box(mesh::build(&snapshot, &mut params));
                drop(ctx.tex_manager().write().take_delta());
            }
            println!(
                "atlas after sizes 7..32: {} pages, {:.1} MiB RGBA texture payload (no GPU allocated in this probe)",
                atlas.page_count(),
                (atlas.page_count() * PAGE_SIZE * PAGE_SIZE * 4) as f64 / 1048576.0
            );
        }
    }
    Ok(())
}
