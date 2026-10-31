#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use eframe::egui;
use env_logger::Env;
use image::RgbaImage;
mod permissions;

use ljosbru::{
    CacheStatus, DecodeConfig, DecodeEvent, DecodeOutcome, DecodeProgress, DecodeSummary,
    MonitorInfo, cache_status, list_monitors, parse_forward_key, run_decode,
};
use permissions::Permission;
use serde::{Deserialize, Serialize};

const SETTINGS_KEY: &str = "settings";
const FORWARD_KEY_PRESETS: &[&str] = &["RightArrow", "DownArrow", "Space", "PageDown", "Return"];
const PRIMARY_MONITOR_LABEL: &str = "Primary monitor";
const PREVIEW_MAX_WIDTH: u32 = 1280;
const PREVIEW_INTERVAL: Duration = Duration::from_millis(250);
const MAX_LOG_LINES: usize = 500;
const GRANTED_COLOR: egui::Color32 = egui::Color32::from_rgb(0x3f, 0xa3, 0x4d);
const PERMISSION_CHECK_INTERVAL: Duration = Duration::from_secs(1);

fn main() -> eframe::Result {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn"))
        .format_target(false)
        .format_timestamp(None)
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ljosbru")
            .with_inner_size([1040.0, 700.0])
            .with_min_inner_size([760.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ljosbru",
        options,
        Box::new(|creation_context| Ok(Box::new(DecodeApp::new(creation_context)))),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    cache_dir: String,
    output: String,
    forward_key: String,
    monitor: Option<usize>,
    delay_between_ms: u64,
    initial_delay_ms: u64,
    retry_timeout_ms: u64,
    save_screenshots: bool,
    minimize_while_decoding: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let home = home_dir();
        Self {
            cache_dir: home.join("ljosbru-cache").display().to_string(),
            output: home.join("ljosbru-output").display().to_string(),
            forward_key: "RightArrow".to_owned(),
            monitor: None,
            delay_between_ms: 100,
            initial_delay_ms: 3_000,
            retry_timeout_ms: 5_000,
            save_screenshots: false,
            minimize_while_decoding: true,
        }
    }
}

impl Settings {
    fn to_config(&self) -> Result<DecodeConfig, String> {
        if self.cache_dir.trim().is_empty() {
            return Err("choose a cache directory".to_owned());
        }
        if self.output.trim().is_empty() {
            return Err("choose an output file".to_owned());
        }
        Ok(DecodeConfig {
            delay_between: Duration::from_millis(self.delay_between_ms),
            forward_keypress: parse_forward_key(self.forward_key.trim())
                .map_err(|error| format!("{error:#}"))?,
            cache_dir: resolve_path(&self.cache_dir),
            output: resolve_path(&self.output),
            monitor: self.monitor,
            initial_delay: Duration::from_millis(self.initial_delay_ms),
            retry_timeout: Duration::from_millis(self.retry_timeout_ms),
            save_screenshots: self.save_screenshots,
        })
    }
}

fn home_dir() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

/// Apps started from Finder run in `/`, so relative paths are resolved against the home directory.
fn resolve_path(path: &str) -> PathBuf {
    let path = path.trim();
    let path = path
        .strip_prefix("~/")
        .map(|rest| home_dir().join(rest))
        .unwrap_or_else(|| PathBuf::from(path));
    if path.is_relative() {
        home_dir().join(path)
    } else {
        path
    }
}

enum WorkerMessage {
    CacheLoaded(DecodeProgress),
    Countdown(Duration),
    Log(String),
    Preview(egui::ColorImage),
    Scanned(String),
    FrameCached(DecodeProgress),
    Finished(Result<DecodeOutcome, String>),
}

enum Phase {
    Idle,
    Countdown(Duration),
    Scanning,
    Completed(DecodeSummary),
    Cancelled,
    Failed(String),
}

struct Running {
    cancel: Arc<AtomicBool>,
    messages: Receiver<WorkerMessage>,
    minimized: bool,
}

struct DecodeApp {
    settings: Settings,
    monitors: Result<Vec<MonitorInfo>, String>,
    cache_status: Result<CacheStatus, String>,
    running: Option<Running>,
    phase: Phase,
    progress: Option<DecodeProgress>,
    last_scan: Option<String>,
    preview: Option<egui::TextureHandle>,
    log: Vec<String>,
    missing_permissions: Vec<Permission>,
    permissions_checked_at: Instant,
    show_permissions: bool,
}

impl DecodeApp {
    fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        let settings = creation_context
            .storage
            .and_then(|storage| eframe::get_value(storage, SETTINGS_KEY))
            .unwrap_or_default();
        let mut app = Self {
            settings,
            monitors: Ok(Vec::new()),
            cache_status: Ok(CacheStatus::default()),
            running: None,
            phase: Phase::Idle,
            progress: None,
            last_scan: None,
            preview: None,
            log: Vec::new(),
            missing_permissions: permissions::missing(),
            permissions_checked_at: Instant::now(),
            show_permissions: false,
        };
        app.show_permissions = !app.missing_permissions.is_empty();
        app.refresh_monitors();
        app.refresh_cache_status();
        app
    }

    fn refresh_monitors(&mut self) {
        self.monitors = list_monitors().map_err(|error| format!("{error:#}"));
    }

    fn refresh_cache_status(&mut self) {
        self.cache_status = cache_status(&resolve_path(&self.settings.cache_dir))
            .map_err(|error| format!("{error:#}"));
    }

    /// Re-checks missing permissions while any are missing, since they are granted in System Settings.
    fn poll_permissions(&mut self, ctx: &egui::Context) {
        if self.missing_permissions.is_empty() {
            return;
        }
        if self.permissions_checked_at.elapsed() >= PERMISSION_CHECK_INTERVAL {
            self.missing_permissions = permissions::missing();
            self.permissions_checked_at = Instant::now();
            if self.missing_permissions.is_empty() {
                self.show_permissions = false;
                self.push_log("All required permissions are granted");
            }
        }
        ctx.request_repaint_after(PERMISSION_CHECK_INTERVAL);
    }

    fn permissions_modal(&mut self, ctx: &egui::Context) {
        if !self.show_permissions {
            return;
        }
        let response = egui::Modal::new(egui::Id::unique("permissions-modal")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading("Permissions needed");
            ui.add_space(4.0);
            ui.label("ljosbru needs these macOS permissions to decode.");
            ui.add_space(12.0);
            for permission in [Permission::ScreenRecording, Permission::Accessibility] {
                let granted = !self.missing_permissions.contains(&permission);
                egui::Frame::group(ui.style())
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (dot, _) = ui
                                .allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                            let color = if granted {
                                GRANTED_COLOR
                            } else {
                                ui.visuals().warn_fg_color
                            };
                            ui.painter().circle_filled(dot.center(), 5.0, color);
                            ui.add_space(4.0);
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(permission.name()).strong(),
                                    )
                                    .extend(),
                                );
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!(
                                            "Needed {}.",
                                            permission.purpose()
                                        ))
                                        .weak(),
                                    )
                                    .extend(),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if granted {
                                        ui.colored_label(GRANTED_COLOR, "Granted");
                                        return;
                                    }
                                    if ui.button("Open Settings…").clicked() {
                                        permissions::request(permission);
                                    }
                                    if permissions::can_reset()
                                        && ui
                                            .button("Reset & retry")
                                            .on_hover_text(
                                                "Use this if ljosbru is already turned on in \
                                                 System Settings but still shows as missing.",
                                            )
                                            .clicked()
                                    {
                                        permissions::reset_and_request(permission);
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(6.0);
            }
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(
                    "Turn on ljosbru in the list that opens. After granting Screen Recording, \
                     macOS may ask you to quit and reopen ljosbru. If ljosbru is already turned \
                     on but still missing, it was approved for a different build: use Reset & \
                     retry and turn it on again.",
                )
                .small()
                .weak(),
            );
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.button("Close").clicked()
            })
            .inner
        });
        if response.inner || response.should_close() {
            self.show_permissions = false;
        }
    }

    fn push_log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > MAX_LOG_LINES {
            self.log.drain(..self.log.len() - MAX_LOG_LINES);
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let config = match self.settings.to_config() {
            Ok(config) => config,
            Err(error) => {
                self.phase = Phase::Failed(error);
                return;
            }
        };

        let (sender, messages) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_ctx = ctx.clone();
        thread::spawn(move || decode_worker(&config, &worker_cancel, &sender, &worker_ctx));

        let minimized = self.settings.minimize_while_decoding;
        if minimized {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        self.running = Some(Running {
            cancel,
            messages,
            minimized,
        });
        self.phase = Phase::Countdown(Duration::from_millis(self.settings.initial_delay_ms));
        self.progress = None;
        self.last_scan = None;
        self.push_log("Started decoding");
    }

    fn stop(&mut self) {
        if let Some(running) = &self.running {
            running.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn process_messages(&mut self, ctx: &egui::Context) {
        let Some(running) = &self.running else {
            return;
        };
        let messages = running.messages.try_iter().collect::<Vec<_>>();
        for message in messages {
            match message {
                WorkerMessage::CacheLoaded(progress) => {
                    if progress.cached_frames > 0 {
                        self.push_log(format!(
                            "Resuming with {} cached frame(s)",
                            progress.cached_frames
                        ));
                    }
                    self.progress = Some(progress);
                }
                WorkerMessage::Countdown(remaining) => self.phase = Phase::Countdown(remaining),
                WorkerMessage::Log(line) => self.push_log(line),
                WorkerMessage::Preview(image) => match &mut self.preview {
                    Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.preview = Some(ctx.load_texture(
                            "screenshot-preview",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                },
                WorkerMessage::Scanned(summary) => {
                    self.phase = Phase::Scanning;
                    self.last_scan = Some(summary);
                }
                WorkerMessage::FrameCached(progress) => {
                    self.progress = Some(progress);
                }
                WorkerMessage::Finished(result) => self.finish(ctx, result),
            }
        }
    }

    fn finish(&mut self, ctx: &egui::Context, result: Result<DecodeOutcome, String>) {
        let minimized = self.running.take().is_some_and(|running| running.minimized);
        self.phase = match result {
            Ok(DecodeOutcome::Completed(summary)) => {
                self.push_log(format!("Wrote {}", summary.output.display()));
                Phase::Completed(summary)
            }
            Ok(DecodeOutcome::Cancelled) => {
                self.push_log("Stopped");
                Phase::Cancelled
            }
            Err(error) => {
                self.push_log(format!("Error: {error}"));
                Phase::Failed(error)
            }
        };
        self.refresh_cache_status();
        if minimized {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let running = self.running.is_some();
        ui.heading("Settings");
        ui.add_space(4.0);
        ui.add_enabled_ui(!running, |ui| {
            egui::Grid::new("settings")
                .num_columns(2)
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Cache directory")
                        .on_hover_text("Decoded frames are stored here. Reuse it to resume.");
                    ui.horizontal(|ui| {
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.settings.cache_dir)
                                .desired_width(190.0),
                        );
                        if response.lost_focus() {
                            self.refresh_cache_status();
                        }
                        if ui.button("Browse…").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .set_directory(resolve_path(&self.settings.cache_dir))
                                .pick_folder()
                        {
                            self.settings.cache_dir = path.display().to_string();
                            self.refresh_cache_status();
                        }
                    });
                    ui.end_row();

                    ui.label("Output file");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.output)
                                .desired_width(190.0),
                        );
                        if ui.button("Browse…").clicked() {
                            let current = resolve_path(&self.settings.output);
                            let mut dialog = rfd::FileDialog::new();
                            if let Some(parent) = current.parent() {
                                dialog = dialog.set_directory(parent);
                            }
                            if let Some(name) = current.file_name() {
                                dialog = dialog.set_file_name(name.to_string_lossy());
                            }
                            if let Some(path) = dialog.save_file() {
                                self.settings.output = path.display().to_string();
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Forward key").on_hover_text(
                        "Pressed after each decoded frame to advance to the next QR code.",
                    );
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.forward_key)
                                .desired_width(110.0),
                        );
                        egui::ComboBox::from_id_salt("forward-key-presets")
                            .selected_text("Presets")
                            .show_ui(ui, |ui| {
                                for preset in FORWARD_KEY_PRESETS {
                                    ui.selectable_value(
                                        &mut self.settings.forward_key,
                                        (*preset).to_owned(),
                                        *preset,
                                    );
                                }
                            });
                    });
                    ui.end_row();

                    ui.label("Monitor");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("monitor")
                            .width(220.0)
                            .selected_text(self.monitor_label(self.settings.monitor))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.settings.monitor,
                                    None,
                                    PRIMARY_MONITOR_LABEL,
                                );
                                if let Ok(monitors) = &self.monitors {
                                    for monitor in monitors {
                                        ui.selectable_value(
                                            &mut self.settings.monitor,
                                            Some(monitor.index),
                                            format!("{}: {}", monitor.index, monitor.description),
                                        );
                                    }
                                }
                            });
                        if ui.button("Refresh").clicked() {
                            self.refresh_monitors();
                        }
                    });
                    ui.end_row();

                    ui.label("Initial delay")
                        .on_hover_text("Time to switch to the window showing the QR codes.");
                    ui.add(
                        egui::DragValue::new(&mut self.settings.initial_delay_ms)
                            .range(0..=60_000)
                            .speed(50)
                            .suffix(" ms"),
                    );
                    ui.end_row();

                    ui.label("Delay between")
                        .on_hover_text("Wait time after pressing the forward key.");
                    ui.add(
                        egui::DragValue::new(&mut self.settings.delay_between_ms)
                            .range(0..=10_000)
                            .speed(5)
                            .suffix(" ms"),
                    );
                    ui.end_row();

                    ui.label("Retry timeout")
                        .on_hover_text("How long to look for a new frame before giving up.");
                    ui.add(
                        egui::DragValue::new(&mut self.settings.retry_timeout_ms)
                            .range(100..=600_000)
                            .speed(50)
                            .suffix(" ms"),
                    );
                    ui.end_row();
                });

            ui.add_space(4.0);
            ui.checkbox(
                &mut self.settings.save_screenshots,
                "Save screenshots to the cache directory",
            );
            ui.checkbox(
                &mut self.settings.minimize_while_decoding,
                "Minimize this window while decoding",
            );
        });

        if let Err(error) = parse_forward_key(self.settings.forward_key.trim()) {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Forward key: {error:#}"),
            );
        }
        if let Err(error) = &self.monitors {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }

        if !self.missing_permissions.is_empty() {
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                let names = self
                    .missing_permissions
                    .iter()
                    .map(|permission| permission.name())
                    .collect::<Vec<_>>()
                    .join(" and ");
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("Missing permission: {names}"),
                );
                if ui.small_button("Fix…").clicked() {
                    self.show_permissions = true;
                }
            });
        }

        ui.add_space(8.0);
        ui.separator();
        self.cache_status_ui(ui);
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let can_start = !running && self.settings.to_config().is_ok();
            if ui
                .add_enabled(
                    can_start,
                    egui::Button::new("▶ Start").min_size(egui::vec2(90.0, 28.0)),
                )
                .clicked()
            {
                if self.missing_permissions.is_empty() {
                    self.start(ui.ctx());
                } else {
                    self.show_permissions = true;
                }
            }
            if ui
                .add_enabled(
                    running,
                    egui::Button::new("■ Stop").min_size(egui::vec2(90.0, 28.0)),
                )
                .clicked()
            {
                self.stop();
            }
        });
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "After pressing Start, bring the window showing the QR codes to the front \
                 before the initial delay runs out, so it receives the forward key.",
            )
            .small()
            .weak(),
        );
    }

    fn monitor_label(&self, index: Option<usize>) -> String {
        let Some(index) = index else {
            return PRIMARY_MONITOR_LABEL.to_owned();
        };
        self.monitors
            .as_ref()
            .ok()
            .and_then(|monitors| monitors.iter().find(|monitor| monitor.index == index))
            .map(|monitor| format!("{index}: {}", monitor.description))
            .unwrap_or_else(|| format!("{index}: not connected"))
    }

    fn cache_status_ui(&self, ui: &mut egui::Ui) {
        match &self.cache_status {
            Ok(status) => match status.total_frames {
                None => {
                    ui.label("No frames cached yet.");
                }
                Some(total) => {
                    ui.label(format!(
                        "{} of {total} frame(s) cached.",
                        status.cached_frames
                    ));
                    if !status.missing_ranges.is_empty() {
                        const SHOWN: usize = 4;
                        let mut missing = status
                            .missing_ranges
                            .iter()
                            .take(SHOWN)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ");
                        if status.missing_ranges.len() > SHOWN {
                            missing.push_str(&format!(
                                " and {} more range(s)",
                                status.missing_ranges.len() - SHOWN
                            ));
                        }
                        ui.label(
                            egui::RichText::new(format!("Missing: {missing}"))
                                .small()
                                .weak(),
                        );
                    }
                }
            },
            Err(error) => {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        }
    }

    fn status_ui(&self, ui: &mut egui::Ui) {
        match &self.phase {
            Phase::Idle => {
                ui.heading("Ready");
            }
            Phase::Countdown(remaining) => {
                ui.heading(format!(
                    "Starting in {:.1} s — bring the QR code window to the front",
                    remaining.as_secs_f32()
                ));
            }
            Phase::Scanning => {
                ui.heading("Decoding…");
            }
            Phase::Completed(_) => {
                ui.heading("Done");
            }
            Phase::Cancelled => {
                ui.heading("Stopped — press Start to resume");
            }
            Phase::Failed(error) => {
                ui.heading("Failed");
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        }

        if let Some(progress) = &self.progress
            && let Some(total) = progress.total_frames
        {
            let fraction = progress.cached_frames as f32 / total.max(1) as f32;
            ui.add(egui::ProgressBar::new(fraction).text(format!(
                "{} / {total} frames · {} · ETA {}",
                progress.cached_frames,
                progress.rate(),
                progress.eta()
            )));
        }
        if self.running.is_some()
            && let Some(scan) = &self.last_scan
        {
            ui.label(egui::RichText::new(format!("Last screenshot: {scan}")).weak());
        }

        if let Phase::Completed(summary) = &self.phase {
            ui.add_space(4.0);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(format!(
                    "Decoded {} byte(s) from {} QR code(s) into {}",
                    summary.original_len,
                    summary.total_chunks,
                    summary.output.display()
                ));
                ui.horizontal(|ui| {
                    ui.label("SHA-256:");
                    ui.monospace(&summary.sha256);
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(summary.sha256.clone());
                    }
                });
            });
        }
    }
}

impl eframe::App for DecodeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.process_messages(&ctx);
        self.poll_permissions(&ctx);
        self.permissions_modal(&ctx);

        egui::Panel::left("settings-panel")
            .resizable(false)
            .exact_size(380.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                self.settings_ui(ui);
            });

        egui::Panel::bottom("activity-panel")
            .resizable(true)
            .default_size(120.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Activity").strong());
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.label(egui::RichText::new(line).monospace().small());
                        }
                    });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(8.0);
            self.status_ui(ui);
            ui.add_space(8.0);
            match &self.preview {
                Some(texture) => {
                    ui.add(
                        egui::Image::from_texture(texture)
                            .max_size(ui.available_size())
                            .maintain_aspect_ratio(true),
                    );
                }
                None => {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Screenshots appear here while decoding.").weak(),
                        );
                    });
                }
            }
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }
}

fn decode_worker(
    config: &DecodeConfig,
    cancel: &AtomicBool,
    sender: &Sender<WorkerMessage>,
    ctx: &egui::Context,
) {
    let send = |message| {
        // The receiver is gone once the app has shut down; nothing is left to notify.
        let _ = sender.send(message);
        ctx.request_repaint();
    };
    let mut last_preview: Option<Instant> = None;
    let result = run_decode(config, cancel, |event| match event {
        DecodeEvent::CacheLoaded(progress) => send(WorkerMessage::CacheLoaded(progress)),
        DecodeEvent::Countdown(remaining) => send(WorkerMessage::Countdown(remaining)),
        DecodeEvent::MonitorSelected { index, description } => send(WorkerMessage::Log(format!(
            "Capturing monitor {index} ({description})"
        ))),
        DecodeEvent::Screenshot(screenshot) => {
            if last_preview.is_none_or(|shown_at| shown_at.elapsed() >= PREVIEW_INTERVAL) {
                last_preview = Some(Instant::now());
                send(WorkerMessage::Preview(preview_image(screenshot)));
            }
        }
        DecodeEvent::Scanned { summary, .. } => send(WorkerMessage::Scanned(summary)),
        DecodeEvent::FrameCached { sequence, progress } => {
            if let Some(total) = progress.total_frames {
                send(WorkerMessage::Log(format!(
                    "Cached frame {sequence}/{total}"
                )));
            }
            send(WorkerMessage::FrameCached(progress));
        }
    });
    send(WorkerMessage::Finished(
        result.map_err(|error| format!("{error:#}")),
    ));
}

fn preview_image(screenshot: &RgbaImage) -> egui::ColorImage {
    let scale = (PREVIEW_MAX_WIDTH as f32 / screenshot.width().max(1) as f32).min(1.0);
    let width = ((screenshot.width() as f32 * scale).round() as u32).max(1);
    let height = ((screenshot.height() as f32 * scale).round() as u32).max(1);
    let thumbnail = image::imageops::thumbnail(screenshot, width, height);
    egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], thumbnail.as_raw())
}
