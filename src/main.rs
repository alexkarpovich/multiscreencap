mod window;
mod recorder;
mod ffmpeg;

#[cfg(target_os = "macos")]
mod macos;

use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;
use tracing::{error, info};

use window::WindowManager;
use recorder::{RecorderState, RecordingConfig};
use ffmpeg::{find_ffmpeg, start_ffmpeg_for_window, send_quit_and_wait};

// Cache for window preview textures with throttling
struct PreviewCache {
    textures: HashMap<u64, egui::TextureHandle>,
    last_update: HashMap<u64, Instant>,
    update_interval: Duration,
}

impl PreviewCache {
    fn new() -> Self {
        Self {
            textures: HashMap::new(),
            last_update: HashMap::new(),
            update_interval: Duration::from_millis(1000), // Update preview every 1000ms max
        }
    }
    
    fn should_update(&self, window_id: u64) -> bool {
        match self.last_update.get(&window_id) {
            Some(last) => last.elapsed() >= self.update_interval,
            None => true, // Never updated, should update
        }
    }
    
    fn get_or_update(
        &mut self,
        ctx: &egui::Context,
        window_id: u64,
        capture_fn: impl FnOnce() -> Option<(Vec<u8>, usize, usize)>,
    ) -> Option<&egui::TextureHandle> {
        if self.should_update(window_id) {
            if let Some((buffer, width, height)) = capture_fn() {
                // Downscale image for preview to reduce memory and GPU load
                let (small_buffer, small_width, small_height) = 
                    downscale_image(&buffer, width, height, 512); // Max 512px width
                
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [small_width, small_height],
                    &small_buffer,
                );
                let texture = ctx.load_texture(
                    format!("card_preview_{}", window_id),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                
                self.textures.insert(window_id, texture);
                self.last_update.insert(window_id, Instant::now());
            }
        }
        
        self.textures.get(&window_id)
    }
}

// Downscale RGBA image to reduce preview size
fn downscale_image(buffer: &[u8], width: usize, height: usize, max_width: usize) -> (Vec<u8>, usize, usize) {
    if width <= max_width {
        return (buffer.to_vec(), width, height);
    }
    
    let scale = max_width as f32 / width as f32;
    let new_width = max_width;
    let new_height = (height as f32 * scale) as usize;
    
    let mut result = vec![0u8; new_width * new_height * 4];
    
    // Simple nearest-neighbor downscaling (fast)
    for y in 0..new_height {
        for x in 0..new_width {
            let src_x = (x as f32 / scale) as usize;
            let src_y = (y as f32 / scale) as usize;
            
            let src_idx = (src_y * width + src_x) * 4;
            let dst_idx = (y * new_width + x) * 4;
            
            result[dst_idx..dst_idx + 4].copy_from_slice(&buffer[src_idx..src_idx + 4]);
        }
    }
    
    (result, new_width, new_height)
}

// Per-window recording settings
#[derive(Clone, Default)]
struct WindowRecordingSettings {
    output_folder: Option<PathBuf>,
    custom_filename: Option<String>,
}


// Application state
struct AppState {
    window_manager: WindowManager,
    recorder: Arc<Mutex<RecorderState>>,
    config: RecordingConfig,
    ffmpeg_path: Option<PathBuf>,
    status: String,
    has_permissions: bool,
    preview_cache: Mutex<PreviewCache>,
    expanded_previews: HashMap<u64, bool>, // Track which windows have preview+settings expanded
    window_settings: HashMap<u64, WindowRecordingSettings>, // Per-window overrides
    starting_recordings: Arc<Mutex<HashMap<u64, bool>>>, // Track which windows are starting
    recording_start_times: Arc<Mutex<HashMap<u64, std::time::Instant>>>, // Track recording start times
}

impl Default for AppState {
    fn default() -> Self {
        let ffmpeg_path = find_ffmpeg();
        let mut window_manager = WindowManager::new();
        let _ = window_manager.refresh();
        
        Self {
            window_manager,
            recorder: Arc::new(Mutex::new(RecorderState::new())),
            config: RecordingConfig::new(),
            ffmpeg_path: ffmpeg_path.clone(),
            status: String::new(),
            has_permissions: {
                #[cfg(target_os = "macos")]
                { macos::has_screen_capture_access() }
                #[cfg(not(target_os = "macos"))]
                { true }
            },
            preview_cache: Mutex::new(PreviewCache::new()),
            expanded_previews: HashMap::new(),
            window_settings: HashMap::new(),
            starting_recordings: Arc::new(Mutex::new(HashMap::new())),
            recording_start_times: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl AppState {
    fn render_window_row(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        window: &window::WindowInfo,
        is_rec: bool,
        to_start: &mut Vec<u64>,
        to_stop: &mut Vec<u64>,
    ) {
        let window_id = window.window_id;
        let is_expanded = self.expanded_previews.get(&window_id).copied().unwrap_or(false);
        
        // Main row with window info and action buttons
        ui.horizontal(|ui| {
            // Preview toggle button
            let preview_icon = if is_expanded { "▼" } else { "▶" };
            if ui.button(preview_icon).clicked() {
                self.expanded_previews.insert(window_id, !is_expanded);
            }
            
            // Window info section (left side) - takes available space
            ui.vertical(|ui| {
                // Window name and dimensions
                ui.horizontal(|ui| {
                    ui.label(window.display_name());
                    ui.label(egui::RichText::new(format!("({})", window.dimensions_str()))
                        .small()
                        .color(ui.style().visuals.weak_text_color()));
                });
                
                // Status/state information right under window name
                let is_starting = self.starting_recordings.lock().get(&window_id).copied().unwrap_or(false);
                
                if is_starting {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.colored_label(egui::Color32::YELLOW, "Starting...");
                    });
                } else if is_rec {
                    // Show recording time with real-time updates
                    if let Some(start_time) = self.recording_start_times.lock().get(&window_id) {
                        let duration = start_time.elapsed();
                        let total_seconds = duration.as_secs();
                        let minutes = total_seconds / 60;
                        let seconds = total_seconds % 60;
                        let milliseconds = duration.subsec_millis();
                        ui.horizontal(|ui| {
                            ui.colored_label(egui::Color32::GREEN, "● REC");
                            ui.label(egui::RichText::new(format!("{:02}:{:02}.{:03}", minutes, seconds, milliseconds))
                                .color(egui::Color32::GREEN)
                                .monospace());
                        });
                    }
                }
            });
            
            // Use right-to-left layout to properly position buttons on the right
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Add margin from the right edge first
                ui.add_space(10.0);
                
                // Action buttons fixed on the right side
                if is_rec {
                    if ui.button("⏹ Stop").clicked() {
                        to_stop.push(window_id);
                    }
                } else {
                    if ui.button("⏺ Start").clicked() {
                        to_start.push(window_id);
                    }
                }
            });
        });
        
        
        // Expanded view: preview on left, settings on right
        if is_expanded {
            ui.indent("expanded", |ui| {
                ui.horizontal(|ui| {
                    // Left: Preview
                    let preview_width = 400.0;
                    let preview_height = 225.0;
                    
                    ui.allocate_ui_with_layout(
                        egui::vec2(preview_width, preview_height),
                        egui::Layout::centered_and_justified(egui::Direction::TopDown),
                        |ui| {
                            #[cfg(target_os = "macos")]
                            {
                                let mut cache = self.preview_cache.lock();
                                
                                if let Some(texture) = cache.get_or_update(
                                    ctx,
                                    window_id,
                                    || macos::capture_window_image(window_id)
                                ) {
                                    let size = texture.size_vec2();
                                    let scale = (preview_width / size.x).min(preview_height / size.y).min(1.0);
                                    let display_size = size * scale;
                                    
                                    ui.image((texture.id(), display_size));
                                } else {
                                    ui.label("Failed to capture preview");
                                }
                            }
                            
                            #[cfg(not(target_os = "macos"))]
                            {
                                ui.label("Preview not available on this platform");
                            }
                        }
                    );
                    
                    ui.add_space(12.0);
                    
                    // Right: Settings
                    ui.vertical(|ui| {
                        let settings = self.window_settings.entry(window_id).or_insert_with(WindowRecordingSettings::default);
                        
                        // Custom output folder
                        ui.horizontal(|ui| {
                            ui.label("Output folder:");
                        });
                        ui.horizontal(|ui| {
                            if let Some(ref folder) = settings.output_folder {
                                ui.label(egui::RichText::new(folder.display().to_string()).small());
                                if ui.small_button("❌").clicked() {
                                    settings.output_folder = None;
                                }
                            } else {
                                ui.label(egui::RichText::new("(use default)").small().italics());
                            }
                            if ui.small_button("📁").clicked() {
                                let initial = settings.output_folder.clone().or_else(|| self.config.output_dir.clone());
                                if let Some(path) = rfd::FileDialog::new().set_directory(initial.unwrap_or_else(|| PathBuf::from("."))).pick_folder() {
                                    settings.output_folder = Some(path);
                                }
                            }
                        });
                        
                        ui.add_space(8.0);
                        
                        // Custom filename
                        ui.horizontal(|ui| {
                            ui.label("Filename:");
                        });
                        ui.horizontal(|ui| {
                            let mut filename = settings.custom_filename.clone().unwrap_or_default();
                            let response = ui.add_sized(
                                egui::vec2(200.0, 20.0),
                                egui::TextEdit::singleline(&mut filename)
                                    .hint_text("auto-generated")
                            );
                            if response.changed() {
                                if filename.is_empty() {
                                    settings.custom_filename = None;
                                } else {
                                    settings.custom_filename = Some(filename);
                                }
                            }
                        });
                    });
                });
            });
        }
        
        ui.separator();
    }
    
    fn refresh_windows(&mut self) {
        match self.window_manager.refresh() {
            Ok(()) => {
                self.status = format!("Found {} windows", self.window_manager.windows().len());
            }
            Err(e) => {
                self.status = format!("Failed to list windows: {}", e);
            }
        }
    }

    fn start_for_window(&mut self, window_id: u64) {
        if self.ffmpeg_path.is_none() {
            self.status = "ffmpeg not found. Install via Homebrew: brew install ffmpeg".to_string();
            return;
        }
        
        let window_info = self.window_manager.get_window(window_id).cloned();
        
        if let Some(info) = window_info {
            let rec = self.recorder.clone();
            if rec.lock().is_recording(window_id) {
                return;
            }
            
            let ffmpeg = self.ffmpeg_path.clone().unwrap();
            let fps = self.config.fps.max(1);
            let bitrate = self.config.bitrate_kbps.max(500);
            
            // Get per-window settings or use defaults
            let window_settings = self.window_settings.get(&window_id).cloned();
            let output_dir = window_settings
                .as_ref()
                .and_then(|s| s.output_folder.clone())
                .or_else(|| self.config.output_dir.clone());
            let custom_filename = window_settings
                .and_then(|s| s.custom_filename.clone());
            
            // Mark as starting and record start time immediately
            self.starting_recordings.lock().insert(window_id, true);
            self.recording_start_times.lock().insert(window_id, std::time::Instant::now());
            
            let starting = self.starting_recordings.clone();
            
            // Start in background thread to avoid blocking UI
            std::thread::spawn(move || {
                match start_ffmpeg_for_window(&ffmpeg, &info, fps, bitrate, output_dir.as_ref(), custom_filename.as_deref()) {
                    Ok((child, stop_signal, _output_path)) => {
                        rec.lock().start_recording(window_id, child, stop_signal);
                        
                        // Wait a moment to ensure ffmpeg has actually started recording
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        
                        // Remove from starting state
                        starting.lock().remove(&window_id);
                        
                        info!("Started recording: {}", info.window_title);
                    }
                    Err(e) => {
                        starting.lock().remove(&window_id);
                        error!("Failed to start ffmpeg for {:?}: {}", info.window_title, e);
                    }
                }
            });
        }
    }

    fn stop_all(&mut self) {
        let mut rec = self.recorder.lock();
        for (mut child, stop_signal) in rec.stop_all() {
                stop_signal.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = send_quit_and_wait(&mut child);
        }
        
        // Clean up all recording start times
        self.recording_start_times.lock().clear();
        
        self.status = "Stopped all recordings".to_string();
    }

    fn stop_for_window(&mut self, id: u64) {
        let mut rec = self.recorder.lock();
        if let Some((mut child, stop_signal)) = rec.stop_recording(id) {
            stop_signal.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = send_quit_and_wait(&mut child);
            
            // Wait a bit for ffmpeg to fully finalize the file
            std::thread::sleep(std::time::Duration::from_millis(500));
            
            // Clean up recording start time
            self.recording_start_times.lock().remove(&id);
            
            self.status = format!("Stopped recording for window {}", id);
        }
    }
}

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Auto-refresh windows list every 3 seconds
        if self.window_manager.should_auto_refresh() {
            self.refresh_windows();
        }
        
        // Request UI refresh frequently when recordings are active for real-time timer updates
        if !self.recording_start_times.lock().is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // Toolbar
            ui.horizontal(|ui| {
                #[cfg(target_os = "macos")]
                {
                    if !self.has_permissions {
                        if ui.button("🔐 Grant Access").clicked() {
                            let granted = macos::request_screen_capture_access();
                            self.has_permissions = granted;
                            if !granted {
                                self.status = "Permission denied. Enable in System Settings > Privacy & Security > Screen Recording.".to_string();
                            } else {
                                self.status = "Permission granted.".to_string();
                                self.refresh_windows();
                            }
                        }
                    }
                }
                
                if ui.button("⏹ Stop All").clicked() {
                    self.stop_all();
                }
                
                ui.separator();
                
                if ui.button("📁 Output Folder…").clicked() {
                    let initial = self.config.output_dir.clone();
                    if let Some(path) = rfd::FileDialog::new().set_directory(initial.unwrap_or_else(|| PathBuf::from("."))).pick_folder() {
                        self.config.output_dir = Some(path);
                    }
                }
                
                ui.separator();
                
                // Show ffmpeg status as icon
                if self.ffmpeg_path.is_none() {
                    ui.colored_label(egui::Color32::RED, "⚠ ffmpeg not found");
                }
            });

            // Settings in compact horizontal layout
            ui.horizontal(|ui| {
                ui.label("📂");
                if let Some(dir) = &self.config.output_dir {
                    ui.label(egui::RichText::new(dir.display().to_string()).small());
                } else {
                    ui.label(egui::RichText::new("(not set)").small());
                }
                
                ui.separator();
                
                ui.label("FPS:");
                ui.add(egui::DragValue::new(&mut self.config.fps).range(1..=120));
                
                ui.label("Bitrate:");
                ui.add(egui::DragValue::new(&mut self.config.bitrate_kbps).range(500..=50000));
                ui.label("kbps");
            });

            ui.separator();

            let mut to_start: Vec<u64> = Vec::new();
            let mut to_stop: Vec<u64> = Vec::new();
            
            // List view with expandable inline previews
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut windows: Vec<_> = self.window_manager.windows().iter().cloned().collect();
                // Sort windows by window_id for consistent ordering
                windows.sort_by_key(|w| w.window_id);
                
                for window in &windows {
                    let is_rec = self.recorder.lock().is_recording(window.window_id);
                    self.render_window_row(ui, ctx, window, is_rec, &mut to_start, &mut to_stop);
                }
                
                if windows.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("No windows found. Click 'Refresh windows' to scan again.");
                    });
                }
            });

            for id in to_start {
                self.start_for_window(id);
            }
            
            for id in to_stop {
                self.stop_for_window(id);
            }
        });
        
        // Footer with status
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&self.status).small());
            });
        });
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .without_time()
        .init();

    let native_options = eframe::NativeOptions::default();
    let app = AppState::default();
    let res = eframe::run_native(
        "Screen Recorder",
        native_options,
        Box::new(|_cc| Ok(Box::new(app))),
    );
    match res {
        Ok(_) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("eframe error: {}", e)),
    }
}

