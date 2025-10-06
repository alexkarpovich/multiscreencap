mod window;
mod recorder;
mod ffmpeg;
mod audio;

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
use audio::AudioDeviceManager;

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


// Tab selection enum
#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Windows,
    Settings,
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
    selected_tab: Tab, // Current tab selection
    audio_device_manager: AudioDeviceManager,
    selected_audio_device: Option<String>, // Selected audio input device ID
}

impl Default for AppState {
    fn default() -> Self {
        let ffmpeg_path = find_ffmpeg();
        let mut window_manager = WindowManager::new();
        let _ = window_manager.refresh();
        
        // Initialize audio device manager and select default device
        let mut audio_device_manager = AudioDeviceManager::new();
        let selected_audio_device = match audio_device_manager.enumerate_devices() {
            Ok(devices) => {
                // Find the default device or use the first one
                let device_id = devices.iter()
                    .find(|d| d.is_default)
                    .or_else(|| devices.first())
                    .map(|d| d.id.clone());
                
                // Start monitoring the selected device
                if let Some(ref device_id) = device_id {
                    if let Err(e) = audio_device_manager.start_level_monitoring(device_id) {
                        eprintln!("Failed to start audio level monitoring for {}: {}", device_id, e);
                    }
                }
                
                device_id
            }
            Err(e) => {
                eprintln!("Failed to enumerate audio devices: {}", e);
                None
            }
        };
        
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
            selected_tab: Tab::Windows, // Default to Windows tab
            audio_device_manager,
            selected_audio_device,
        }
    }
}

impl AppState {
    fn select_audio_device(&mut self, device_id: String) {
        // Stop monitoring previous device
        if let Some(ref old_device_id) = self.selected_audio_device {
            if old_device_id != &device_id {
                self.audio_device_manager.stop_level_monitoring(old_device_id);
            }
        }
        
        // Update selection
        self.selected_audio_device = Some(device_id.clone());
        
        // Start monitoring new device
        if let Err(e) = self.audio_device_manager.start_level_monitoring(&device_id) {
            eprintln!("Failed to start audio level monitoring for {}: {}", device_id, e);
        }
    }
    
    fn render_audio_level_indicator(&self, ui: &mut egui::Ui, level: f32) {
        ui.horizontal(|ui| {
            ui.label("Level:");
            
            // Create 14 bars (░░░░░░░░░░░░░░) with reduced spacing
            let bars = "░░░░░░░░░░░░░░";
            let num_bars = bars.len();
            let active_bars = (level * num_bars as f32).round() as usize;
            
            // Use a more compact layout by reducing spacing between characters
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0; // Remove horizontal spacing
                
                for (i, bar_char) in bars.chars().enumerate() {
                    let color = if i < active_bars {
                        // Color gradient from green to red
                        if i < num_bars / 3 {
                            egui::Color32::GREEN
                        } else if i < 2 * num_bars / 3 {
                            egui::Color32::YELLOW
                        } else {
                            egui::Color32::RED
                        }
                    } else {
                        ui.style().visuals.weak_text_color()
                    };
                    
                    ui.colored_label(color, bar_char.to_string());
                }
            });
            
            ui.add_space(8.0); // Small space before percentage
            
            // Show numeric level
            ui.label(format!("{:.1}%", level * 100.0));
        });
    }
    
    fn render_settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.heading("Recording Settings");
            ui.add_space(10.0);
            
            // Output directory setting
            ui.horizontal(|ui| {
                ui.label("📂 Output Directory:");
                if let Some(dir) = &self.config.output_dir {
                    ui.label(egui::RichText::new(dir.display().to_string()).small());
                } else {
                    ui.label(egui::RichText::new("(not set)").small().italics());
                }
                if ui.button("📁 Browse").clicked() {
                    let initial = self.config.output_dir.clone();
                    if let Some(path) = rfd::FileDialog::new()
                        .set_directory(initial.unwrap_or_else(|| PathBuf::from(".")))
                        .pick_folder() {
                        self.config.output_dir = Some(path);
                    }
                }
            });
            
            ui.add_space(10.0);
            
            // FPS setting
            ui.horizontal(|ui| {
                ui.label("FPS:");
                ui.add(egui::DragValue::new(&mut self.config.fps).range(1..=120));
                ui.label("frames per second");
            });
            
            ui.add_space(10.0);
            
            // Bitrate setting
            ui.horizontal(|ui| {
                ui.label("Bitrate:");
                ui.add(egui::DragValue::new(&mut self.config.bitrate_kbps).range(500..=50000));
                ui.label("kbps");
            });
            
            ui.add_space(10.0);
            
            // Encoder selection
            ui.horizontal(|ui| {
                ui.label("Encoder:");
                egui::ComboBox::from_id_salt("encoder_select")
                    .selected_text(match self.config.encoder {
                        ffmpeg::VideoEncoder::H264VideoToolbox => "H.264 VideoToolbox (Hardware)",
                        ffmpeg::VideoEncoder::H264VideoToolboxFallback => "H.264 VideoToolbox (Fallback)",
                        ffmpeg::VideoEncoder::Libx264 => "H.264 libx264 (Software)",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.config.encoder, ffmpeg::VideoEncoder::Libx264, "H.264 libx264 (Software)");
                        ui.selectable_value(&mut self.config.encoder, ffmpeg::VideoEncoder::H264VideoToolbox, "H.264 VideoToolbox (Hardware)");
                        ui.selectable_value(&mut self.config.encoder, ffmpeg::VideoEncoder::H264VideoToolboxFallback, "H.264 VideoToolbox (Fallback)");
                    });
            });
            
            ui.add_space(20.0);
            
            // Audio recording toggle
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.config.audio_enabled, "🎤 Record Audio");
                if self.config.audio_enabled {
                    ui.colored_label(egui::Color32::GREEN, "✓ Audio will be recorded");
                } else {
                    ui.colored_label(egui::Color32::GRAY, "Audio recording disabled");
                }
            });
            
            if self.config.audio_enabled {
                ui.label(egui::RichText::new("Note: Audio will be recorded from the selected audio input device above.").small().italics());
                ui.label(egui::RichText::new("For system audio (what's playing), install BlackHole and select it as your audio device.").small().italics());
            }
            
            ui.add_space(10.0);
            
            // Audio input device selection
            ui.horizontal(|ui| {
                ui.label("🎤 Audio Input:");
                egui::ComboBox::from_id_salt("audio_input_select")
                    .selected_text(
                        self.selected_audio_device.as_ref()
                            .and_then(|id| {
                                self.audio_device_manager.get_devices()
                                    .iter()
                                    .find(|d| d.id == *id)
                                    .map(|d| d.name.as_str())
                            })
                            .unwrap_or("No device selected")
                    )
                    .show_ui(ui, |ui| {
                        // Refresh devices button
                        if ui.button("🔄 Refresh").clicked() {
                            if let Ok(devices) = self.audio_device_manager.enumerate_devices() {
                                if self.selected_audio_device.is_none() && !devices.is_empty() {
                                    // Auto-select default device if none selected
                                    self.selected_audio_device = devices.iter()
                                        .find(|d| d.is_default)
                                        .or_else(|| devices.first())
                                        .map(|d| d.id.clone());
                                }
                            }
                        }
                        
                        ui.separator();
                        
                        let devices = self.audio_device_manager.get_devices().to_vec();
                        for device in devices {
                            let display_name = if device.is_default {
                                format!("{} (Default)", device.name)
                            } else {
                                device.name.clone()
                            };
                            
                            if ui.selectable_value(&mut self.selected_audio_device, Some(device.id.clone()), display_name).clicked() {
                                self.select_audio_device(device.id.clone());
                            }
                        }
                    });
            });
            
            
            // Audio level indicator
            if let Some(device_id) = &self.selected_audio_device {
                if let Some(monitor) = self.audio_device_manager.get_level_monitor(device_id) {
                    let level = monitor.get_level();
                    self.render_audio_level_indicator(ui, level);
                }
            }
            
            ui.add_space(20.0);
            
            // ffmpeg status
            ui.horizontal(|ui| {
                if self.ffmpeg_path.is_none() {
                    ui.colored_label(egui::Color32::RED, "⚠ ffmpeg not found");
                    ui.label("Install via Homebrew: brew install ffmpeg");
                } else {
                    ui.colored_label(egui::Color32::GREEN, "✓ ffmpeg found");
                    if let Some(path) = &self.ffmpeg_path {
                        ui.label(egui::RichText::new(path.display().to_string()).small());
                    }
                }
            });
            
            ui.add_space(20.0);
            
            // Permissions status
            #[cfg(target_os = "macos")]
            {
                ui.horizontal(|ui| {
                    if !self.has_permissions {
                        ui.colored_label(egui::Color32::RED, "⚠ Screen recording permission required");
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
                    } else {
                        ui.colored_label(egui::Color32::GREEN, "✓ Screen recording permission granted");
                    }
                });
            }
        });
    }
    
    fn render_windows_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut to_start: Vec<u64> = Vec::new();
        let mut to_stop: Vec<u64> = Vec::new();
        
        // Grid view with expandable inline previews - use full width and height
        egui::ScrollArea::vertical()
            .auto_shrink([false, false]) // Don't auto-shrink horizontally or vertically
            .show(ui, |ui| {
            let mut windows: Vec<_> = self.window_manager.windows().iter().cloned().collect();
            // Sort windows by window_id for consistent ordering
            windows.sort_by_key(|w| w.window_id);
            
            if windows.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No windows found. Click 'Refresh windows' to scan again.");
                });
            } else {
                // Use full available width and height
                let available_width = ui.available_width();
                let available_height = ui.available_height();
                ui.allocate_ui_with_layout(
                    egui::vec2(available_width, available_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        for window in &windows {
                            let is_rec = self.recorder.lock().is_recording(window.window_id);
                            self.render_window_with_expanded_content(ui, ctx, window, is_rec, &mut to_start, &mut to_stop);
                        }
                    }
                );
            }
        });

        for id in to_start {
            self.start_for_window(id);
        }
        
        for id in to_stop {
            self.stop_for_window(id);
        }
    }
    
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
        
        // Window row: |fixed expand icon|stretching content|fixed action buttons|
        ui.horizontal(|ui| {
            // 1. Fixed expand icon (30px width)
            ui.allocate_ui_with_layout(
                egui::vec2(30.0, ui.available_height()),
                egui::Layout::centered_and_justified(egui::Direction::TopDown),
                |ui| {
                    let preview_icon = if is_expanded { "▼" } else { "▶" };
                    if ui.button(preview_icon).clicked() {
                        if is_expanded {
                            // If currently expanded, close it
                            self.expanded_previews.remove(&window_id);
                        } else {
                            // If currently closed, close all others and open this one
                            self.expanded_previews.clear();
                            self.expanded_previews.insert(window_id, true);
                        }
                    }
                }
            );
            
            // 2. Stretching content with window name (fills remaining space)
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.vertical(|ui| {
                    // Window name and dimensions - use full width
                    ui.horizontal(|ui| {
                        // Window name with ellipsis truncation - takes remaining space
                        ui.label(window.display_name());
                        
                        // Dimensions text - fixed size
                        ui.label(egui::RichText::new(format!("({})", window.dimensions_str()))
                            .small()
                            .color(ui.style().visuals.weak_text_color()));
                    });
                    
                    // Status information
                    let is_starting = self.starting_recordings.lock().get(&window_id).copied().unwrap_or(false);
                    
                    if is_starting {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.colored_label(egui::Color32::YELLOW, "Starting...");
                        });
                    } else if is_rec {
                        // Show recording time
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
            });
            
            // 3. Fixed area for action buttons (100px width) - pulled to the right
            ui.allocate_ui_with_layout(
                egui::vec2(100.0, ui.available_height()),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.add_space(10.0); // 10px margin from right edge
                    
                    if is_rec {
                        if ui.button("⏹ Stop").clicked() {
                            to_stop.push(window_id);
                        }
                    } else {
                        if ui.button("⏺ Start").clicked() {
                            to_start.push(window_id);
                        }
                    }
                }
            );
        });
    
        // Expanded content below the fixed-height row
        if is_expanded {
            ui.add_space(6.0);
            ui.indent("expanded", |ui| {
                ui.horizontal(|ui| {
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
                                    || macos::capture_window_image(window_id),
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
                        },
                    );
    
                    ui.add_space(12.0);
    
                    // Settings panel (unchanged)
                    ui.vertical(|ui| {
                        let settings = self
                            .window_settings
                            .entry(window_id)
                            .or_insert_with(WindowRecordingSettings::default);
    
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
                                let initial = settings
                                    .output_folder
                                    .clone()
                                    .or_else(|| self.config.output_dir.clone());
                                if let Some(path) = rfd::FileDialog::new()
                                    .set_directory(initial.unwrap_or_else(|| PathBuf::from(".")))
                                    .pick_folder()
                                {
                                    settings.output_folder = Some(path);
                                }
                            }
                        });
    
                        ui.add_space(8.0);
    
                        ui.horizontal(|ui| {
                            ui.label("Filename:");
                        });
                        ui.horizontal(|ui| {
                            let mut filename = settings.custom_filename.clone().unwrap_or_default();
                            let response = ui.add_sized(
                                egui::vec2(200.0, 20.0),
                                egui::TextEdit::singleline(&mut filename).hint_text("auto-generated"),
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
    
    fn render_window_with_expanded_content(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        window: &window::WindowInfo,
        is_rec: bool,
        to_start: &mut Vec<u64>,
        to_stop: &mut Vec<u64>,
    ) {
        use egui::{Pos2, Rect};
    
        let window_id = window.window_id;
        let is_expanded = self.expanded_previews.get(&window_id).copied().unwrap_or(false);
    
        // Fixed metrics
        const EXPAND_W: f32 = 30.0;    // expand/collapse icon area width
        const SPACING_W: f32 = 10.0;   // spacing between expand button and window name
        const BUTTONS_W: f32 = 120.0;  // start/stop buttons area width
        const ROW_H: f32 = 32.0;       // row height
    
        // Allocate entire row once; split into explicit sub-rects to avoid layout drift
        let row_resp = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::hover());
        let row_rect = row_resp.0;
    
        // Row background removed as requested
    
        // Left fixed rect (expand icon)
        let expand_rect = Rect {
            min: row_rect.min,
            max: Pos2 { x: row_rect.min.x + EXPAND_W, y: row_rect.max.y },
        };
    
        // Right fixed rect (buttons)
        let buttons_rect = Rect {
            min: Pos2 { x: row_rect.max.x - BUTTONS_W, y: row_rect.min.y },
            max: row_rect.max,
        };
    
        // Middle fill rect (between expand and buttons, accounting for spacing)
        let mid_rect = Rect {
            min: Pos2 { x: expand_rect.max.x + SPACING_W, y: row_rect.min.y },
            max: Pos2 { x: buttons_rect.min.x, y: row_rect.max.y },
        };
    
        // 1) Expand/collapse icon (fixed left) - text only, no background/border/hover effects
        {
            ui.allocate_ui_at_rect(expand_rect, |ui| {
                ui.with_layout(egui::Layout::centered_and_justified(egui::Direction::LeftToRight), |ui| {
                    let preview_icon = if is_expanded { "▼" } else { "▶" };
                    let button_size = 24.0; // Button size for clickable area
                    let resp = ui.add_sized(egui::vec2(button_size, button_size), egui::Button::new(preview_icon)
                        .fill(egui::Color32::TRANSPARENT)
                        .stroke(egui::Stroke::NONE)
                        .rounding(egui::Rounding::ZERO));
                    if resp.clicked() {
                        // Toggle; keep the "single expanded" behavior you had
                        if is_expanded {
                            self.expanded_previews.remove(&window_id);
                        } else {
                            self.expanded_previews.clear();
                            self.expanded_previews.insert(window_id, true);
                        }
                    }
                });
            });
        }
    
        // 2) Middle: name and dimensions (vertical layout)
        {
            // Name and dimensions rect (full middle area)
            let name_dims_rect = mid_rect;

            // Name and dimensions: vertical layout, left-aligned
            {
                ui.allocate_ui_at_rect(name_dims_rect, |ui| {
                    ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                        // Window name: left-aligned, non-wrapping, truncates with ellipsis
                        let name_label = egui::Label::new(egui::RichText::new(window.display_name()))
                            .truncate();
                        ui.add(name_label);
                        
                        // Dimensions: left-aligned, smaller text
                        let dims_text = format!("({})", window.dimensions_str());
                        ui.label(
                            egui::RichText::new(dims_text)
                                .small()
                                .color(ui.style().visuals.weak_text_color()),
                        );
                    });
                });
            }
        }
    
        // 3) Buttons: fixed area, flush right
        {
            ui.allocate_ui_at_rect(buttons_rect, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if is_rec {
                        // Create stop button with runtime and red styling
                        let runtime_text = if let Some(start_time) = self.recording_start_times.lock().get(&window_id) {
                            let duration = start_time.elapsed();
                            let total_seconds = duration.as_secs();
                            let minutes = total_seconds / 60;
                            let seconds = total_seconds % 60;
                            let milliseconds = duration.subsec_millis();
                            format!("{:02}:{:02}.{:03}", minutes, seconds, milliseconds)
                        } else {
                            "00:00.000".to_string()
                        };
                        
                        let stop_button_text = format!("⏹ Stop\n{}", runtime_text);
                        if ui.add_sized(egui::vec2(90.0, ROW_H), egui::Button::new(stop_button_text).fill(egui::Color32::from_rgb(220, 53, 69))).clicked() {
                            to_stop.push(window_id);
                        }
                    } else {
                        if ui.add_sized(egui::vec2(90.0, ROW_H), egui::Button::new("⏺ Start")).clicked() {
                            to_start.push(window_id);
                        }
                    }
                });
            });
        }
    
        // Expanded content below fixed-height row
        if is_expanded {
            ui.add_space(6.0);
            ui.indent("expanded", |ui| {
                ui.horizontal(|ui| {
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
                                    || macos::capture_window_image(window_id),
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
                        },
                    );
    
                    ui.add_space(12.0);
    
                    // Settings (unchanged)
                    ui.vertical(|ui| {
                        let settings = self
                            .window_settings
                            .entry(window_id)
                            .or_insert_with(WindowRecordingSettings::default);
    
                        ui.horizontal(|ui| {
                            ui.label("Output folder:");
                        });
                        ui.horizontal(|ui| {
                            if let Some(folder) = &settings.output_folder {
                                ui.label(egui::RichText::new(folder.display().to_string()).small());
                                if ui.small_button("❌").clicked() {
                                    settings.output_folder = None;
                                }
                            } else {
                                ui.label(egui::RichText::new("(use default)").small().italics());
                            }
                            if ui.small_button("📁").clicked() {
                                let initial = settings
                                    .output_folder
                                    .clone()
                                    .or_else(|| self.config.output_dir.clone());
                                if let Some(path) = rfd::FileDialog::new()
                                    .set_directory(initial.unwrap_or_else(|| PathBuf::from(".")))
                                    .pick_folder()
                                {
                                    settings.output_folder = Some(path);
                                }
                            }
                        });
    
                        ui.add_space(8.0);
    
                        ui.horizontal(|ui| {
                            ui.label("Filename:");
                        });
                        ui.horizontal(|ui| {
                            let mut filename = settings.custom_filename.clone().unwrap_or_default();
                            let response = ui.add_sized(
                                egui::vec2(200.0, 20.0),
                                egui::TextEdit::singleline(&mut filename).hint_text("auto-generated"),
                            );
                             if response.changed() {
                                 settings.custom_filename = if filename.is_empty() {
                                     None
                                 } else {
                                     Some(filename)
                                 };
                             }
                        });
                        
                        ui.add_space(8.0);
                        
                        // Audio level indicator for this window
                        if let Some(device_id) = &self.selected_audio_device {
                            if let Some(monitor) = self.audio_device_manager.get_level_monitor(device_id) {
                                let level = monitor.get_level();
                                self.render_audio_level_indicator(ui, level);
                            }
                        }
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
            let mut config = self.config.clone();
            // Set audio configuration from the selected device
            config.audio_input_device = if self.config.audio_enabled {
                self.selected_audio_device.clone()
            } else {
                None
            };
            
            std::thread::spawn(move || {
                match start_ffmpeg_for_window(&ffmpeg, &info, fps, bitrate, output_dir.as_ref(), custom_filename.as_deref(), &config) {
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
        let recordings_to_stop = rec.stop_all();
        
        // Clean up all recording start times immediately
        self.recording_start_times.lock().clear();
        
        self.status = "Stopping all recordings...".to_string();
        
        // Stop recordings in background thread to avoid blocking UI
        if !recordings_to_stop.is_empty() {
            std::thread::spawn(move || {
                for (mut child, stop_signal) in recordings_to_stop {
                    stop_signal.store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = send_quit_and_wait(&mut child);
                }
                info!("All recordings stopped");
            });
        }
    }

    fn stop_for_window(&mut self, id: u64) {
        let mut rec = self.recorder.lock();
        if let Some((child, stop_signal)) = rec.stop_recording(id) {
            // Clean up recording start time immediately
            self.recording_start_times.lock().remove(&id);
            
            self.status = format!("Stopping recording for window {}...", id);
            
            // Stop recording in background thread to avoid blocking UI
            std::thread::spawn(move || {
                stop_signal.store(true, std::sync::atomic::Ordering::Relaxed);
                let mut child = child;
                let _ = send_quit_and_wait(&mut child);
                
                // Wait a bit for ffmpeg to fully finalize the file
                std::thread::sleep(std::time::Duration::from_millis(500));
                
                info!("Stopped recording for window {}", id);
            });
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
        
        // Request UI refresh when audio monitoring is active for real-time level updates
        if self.selected_audio_device.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // Top toolbar with global actions
            ui.horizontal(|ui| {
                if ui.button("⏹ Stop All").clicked() {
                    self.stop_all();
                }
                
                ui.separator();
                
                // Show ffmpeg status as icon
                if self.ffmpeg_path.is_none() {
                    ui.colored_label(egui::Color32::RED, "⚠ ffmpeg not found");
                }
            });

            ui.separator();

            // Tab bar
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.selected_tab, Tab::Windows, "Windows");
                ui.selectable_value(&mut self.selected_tab, Tab::Settings, "Settings");
            });

            ui.separator();

            // Tab content
            match self.selected_tab {
                Tab::Windows => {
                    self.render_windows_tab(ui, ctx);
                }
                Tab::Settings => {
                    self.render_settings_tab(ui);
                }
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

