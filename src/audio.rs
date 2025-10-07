use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;

// Audio device enumeration will be implemented using Core Audio APIs
// For now, we use a simplified approach with hardcoded devices

/// Represents an audio input device
#[derive(Clone, Debug, PartialEq)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Audio level monitoring for a device
pub struct AudioLevelMonitor {
    pub device_id: String,
    pub level: Arc<Mutex<f32>>, // 0.0 to 1.0
    pub is_monitoring: Arc<AtomicBool>,
    pub audio_stream: Option<Stream>,
}

impl AudioLevelMonitor {
    pub fn new(device_id: String) -> Self {
        Self {
            device_id,
            level: Arc::new(Mutex::new(0.0)),
            is_monitoring: Arc::new(AtomicBool::new(false)),
            audio_stream: None,
        }
    }

    pub fn get_level(&self) -> f32 {
        self.level.lock().map(|guard| *guard).unwrap_or(0.0)
    }

    pub fn start_monitoring(&mut self) -> Result<()> {
        if self.is_monitoring.load(Ordering::Relaxed) {
            return Ok(());
        }

        self.is_monitoring.store(true, Ordering::Relaxed);
        
        // Get the default audio host
        let host = cpal::default_host();
        
        // Find the specific device by index or name match
        let device = if let Ok(index) = self.device_id.parse::<usize>() {
            // Use index-based lookup
            host.input_devices()
                .map_err(|e| anyhow!("Failed to enumerate input devices: {}", e))?
                .nth(index)
                .or_else(|| host.default_input_device())
        } else {
            // Fallback to name-based lookup for legacy compatibility
            host.input_devices()
                .map_err(|e| anyhow!("Failed to enumerate input devices: {}", e))?
                .find(|d| d.name().map(|name| name == self.device_id).unwrap_or(false))
                .or_else(|| host.default_input_device())
        }
        .ok_or_else(|| anyhow!("No input device available"))?;
        
        // Get the default input config
        let config = device.default_input_config()
            .map_err(|e| anyhow!("Failed to get default input config: {}", e))?;
        
        let level = self.level.clone();
        let is_monitoring = self.is_monitoring.clone();
        
        // Create audio stream
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => {
                device.build_input_stream(
                    &config.into(),
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        if is_monitoring.load(Ordering::Relaxed) {
                            let rms = calculate_rms(data);
                            if let Ok(mut level_guard) = level.lock() {
                                *level_guard = rms;
                            }
                        }
                    },
                    move |err| {
                        eprintln!("Audio stream error: {}", err);
                    },
                    None,
                )?
            },
            cpal::SampleFormat::I16 => {
                device.build_input_stream(
                    &config.into(),
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        if is_monitoring.load(Ordering::Relaxed) {
                            let rms = calculate_rms_i16(data);
                            if let Ok(mut level_guard) = level.lock() {
                                *level_guard = rms;
                            }
                        }
                    },
                    move |err| {
                        eprintln!("Audio stream error: {}", err);
                    },
                    None,
                )?
            },
            cpal::SampleFormat::U16 => {
                device.build_input_stream(
                    &config.into(),
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        if is_monitoring.load(Ordering::Relaxed) {
                            let rms = calculate_rms_u16(data);
                            if let Ok(mut level_guard) = level.lock() {
                                *level_guard = rms;
                            }
                        }
                    },
                    move |err| {
                        eprintln!("Audio stream error: {}", err);
                    },
                    None,
                )?
            },
            _ => return Err(anyhow!("Unsupported sample format")),
        };
        
        // Start the stream
        stream.play().map_err(|e| anyhow!("Failed to start audio stream: {}", e))?;
        
        self.audio_stream = Some(stream);
        Ok(())
    }

    pub fn stop_monitoring(&mut self) {
        self.is_monitoring.store(false, Ordering::Relaxed);
        self.audio_stream = None;
        // Reset the audio level when stopping
        if let Ok(mut level_guard) = self.level.lock() {
            *level_guard = 0.0;
        }
    }
}

// Helper functions to calculate RMS (Root Mean Square) for different sample formats
fn calculate_rms(data: &[f32]) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    
    let sum_squares: f32 = data.iter().map(|&x| x * x).sum();
    let rms = (sum_squares / data.len() as f32).sqrt();
    
    // Apply amplification and smoothing for better visibility
    let amplified = rms * 3.0; // Amplify by 3x for better visibility
    let smoothed = amplified.min(1.0);
    
    // Apply a slight curve to make low levels more visible
    if smoothed < 0.1 {
        smoothed * 2.0 // Make very low levels more visible
    } else {
        smoothed
    }
}

fn calculate_rms_i16(data: &[i16]) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    
    let sum_squares: f64 = data.iter().map(|&x| (x as f64 / 32768.0).powi(2)).sum();
    let rms = (sum_squares / data.len() as f64).sqrt() as f32;
    
    // Apply amplification and smoothing for better visibility
    let amplified = rms * 3.0; // Amplify by 3x for better visibility
    let smoothed = amplified.min(1.0);
    
    // Apply a slight curve to make low levels more visible
    if smoothed < 0.1 {
        smoothed * 2.0 // Make very low levels more visible
    } else {
        smoothed
    }
}

fn calculate_rms_u16(data: &[u16]) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    
    let sum_squares: f64 = data.iter().map(|&x| ((x as f64 - 32768.0) / 32768.0).powi(2)).sum();
    let rms = (sum_squares / data.len() as f64).sqrt() as f32;
    
    // Apply amplification and smoothing for better visibility
    let amplified = rms * 3.0; // Amplify by 3x for better visibility
    let smoothed = amplified.min(1.0);
    
    // Apply a slight curve to make low levels more visible
    if smoothed < 0.1 {
        smoothed * 2.0 // Make very low levels more visible
    } else {
        smoothed
    }
}

/// Audio device manager that handles enumeration and level monitoring
pub struct AudioDeviceManager {
    devices: Vec<AudioDevice>,
    level_monitors: HashMap<String, AudioLevelMonitor>,
    is_enumerating: Arc<AtomicBool>,
}

impl AudioDeviceManager {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
            level_monitors: HashMap::new(),
            is_enumerating: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Enumerate available audio input devices
    pub fn enumerate_devices(&mut self) -> Result<Vec<AudioDevice>> {
        if self.is_enumerating.load(Ordering::Relaxed) {
            return Ok(self.devices.clone());
        }

        self.is_enumerating.store(true, Ordering::Relaxed);
        
        let devices = self.enumerate_devices_impl()?;
        self.devices = devices.clone();
        
        // Create level monitors for new devices
        for device in &self.devices {
            if !self.level_monitors.contains_key(&device.id) {
                self.level_monitors.insert(
                    device.id.clone(),
                    AudioLevelMonitor::new(device.id.clone()),
                );
            }
        }
        
        self.is_enumerating.store(false, Ordering::Relaxed);
        Ok(devices)
    }

    fn enumerate_devices_impl(&self) -> Result<Vec<AudioDevice>> {
        #[cfg(target_os = "macos")]
        {
            self.enumerate_macos_devices()
        }
        #[cfg(not(target_os = "macos"))]
        {
            // For non-macOS platforms, return a dummy device
            Ok(vec![AudioDevice {
                id: "default".to_string(),
                name: "Default Audio Input".to_string(),
                is_default: true,
            }])
        }
    }

    #[cfg(target_os = "macos")]
    fn enumerate_macos_devices(&self) -> Result<Vec<AudioDevice>> {
        // Use CPAL devices directly for the settings tab
        let mut devices = Vec::new();
        let host = cpal::default_host();
        
        // Get all input devices
        let input_devices = host.input_devices()
            .map_err(|e| anyhow!("Failed to enumerate input devices: {}", e))?;
        
        // Get default device for comparison
        let default_device = host.default_input_device();
        
        // Collect all devices with their actual names and CPAL indices
        for (cpal_index, cpal_device) in input_devices.enumerate() {
            if let Ok(device_name) = cpal_device.name() {
                let is_default = default_device.as_ref().and_then(|d| {
                    d.name().ok().and_then(|default_name| {
                        Some(device_name == default_name)
                    })
                }).unwrap_or(false);
                
                devices.push(AudioDevice {
                    id: cpal_index.to_string(), // Use CPAL index for device ID
                    name: device_name,
                    is_default,
                });
            }
        }
        
        // If no devices found, add a fallback
        if devices.is_empty() {
            devices.push(AudioDevice {
                id: "0".to_string(),
                name: "Default Audio Input".to_string(),
                is_default: true,
            });
        }

        Ok(devices)
    }

    /// Get level monitor for a device
    pub fn get_level_monitor(&self, device_id: &str) -> Option<&AudioLevelMonitor> {
        self.level_monitors.get(device_id)
    }

    /// Start monitoring audio levels for a device
    pub fn start_level_monitoring(&mut self, device_id: &str) -> Result<()> {
        if let Some(monitor) = self.level_monitors.get_mut(device_id) {
            // Force stop any existing monitoring first
            monitor.stop_monitoring();
            // Then start fresh monitoring
            monitor.start_monitoring()?;
            Ok(())
        } else {
            Err(anyhow!("Device not found: {}", device_id))
        }
    }

    /// Stop monitoring audio levels for a device
    pub fn stop_level_monitoring(&mut self, device_id: &str) {
        if let Some(monitor) = self.level_monitors.get_mut(device_id) {
            monitor.stop_monitoring();
        }
    }


    /// Check if currently enumerating devices
    pub fn is_enumerating(&self) -> bool {
        self.is_enumerating.load(Ordering::Relaxed)
    }

    /// Get all devices
    pub fn get_devices(&self) -> &[AudioDevice] {
        &self.devices
    }

}

impl Default for AudioDeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Get the ffmpeg device index for a given device ID
/// This maps CPAL device IDs to their corresponding ffmpeg avfoundation indices
pub fn get_ffmpeg_device_index(device_id: &str) -> Option<usize> {
    // Try to parse as a number first (CPAL index)
    if let Ok(cpal_index) = device_id.parse::<usize>() {
        // Get the device name from CPAL using the index
        let host = cpal::default_host();
        if let Ok(mut devices) = host.input_devices() {
            if let Some(device) = devices.nth(cpal_index) {
                if let Ok(device_name) = device.name() {
                    // Now find the ffmpeg index for this device name
                    if let Ok(ffmpeg_devices) = get_ffmpeg_device_mapping() {
                        for (ffmpeg_index, ffmpeg_name) in ffmpeg_devices {
                            if ffmpeg_name == device_name {
                                return Some(ffmpeg_index);
                            }
                        }
                    }
                }
            }
        }
    }
    
    // If it's not a number, try to find the device by name using ffmpeg mapping
    if let Ok(ffmpeg_devices) = get_ffmpeg_device_mapping() {
        for (index, name) in ffmpeg_devices {
            if name == device_id {
                return Some(index);
            }
        }
    }
    
    // Fallback to first device if not found
    Some(0)
}

/// Get the optimal sample rate for a given audio device
/// This helps avoid sample rate conversion artifacts
pub fn get_optimal_sample_rate(device_id: &str) -> u32 {
    // Try to get the device's native sample rate
    let host = cpal::default_host();
    
    if let Ok(index) = device_id.parse::<usize>() {
        // Use index-based lookup
        if let Ok(mut devices) = host.input_devices() {
            if let Some(device) = devices.nth(index) {
                if let Ok(config) = device.default_input_config() {
                    return config.sample_rate().0;
                }
            }
        }
    } else {
        // Fallback to name-based lookup
        if let Ok(mut devices) = host.input_devices() {
            if let Some(device) = devices.find(|d| d.name().map(|name| name == device_id).unwrap_or(false)) {
                if let Ok(config) = device.default_input_config() {
                    return config.sample_rate().0;
                }
            }
        }
    }
    
    // Default to 48kHz if we can't determine the device's native rate
    48000
}

/// Get the optimal buffer size for a given audio device
/// This helps compensate for device latency and prevent buffer issues
pub fn get_optimal_buffer_size(device_id: &str) -> u32 {
    // Different devices may need different buffer sizes
    // External devices typically need larger buffers
    match device_id {
        "Microsoft Teams Audio" => 2048, // Teams audio can be unstable, use larger buffer
        "External Microphone" => 1536,   // External devices often have higher latency
        "MacBook Pro Microphone" => 1024, // Built-in mic, smaller buffer is usually fine
        _ => {
            // Try to determine if it's an external device by name
            if device_id.to_lowercase().contains("external") || 
               device_id.to_lowercase().contains("usb") ||
               device_id.to_lowercase().contains("bluetooth") {
                1536 // Larger buffer for external devices
            } else {
                1024 // Default buffer size
            }
        }
    }
}

/// Get the actual ffmpeg audio device indices by querying ffmpeg directly
/// This ensures we have the correct mapping between device names and indices
pub fn get_ffmpeg_device_mapping() -> Result<Vec<(usize, String)>> {
    use std::process::Command;
    
    let output = Command::new("ffmpeg")
        .args(["-f", "avfoundation", "-list_devices", "true", "-i", ""])
        .output()
        .map_err(|e| anyhow!("Failed to run ffmpeg: {}", e))?;
    
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut devices = Vec::new();
    let mut in_audio_section = false;
    
    for line in stderr.lines() {
        // Check if we're entering the audio devices section
        if line.contains("AVFoundation audio devices:") {
            in_audio_section = true;
            continue;
        }
        
        // Check if we're entering the video devices section (stop parsing audio)
        if line.contains("AVFoundation video devices:") {
            in_audio_section = false;
            continue;
        }
        
        // Only parse audio devices when we're in the audio section
        if in_audio_section && line.contains("[AVFoundation indev @") && line.contains("] [") {
            // Parse lines like: [AVFoundation indev @ 0x12b804280] [0] Microsoft Teams Audio
            if let Some(start) = line.find("] [") {
                let device_part = &line[start + 3..];
                if let Some(end) = device_part.find("] ") {
                    if let Ok(index) = device_part[..end].parse::<usize>() {
                        let name = device_part[end + 2..].trim().to_string();
                        devices.push((index, name));
                    }
                }
            }
        }
    }
    
    Ok(devices)
}

/// Debug function to list all available audio devices with their indices
/// This helps verify that device enumeration is working correctly
pub fn debug_list_audio_devices() -> Result<()> {
    println!("=== Audio Device Debug Information ===");
    
    // List CPAL devices (what the application actually uses for device enumeration)
    let host = cpal::default_host();
    println!("\nCPAL Audio Devices (Application's Device List):");
    if let Ok(devices) = host.input_devices() {
        for (index, device) in devices.enumerate() {
            if let Ok(name) = device.name() {
                println!("  [{}] {}", index, name);
            } else {
                println!("  [{}] <unnamed device>", index);
            }
        }
    } else {
        println!("  Failed to enumerate CPAL devices");
    }
    
    // List ffmpeg audio devices (for reference only)
    println!("\nFFmpeg Audio Devices (for reference):");
    match get_ffmpeg_device_mapping() {
        Ok(ffmpeg_devices) => {
            if ffmpeg_devices.is_empty() {
                println!("  No audio devices found in ffmpeg output");
            } else {
                for (index, name) in ffmpeg_devices {
                    println!("  [{}] {}", index, name);
                }
            }
        }
        Err(e) => {
            println!("  Failed to get ffmpeg devices: {}", e);
        }
    }
    
    Ok(())
}
