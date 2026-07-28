//! Giverny — a native terminal built around Claude Code.

use eframe::egui;
use giverny_term::proxy::TabEvent;
use giverny_term::pty::{GridSize, SpawnCfg};
use giverny_term::render::theme::Theme;
use giverny_term::session::TermSession;
use giverny_term::widget::TermView;

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
    eframe::run_native(
        "Giverny",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

struct App {
    view: TermView,
    session: Option<TermSession>,
    title: String,
    exited: bool,
    frames: u32,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let theme = Theme::monet_dark();
        let view = TermView::new(theme.clone(), 11.0).expect("font discovery");
        let mut app =
            App { view, session: None, title: String::from("shell"), exited: false, frames: 0 };
        app.spawn_shell(&cc.egui_ctx);
        app
    }

    fn spawn_shell(&mut self, ctx: &egui::Context) {
        let cwd = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
        let cfg = SpawnCfg {
            shell: None,
            cwd,
            env_extra: vec![],
            tab_id: "tab-1".into(),
            nonce: fresh_nonce(),
            claude_config_dir: None,
            size: GridSize { cols: 120, rows: 30, cell_width: 9, cell_height: 18 },
        };
        match TermSession::spawn(&cfg, ctx.clone(), self.view.theme.clone()) {
            Ok(session) => {
                self.session = Some(session);
                self.exited = false;
            }
            Err(err) => {
                tracing::error!("failed to spawn shell: {err:#}");
                self.title = format!("spawn failed: {err}");
            }
        }
    }

    fn drain_events(&mut self) {
        let Some(session) = &self.session else { return };
        while let Ok(ev) = session.events.try_recv() {
            match ev {
                TabEvent::Title(Some(t)) => self.title = t,
                TabEvent::Title(None) => self.title = String::from("shell"),
                TabEvent::Bell => tracing::debug!("bell"),
                TabEvent::Tee(events) => {
                    for te in events {
                        tracing::debug!("tee: {te:?}");
                    }
                }
                TabEvent::ChildExit(status) => {
                    tracing::info!("child exited: {status:?}");
                }
                TabEvent::LoopDone(_) => self.exited = true,
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frames = self.frames.saturating_add(1);
        self.drain_events();

        egui::Panel::left("rail")
            .resizable(true)
            .default_size(230.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let dot = if self.exited { "○" } else { "●" };
                    ui.colored_label(
                        if self.exited {
                            egui::Color32::GRAY
                        } else {
                            egui::Color32::from_rgb(0x7b, 0xa2, 0x5a)
                        },
                        dot,
                    );
                    ui.label(egui::RichText::new(&self.title).strong());
                });
                if self.exited && ui.button("respawn shell").clicked() {
                    let ctx = ui.ctx().clone();
                    self.spawn_shell(&ctx);
                }
            });

        egui::CentralPanel::default().show(ui, |ui| {
            match &mut self.session {
                Some(session) => {
                    let response = self.view.show(ui, session);
                    // Focus the terminal on startup.
                    if self.frames < 3 {
                        response.request_focus();
                    }
                }
                None => {
                    ui.centered_and_justified(|ui| {
                        ui.label("no session — see logs");
                    });
                }
            }
        });
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            session.shutdown();
        }
    }
}

fn fresh_nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", nanos, std::process::id())
}
