//! Desktop UI (eframe/egui), laid out like Blender: top bar with workspace tabs, editor areas
//! with header strips, side regions with collapsible panels, and a status bar.

pub mod app;
pub mod chat;
pub mod history_ws;
pub mod library_ws;
pub mod models_ws;
pub mod server_ws;
pub mod settings;
pub mod setup;
pub mod statusbar;
pub mod theme;
pub mod topbar;
pub mod util;
pub mod widgets;
pub mod write;

pub use app::BonsaiApp;
