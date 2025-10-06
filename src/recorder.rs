use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::ffmpeg::VideoEncoder;

/// Configuration for recording
#[derive(Clone)]
pub struct RecordingConfig {
    pub fps: i32,
    pub bitrate_kbps: i32,
    pub output_dir: Option<PathBuf>,
    pub encoder: VideoEncoder,
    pub audio_input_device: Option<String>, // Audio input device ID
    pub audio_enabled: bool, // Whether to record audio
}

impl RecordingConfig {
    pub fn new() -> Self {
        // Set default output directory to current directory
        let default_dir = std::env::current_dir().ok();
        
        Self {
            fps: 30,
            bitrate_kbps: 6000,
            output_dir: default_dir,
            encoder: VideoEncoder::Libx264, // Default to software encoder for reliability
            audio_input_device: None,
            audio_enabled: false, // Default to no audio recording
        }
    }
}

/// Manages recording state and processes
pub struct RecorderState {
    running: HashMap<u64, (Child, Arc<AtomicBool>)>,
}

impl RecorderState {
    pub fn new() -> Self {
        Self { running: HashMap::new() }
    }

    pub fn is_recording(&self, window_id: u64) -> bool {
        self.running.contains_key(&window_id)
    }
    
    pub fn start_recording(&mut self, window_id: u64, child: Child, stop_signal: Arc<AtomicBool>) {
        self.running.insert(window_id, (child, stop_signal));
    }
    
    pub fn stop_recording(&mut self, window_id: u64) -> Option<(Child, Arc<AtomicBool>)> {
        self.running.remove(&window_id)
    }
    
    pub fn stop_all(&mut self) -> Vec<(Child, Arc<AtomicBool>)> {
        self.running.drain().map(|(_, v)| v).collect()
    }
}

