//! Giverny — a native terminal built around Claude Code.

use eframe::egui;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "giverny=info,warn".into()),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("giverny")
            .with_title("Giverny")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native("Giverny", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

struct App {}

impl App {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {}
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::left("rail")
            .resizable(true)
            .default_size(240.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.label("rail placeholder");
            });
        egui::CentralPanel::default().show(ui, |ui| {
            ui.centered_and_justified(|ui| {
                ui.label("giverny — terminal coming up");
            });
        });
    }
}
