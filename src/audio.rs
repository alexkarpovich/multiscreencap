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
        // Use CPAL to enumerate actual devices
        let mut devices = Vec::new();
        let host = cpal::default_host();
        
        // Get all input devices
        let input_devices = host.input_devices()
            .map_err(|e| anyhow!("Failed to enumerate input devices: {}", e))?;
        
        // Get default device for comparison
        let default_device = host.default_input_device();
        
        // Collect all devices first
        let cpal_devices: Vec<_> = input_devices.collect();
        
        // Map CPAL devices to ffmpeg indices based on known device names
        // ffmpeg order: [0] Microsoft Teams Audio, [1] External Microphone, [2] MacBook Pro Microphone
        let device_mappings = [
            ("Microsoft Teams Audio", 0),
            ("External Microphone", 1), 
            ("MacBook Pro Microphone", 2),
        ];
        
        for (device_name, ffmpeg_index) in device_mappings.iter() {
            // Find the CPAL device with this name
            if let Some(cpal_device) = cpal_devices.iter().find(|d| {
                d.name().map(|name| name == *device_name).unwrap_or(false)
            }) {
                let is_default = default_device.as_ref().and_then(|d| {
                    d.name().ok().and_then(|default_name| {
                        cpal_device.name().ok().map(|device_name| device_name == default_name)
                    })
                }).unwrap_or(false);
                
                devices.push(AudioDevice {
                    id: ffmpeg_index.to_string(),
                    name: device_name.to_string(),
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
/// This maps device IDs to their corresponding ffmpeg avfoundation indices
pub fn get_ffmpeg_device_index(device_id: &str) -> Option<usize> {
    // Try to parse as a number first (new index-based approach)
    if let Ok(index) = device_id.parse::<usize>() {
        return Some(index);
    }
    
    // Fallback to legacy device name mapping for backward compatibility
    match device_id {
        "Microsoft Teams Audio" => Some(0),
        "External Microphone" => Some(1),
        "MacBook Pro Microphone" => Some(2),
        _ => Some(0), // Default to first device
    }
}
