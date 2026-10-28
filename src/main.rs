//! Bonsai H3 Writer - desktop app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod gui;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(format!("{} {}", bh3::APP_NAME, bh3::VERSION))
        .with_app_id(bh3::APP_ID)
        .with_inner_size([1440.0, 900.0])
        .with_min_inner_size([980.0, 620.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(bh3::APP_NAME, options, Box::new(|cc| Ok(Box::new(gui::BonsaiApp::new(cc)))))
}
