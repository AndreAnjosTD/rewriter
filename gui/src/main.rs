//! Desktop front end for rustrewrite.
//!
//! The app is a view over the CLI: it builds a command, reads that command's
//! newline-delimited JSON, and never reimplements a rewrite or the gate.

mod app;
mod cli;
mod model;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("rustrewrite"),
        ..Default::default()
    };
    eframe::run_native(
        "rustrewrite",
        options,
        Box::new(|cc| Ok(Box::new(app::RustrewriteApp::new(cc)))),
    )
}
