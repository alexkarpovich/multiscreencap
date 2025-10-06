use anyhow::Result;
use std::time::Instant;

#[cfg(target_os = "macos")]
use crate::macos;

#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub window_id: u64,
    pub owner_name: String,
    pub window_title: String,
    #[allow(dead_code)]
    pub x: i32,
    #[allow(dead_code)]
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl WindowInfo {
    pub fn display_name(&self) -> String {
        format!(
            "{} — {}",
            self.owner_name,
            if self.window_title.is_empty() { "(untitled)" } else { &self.window_title }
        )
    }
    
    pub fn dimensions_str(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }
}

/// Manages window enumeration
pub struct WindowManager {
    windows: Vec<WindowInfo>,
    last_refresh: Instant,
}

impl WindowManager {
    pub fn new() -> Self {
        Self {
            windows: Vec::new(),
            last_refresh: Instant::now(),
        }
    }
    
    pub fn refresh(&mut self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            self.windows = macos::list_windows()?;
            self.last_refresh = Instant::now();
        }
        
        #[cfg(not(target_os = "macos"))]
        {
            return Err(anyhow::anyhow!("This app currently supports macOS only for window capture."));
        }
        
        Ok(())
    }
    
    pub fn should_auto_refresh(&self) -> bool {
        self.last_refresh.elapsed() > std::time::Duration::from_secs(3)
    }
    
    pub fn get_window(&self, window_id: u64) -> Option<&WindowInfo> {
        self.windows.iter().find(|w| w.window_id == window_id)
    }
    
    pub fn windows(&self) -> &[WindowInfo] {
        &self.windows
    }
}

