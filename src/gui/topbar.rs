//! Top bar (Blender's top bar): app icon and menus on the left, the workspace tabs after them and
//! the Bonsai power switch on the right. It also handles the global shortcuts (workspaces, zoom)
//! and shows the About dialog.

use std::path::Path;
use std::time::Duration;

use bh3::server::ServerState;
use bh3::service::Command;
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Id, Key, KeyboardShortcut, Layout, Margin, Modifiers, Rect, RichText,
    Sense, Stroke, StrokeKind, Ui, UiBuilder,
};

use super::app::{AppState, ToastKind, Workspace};
use super::settings::{self, Category};
use super::theme::{self, Palette};
use super::util;
use super::widgets::{self, icons};

/// Height of the bar.
const BAR_HEIGHT: f32 = 30.0;
/// UI scale limits (the Settings slider uses the same) and the step of View > Zoom.
pub const SCALE_MIN: f32 = 0.75;
pub const SCALE_MAX: f32 = 2.0;
const SCALE_STEP: f32 = 0.1;
/// Ctrl+1..6 select the workspaces in [`Workspace::all`] order.
const WORKSPACE_KEYS: [Key; 6] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6];
/// egui memory key of the About dialog's open flag.
const ABOUT_KEY: &str = "bh3-about-open";
/// egui memory key of "one of the top bar's menus is open" (set while the menus are drawn).
const MENU_OPEN_KEY: &str = "bh3-menu-open";
/// Size of the power switch pill.
const SWITCH_W: f32 = 34.0;
const SWITCH_H: f32 = 18.0;
/// Clicks on the power switch this close together give one command: a double-click must not
/// stop and restart Bonsai, or start it and cancel the start.
const SWITCH_DEBOUNCE_SECS: f64 = 0.6;
/// egui memory key of the time of the last power-switch command.
const SWITCH_CLICK_KEY: &str = "bh3-power-click";

/// What a menu entry asks for (applied after the menus are drawn).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    OpenConfigFolder,
    OpenHistoryFolder,
    Quit,
    Start,
    Stop,
    Restart,
    FreeGpu,
    ChangeFolder,
    OpenBonsaiFolder,
    OpenServerLog,
    OpenWebUi,
    Show(Workspace),
    ZoomIn,
    ZoomOut,
    ZoomReset,
    GuideRef,
    GuideBase,
    About,
}

pub fn show(app: &mut AppState, ctx: &egui::Context) {
    settings::flush_pending(app, ctx);
    handle_shortcuts(app, ctx);

    let mut action = None;
    egui::TopBottomPanel::top("bh3-topbar")
        .exact_height(BAR_HEIGHT)
        .frame(egui::Frame::new().fill(Palette::BAR).inner_margin(Margin::symmetric(6, 0)))
        .show(ctx, |ui| {
            let full = ui.max_rect();
            // Right side first, so the tabs know how much room is left.
            let mut right = ui.new_child(
                UiBuilder::new()
                    .id_salt("bh3-topbar-right")
                    .max_rect(full)
                    .layout(Layout::right_to_left(Align::Center)),
            );
            power_area(&mut right, app);
            let right_edge = (right.min_rect().left() - 12.0).max(full.left());

            let left_rect = Rect::from_min_max(full.min, egui::pos2(right_edge, full.bottom()));
            let mut left = ui.new_child(
                UiBuilder::new()
                    .id_salt("bh3-topbar-left")
                    .max_rect(left_rect)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            left.set_clip_rect(left_rect.intersect(ui.clip_rect()));
            left.spacing_mut().item_spacing.x = 2.0;
            let icon = left
                .add(widgets::app_icon(18.0).sense(Sense::click()))
                .on_hover_text(format!("About {}", bh3::APP_NAME));
            if icon.clicked() {
                action = Some(Action::About);
            }
            left.add_space(4.0);
            if let Some(a) = menus(&mut left, app) {
                action = Some(a);
            }
            left.add_space(14.0);
            workspace_tabs(&mut left, app);
        });
    if let Some(a) = action {
        apply(app, ctx, a);
    }
    show_about(ctx);
}

// --- shortcuts -------------------------------------------------------------------------------

fn workspace_shortcut(index: usize) -> Option<KeyboardShortcut> {
    WORKSPACE_KEYS.get(index).map(|key| KeyboardShortcut::new(Modifiers::COMMAND, *key))
}

/// "Ctrl+1" for the workspace at `index`.
fn workspace_shortcut_text(ctx: &egui::Context, index: usize) -> String {
    workspace_shortcut(index).map(|s| ctx.format_shortcut(&s)).unwrap_or_default()
}

fn handle_shortcuts(app: &mut AppState, ctx: &egui::Context) {
    // egui's own Ctrl+Plus/Minus/0 zoom would not be saved: the app handles these keys itself.
    if ctx.options(|o| o.zoom_with_keyboard) {
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
    }
    // A dialog, popup or menu is on top: its keys are consumed below but do nothing.
    let blocked = app.show_setup || about_open(ctx) || overlay_open(ctx);
    let all = Workspace::all();
    let current = all.iter().position(|w| *w == app.workspace).unwrap_or(0);
    let mut target = None;
    let mut zoom: Option<Action> = None;
    ctx.input_mut(|i| {
        for (index, ws) in all.iter().enumerate() {
            if workspace_shortcut(index).is_some_and(|s| i.consume_shortcut(&s)) {
                target = Some(*ws);
            }
        }
        if i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::PageDown)) {
            target = Some(all[(current + 1) % all.len()]);
        }
        if i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::PageUp)) {
            target = Some(all[(current + all.len() - 1) % all.len()]);
        }
        use egui::gui_zoom::kb_shortcuts as zk;
        if i.consume_shortcut(&zk::ZOOM_RESET) {
            zoom = Some(Action::ZoomReset);
        } else if i.consume_shortcut(&zk::ZOOM_IN) || i.consume_shortcut(&zk::ZOOM_IN_SECONDARY) {
            zoom = Some(Action::ZoomIn);
        } else if i.consume_shortcut(&zk::ZOOM_OUT) {
            zoom = Some(Action::ZoomOut);
        }
    });
    if blocked {
        return;
    }
    if let Some(ws) = target {
        app.workspace = ws;
    }
    if let Some(z) = zoom {
        apply(app, ctx, z);
    }
}

/// A modal dialog (of any workspace), a popup (combo box list, colour picker), a context menu or
/// a top-bar menu was shown last frame.
fn overlay_open(ctx: &egui::Context) -> bool {
    let menu_open = ctx.data(|d| d.get_temp::<bool>(Id::new(MENU_OPEN_KEY))).unwrap_or(false);
    menu_open
        || ctx.is_context_menu_open()
        || ctx.memory(|m| {
            // egui keeps a popup "open" after its widget stops being drawn: only a visible one counts.
            m.top_modal_layer().is_some()
                || (m.any_popup_open() && m.areas().visible_layer_ids().iter().any(|layer| m.is_popup_open(layer.id)))
        })
}

// --- menus -----------------------------------------------------------------------------------

/// Flat menu titles like Blender's: no frame until hovered or open.
fn menu_title_style(ui: &mut Ui) {
    let style = ui.style_mut();
    style.spacing.button_padding = egui::vec2(8.0, 3.0);
    let w = &mut style.visuals.widgets;
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0_f32, Palette::TEXT);
    w.hovered.weak_bg_fill = Palette::BAR_HOVER;
    w.hovered.bg_stroke = Stroke::NONE;
    w.active.weak_bg_fill = Color32::from_gray(62);
    w.active.bg_stroke = Stroke::NONE;
    w.open.weak_bg_fill = Color32::from_gray(62);
    w.open.bg_stroke = Stroke::NONE;
}

/// Common setup of a menu's content.
fn menu_body(ui: &mut Ui) {
    ui.set_min_width(250.0);
    ui.spacing_mut().item_spacing.y = 1.0;
}

fn menus(ui: &mut Ui, app: &AppState) -> Option<Action> {
    let mut action = None;
    ui.scope(|ui| {
        menu_title_style(ui);
        ui.menu_button("File", |ui| {
            menu_body(ui);
            file_menu(ui, &mut action);
        });
        ui.menu_button("Bonsai", |ui| {
            menu_body(ui);
            bonsai_menu(ui, app, &mut action);
        });
        ui.menu_button("View", |ui| {
            menu_body(ui);
            view_menu(ui, app, &mut action);
        });
        ui.menu_button("Help", |ui| {
            menu_body(ui);
            help_menu(ui, &mut action);
        });
        // egui stores the open menu of a bar under the id of the Ui the menu buttons are in.
        let open = egui::menu::BarState::load(ui.ctx(), ui.id()).is_some();
        ui.ctx().data_mut(|d| d.insert_temp(Id::new(MENU_OPEN_KEY), open));
    });
    action
}

/// A menu entry that is always enabled.
fn item(
    ui: &mut Ui,
    action: &mut Option<Action>,
    a: Action,
    icon: &str,
    label: &str,
    shortcut: &str,
) -> egui::Response {
    let response = widgets::menu_item(ui, icon, label, shortcut, true);
    if response.clicked() {
        *action = Some(a);
    }
    response
}

/// A menu entry that may be disabled; `tip` explains it either way.
fn item_if(ui: &mut Ui, action: &mut Option<Action>, a: Action, icon: &str, label: &str, enabled: bool, tip: &str) {
    if widgets::menu_item(ui, icon, label, "", enabled).on_hover_text(tip).clicked() {
        *action = Some(a);
    }
}

fn file_menu(ui: &mut Ui, action: &mut Option<Action>) {
    item(ui, action, Action::OpenConfigFolder, icons::FOLDER, "Open config folder", "")
        .on_hover_text("Settings, history, chats, library edits and the server log");
    item(ui, action, Action::OpenHistoryFolder, icons::HISTORY, "Open history folder", "")
        .on_hover_text("One .json file per written prompt");
    widgets::menu_separator(ui);
    item(ui, action, Action::Quit, "🚪", "Quit", "Alt+F4")
        .on_hover_text("Close the app (stops Bonsai if the app started it and \"Stop on exit\" is on)");
}

fn bonsai_menu(ui: &mut Ui, app: &AppState, action: &mut Option<Action>) {
    let state = &app.status.state;
    let running = matches!(state, ServerState::Ready | ServerState::Starting | ServerState::Loading);
    let install_ok = app.status.install_ok;
    let no_folder = "Choose the Bonsai folder first (Bonsai > Change Bonsai folder...)";

    let start_tip = if !install_ok {
        no_folder
    } else if running {
        "Bonsai is already running"
    } else {
        "Start Bonsai now, even when other programs use the GPU"
    };
    item_if(ui, action, Action::Start, icons::PLAY, "Start", install_ok && !running, start_tip);
    let stop_tip = if running { "Stop Bonsai (cancels running jobs)" } else { "Bonsai is not running" };
    item_if(ui, action, Action::Stop, icons::STOP, "Stop", running, stop_tip);
    let restart_tip = if install_ok { "Stop and start Bonsai again (new launch options apply)" } else { no_folder };
    let restart_ok = install_ok && !matches!(state, ServerState::Stopping);
    item_if(ui, action, Action::Restart, icons::RESTART, "Restart", restart_ok, restart_tip);
    let again = if app.config.auto_start {
        "It starts again by itself when a prompt or chat message needs it."
    } else {
        "Turn it on again with the switch in the top bar."
    };
    let free_tip =
        format!("Stop Bonsai so ComfyUI gets the whole GPU: Bonsai 27B holds about 7.7 GB of the 8 GB. {again}");
    item_if(ui, action, Action::FreeGpu, "⏏", "Free GPU for ComfyUI", running, &free_tip);
    widgets::menu_separator(ui);
    item(ui, action, Action::ChangeFolder, icons::FOLDER, "Change Bonsai folder...", "")
        .on_hover_text("Pick the Bonsai-demo folder (the one with start_bonsai.bat)");
    let folder = app.config.bonsai_dir.as_ref();
    let folder_tip = folder.map_or("No Bonsai folder chosen yet".to_string(), |d| d.display().to_string());
    item_if(ui, action, Action::OpenBonsaiFolder, icons::FOLDER, "Open Bonsai folder", folder.is_some(), &folder_tip);
    let log = bh3::server::default_log_path();
    let has_log = log.is_file();
    let log_tip = if has_log {
        log.display().to_string()
    } else {
        "No log yet: it is written when the app starts Bonsai".to_string()
    };
    item_if(ui, action, Action::OpenServerLog, icons::NOTE, "Open server log", has_log, &log_tip);
    let ready = state.is_ready();
    let web_tip = if ready {
        format!("Open {} in the browser", app.config.base_url())
    } else {
        "Bonsai is not running".to_string()
    };
    item_if(ui, action, Action::OpenWebUi, "🌐", "Open Bonsai web UI", ready, &web_tip);
}

fn view_menu(ui: &mut Ui, app: &AppState, action: &mut Option<Action>) {
    let ctx = ui.ctx().clone();
    for (index, ws) in Workspace::all().into_iter().enumerate() {
        let icon = if ws == app.workspace { icons::CHECK } else { ws.icon() };
        let shortcut = workspace_shortcut_text(&ctx, index);
        item(ui, action, Action::Show(ws), icon, ws.label(), &shortcut);
    }
    widgets::menu_separator(ui);
    let scale = app.config.ui_scale;
    let r = widgets::menu_item(ui, icons::SEARCH, "Zoom in", "Ctrl++", scale < SCALE_MAX - 0.001)
        .on_hover_text("Make the whole interface bigger (UI scale +10 %)");
    if r.clicked() {
        *action = Some(Action::ZoomIn);
    }
    let r = widgets::menu_item(ui, "", "Zoom out", "Ctrl+-", scale > SCALE_MIN + 0.001)
        .on_hover_text("Make the whole interface smaller (UI scale -10 %)");
    if r.clicked() {
        *action = Some(Action::ZoomOut);
    }
    let label = format!("Reset zoom ({:.0} %)", scale * 100.0);
    let r = widgets::menu_item(ui, "", &label, "Ctrl+0", (scale - 1.0).abs() > 0.001)
        .on_hover_text("Back to 100 % (also in Settings > Interface)");
    if r.clicked() {
        *action = Some(Action::ZoomReset);
    }
}

fn help_menu(ui: &mut Ui, action: &mut Option<Action>) {
    item(ui, action, Action::GuideRef, "🌐", "H3 guide - r2v reference mode", "").on_hover_text(bh3::GUIDE_REF_URL);
    item(ui, action, Action::GuideBase, "🌐", "H3 guide - f2v first frame", "").on_hover_text(bh3::GUIDE_BASE_URL);
    widgets::menu_separator(ui);
    item(ui, action, Action::About, icons::INFO, &format!("About {}...", bh3::APP_NAME), "");
}

fn apply(app: &mut AppState, ctx: &egui::Context, action: Action) {
    match action {
        Action::OpenConfigFolder => open_or_toast(app, &bh3::config::config_dir()),
        Action::OpenHistoryFolder => open_or_toast(app, &bh3::history::history_dir()),
        Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        Action::Start => app.send(Command::StartServer),
        Action::Stop => app.send(Command::StopServer),
        Action::Restart => app.send(Command::RestartServer),
        Action::FreeGpu => {
            app.send(Command::StopServer);
            app.toast(ToastKind::Info, "Stopping Bonsai to free the GPU for ComfyUI...");
        }
        Action::ChangeFolder => app.show_setup = true,
        Action::OpenBonsaiFolder => {
            if let Some(dir) = app.config.bonsai_dir.clone() {
                open_or_toast(app, &dir);
            }
        }
        Action::OpenServerLog => open_or_toast(app, &bh3::server::default_log_path()),
        Action::OpenWebUi => util::open_url(ctx, &app.config.base_url()),
        Action::Show(ws) => app.workspace = ws,
        Action::ZoomIn => set_scale(app, app.config.ui_scale + SCALE_STEP),
        Action::ZoomOut => set_scale(app, app.config.ui_scale - SCALE_STEP),
        Action::ZoomReset => set_scale(app, 1.0),
        Action::GuideRef => util::open_url(ctx, bh3::GUIDE_REF_URL),
        Action::GuideBase => util::open_url(ctx, bh3::GUIDE_BASE_URL),
        Action::About => set_about(ctx, true),
    }
}

fn open_or_toast(app: &mut AppState, path: &Path) {
    if let Err(e) = util::open_path(path) {
        app.toast(ToastKind::Error, format!("Could not open {}: {e:#}", path.display()));
    }
}

/// Store a new UI scale (rounded to 5 %, clamped) and save it when it changed.
pub fn set_scale(app: &mut AppState, scale: f32) {
    let scale = ((scale * 20.0).round() / 20.0).clamp(SCALE_MIN, SCALE_MAX);
    if (scale - app.config.ui_scale).abs() > 0.001 {
        app.config.ui_scale = scale;
        app.save_config();
    }
}

// --- workspace tabs --------------------------------------------------------------------------

fn workspace_tabs(ui: &mut Ui, app: &mut AppState) {
    const PAD: f32 = 10.0;
    const ICON_GAP: f32 = 6.0;
    let ctx = ui.ctx().clone();
    let accent = theme::accent(&ctx);
    let font = FontId::proportional(13.5);
    let icon_font = FontId::proportional(13.0);
    let tabs: Vec<_> = Workspace::all()
        .into_iter()
        .map(|ws| {
            let icon = ui.painter().layout_no_wrap(ws.icon().to_string(), icon_font.clone(), Color32::PLACEHOLDER);
            let label = ui.painter().layout_no_wrap(ws.label().to_string(), font.clone(), Color32::PLACEHOLDER);
            (ws, icon, label)
        })
        .collect();
    let spacing = ui.spacing().item_spacing.x;
    let full_width: f32 = tabs.iter().map(|(_, i, l)| PAD * 2.0 + i.size().x + ICON_GAP + l.size().x + spacing).sum();
    // Not enough room: inactive tabs show only their icon (the tooltip names them).
    let compact = full_width > ui.available_width();

    for (index, (ws, icon, label)) in tabs.into_iter().enumerate() {
        let active = ws == app.workspace;
        let with_label = active || !compact;
        let width = PAD * 2.0 + icon.size().x + if with_label { ICON_GAP + label.size().x } else { 0.0 };
        let (rect, response) = ui.allocate_exact_size(egui::vec2(width, BAR_HEIGHT), Sense::click());
        if ui.is_rect_visible(rect) {
            let tab = Rect::from_min_max(egui::pos2(rect.left(), rect.top() + 4.0), rect.max);
            let top_round = CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 };
            let painter = ui.painter();
            if active {
                painter.rect_filled(tab, top_round, Palette::HEADER);
                let line = Rect::from_min_max(egui::pos2(tab.left(), tab.bottom() - 2.0), tab.max);
                painter.rect_filled(line, CornerRadius::ZERO, accent);
            } else if response.hovered() {
                painter.rect_filled(tab, top_round, Palette::BAR_HOVER);
            }
            let color = if active || response.hovered() { Palette::TEXT } else { Palette::TEXT_DIM };
            let icon_color = if active { accent } else { color };
            let center_y = tab.center().y - 1.0;
            let x = rect.left() + PAD;
            let icon_w = icon.size().x;
            painter.galley_with_override_text_color(egui::pos2(x, center_y - icon.size().y / 2.0), icon, icon_color);
            if with_label {
                let pos = egui::pos2(x + icon_w + ICON_GAP, center_y - label.size().y / 2.0);
                painter.galley_with_override_text_color(pos, label, color);
            }
        }
        let tip = format!("{} workspace ({})", ws.label(), workspace_shortcut_text(&ctx, index));
        if response.on_hover_text(tip).clicked() {
            app.workspace = ws;
        }
    }
}

// --- Bonsai power switch ---------------------------------------------------------------------

/// Right end of the bar (laid out right to left): the switch, then the AUTO and busy badges.
fn power_area(ui: &mut Ui, app: &mut AppState) {
    ui.spacing_mut().item_spacing.x = 6.0;
    power_switch(ui, app);
    if app.config.auto_start {
        let auto_stop = auto_stop_text(app.config.auto_stop_idle_minutes)
            .map(|t| format!("\nAuto-stop: {t}, when this app started it."))
            .unwrap_or_default();
        let tip = format!(
            "Auto-start is on: Bonsai starts by itself when a prompt or chat message needs it.{auto_stop}\n\
             Click to change it in Settings > Bonsai server."
        );
        if small_badge(ui, "AUTO", Palette::INFO).on_hover_text(tip).clicked() {
            app.workspace = Workspace::Settings;
            app.settings.category = Category::Server;
        }
    }
    if app.status.busy {
        let queued = app.status.queued;
        let (text, tip) = if queued > 0 {
            (format!("busy +{queued}"), format!("Bonsai is working on a job; {queued} more waiting"))
        } else {
            ("busy".to_string(), "Bonsai is working on a job".to_string())
        };
        small_badge(ui, &text, Palette::WARN).on_hover_text(tip);
    }
}

/// Compact clickable badge sized for the bar.
fn small_badge(ui: &mut Ui, text: &str, color: Color32) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(10.5), color);
    let size = egui::vec2(galley.size().x + 12.0, 16.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if response.hovered() { color.gamma_multiply(0.35) } else { color.gamma_multiply(0.2) };
        let stroke = Stroke::new(1.0_f32, color.gamma_multiply(0.6));
        ui.painter().rect(rect, CornerRadius::same(8), fill, stroke, StrokeKind::Inside);
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    response
}

fn auto_stop_text(minutes: u32) -> Option<String> {
    match minutes {
        0 => None,
        1 => Some("after 1 idle minute".to_string()),
        m => Some(format!("after {m} idle minutes")),
    }
}

fn power_switch(ui: &mut Ui, app: &mut AppState) {
    let state = app.status.state.clone();
    let can_start = app.status.install_ok;
    let (label, label_color, on, pulsing) = match &state {
        ServerState::Ready => ("Bonsai: Ready", Palette::TEXT, true, false),
        ServerState::Starting => ("Bonsai: Starting...", Palette::WARN, true, true),
        ServerState::Loading => ("Bonsai: Loading...", Palette::WARN, true, true),
        ServerState::Stopping => ("Bonsai: Stopping...", Palette::WARN, false, true),
        ServerState::Failed(_) => ("Bonsai: Failed", Palette::ERR, false, false),
        ServerState::Stopped => ("Bonsai: Off", Palette::TEXT_DIM, false, false),
    };
    let ctx = ui.ctx().clone();
    let galley = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(13.5), label_color);
    let pad = 7.0;
    let size = egui::vec2(pad + galley.size().x + 9.0 + SWITCH_W + pad, 24.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if response.hovered() {
            painter.rect_filled(rect, CornerRadius::same(4), Palette::BAR_HOVER);
        }
        let text_pos = egui::pos2(rect.left() + pad, rect.center().y - galley.size().y / 2.0);
        painter.galley(text_pos, galley, label_color);

        let pill = Rect::from_min_size(
            egui::pos2(rect.right() - pad - SWITCH_W, rect.center().y - SWITCH_H / 2.0),
            egui::vec2(SWITCH_W, SWITCH_H),
        );
        let fill = match &state {
            ServerState::Ready => Palette::OK,
            ServerState::Failed(_) => Palette::ERR,
            ServerState::Stopped => Palette::WIDGET,
            _ => {
                let pulse = 0.5 + 0.5 * (app.time * 4.5).sin() as f32;
                Color32::from_gray(70).lerp_to_gamma(Palette::WARN, 0.35 + 0.65 * pulse)
            }
        };
        let fill = if response.hovered() { fill.lerp_to_gamma(Color32::WHITE, 0.12) } else { fill };
        let radius = CornerRadius::same((SWITCH_H / 2.0) as u8);
        painter.rect(pill, radius, fill, Stroke::new(1.0_f32, Palette::OUTLINE), StrokeKind::Outside);
        let t = ctx.animate_bool_with_time(Id::new("bh3-power-knob"), on, 0.18);
        let x = egui::lerp((pill.left() + SWITCH_H / 2.0)..=(pill.right() - SWITCH_H / 2.0), t);
        let knob = if on { Color32::WHITE } else { Color32::from_gray(200) };
        let knob_r = SWITCH_H / 2.0 - 2.5;
        painter.circle(
            egui::pos2(x, pill.center().y),
            knob_r,
            knob,
            Stroke::new(1.0_f32, Color32::from_black_alpha(60)),
        );
    }
    if pulsing {
        ctx.request_repaint_after(Duration::from_millis(40));
    }

    let response = response.on_hover_text(power_tooltip(app, can_start));
    if response.clicked() && take_switch_click(&ctx, app.time) {
        match &state {
            ServerState::Ready | ServerState::Starting | ServerState::Loading => app.send(Command::StopServer),
            _ if !can_start => app.show_setup = true,
            _ => app.send(Command::StartServer),
        }
    }
}

/// Whether a click on the power switch at `now` gives a command; remembers the time when it does.
fn take_switch_click(ctx: &egui::Context, now: f64) -> bool {
    let id = Id::new(SWITCH_CLICK_KEY);
    let accept = switch_click_counts(ctx.data(|d| d.get_temp::<f64>(id)), now);
    if accept {
        ctx.data_mut(|d| d.insert_temp(id, now));
    }
    accept
}

/// A click counts unless it comes within [`SWITCH_DEBOUNCE_SECS`] after the last one that counted.
fn switch_click_counts(last: Option<f64>, now: f64) -> bool {
    last.is_none_or(|t| now < t || now - t >= SWITCH_DEBOUNCE_SECS)
}

fn power_tooltip(app: &AppState, can_start: bool) -> String {
    let st = &app.status;
    let mut lines = vec![match &st.state {
        ServerState::Ready if st.started_by_us => "Bonsai is running (started by this app)".to_string(),
        ServerState::Ready => "Bonsai is running (started outside this app)".to_string(),
        ServerState::Starting => "Bonsai is starting...".to_string(),
        ServerState::Loading => "Bonsai is loading the model...".to_string(),
        ServerState::Stopping => "Bonsai is stopping...".to_string(),
        ServerState::Failed(e) => format!("The last start failed: {e}"),
        ServerState::Stopped => "Bonsai is off".to_string(),
    }];
    if let Some(props) = &st.props {
        lines.push(format!("Model: {} · context {} tokens", props.model_name, props.n_ctx));
    } else if let Some(model) = st.install.as_ref().and_then(|i| i.primary_model()) {
        let name = model.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        lines.push(format!("Model: {name}"));
    }
    lines.push(String::new());
    let click = match &st.state {
        ServerState::Ready => "Click to stop Bonsai and free the GPU.",
        ServerState::Starting | ServerState::Loading => "Click to cancel the start.",
        ServerState::Stopping => "Click to start it again once it has stopped.",
        _ if !can_start => "No usable Bonsai folder yet: click to choose it.",
        _ => "Click to start Bonsai.",
    };
    lines.push(click.to_string());
    lines.push(if app.config.auto_start {
        "Auto-start: on - starts by itself when a prompt or chat message needs it.".to_string()
    } else {
        "Auto-start: off - turn Bonsai on here before writing.".to_string()
    });
    lines.push(match auto_stop_text(app.config.auto_stop_idle_minutes) {
        Some(t) => {
            let left = st.auto_stop_in_secs.map(|s| format!(" (in {})", widgets::fmt_secs(s))).unwrap_or_default();
            format!("Auto-stop: {t}, when this app started it{left}.")
        }
        None => "Auto-stop: off.".to_string(),
    });
    lines.join("\n")
}

// --- About -----------------------------------------------------------------------------------

fn about_open(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(Id::new(ABOUT_KEY))).unwrap_or(false)
}

fn set_about(ctx: &egui::Context, open: bool) {
    ctx.data_mut(|d| d.insert_temp(Id::new(ABOUT_KEY), open));
}

fn show_about(ctx: &egui::Context) {
    if !about_open(ctx) {
        return;
    }
    let modal = egui::Modal::new(Id::new("bh3-about")).frame(widgets::dialog_frame()).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.style_mut().interaction.selectable_labels = false;
        ui.horizontal(|ui| {
            ui.add(widgets::app_icon(60.0));
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.add_space(4.0);
                ui.label(RichText::new(bh3::APP_NAME).size(19.0).strong().color(Palette::TEXT));
                ui.label(RichText::new(format!("Version {}", bh3::VERSION)).color(Palette::TEXT_DIM));
                let license = "MIT License · © 2026 Bonsai H3 Writer contributors";
                ui.label(RichText::new(license).small().color(Palette::TEXT_FAINT));
            });
        });
        ui.add_space(12.0);
        ui.label(
            "Writes MiniMax H3 video prompts (r2v reference mode and f2v first frame) with your local \
             Bonsai 27B and checks them against the official H3 prompt guides.",
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "It starts Bonsai when a prompt needs it and stops it after idle minutes, so ComfyUI \
                 gets the 8 GB GPU back.",
            )
            .color(Palette::TEXT_DIM),
        );
        ui.add_space(10.0);
        ui.hyperlink_to("🌐 H3 guide - r2v reference mode", bh3::GUIDE_REF_URL);
        ui.hyperlink_to("🌐 H3 guide - f2v first frame", bh3::GUIDE_BASE_URL);
        ui.add_space(14.0);
        let mut close = false;
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                close = ui
                    .add(egui::Button::new("Close").min_size(egui::vec2(84.0, 24.0)))
                    .on_hover_text("Close (Esc or Enter)")
                    .clicked();
            });
        });
        close
    });
    let enter = ctx.input(|i| i.key_pressed(Key::Enter));
    if modal.inner || modal.should_close() || enter {
        set_about(ctx, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(ctx: &egui::Context, run_ui: impl FnMut(&egui::Context)) {
        let _ = ctx.run(egui::RawInput::default(), run_ui);
    }

    #[test]
    fn double_click_on_the_switch_gives_one_command() {
        assert!(switch_click_counts(None, 3.0));
        assert!(!switch_click_counts(Some(3.0), 3.25));
        assert!(switch_click_counts(Some(3.0), 3.0 + SWITCH_DEBOUNCE_SECS));
        assert!(switch_click_counts(Some(9.0), 1.0));
    }

    #[test]
    fn shortcuts_wait_for_dialogs_and_popups() {
        let ctx = egui::Context::default();
        pass(&ctx, |ctx| {
            egui::Modal::new(Id::new("test-modal")).show(ctx, |ui| ui.label("Sure?"));
        });
        pass(&ctx, |ctx| assert!(overlay_open(ctx)));
        pass(&ctx, |ctx| assert!(!overlay_open(ctx)));

        let popup = Id::new("test-popup");
        ctx.memory_mut(|m| m.open_popup(popup));
        pass(&ctx, |ctx| {
            egui::Area::new(popup).order(egui::Order::Foreground).show(ctx, |ui| ui.label("list"));
        });
        pass(&ctx, |ctx| assert!(overlay_open(ctx)));
        // Still open in egui's memory but no longer drawn (its workspace was left): no block.
        pass(&ctx, |ctx| assert!(!overlay_open(ctx)));
        assert!(ctx.memory(|m| m.any_popup_open()));
    }
}
