//! Self-capture for documentation.
//!
//! Wayland gives no way to screenshot a window from outside without a
//! portal prompt, so Giverny photographs its own framebuffer instead: exact
//! window contents, no compositor involvement, repeatable in CI. Enabled
//! only by `GIVERNY_CAPTURE=<dir>[:<frames>[:<every_n_frames>]]`, so it is
//! invisible in normal use.
//!
//! Frames are written as binary PPM — a 15-line format with no dependency —
//! and converted to PNG/GIF by `tools/capture/to_assets.py`.

use std::path::PathBuf;

pub struct Capture {
    dir: PathBuf,
    /// How many frames still to grab.
    remaining: u32,
    /// Grab every Nth painted frame (spreads a GIF over real animation).
    stride: u32,
    tick: u32,
    saved: u32,
    /// Skip the first frames so fonts and the first PTY output settle.
    warmup: u32,
}

impl Capture {
    /// `None` unless `GIVERNY_CAPTURE` is set.
    pub fn from_env() -> Option<Self> {
        let spec = std::env::var("GIVERNY_CAPTURE").ok()?;
        let mut parts = spec.split(':');
        let dir = PathBuf::from(parts.next()?);
        let remaining = parts.next().and_then(|f| f.parse().ok()).unwrap_or(1);
        let stride = parts
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6)
            .max(1);
        std::fs::create_dir_all(&dir).ok()?;
        Some(Capture {
            dir,
            remaining,
            stride,
            tick: 0,
            saved: 0,
            warmup: 90,
        })
    }

    pub fn done(&self) -> bool {
        self.remaining == 0
    }

    /// Called once per frame: asks for a screenshot when due, and writes any
    /// screenshot the previous request produced.
    pub fn on_frame(&mut self, ctx: &egui::Context) {
        // Collect whatever the last request produced.
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = shot {
            let path = self.dir.join(format!("frame-{:03}.ppm", self.saved));
            match write_ppm(&path, &image) {
                Ok(()) => tracing::info!("captured {}", path.display()),
                Err(err) => tracing::error!("capture failed: {err}"),
            }
            self.saved += 1;
            self.remaining = self.remaining.saturating_sub(1);
            if self.remaining == 0 {
                tracing::info!("capture complete: {} frame(s)", self.saved);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }

        self.tick += 1;
        if self.warmup > 0 {
            self.warmup -= 1;
            ctx.request_repaint();
            return;
        }
        if self.tick.is_multiple_of(self.stride) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        // Keep frames coming even when nothing else asks for a repaint.
        ctx.request_repaint();
    }
}

/// Binary PPM (P6). Chosen so capture needs no image-encoding dependency.
fn write_ppm(path: &std::path::Path, image: &egui::ColorImage) -> std::io::Result<()> {
    use std::io::Write;
    let [w, h] = image.size;
    let mut out = Vec::with_capacity(w * h * 3 + 32);
    out.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    for px in &image.pixels {
        out.extend_from_slice(&[px.r(), px.g(), px.b()]);
    }
    let mut f = std::fs::File::create(path)?;
    f.write_all(&out)
}
