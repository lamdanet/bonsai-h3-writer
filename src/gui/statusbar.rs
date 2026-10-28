//! Bottom status bar (Blender's status bar): what is happening, or the shortcuts of the current
//! workspace, on the left; server state, model, who started it, auto-stop countdown, VRAM, speed of
//! the last run and the version on the right, separated by thin lines. Hover a segment for details.

use std::sync::Arc;

use bh3::client::Timings;
use bh3::server::ServerState;
use eframe::egui::{self, text::LayoutJob, Color32, CornerRadius, FontId, Galley, Margin, Rect, Sense, Stroke, Ui};

use super::app::{AppState, Workspace};
use super::theme::Palette;
use super::widgets;

const BAR_HEIGHT: f32 = 24.0;
const FONT_SIZE: f32 = 12.0;
/// Space between the parts of a segment and around the separator lines.
const PART_GAP: f32 = 5.0;
const SEG_GAP: f32 = 9.0;
const MINI_BAR_W: f32 = 60.0;
/// Room always left for the activity text on the left.
const MIN_LEFT: f32 = 240.0;
/// Longest model name shown (cut in the middle beyond).
const MODEL_CHARS: usize = 30;

/// One piece of a segment.
enum Part {
    Text(String, Color32),
    Dot(Color32),
    /// Fill fraction and colour of a 60 px mini bar.
    Bar(f32, Color32),
    /// A key name drawn as a keycap (shortcut hints).
    Key(String),
    Spinner,
}

/// A group of parts with one tooltip; segments are separated by thin lines.
struct Segment {
    key: &'static str,
    parts: Vec<Part>,
    tip: String,
    /// Kept longest when the window is narrow (higher = more important).
    priority: u8,
    /// A click opens this workspace.
    opens: Option<Workspace>,
}

/// A part ready to paint.
enum Laid {
    Text(Arc<Galley>),
    Dot(Color32),
    Bar(f32, Color32),
    Key(Arc<Galley>),
    Spinner,
}

impl Laid {
    fn width(&self) -> f32 {
        match self {
            Laid::Text(g) => g.size().x,
            Laid::Dot(_) => 8.0,
            Laid::Bar(..) => MINI_BAR_W,
            Laid::Key(g) => g.size().x + 10.0,
            Laid::Spinner => 12.0,
        }
    }
}

pub fn show(app: &mut AppState, ctx: &egui::Context) {
    egui::TopBottomPanel::bottom("bh3-statusbar")
        .exact_height(BAR_HEIGHT)
        .frame(egui::Frame::new().fill(Palette::BAR).inner_margin(Margin::symmetric(10, 0)))
        .show(ctx, |ui| {
            let rect = ui.max_rect();
            let segments = right_segments(app);
            let mut laid: Vec<(Segment, Vec<Laid>, f32)> = segments
                .into_iter()
                .map(|seg| {
                    let parts = lay_out(ui, &seg.parts, None);
                    let width = parts_width(&parts);
                    (seg, parts, width)
                })
                .collect();
            // Drop the least important segments until everything fits next to the activity text.
            let budget = (rect.width() - MIN_LEFT).max(0.0);
            while laid.len() > 1 && total_width(&laid) > budget {
                let Some(drop) = laid.iter().enumerate().min_by_key(|(_, (s, _, _))| s.priority).map(|(i, _)| i) else {
                    break;
                };
                laid.remove(drop);
            }

            // Right side, painted from the right edge.
            let mut x = rect.right();
            let mut clicked = None;
            for (i, (seg, parts, width)) in laid.iter().enumerate().rev() {
                let seg_rect = Rect::from_min_max(egui::pos2(x - width, rect.top()), egui::pos2(x, rect.bottom()));
                let sense = if seg.opens.is_some() { Sense::click() } else { Sense::hover() };
                let response = ui.interact(seg_rect.expand2(egui::vec2(4.0, 0.0)), ui.id().with(seg.key), sense);
                if seg.opens.is_some() && response.hovered() {
                    let hover = seg_rect.expand2(egui::vec2(5.0, -3.0));
                    ui.painter().rect_filled(hover, CornerRadius::same(3), Palette::BAR_HOVER);
                }
                paint_parts(ui, parts, seg_rect);
                if response.on_hover_text(seg.tip.as_str()).clicked() {
                    clicked = seg.opens;
                }
                x = seg_rect.left() - SEG_GAP;
                if i > 0 {
                    separator(ui, x, rect);
                    x -= SEG_GAP;
                }
            }

            // Left side: activity, or "Ready" and the shortcuts of the workspace.
            let left_room = (x - SEG_GAP - rect.left()).max(0.0);
            let (parts, tip) = left_parts(app);
            let mut items = lay_out(ui, &parts, None);
            while items.len() > 2 && parts_width(&items) > left_room {
                // Drop the last hint (key + action).
                items.truncate(items.len() - 2);
            }
            if parts_width(&items) > left_room {
                // Still too long (a long activity or warning): cut the text with "…".
                items = lay_out(ui, &parts[..items.len()], Some(left_room));
            }
            let width = parts_width(&items).min(left_room);
            let left_rect = Rect::from_min_max(rect.min, egui::pos2(rect.left() + width, rect.bottom()));
            paint_parts(ui, &items, left_rect);
            if let Some(tip) = tip {
                ui.interact(left_rect, ui.id().with("activity"), Sense::hover()).on_hover_text(tip);
            }
            if let Some(ws) = clicked {
                app.workspace = ws;
            }
        });
}

// --- content ---------------------------------------------------------------------------------

fn right_segments(app: &AppState) -> Vec<Segment> {
    let st = &app.status;
    let running = matches!(st.state, ServerState::Ready | ServerState::Starting | ServerState::Loading);
    let mut segs = Vec::new();

    // Server state.
    let (dot, text) = match &st.state {
        ServerState::Ready => (Palette::OK, "Bonsai ready"),
        ServerState::Starting => (Palette::WARN, "Starting..."),
        ServerState::Loading => (Palette::WARN, "Loading model..."),
        ServerState::Stopping => (Palette::WARN, "Stopping..."),
        ServerState::Failed(_) => (Palette::ERR, "Start failed"),
        ServerState::Stopped if !st.install_ok => (Palette::ERR, "No Bonsai folder"),
        ServerState::Stopped => (Color32::from_gray(110), "Bonsai off"),
    };
    let mut tip = match &st.state {
        ServerState::Failed(e) => format!("The last start failed: {e}"),
        other => format!("Bonsai: {}", other.label()),
    };
    match &st.install {
        Some(install) => tip.push_str(&format!("\nFolder: {}", install.root.display())),
        None => tip.push_str("\nNo Bonsai folder chosen: Bonsai > Change Bonsai folder..."),
    }
    if let Some(up) = st.uptime_secs.filter(|_| running) {
        tip.push_str(&format!("\nUp for {}", widgets::fmt_secs(up)));
    }
    tip.push_str("\nClick to open the Bonsai workspace.");
    segs.push(Segment {
        key: "state",
        parts: vec![Part::Dot(dot), Part::Text(text.to_string(), Palette::TEXT)],
        tip,
        priority: 10,
        opens: Some(Workspace::Server),
    });

    // Model and context.
    if let Some(props) = &st.props {
        let mut tip = format!("Model: {}", props.model_name);
        if !props.model_ftype.is_empty() {
            tip.push_str(&format!("\nType: {}", props.model_ftype));
        }
        tip.push_str(&format!(
            "\nContext: {} tokens per slot · {} slot(s)\nVision input: {}",
            props.n_ctx,
            props.total_slots,
            if props.vision { "yes" } else { "no (no projector loaded)" }
        ));
        if !props.build_info.is_empty() {
            tip.push_str(&format!("\nllama.cpp build {}", props.build_info));
        }
        segs.push(Segment {
            key: "model",
            parts: vec![
                Part::Text(widgets::short_model_name(&props.model_name, MODEL_CHARS), Palette::TEXT),
                Part::Text(format!("ctx {}", fmt_ctx(props.n_ctx)), Palette::TEXT_DIM),
            ],
            tip,
            priority: 8,
            opens: Some(Workspace::Server),
        });
    } else if let Some(model) = st.install.as_ref().and_then(|i| i.primary_model()) {
        let name = model.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        segs.push(Segment {
            key: "model",
            parts: vec![Part::Text(widgets::short_model_name(&name, MODEL_CHARS), Palette::TEXT_FAINT)],
            tip: format!("Model the launcher loads: {name}"),
            priority: 4,
            opens: Some(Workspace::Server),
        });
    }

    // Who started it.
    if running {
        let (text, tip) = if st.started_by_us {
            (
                "started by app",
                "This app started Bonsai: it stops it after the idle minutes and when the app closes (see Settings).",
            )
        } else {
            (
                "external",
                "Bonsai was started outside this app (e.g. start_bonsai.bat): the app uses it but never stops it by itself.",
            )
        };
        segs.push(Segment {
            key: "origin",
            parts: vec![Part::Text(text.to_string(), Palette::TEXT_DIM)],
            tip: tip.to_string(),
            priority: 5,
            opens: None,
        });
    }

    // Idle auto-stop countdown.
    if let Some(secs) = st.auto_stop_in_secs {
        let color = if secs < 60 { Palette::WARN } else { Palette::TEXT_DIM };
        segs.push(Segment {
            key: "autostop",
            parts: vec![Part::Text(format!("auto-stop {}", widgets::fmt_secs(secs)), color)],
            tip: format!(
                "Bonsai stops after {} idle minutes to free the GPU for ComfyUI.\nIdle for {}. Change it in Settings > Bonsai server.",
                app.config.auto_stop_idle_minutes,
                widgets::fmt_secs(st.idle_secs)
            ),
            priority: 7,
            opens: None,
        });
    }

    // GPU memory and temperature.
    if let Some(gpu) = &st.gpu {
        let fraction = gpu.mem_fraction();
        let gb = |mb: u32| mb as f32 / 1024.0;
        let temp_color = if gpu.temp_c >= 87 {
            Palette::ERR
        } else if gpu.temp_c >= 78 {
            Palette::WARN
        } else {
            Palette::TEXT_DIM
        };
        segs.push(Segment {
            key: "gpu",
            parts: vec![
                Part::Text(format!("VRAM {:.1} / {:.1} GB", gb(gpu.mem_used_mb), gb(gpu.mem_total_mb)), Palette::TEXT),
                Part::Bar(fraction, widgets::usage_color(fraction)),
                Part::Text(format!("{}°C", gpu.temp_c), temp_color),
            ],
            tip: gpu_tip(gpu),
            priority: 9,
            opens: Some(Workspace::Server),
        });
    }

    // Speed of the last request.
    let demo_timings;
    let timings = match &app.last_timings {
        Some(t) => Some(t),
        None if widgets::demo_mode() => {
            demo_timings = sample_timings();
            Some(&demo_timings)
        }
        None => None,
    };
    if let Some(t) = timings {
        segs.push(Segment {
            key: "lastrun",
            parts: vec![
                Part::Text(format!("{:.1} tok/s", t.predicted_per_second), Palette::TEXT),
                Part::Text(format!("· {} new + {} cached", t.prompt_n, t.cache_n), Palette::TEXT_DIM),
            ],
            tip: timings_tip(t),
            priority: 6,
            opens: None,
        });
    }

    segs.push(Segment {
        key: "version",
        parts: vec![Part::Text(format!("v{}", bh3::VERSION), Palette::TEXT_FAINT)],
        tip: format!("{} {} - Help > About", bh3::APP_NAME, bh3::VERSION),
        priority: 3,
        opens: None,
    });
    segs
}

/// Left side: activity with a spinner, a warning, or "Ready" plus the workspace's shortcuts.
fn left_parts(app: &AppState) -> (Vec<Part>, Option<String>) {
    let st = &app.status;
    let queued = if st.queued > 0 { format!("  (+{} waiting)", st.queued) } else { String::new() };
    if let Some(activity) = app.activity.as_deref().filter(|a| !a.trim().is_empty()) {
        return (vec![Part::Spinner, Part::Text(format!("{activity}{queued}"), Palette::TEXT)], None);
    }
    let busy_text = match &st.state {
        ServerState::Starting => Some("Starting Bonsai..."),
        ServerState::Loading => Some("Loading the model..."),
        ServerState::Stopping => Some("Stopping Bonsai..."),
        _ if st.busy => Some("Bonsai is working..."),
        _ => None,
    };
    if let Some(text) = busy_text {
        return (vec![Part::Spinner, Part::Text(format!("{text}{queued}"), Palette::TEXT)], None);
    }
    if let ServerState::Failed(e) = &st.state {
        let text = format!("{} Bonsai could not start: {e}", widgets::icons::WARN);
        return (vec![Part::Text(text, Palette::ERR)], Some(format!("The last start failed: {e}")));
    }
    if !st.install_ok {
        let text = format!("{} Choose the Bonsai folder: Bonsai > Change Bonsai folder...", widgets::icons::WARN);
        let tip = "The app needs the Bonsai-demo folder (the one with start_bonsai.bat) to start Bonsai.";
        return (vec![Part::Text(text, Palette::WARN)], Some(tip.to_string()));
    }
    let mut parts = vec![Part::Text("Ready".to_string(), Palette::TEXT_DIM)];
    let hints: &[(&str, &str)] = match app.workspace {
        Workspace::Write => &[("Ctrl+Enter", "Write prompt"), ("Esc", "Cancel"), ("Ctrl+C", "Copy prompt")],
        Workspace::Chat => &[("Enter", "Send"), ("Shift+Enter", "New line"), ("Esc", "Stop")],
        Workspace::Library => &[("Ctrl+S", "Save")],
        Workspace::History | Workspace::Server | Workspace::Settings => &[],
    };
    for (key, what) in hints.iter().chain(&[("Ctrl+1…6", "Workspaces")]) {
        parts.push(Part::Key(key.to_string()));
        parts.push(Part::Text(what.to_string(), Palette::TEXT_DIM));
    }
    (parts, None)
}

fn gpu_tip(gpu: &bh3::gpu::GpuInfo) -> String {
    let mut lines = vec![
        gpu.name.clone(),
        format!(
            "VRAM: {} of {} used ({:.0} %)",
            bh3::gpu::fmt_mb(gpu.mem_used_mb),
            bh3::gpu::fmt_mb(gpu.mem_total_mb),
            gpu.mem_fraction() * 100.0
        ),
        format!("Load: {} % · Temperature: {} °C", gpu.util_pct, gpu.temp_c),
    ];
    if !gpu.processes.is_empty() {
        lines.push("Programs on the GPU:".to_string());
        for p in &gpu.processes {
            let who = if bh3::gpu::is_llama_server(&p.name) { " - Bonsai".to_string() } else { String::new() };
            lines.push(format!("  {}{who}", bh3::gpu::describe_process(p)));
        }
    }
    let others = bh3::gpu::other_processes(gpu);
    if others.iter().any(|p| bh3::gpu::process_hint(&p.name).is_some_and(|h| h.starts_with("ComfyUI"))) {
        lines.push(format!(
            "{} ComfyUI holds GPU memory: Bonsai (about 7.7 GB) may not fit until it is closed.",
            widgets::icons::WARN
        ));
    }
    lines.join("\n")
}

fn timings_tip(t: &Timings) -> String {
    format!(
        "Last request\nPrompt: {} new tokens at {:.0} tok/s ({:.1} s), {} reused from the cache\nOutput: {} tokens at {:.1} tok/s ({:.1} s)",
        t.prompt_n,
        t.prompt_per_second,
        t.prompt_ms / 1000.0,
        t.cache_n,
        t.predicted_n,
        t.predicted_per_second,
        t.predicted_ms / 1000.0
    )
}

/// `BH3_DEMO=1` only: numbers of a typical prompt run, so the segment can be checked in screenshots.
fn sample_timings() -> Timings {
    Timings {
        prompt_n: 722,
        cache_n: 1138,
        prompt_ms: 610.0,
        prompt_per_second: 1183.6,
        predicted_n: 412,
        predicted_ms: 12_915.0,
        predicted_per_second: 31.9,
    }
}

/// `16384` -> `16k`.
fn fmt_ctx(n: u32) -> String {
    if n >= 1024 && n.is_multiple_of(1024) {
        format!("{}k", n / 1024)
    } else if n >= 1000 {
        format!("{:.1}k", n as f32 / 1024.0)
    } else {
        n.to_string()
    }
}

// --- layout and painting ---------------------------------------------------------------------

fn font() -> FontId {
    FontId::proportional(FONT_SIZE)
}

/// Galleys for the parts; with `truncate_to`, a long text is cut with "…" to fit that width.
fn lay_out(ui: &Ui, parts: &[Part], truncate_to: Option<f32>) -> Vec<Laid> {
    let painter = ui.painter();
    let mut used = 0.0;
    parts
        .iter()
        .map(|part| {
            let laid = match part {
                Part::Text(text, color) => {
                    let mut job = LayoutJob::simple_singleline(text.clone(), font(), *color);
                    if let Some(max) = truncate_to {
                        job.wrap = egui::text::TextWrapping::truncate_at_width((max - used).max(20.0));
                    }
                    Laid::Text(painter.layout_job(job))
                }
                Part::Dot(c) => Laid::Dot(*c),
                Part::Bar(f, c) => Laid::Bar(*f, *c),
                Part::Key(k) => Laid::Key(painter.layout_no_wrap(k.clone(), FontId::proportional(10.5), Palette::TEXT)),
                Part::Spinner => Laid::Spinner,
            };
            used += laid.width() + PART_GAP;
            laid
        })
        .collect()
}

fn parts_width(parts: &[Laid]) -> f32 {
    let sum: f32 = parts.iter().map(Laid::width).sum();
    sum + PART_GAP * parts.len().saturating_sub(1) as f32
}

fn total_width(laid: &[(Segment, Vec<Laid>, f32)]) -> f32 {
    let sum: f32 = laid.iter().map(|(_, _, w)| *w).sum();
    sum + 2.0 * SEG_GAP * laid.len().saturating_sub(1) as f32
}

fn paint_parts(ui: &Ui, parts: &[Laid], rect: Rect) {
    let painter = ui.painter().with_clip_rect(rect.expand(1.0).intersect(ui.clip_rect()));
    let cy = rect.center().y;
    let mut x = rect.left();
    for part in parts {
        let w = part.width();
        match part {
            Laid::Text(g) => painter.galley(egui::pos2(x, cy - g.size().y / 2.0), g.clone(), Palette::TEXT),
            Laid::Dot(c) => {
                painter.circle_filled(egui::pos2(x + 4.0, cy), 3.5, *c);
            }
            Laid::Bar(fraction, color) => {
                let bar = Rect::from_min_size(egui::pos2(x, cy - 3.0), egui::vec2(MINI_BAR_W, 6.0));
                painter.rect_filled(bar, CornerRadius::same(2), Palette::INPUT);
                let filled = Rect::from_min_size(bar.min, egui::vec2(MINI_BAR_W * fraction.clamp(0.0, 1.0), 6.0));
                painter.rect_filled(filled, CornerRadius::same(2), *color);
            }
            Laid::Key(g) => {
                let cap = Rect::from_min_size(egui::pos2(x, cy - 8.0), egui::vec2(w, 16.0));
                painter.rect(
                    cap,
                    CornerRadius::same(3),
                    Color32::from_gray(48),
                    Stroke::new(1.0_f32, Color32::from_gray(78)),
                    egui::StrokeKind::Inside,
                );
                painter.galley(egui::pos2(x + 5.0, cy - g.size().y / 2.0), g.clone(), Palette::TEXT);
            }
            Laid::Spinner => paint_spinner(ui, egui::pos2(x + 6.0, cy)),
        }
        x += w + PART_GAP;
    }
}

/// Thin spinner arc (egui's Spinner is too heavy for the small bar).
fn paint_spinner(ui: &Ui, center: egui::Pos2) {
    let time = ui.input(|i| i.time);
    let start = time * std::f64::consts::TAU * 0.8;
    let sweep = 1.2 + 0.9 * (time * 2.2).sin();
    let points: Vec<egui::Pos2> = (0..=16)
        .map(|i| {
            let a = start + sweep * i as f64 / 16.0 * std::f64::consts::PI;
            center + 5.0 * egui::vec2(a.cos() as f32, a.sin() as f32)
        })
        .collect();
    ui.painter().add(egui::Shape::line(points, Stroke::new(1.6_f32, Palette::TEXT)));
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
}

fn separator(ui: &Ui, x: f32, bar: Rect) {
    let y = bar.center().y;
    ui.painter().vline(x.round() + 0.5, (y - 7.0)..=(y + 7.0), Stroke::new(1.0_f32, Color32::from_gray(58)));
}
