//! App shell: state shared by all workspaces, event routing, layout of the window.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bh3::config::AppConfig;
use bh3::library::Library;
use bh3::server::{self, BonsaiInstall};
use bh3::service::{self, Command, Event, JobId, JobKind, ServiceHandle, StatusSnapshot};
use eframe::egui::{self, Align, Color32, Key, Layout, RichText};

use super::theme::{self, Palette};
use super::widgets::{self, icons};
use super::{chat, history_ws, library_ws, models_ws, server_ws, settings, setup, statusbar, topbar, write};

/// Area id of the toasts (a foreground layer that does not take Esc).
const TOASTS_ID: &str = "bh3-toasts";

/// Blender-style workspaces (tabs in the top bar).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Workspace {
    #[default]
    Write,
    Chat,
    Library,
    History,
    Server,
    Settings,
}

impl Workspace {
    pub fn all() -> [Workspace; 6] {
        [
            Workspace::Write,
            Workspace::Chat,
            Workspace::Library,
            Workspace::History,
            Workspace::Server,
            Workspace::Settings,
        ]
    }
    pub fn label(&self) -> &'static str {
        match self {
            Workspace::Write => "Write",
            Workspace::Chat => "Chat",
            Workspace::Library => "Library",
            Workspace::History => "History",
            Workspace::Server => "Bonsai",
            Workspace::Settings => "Settings",
        }
    }
    pub fn icon(&self) -> &'static str {
        use super::widgets::icons;
        match self {
            Workspace::Write => icons::WRITE,
            Workspace::Chat => icons::CHAT,
            Workspace::Library => icons::LIBRARY,
            Workspace::History => icons::HISTORY,
            Workspace::Server => icons::SERVER,
            Workspace::Settings => icons::SETTINGS,
        }
    }
    /// From a name like `write`, `chat`, `bonsai` / `server`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "write" => Some(Workspace::Write),
            "chat" => Some(Workspace::Chat),
            "library" => Some(Workspace::Library),
            "history" => Some(Workspace::History),
            "server" | "bonsai" | "models" => Some(Workspace::Server),
            "settings" => Some(Workspace::Settings),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToastKind {
    Info,
    Ok,
    Warn,
    Error,
}

impl ToastKind {
    pub fn color(&self) -> Color32 {
        match self {
            ToastKind::Info => Palette::INFO,
            ToastKind::Ok => Palette::OK,
            ToastKind::Warn => Palette::WARN,
            ToastKind::Error => Palette::ERR,
        }
    }
}

/// A short message in the bottom-right corner.
#[derive(Clone, Debug)]
pub struct Toast {
    pub kind: ToastKind,
    pub text: String,
    /// `AppState::time` when it disappears.
    pub expires: f64,
}

/// Everything the workspaces share. Workspaces get `&mut AppState`.
pub struct AppState {
    pub config: AppConfig,
    pub library: Library,
    pub service: ServiceHandle,
    pub status: StatusSnapshot,
    pub workspace: Workspace,
    pub toasts: Vec<Toast>,
    /// Seconds since start (egui input time), updated every frame.
    pub time: f64,
    /// Show the first-run / change-folder dialog.
    pub show_setup: bool,
    pub setup: setup::SetupState,
    pub settings: settings::SettingsState,
    pub write: write::WriteState,
    pub chat: chat::ChatState,
    pub history: history_ws::HistoryState,
    pub library_ws: library_ws::LibraryState,
    pub server_ws: server_ws::ServerWsState,
    /// Downloads, imports and the Models area.
    pub models: models_ws::ModelsState,
    /// What is happening right now, for the status bar (e.g. "Writing... 12 s"); workspaces set/clear it.
    pub activity: Option<String>,
    /// Speed/cache numbers of the last finished request (status bar).
    pub last_timings: Option<bh3::client::Timings>,
    /// Esc may cancel a job this frame: no popup, menu or dialog was open and no text field had
    /// the keyboard (Esc closes or leaves those first).
    pub esc_free: bool,
    next_job: JobId,
    /// Submitted jobs that have not ended yet (`true` = chat answer, else a prompt).
    open_jobs: Vec<(JobId, bool)>,
    theme_applied: Option<([u8; 3], f32)>,
}

impl AppState {
    /// Fresh job id.
    pub fn new_job_id(&mut self) -> JobId {
        self.next_job += 1;
        self.next_job
    }

    /// Send a job to the service; returns its id. Prompt and chat jobs count as work a close
    /// would lose; a picture description does not (it is asked again next time).
    pub fn submit(&mut self, kind: JobKind) -> JobId {
        let id = self.new_job_id();
        let tracked = match kind {
            JobKind::Write(_) => Some(false),
            JobKind::Chat { .. } => Some(true),
            JobKind::Describe { .. } => None,
        };
        self.service.send(Command::Submit { id, kind });
        if let Some(chat) = tracked {
            self.open_jobs.push((id, chat));
        }
        id
    }

    /// What closing the window now would lose, one line each (empty: nothing).
    pub fn unsaved_work(&self) -> Vec<&'static str> {
        let mut losses = close_losses(&self.open_jobs, self.library_ws.has_unsaved());
        if self.models.any_running() {
            losses.push("A download or import is running: it stops (a download continues next time).");
        }
        losses
    }

    pub fn cancel(&self, id: JobId) {
        self.service.send(Command::Cancel(id));
    }

    pub fn send(&self, cmd: Command) {
        self.service.send(cmd);
    }

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        let secs = if kind == ToastKind::Error { 8.0 } else { 4.5 };
        self.toasts.push(Toast { kind, text: text.into(), expires: self.time + secs });
    }

    /// Save the config to disk and pass it to the service.
    pub fn save_config(&mut self) {
        if let Err(e) = self.config.save() {
            self.toast(ToastKind::Error, format!("Could not save settings: {e:#}"));
        }
        self.service.send(Command::UpdateConfig(Box::new(self.config.clone())));
    }

    /// Check `dir`, store it as the Bonsai folder and tell the service.
    /// Choosing a folder switches the app's own runtime off (its model is no longer used).
    pub fn set_bonsai_dir(&mut self, dir: PathBuf) -> anyhow::Result<BonsaiInstall> {
        let install = BonsaiInstall::detect(&dir)?;
        self.config.bonsai_dir = Some(dir);
        self.config.runtime.model = None;
        self.config.runtime.mmproj = None;
        self.save_config();
        self.service.send(Command::SetInstall(Some(install.clone())));
        Ok(install)
    }

    /// Save the config and give the service the install it now chooses (after a change of
    /// `config.runtime`: model, projector or server files).
    pub fn apply_runtime(&mut self) -> Option<BonsaiInstall> {
        self.save_config();
        let install = server::install_from_config(&self.config);
        self.service.send(Command::SetInstall(install.clone()));
        install
    }

    pub fn bonsai_ready(&self) -> bool {
        self.status.state.is_ready()
    }

    fn apply_theme_if_changed(&mut self, ctx: &egui::Context) {
        let key = (self.config.accent, self.config.ui_scale);
        if self.theme_applied != Some(key) {
            theme::apply(ctx, key.0, key.1);
            self.theme_applied = Some(key);
        }
    }
}

/// Screenshot request from `BH3_SCREENSHOT` (used to check the UI from scripts).
struct ScreenshotPlan {
    path: PathBuf,
    after_frames: u64,
    requested: bool,
}

/// What closing the window would lose: running prompt / chat jobs (`true` = chat) and unsaved
/// Library edits. The Write form and prompt are not on it: they are kept as a draft.
fn close_losses(open_jobs: &[(JobId, bool)], library_unsaved: bool) -> Vec<&'static str> {
    let mut out = Vec::new();
    if open_jobs.iter().any(|(_, chat)| !chat) {
        out.push("A prompt is being written: it is cancelled.");
    }
    if open_jobs.iter().any(|(_, chat)| *chat) {
        out.push("A chat answer is being written: it is cancelled.");
    }
    if library_unsaved {
        out.push("The Library has unsaved edits: they are lost.");
    }
    out
}

/// The job an event ends (its last event).
fn ended_job(ev: &Event) -> Option<JobId> {
    match ev {
        Event::WriteDone { id, .. }
        | Event::ChatDone { id, .. }
        | Event::DescribeDone { id, .. }
        | Event::JobFailed { id, .. } => Some(*id),
        Event::JobCancelled(id) => Some(*id),
        _ => None,
    }
}

/// A popup, menu or dialog was shown last frame (asked before anything is drawn this frame).
/// Toasts do not count; tooltips are not on the foreground layer.
fn overlay_open(ctx: &egui::Context) -> bool {
    let toasts = egui::LayerId::new(egui::Order::Foreground, egui::Id::new(TOASTS_ID));
    ctx.is_context_menu_open()
        || ctx.memory(|m| {
            m.any_popup_open()
                || m.areas().visible_layer_ids().iter().any(|l| l.order == egui::Order::Foreground && *l != toasts)
        })
}

/// The eframe app.
pub struct BonsaiApp {
    state: AppState,
    frame: u64,
    screenshot: Option<ScreenshotPlan>,
    /// The "Quit anyway?" dialog is open.
    close_confirm: bool,
    /// Closing is allowed (Quit anyway, or a screenshot run).
    allow_close: bool,
    /// A widget had the keyboard at the end of the last frame (egui drops the focus at the start
    /// of the frame in which Esc is pressed, so this frame cannot tell).
    focus_last_frame: bool,
}

impl BonsaiApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        theme::install_fonts(&cc.egui_ctx);

        let (config, config_warning) = AppConfig::load_with_warning();
        let library = Library::load();
        let repaint_ctx = cc.egui_ctx.clone();
        let service = service::spawn(config.clone(), Arc::new(move || repaint_ctx.request_repaint()));

        let install = server::install_from_config(&config);
        let show_setup = install.as_ref().map(|i| !i.is_usable()).unwrap_or(true);
        service.send(Command::SetInstall(install));

        let mut state = AppState {
            config,
            library,
            service,
            status: StatusSnapshot::default(),
            workspace: Workspace::default(),
            toasts: Vec::new(),
            time: 0.0,
            show_setup,
            setup: setup::SetupState::default(),
            settings: settings::SettingsState::default(),
            write: write::WriteState::default(),
            chat: chat::ChatState::default(),
            history: history_ws::HistoryState::default(),
            library_ws: library_ws::LibraryState::default(),
            server_ws: server_ws::ServerWsState::default(),
            models: models_ws::ModelsState::default(),
            activity: None,
            last_timings: None,
            esc_free: true,
            next_job: 0,
            open_jobs: Vec::new(),
            theme_applied: None,
        };
        state.apply_theme_if_changed(&cc.egui_ctx);
        if let Some(warning) = config_warning {
            state.toast(ToastKind::Warn, warning);
        }
        // The Write draft of the last session (or the demo) before any workspace is shown.
        write::init(&mut state);

        if let Ok(name) = std::env::var("BH3_WORKSPACE") {
            if let Some(ws) = Workspace::from_name(&name) {
                state.workspace = ws;
            }
            if name.trim().eq_ignore_ascii_case("models") {
                state.server_ws.show_models();
            }
        }
        match std::env::var("BH3_SETUP").as_deref() {
            Ok("1") => state.show_setup = true,
            Ok("0") => state.show_setup = false,
            _ => {}
        }
        let screenshot = std::env::var_os("BH3_SCREENSHOT").map(|p| ScreenshotPlan {
            path: PathBuf::from(p),
            after_frames: std::env::var("BH3_SCREENSHOT_FRAMES").ok().and_then(|f| f.parse().ok()).unwrap_or(45),
            requested: false,
        });
        Self { state, frame: 0, screenshot, close_confirm: false, allow_close: false, focus_last_frame: false }
    }

    fn route_events(&mut self) {
        for ev in self.state.service.drain() {
            if let Some(id) = ended_job(&ev) {
                self.state.open_jobs.retain(|(j, _)| *j != id);
            }
            match ev {
                Event::Status(s) => self.state.status = *s,
                Event::Notice(text) => self.state.toast(ToastKind::Info, text),
                Event::Error(text) => self.state.toast(ToastKind::Error, text),
                Event::ShutdownComplete => {}
                other => {
                    write::on_event(&mut self.state, &other);
                    chat::on_event(&mut self.state, &other);
                    library_ws::on_event(&mut self.state, &other);
                    history_ws::on_event(&mut self.state, &other);
                    server_ws::on_event(&mut self.state, &other);
                    models_ws::on_event(&mut self.state, &other);
                }
            }
        }
    }

    fn show_toasts(&mut self, ctx: &egui::Context) {
        let now = self.state.time;
        self.state.toasts.retain(|t| t.expires > now);
        if self.state.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new(TOASTS_ID))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-14.0, -34.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                let mut close = None;
                for (i, t) in self.state.toasts.iter().enumerate() {
                    egui::Frame::new()
                        .fill(Palette::HEADER)
                        .stroke(egui::Stroke::new(1.0_f32, t.kind.color()))
                        .corner_radius(egui::CornerRadius::same(5))
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("●").color(t.kind.color()));
                                ui.label(&t.text);
                                if ui.small_button("✖").clicked() {
                                    close = Some(i);
                                }
                            });
                        });
                    ui.add_space(4.0);
                }
                if let Some(i) = close {
                    self.state.toasts.remove(i);
                }
            });
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(plan) = self.screenshot.as_mut() else { return };
        if !plan.requested && self.frame >= plan.after_frames {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            plan.requested = true;
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let [w, h] = img.size;
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, bytes) {
                if let Err(e) = buf.save(&plan.path) {
                    eprintln!("screenshot: {e}");
                }
            }
            // A screenshot run ends without asking (the demo has unsaved Library edits).
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }

    /// Closing the window while a prompt or chat answer is being written, or with unsaved Library
    /// edits, asks first; `keys` = Enter or Esc was pressed for the dialog (both keep the app open).
    fn guard_close(&mut self, ctx: &egui::Context, keys: bool) {
        let requested = ctx.input(|i| i.viewport().close_requested());
        if requested && !self.allow_close && (self.close_confirm || !self.state.unsaved_work().is_empty()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.close_confirm {
                // A text field behind the dialog must not keep the keyboard.
                ctx.memory_mut(|m| m.stop_text_input());
                self.close_confirm = true;
            }
        }
        if !self.close_confirm {
            return;
        }
        match close_dialog(ctx, &self.state.unsaved_work(), keys) {
            Some(true) => {
                self.close_confirm = false;
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Some(false) => self.close_confirm = false,
            None => {}
        }
    }
}

/// While the close dialog is open, its keys: Enter / Esc (returned as pressed); every other key
/// is dropped so the workspaces behind it do not act on it.
fn take_dialog_keys(ctx: &egui::Context) -> bool {
    ctx.input_mut(|i| {
        let pressed = i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape);
        i.events.retain(|e| {
            !matches!(
                e,
                egui::Event::Key { .. }
                    | egui::Event::Text(_)
                    | egui::Event::Paste(_)
                    | egui::Event::Copy
                    | egui::Event::Cut
            )
        });
        pressed
    })
}

/// "Quit anyway?" with what would be lost. `Some(true)`: Quit anyway; `Some(false)`: Cancel
/// (also Enter, Esc or a click outside); `None`: still open.
fn close_dialog(ctx: &egui::Context, losses: &[&str], keys: bool) -> Option<bool> {
    let modal = egui::Modal::new(egui::Id::new("bh3-close-confirm")).frame(widgets::dialog_frame()).show(ctx, |ui| {
        ui.set_width(400.0);
        ui.style_mut().interaction.selectable_labels = false;
        let title = format!("{} Quit {}?", icons::WARN, bh3::APP_NAME);
        ui.label(RichText::new(title).size(15.5).strong().color(Palette::TEXT));
        ui.add_space(6.0);
        if losses.is_empty() {
            ui.label("Nothing would be lost now.");
        }
        for loss in losses {
            ui.label(format!("•  {loss}"));
        }
        if !widgets::demo_mode() {
            ui.add_space(4.0);
            ui.label(
                RichText::new("The form and the prompt in Write are kept for the next start.").color(Palette::TEXT_DIM),
            );
        }
        ui.add_space(14.0);
        let mut choice = None;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let quit = egui::Button::new(RichText::new("Quit anyway").strong().color(Color32::WHITE))
                .fill(Palette::ERR.lerp_to_gamma(Palette::WIDGET, 0.3))
                .min_size(egui::vec2(110.0, 26.0));
            if ui.add(quit).clicked() {
                choice = Some(true);
            }
            let cancel = egui::Button::new("Cancel").min_size(egui::vec2(90.0, 26.0));
            if ui.add(cancel).on_hover_text("Enter / Esc").clicked() {
                choice = Some(false);
            }
        });
        choice
    });
    match modal.inner {
        Some(choice) => Some(choice),
        None if keys || modal.should_close() => Some(false),
        None => None,
    }
}

impl eframe::App for BonsaiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame += 1;
        self.state.time = ctx.input(|i| i.time);
        // Asked before anything is drawn: popups and menus close on Esc while they are drawn.
        self.state.esc_free = !self.close_confirm && !self.focus_last_frame && !overlay_open(ctx);
        let dialog_keys = self.close_confirm && take_dialog_keys(ctx);
        self.state.apply_theme_if_changed(ctx);
        self.route_events();
        // The elapsed time of a running prompt counts on every workspace.
        write::update_activity(&mut self.state);
        chat::refresh_activity(&mut self.state);
        // Launch settings of the running server and library files changed on disk, on any workspace.
        server_ws::track_run(&mut self.state);
        library_ws::check_disk(&mut self.state);
        if self.state.workspace != Workspace::Chat {
            chat::flush_pending(&mut self.state);
        }

        topbar::show(&mut self.state, ctx);
        statusbar::show(&mut self.state, ctx);
        match self.state.workspace {
            Workspace::Write => write::show(&mut self.state, ctx),
            Workspace::Chat => chat::show(&mut self.state, ctx),
            Workspace::Library => library_ws::show(&mut self.state, ctx),
            Workspace::History => history_ws::show(&mut self.state, ctx),
            Workspace::Server => server_ws::show(&mut self.state, ctx),
            Workspace::Settings => settings::show(&mut self.state, ctx),
        }
        if self.state.show_setup {
            setup::show(&mut self.state, ctx);
        }
        models_ws::delete_dialog(&mut self.state, ctx);
        self.guard_close(ctx, dialog_keys);
        self.show_toasts(ctx);
        self.handle_screenshot(ctx);
        write::autosave(&mut self.state, false);
        self.focus_last_frame = ctx.memory(|m| m.focused().is_some());
        // Status (server state, GPU, idle timer) changes without input: refresh twice a second.
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        write::autosave(&mut self.state, true);
        chat::flush_pending(&mut self.state);
        self.state.service.send(Command::Shutdown);
        // Give the service time to stop a server we started (stop-on-exit).
        self.state.service.wait_shutdown(Duration::from_secs(15));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_asks_only_when_something_would_be_lost() {
        assert!(close_losses(&[], false).is_empty());
        assert_eq!(close_losses(&[(3, false)], false).len(), 1);
        let all = close_losses(&[(3, false), (4, true), (5, true)], true);
        assert_eq!(all.len(), 3, "{all:?}");
        assert!(all[1].contains("chat") && all[2].contains("Library"));
    }

    #[test]
    fn only_final_events_end_a_job() {
        assert_eq!(ended_job(&Event::JobCancelled(7)), Some(7));
        assert_eq!(ended_job(&Event::JobFailed { id: 8, error: String::new() }), Some(8));
        assert_eq!(ended_job(&Event::JobQueued(9)), None);
        assert_eq!(ended_job(&Event::JobPhase { id: 9, phase: String::new() }), None);
    }
}
