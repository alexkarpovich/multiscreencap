use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::thread;
use std::io::Write;
use tracing::{debug, error, info};

use crate::window::WindowInfo;

#[cfg(target_os = "macos")]
use crate::macos;



/// Builder for ffmpeg commands to separate concerns
pub struct FfmpegCommandBuilder {
    ffmpeg_path: PathBuf,
    width: usize,
    height: usize,
    fps: i32,
    bitrate_kbps: i32,
    output_path: PathBuf,
    window_x: i32,
    window_y: i32,
    window_id: u64,
}

impl FfmpegCommandBuilder {
    pub fn new(ffmpeg_path: PathBuf, width: usize, height: usize, fps: i32, bitrate_kbps: i32, output_path: PathBuf, window_x: i32, window_y: i32, window_id: u64) -> Self {
        Self {
            ffmpeg_path,
            width,
            height,
            fps,
            bitrate_kbps,
            output_path,
            window_x,
            window_y,
            window_id,
        }
    }
    
    pub fn build(&self) -> Command {
        let mut cmd = Command::new(&self.ffmpeg_path);
        cmd.arg("-hide_banner")
            .arg("-loglevel").arg("warning")
            .arg("-y");
        
        // Use raw video input from stdin for direct window capture
        cmd.arg("-f").arg("rawvideo")
            .arg("-pix_fmt").arg("rgba")
            .arg("-s").arg(format!("{}x{}", self.width, self.height))
            .arg("-r").arg(format!("{}", self.fps))
            .arg("-i").arg("-");  // Read from stdin
        
        // Output encoding with proper settings
        cmd.arg("-pix_fmt").arg("yuv420p")
            .arg("-c:v").arg("h264_videotoolbox")
            .arg("-b:v").arg(format!("{}k", self.bitrate_kbps))
            .arg("-maxrate").arg(format!("{}k", self.bitrate_kbps + 1000))
            .arg("-bufsize").arg(format!("{}k", self.bitrate_kbps * 2))
            .arg("-r").arg(format!("{}", self.fps))  // Ensure output frame rate
            .arg("-vsync").arg("cfr");  // Constant frame rate
        
        // Use frag_keyframe for better streaming compatibility and proper finalization
        cmd.arg("-movflags").arg("frag_keyframe+empty_moov+default_base_moof")
            .arg(&self.output_path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cmd
    }
}

/// Send quit signal to ffmpeg and wait for it to exit
pub fn send_quit_and_wait(child: &mut Child) -> Result<()> {
    // For avfoundation, send SIGTERM to gracefully stop recording
    if let Err(e) = child.kill() {
        error!("Failed to send SIGTERM to ffmpeg: {}", e);
    }
    
    // Wait for ffmpeg to finish processing and exit gracefully
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                info!("ffmpeg exited with status: {:?}", status);
                break;
            }
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(10) {
                    info!("ffmpeg didn't exit within 10s, force killing process");
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
            sanitize_filename::Options { truncate: true, ..Default::default() },
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
            sanitize_filename::Options { truncate: true, ..Default::default() },
        );
        format!("recording_{}_{}_{}.mp4", info.window_id, sanitized_title, ts)
    };
    
    let base_dir = output_dir
        .map(|d| d.to_path_buf())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    
    std::fs::create_dir_all(&base_dir)
        .with_context(|| format!("failed to create output directory: {}", base_dir.display()))?;
    
    Ok(base_dir.join(filename))
}

/// Start ffmpeg process for window recording
pub fn start_ffmpeg_for_window(
    ffmpeg: &PathBuf, 
    info: &WindowInfo, 
    fps: i32, 
    bitrate_kbps: i32, 
    output_dir: Option<&PathBuf>,
    custom_filename: Option<&str>,
) -> Result<(Child, Arc<AtomicBool>, PathBuf)> {
    let out_path = build_output_path(info, output_dir, custom_filename)?;
    info!("Recording window {} ({}x{}) -> {}", info.window_id, info.width, info.height, out_path.display());

    #[cfg(target_os = "macos")]
    {
        // First, capture a frame to get the actual window dimensions
        let (actual_width, actual_height) = if let Some((_, w, h)) = macos::capture_window_image(info.window_id) {
            info!("Detected actual window dimensions: {}x{}", w, h);
            (w, h)
        } else {
            error!("Failed to capture window to get dimensions, using stored dimensions");
            let w = if info.width % 2 != 0 { info.width + 1 } else { info.width }.max(2);
            let h = if info.height % 2 != 0 { info.height + 1 } else { info.height }.max(2);
            (w as usize, h as usize)
        };
        
        info!("Recording window {} at {}x{} with direct capture", info.window_id, actual_width, actual_height);
        
        // Build and spawn ffmpeg command with actual dimensions
        let cmd_builder = FfmpegCommandBuilder::new(
            ffmpeg.clone(), 
            actual_width, 
            actual_height, 
            fps, 
            bitrate_kbps, 
            out_path.clone(),
            info.x,
            info.y,
            info.window_id
        );
        let mut cmd = cmd_builder.build();
        
        // Log the command being executed
        info!("Executing ffmpeg command: {:?}", cmd);
        
        let mut child = cmd.stdin(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn ffmpeg for window {}", info.window_id))?;
        
        // Log ffmpeg stderr in background
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                use std::io::{BufRead, BufReader};
                let reader = BufReader::new(stderr);
                for line in reader.lines().filter_map(|l| l.ok()) {
                    // Log all ffmpeg output for debugging
                    if line.contains("ERROR") || line.contains("WARNING") || line.contains("error") || line.contains("warning") {
                        error!("ffmpeg: {}", line);
                    } else if line.contains("Stream") || line.contains("audio") || line.contains("Audio") {
                        info!("ffmpeg: {}", line);
                    } else {
                        debug!("ffmpeg: {}", line);
                    }
                }
            });
        }
        
        // Create stop signal for the capture thread
        let stop_signal = Arc::new(AtomicBool::new(false));
        
        // Log ffmpeg stderr in background for debugging
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                use std::io::{BufRead, BufReader};
                let reader = BufReader::new(stderr);
                for line in reader.lines().filter_map(|l| l.ok()) {
                    if line.contains("ERROR") || line.contains("WARNING") || line.contains("error") || line.contains("warning") {
                        error!("ffmpeg: {}", line);
                    } else if line.contains("Stream") || line.contains("audio") || line.contains("Audio") {
                        info!("ffmpeg: {}", line);
                    } else {
                        debug!("ffmpeg: {}", line);
                    }
                }
            });
        }
        
        // Start window capture thread that feeds frames to ffmpeg
        let window_id = info.window_id;
        let fps = fps;
        let stop_signal_clone = stop_signal.clone();
        let expected_width = actual_width;
        let expected_height = actual_height;
        
        if let Some(stdin) = child.stdin.take() {
            std::thread::spawn(move || {
                info!("Starting direct window capture for window {} at {} FPS", window_id, fps);
                
                let frame_duration = Duration::from_millis(1000 / fps as u64);
                let mut stdin = stdin;
                
                while !stop_signal_clone.load(Ordering::Relaxed) {
                    let frame_start = Instant::now();
                    
                    // Capture the specific window frame
                    if let Some((buffer, captured_width, captured_height)) = macos::capture_window_image(window_id) {
                        // Verify dimensions match what ffmpeg expects
                        if captured_width == expected_width && captured_height == expected_height {
                            // Write the raw RGBA frame to ffmpeg stdin
                            if let Err(e) = stdin.write_all(&buffer) {
                                error!("Failed to write frame to ffmpeg: {}", e);
                                break;
                            }
                            if let Err(e) = stdin.flush() {
                                error!("Failed to flush frame to ffmpeg: {}", e);
                                break;
                            }
                        } else {
                            error!("Captured frame size {}x{} doesn't match expected {}x{}", 
                                captured_width, captured_height, expected_width, expected_height);
                        }
                    } else {
                        error!("Failed to capture window frame for window {}", window_id);
                    }
                    
                    // Maintain frame rate timing
                    let elapsed = frame_start.elapsed();
                    if elapsed < frame_duration {
                        thread::sleep(frame_duration - elapsed);
                    }
                }
                
                info!("Window capture thread stopped for window {}", window_id);
            });
        }
        
        info!("Recording {} (ID: {}) -> {}", info.window_title, info.window_id, out_path.display());
        Ok((child, stop_signal, out_path))
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
        "/opt/homebrew/bin/ffmpeg",      // Homebrew (Apple Silicon)
        "/usr/local/bin/ffmpeg",         // Homebrew (Intel)
        "/sw/bin/ffmpeg",                // Fink
        "/opt/local/bin/ffmpeg",         // MacPorts
        "/usr/bin/ffmpeg",               // System (rare)
    ];
    for c in candidates {
        let pb = PathBuf::from(c);
        if pb.exists() {
            return Some(pb);
        }
    }
    None
}

