use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};

use crate::window::WindowInfo;
use crate::audio::get_ffmpeg_device_index;

#[cfg(target_os = "macos")]
use crate::macos;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VideoEncoder {
    H264VideoToolbox,
    H264VideoToolboxFallback,
    Libx264,
    // You can add ProRes/HEVC variants if you want different tradeoffs.
}

/// Builder for ffmpeg commands to separate concerns
pub struct FfmpegCommandBuilder {
    ffmpeg_path: PathBuf,
    width: usize,
    height: usize,
    fps: i32,
    bitrate_kbps: i32,
    output_path: PathBuf,
    encoder: VideoEncoder,
    audio_input_device: Option<String>,
}

impl FfmpegCommandBuilder {
    pub fn new(
        ffmpeg_path: PathBuf,
        width: usize,
        height: usize,
        fps: i32,
        bitrate_kbps: i32,
        output_path: PathBuf,
        encoder: VideoEncoder,
        audio_input_device: Option<String>,
    ) -> Self {
        Self {
            ffmpeg_path,
            width,
            height,
            fps,
            bitrate_kbps,
            output_path,
            encoder,
            audio_input_device,
        }
    }

    pub fn build(&self) -> Command {
        let mut cmd = Command::new(&self.ffmpeg_path);
        cmd.arg("-hide_banner")
            .arg("-loglevel")
            .arg("warning")
            .arg("-y");

        // rawvideo from stdin has no timestamps; -r defines input fps
        cmd.arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg("rgba")
            .arg("-s")
            .arg(format!("{}x{}", self.width, self.height))
            .arg("-r")
            .arg(format!("{}", self.fps))
            .arg("-i")
            .arg("-");

        // Add audio input if device is provided - this creates a second input stream
        if self.audio_input_device.is_some() {
            // Use avfoundation on macOS for audio capture
            #[cfg(target_os = "macos")]
            {
                // For macOS, map device names to ffmpeg device indices
                let device_index = self.audio_input_device.as_ref()
                    .and_then(|device_name| get_ffmpeg_device_index(device_name))
                    .unwrap_or(2); // Default to MacBook Pro Microphone
                
                
                cmd.arg("-f")
                    .arg("avfoundation")
                    .arg("-i")
                    .arg(format!(":{}", device_index));
            }
            #[cfg(not(target_os = "macos"))]
            {
                // For non-macOS platforms, use default audio input
                cmd.arg("-f")
                    .arg("pulse")
                    .arg("-i")
                    .arg("default");
            }
        }

        // Force CFR on output to match wall-clock emission
        cmd.arg("-vsync")
            .arg("cfr")
            .arg("-r")
            .arg(format!("{}", self.fps))
            .arg("-pix_fmt")
            .arg("yuv420p");

        match self.encoder {
            VideoEncoder::H264VideoToolbox => {
                // Ensure bitrate is within VideoToolbox limits and dimensions are valid
                let safe_bitrate = self.bitrate_kbps.min(50000).max(500);
                // Ensure dimensions are even numbers (required by VideoToolbox)
                let safe_width = if self.width % 2 == 0 { self.width } else { self.width - 1 };
                let safe_height = if self.height % 2 == 0 { self.height } else { self.height - 1 };
                
                cmd.arg("-c:v")
                    .arg("h264_videotoolbox")
                    .arg("-b:v")
                    .arg(format!("{}k", safe_bitrate))
                    .arg("-maxrate")
                    .arg(format!("{}k", safe_bitrate + 1000))
                    .arg("-bufsize")
                    .arg(format!("{}k", safe_bitrate * 2))
                    .arg("-g")
                    .arg(format!("{}", self.fps * 2))
                    .arg("-profile:v")
                    .arg("high")
                    .arg("-level")
                    .arg("4.1")
                    .arg("-allow_sw")
                    .arg("1")
                    .arg("-realtime")
                    .arg("1")
                    .arg("-s")
                    .arg(format!("{}x{}", safe_width, safe_height));
            }
            VideoEncoder::H264VideoToolboxFallback => {
                // More conservative VideoToolbox settings
                let safe_bitrate = self.bitrate_kbps.min(20000).max(1000);
                // Ensure dimensions are even numbers (required by VideoToolbox)
                let safe_width = if self.width % 2 == 0 { self.width } else { self.width - 1 };
                let safe_height = if self.height % 2 == 0 { self.height } else { self.height - 1 };
                
                cmd.arg("-c:v")
                    .arg("h264_videotoolbox")
                    .arg("-b:v")
                    .arg(format!("{}k", safe_bitrate))
                    .arg("-profile:v")
                    .arg("main")
                    .arg("-level")
                    .arg("3.1")
                    .arg("-allow_sw")
                    .arg("1")
                    .arg("-s")
                    .arg(format!("{}x{}", safe_width, safe_height));
            }
            VideoEncoder::Libx264 => {
                cmd.arg("-c:v")
                    .arg("libx264")
                    .arg("-preset")
                    .arg("veryfast")
                    .arg("-tune")
                    .arg("zerolatency")
                    .arg("-b:v")
                    .arg(format!("{}k", self.bitrate_kbps))
                    .arg("-g")
                    .arg(format!("{}", self.fps * 2))
                    .arg("-x264-params")
                    .arg(format!(
                        "keyint={}:min-keyint={}:scenecut=0",
                        self.fps * 2,
                        self.fps
                    ));
            }
        }

        // Add audio codec if device is provided
        if self.audio_input_device.is_some() {
            cmd.arg("-c:a")
                .arg("aac")
                .arg("-b:a")
                .arg("192k") // Higher bitrate for better quality
                .arg("-ar")
                .arg("48000") // Higher sample rate
                .arg("-ac")
                .arg("2") // Stereo
                .arg("-af")
                .arg("highpass=f=80,lowpass=f=15000,volume=0.8") // Noise reduction and volume normalization
                .arg("-map")
                .arg("0:v") // Map video from first input (stdin)
                .arg("-map")
                .arg("1:a") // Map audio from second input (audio device)
                .arg("-shortest"); // End when the shortest input ends
        } else {
            // If no audio, just map the video stream
            cmd.arg("-map")
                .arg("0:v");
        }

        // MP4 with faststart for better compatibility
        cmd.arg("-movflags")
            .arg("faststart")
            .arg(&self.output_path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cmd
    }
}

/// Spawn ffmpeg with the chosen encoder; stdin is piped for raw frames.
fn spawn_ffmpeg_checked(
    ffmpeg: &PathBuf,
    width: usize,
    height: usize,
    fps: i32,
    bitrate_kbps: i32,
    out_path: &PathBuf,
    encoder: VideoEncoder,
    audio_input_device: Option<String>,
) -> Result<Child> {
    // Log audio configuration for debugging
    if audio_input_device.is_some() {
        info!("Audio recording enabled with device: {:?}", audio_input_device);
    } else {
        info!("Audio recording disabled");
    }
    
    let builder = FfmpegCommandBuilder::new(
        ffmpeg.clone(),
        width,
        height,
        fps,
        bitrate_kbps,
        out_path.clone(),
        encoder,
        audio_input_device,
    );
    let mut cmd = builder.build();
    info!("Executing ffmpeg command: {:?}", cmd);
    
    // Log the full command as a string for debugging
    let cmd_str = format!("{:?}", cmd);
    info!("Full ffmpeg command: {}", cmd_str);

    let child = cmd
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| "failed to spawn ffmpeg")?;
    
    // Log that ffmpeg process started
    info!("ffmpeg process started successfully");

    Ok(child)
}

/// Check if ffmpeg process failed due to VideoToolbox encoder issues
fn is_videotoolbox_error(child: &mut Child) -> bool {
    if let Ok(Some(status)) = child.try_wait() {
        if !status.success() {
            // Check stderr for VideoToolbox-specific errors
            if let Some(stderr) = child.stderr.as_mut() {
                let mut stderr_content = String::new();
                if std::io::Read::read_to_string(stderr, &mut stderr_content).is_ok() {
                    return stderr_content.contains("h264_videotoolbox") && 
                           (stderr_content.contains("-12903") || 
                            stderr_content.contains("-12902") ||
                            stderr_content.contains("cannot create compression session") ||
                            stderr_content.contains("cannot prepare encoder") ||
                            stderr_content.contains("Error while opening encoder"));
                }
            }
        }
    }
    false
}

/// Send quit signal to ffmpeg and wait for it to exit
pub fn send_quit_and_wait(child: &mut Child) -> Result<()> {
    info!("Stopping ffmpeg process...");

    // Close stdin first to signal end of input
    if let Some(stdin) = child.stdin.take() {
        drop(stdin);
    }
    
    // Give ffmpeg a moment to process the EOF
    std::thread::sleep(Duration::from_millis(100));

    // Wait for ffmpeg to finish processing and exit gracefully
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                info!("ffmpeg exited with status: {:?}", status);
                if !status.success() {
                    error!("ffmpeg exited with error status: {:?}", status);
                }
                break;
            }
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(5) {
                    info!("ffmpeg didn't exit within 5s, force killing process");
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                error!("Error waiting for ffmpeg: {}", e);
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }

    // Give filesystem extra time to flush
    std::thread::sleep(Duration::from_millis(500));
    info!("ffmpeg process stopped");
    Ok(())
}

/// Build output file path for recording
pub fn build_output_path(
    info: &WindowInfo,
    output_dir: Option<&PathBuf>,
    custom_filename: Option<&str>,
) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs();

    // Use custom filename or generate default
    let filename = if let Some(custom_name) = custom_filename {
        // Sanitize custom filename and ensure .mp4 extension
        let sanitized = sanitize_filename::sanitize_with_options(
            custom_name,
            sanitize_filename::Options {
                truncate: true,
                ..Default::default()
            },
        );
        if sanitized.ends_with(".mp4") {
            sanitized
        } else {
            format!("{}_{}.mp4", sanitized, ts)
        }
    } else {
        // Default auto-generated filename
        let sanitized_title = sanitize_filename::sanitize_with_options(
            format!("{}_{}", info.owner_name, info.window_title),
            sanitize_filename::Options {
                truncate: true,
                ..Default::default()
            },
        );
        format!(
            "recording_{}_{}_{}.mp4",
            info.window_id, sanitized_title, ts
        )
    };

    let base_dir = output_dir
        .map(|d| d.to_path_buf())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    std::fs::create_dir_all(&base_dir)
        .with_context(|| format!("failed to create output directory: {}", base_dir.display()))?;

    Ok(base_dir.join(filename))
}

/// Nearest-neighbor resize of RGBA buffer to a fixed size
fn resize_rgba_nn(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return vec![0u8; dw.saturating_mul(dh).saturating_mul(4)];
    }
    let mut dst = vec![0u8; dw * dh * 4];
    let x_ratio = (sw as f64) / (dw as f64);
    let y_ratio = (sh as f64) / (dh as f64);

    for y in 0..dh {
        let sy = (y as f64 * y_ratio).floor() as usize;
        let sy = sy.min(sh - 1);
        let dst_row = y * dw * 4;
        let src_row = sy * sw * 4;
        for x in 0..dw {
            let sx = (x as f64 * x_ratio).floor() as usize;
            let sx = sx.min(sw - 1);
            let s_idx = src_row + sx * 4;
            let d_idx = dst_row + x * 4;
            dst[d_idx..d_idx + 4].copy_from_slice(&src[s_idx..s_idx + 4]);
        }
    }
    dst
}

/// Start ffmpeg process for window recording
pub fn start_ffmpeg_for_window(
    ffmpeg: &PathBuf,
    info: &WindowInfo,
    fps: i32,
    bitrate_kbps: i32,
    output_dir: Option<&PathBuf>,
    custom_filename: Option<&str>,
    config: &crate::recorder::RecordingConfig,
) -> Result<(Child, Arc<AtomicBool>, PathBuf)> {
    let out_path = build_output_path(info, output_dir, custom_filename)?;
    info!(
        "Recording window {} ({}x{}) -> {}",
        info.window_id,
        info.width,
        info.height,
        out_path.display()
    );

    #[cfg(target_os = "macos")]
    {
        // First capture to discover actual size and seed a frame
        let (mut actual_w, mut actual_h, mut last_frame) =
            if let Some((buffer, w, h)) = macos::capture_window_image(info.window_id) {
                info!("Detected actual window dimensions: {}x{}", w, h);
                (w, h, Some(buffer))
            } else {
                warn!("Failed to capture window for dimensions; using stored values");
                (
                    info.width.max(2) as usize,
                    info.height.max(2) as usize,
                    None,
                )
            };

        // Enforce even dimensions for YUV420 encoders
        if actual_w % 2 != 0 {
            actual_w += 1;
        }
        if actual_h % 2 != 0 {
            actual_h += 1;
        }

        let expected_w = actual_w;
        let expected_h = actual_h;
        info!("Fixed stream size: {}x{}", expected_w, expected_h);

        // Normalize the seeded frame if it doesn't match expected size
        if let Some(ref buf) = last_frame {
            // We know the real w,h from the capture above; if mismatch, normalize
            if let Some((_, w, h)) = macos::capture_window_image(info.window_id) {
                if w != expected_w || h != expected_h {
                    last_frame = Some(resize_rgba_nn(buf, w, h, expected_w, expected_h));
                }
            }
        }

        // Use encoder from config
        let mut encoder = config.encoder;
        let mut child = spawn_ffmpeg_checked(
            ffmpeg,
            expected_w,
            expected_h,
            fps,
            bitrate_kbps,
            &out_path,
            encoder,
            config.audio_input_device.clone(),
        )
        .context("failed to spawn ffmpeg (hardware)")?;

        // If ffmpeg exits early or has VideoToolbox errors, fall back to libx264
        thread::sleep(Duration::from_millis(250));
        if let Ok(Some(status)) = child.try_wait() {
            error!("Hardware encoder process exited immediately: {:?}", status);
            encoder = VideoEncoder::Libx264;
            child = spawn_ffmpeg_checked(
                ffmpeg,
                expected_w,
                expected_h,
                fps,
                bitrate_kbps,
                &out_path,
                encoder,
                config.audio_input_device.clone(),
            )
            .context("failed to spawn ffmpeg (libx264 fallback)")?;
            info!(
                "Using software encoder (libx264) for window {}",
                info.window_id
            );
        } else if is_videotoolbox_error(&mut child) {
            error!("VideoToolbox encoder failed, trying fallback configuration");
            // Kill the failed process
            let _ = child.kill();
            encoder = VideoEncoder::H264VideoToolboxFallback;
            child = spawn_ffmpeg_checked(
                ffmpeg,
                expected_w,
                expected_h,
                fps,
                bitrate_kbps,
                &out_path,
                encoder,
                config.audio_input_device.clone(),
            )
            .context("failed to spawn ffmpeg (VideoToolbox fallback)")?;
            
            // Check if fallback also fails
            thread::sleep(Duration::from_millis(250));
            if let Ok(Some(status)) = child.try_wait() {
                error!("VideoToolbox fallback also failed: {:?}, using libx264", status);
                encoder = VideoEncoder::Libx264;
                child = spawn_ffmpeg_checked(
                    ffmpeg,
                    expected_w,
                    expected_h,
                    fps,
                    bitrate_kbps,
                    &out_path,
                    encoder,
                    config.audio_input_device.clone(),
                )
                .context("failed to spawn ffmpeg (libx264 fallback)")?;
                info!(
                    "Using software encoder (libx264) for window {}",
                    info.window_id
                );
            } else {
                info!(
                    "Using VideoToolbox fallback encoder for window {}",
                    info.window_id
                );
            }
        } else {
            info!("Hardware encoder started OK for window {}", info.window_id);
        }

        // Log ffmpeg stderr in background (single reader)
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().filter_map(|l| l.ok()) {
                    let low = line.to_ascii_lowercase();
                    if low.contains("error") || low.contains("warning") {
                        error!("ffmpeg: {}", line);
                    } else if line.contains("Stream") || line.contains("audio") || line.contains("Audio") {
                        info!("ffmpeg: {}", line);
                    } else {
                        debug!("ffmpeg: {}", line);
                    }
                }
            });
        }

        // Create stop signal for the capture/emitter thread
        let stop_signal = Arc::new(AtomicBool::new(false));

        // Start window capture thread that feeds frames to ffmpeg
        let window_id = info.window_id;
        let fps_i32 = fps;
        let fps_u64 = fps as u64;
        let stop_signal_clone = stop_signal.clone();

        // Take stdin so we can write frames
        if let Some(stdin) = child.stdin.take() {
            std::thread::spawn(move || {
                info!(
                    "Starting direct window capture for window {} at {} FPS",
                    window_id, fps_i32
                );

                // Fixed emission schedule based on wall clock
                let frame_interval = Duration::from_nanos(1_000_000_000 / fps_u64);
                let mut next_due = Instant::now() + frame_interval;

                let mut frame_count: u64 = 0;
                let start_time = Instant::now();

                let mut writer = BufWriter::with_capacity(1 << 20, stdin); // 1 MiB buffer

                // Seed a first frame if missing
                if last_frame.is_none() {
                    loop {
                        if let Some((buffer, w, h)) = macos::capture_window_image(window_id) {
                            let normalized = if w == expected_w && h == expected_h {
                                buffer
                            } else {
                                debug!(
                                    "Initial capture {}x{} != expected {}x{}, normalizing",
                                    w, h, expected_w, expected_h
                                );
                                resize_rgba_nn(&buffer, w, h, expected_w, expected_h)
                            };
                            last_frame = Some(normalized);
                            break;
                        }
                        if stop_signal_clone.load(Ordering::Relaxed) {
                            info!("Stopped before first frame was captured");
                            return;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                }

                // Track last different source size to avoid log spam
                let mut last_src_w: usize = expected_w;
                let mut last_src_h: usize = expected_h;

                loop {
                    if stop_signal_clone.load(Ordering::Relaxed) {
                        break;
                    }

                    // 1) Emit frames that are due (handles back-pressure correctly)
                    while Instant::now() >= next_due {
                        if let Some(ref buf) = last_frame {
                            if let Err(e) = writer.write_all(buf) {
                                error!("Failed to write frame to ffmpeg: {}", e);
                                return;
                            }
                            frame_count += 1;

                            if frame_count % (fps_u64.max(1)) == 0 {
                                let elapsed = start_time.elapsed();
                                let effective_fps = frame_count as f64 / elapsed.as_secs_f64();
                                info!(
                                    "Emitted {} frames in {:.2}s (effective FPS: {:.2}, target: {})",
                                    frame_count, elapsed.as_secs_f64(), effective_fps, fps_i32
                                );
                            }
                        } else {
                            thread::sleep(Duration::from_millis(1));
                        }
                        next_due += frame_interval;
                    }

                    // 2) Try to refresh last_frame with a new capture if we have time
                    if let Some((buffer, w, h)) = macos::capture_window_image(window_id) {
                        if w != expected_w || h != expected_h {
                            if w != last_src_w || h != last_src_h {
                                warn!(
                                    "Captured frame size {}x{} doesn't match expected {}x{} — normalizing",
                                    w, h, expected_w, expected_h
                                );
                                last_src_w = w;
                                last_src_h = h;
                            }
                            let normalized = resize_rgba_nn(&buffer, w, h, expected_w, expected_h);
                            last_frame = Some(normalized);
                        } else {
                            last_frame = Some(buffer);
                            last_src_w = w;
                            last_src_h = h;
                        }
                    } else {
                        debug!("Window capture returned None; reusing last frame");
                    }

                    // 3) Sleep a little until the next due time to avoid busy-wait
                    let now = Instant::now();
                    if next_due > now {
                        let sleep_for = (next_due - now).min(Duration::from_millis(2));
                        thread::sleep(sleep_for);
                    }
                }

                if let Err(e) = writer.flush() {
                    error!("Failed to flush frames to ffmpeg: {}", e);
                }

                let total_elapsed = start_time.elapsed();
                let effective_fps = if total_elapsed.as_secs_f64() > 0.0 {
                    frame_count as f64 / total_elapsed.as_secs_f64()
                } else {
                    0.0
                };
                info!(
                    "Recording completed: {} frames in {:.2}s (effective FPS: {:.2}, expected: {})",
                    frame_count, total_elapsed.as_secs_f64(), effective_fps, fps_i32
                );
                info!("Window capture thread stopped for window {}", window_id);
            });
        }

        info!(
            "Recording {} (ID: {}) -> {}",
            info.window_title,
            info.window_id,
            out_path.display()
        );
        return Ok((child, stop_signal, out_path));
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err(anyhow!("Window capture is only supported on macOS"))
    }
}

/// Find ffmpeg executable in common locations
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Ok(p) = which::which("ffmpeg") {
        return Some(p);
    }
    let candidates = [
        "/opt/homebrew/bin/ffmpeg", // Homebrew (Apple Silicon)
        "/usr/local/bin/ffmpeg",    // Homebrew (Intel)
        "/sw/bin/ffmpeg",           // Fink
        "/opt/local/bin/ffmpeg",    // MacPorts
        "/usr/bin/ffmpeg",          // System (rare)
    ];
    for c in candidates {
        let pb = PathBuf::from(c);
        if pb.exists() {
            return Some(pb);
        }
    }
    None
}
