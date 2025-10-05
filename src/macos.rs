use anyhow::{anyhow, Result};
use core_foundation::array::CFArrayRef;
use core_foundation::base::TCFType;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::{CFNumber, CFNumberRef};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::geometry::CGRect;
use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex};
use core_foundation_sys::dictionary::CFDictionaryGetValueIfPresent;
use std::ffi::c_void;

use crate::window::WindowInfo;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relativeToWindow: u32) -> CFArrayRef;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGWindowListCreateImage(
        rect: core_graphics::geometry::CGRect,
        listOption: u32,
        windowID: u32,
        imageOption: u32,
    ) -> core_graphics::sys::CGImageRef;
    fn CGImageRelease(image: core_graphics::sys::CGImageRef);
    fn CGImageGetWidth(image: core_graphics::sys::CGImageRef) -> usize;
    fn CGImageGetHeight(image: core_graphics::sys::CGImageRef) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> core_graphics::sys::CGColorSpaceRef;
    fn CGColorSpaceRelease(space: core_graphics::sys::CGColorSpaceRef);
    fn CGBitmapContextCreate(
        data: *mut std::ffi::c_void,
        width: usize,
        height: usize,
        bitsPerComponent: usize,
        bytesPerRow: usize,
        space: core_graphics::sys::CGColorSpaceRef,
        bitmapInfo: u32,
    ) -> core_graphics::sys::CGContextRef;
    fn CGContextDrawImage(
        c: core_graphics::sys::CGContextRef,
        rect: core_graphics::geometry::CGRect,
        image: core_graphics::sys::CGImageRef,
    );
    fn CGContextRelease(c: core_graphics::sys::CGContextRef);
}

const K_CG_WINDOW_IMAGE_BOUNDS_IGNORE_FRAMING: u32 = 1 << 0;
const K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;

// kCGWindowListOption flags
const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;
const K_CG_WINDOW_LIST_OPTION_INCLUDING_WINDOW: u32 = 1 << 3;  // 0x08 - Include only this window
const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4; // 0x10

// Well-known dictionary keys
fn cfstr(s: &'static str) -> CFString {
    CFString::from_static_string(s)
}

pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let mask = K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS;
    let array_ref = unsafe { CGWindowListCopyWindowInfo(mask, 0) };
    if array_ref.is_null() {
        return Err(anyhow!("CGWindowListCopyWindowInfo returned null"));
    }
    let mut result = Vec::new();

    let count = unsafe { CFArrayGetCount(array_ref) } as isize;
    for idx in 0..count {
        let value = unsafe { CFArrayGetValueAtIndex(array_ref, idx) };
        if value.is_null() {
            continue;
        }
        let dict: CFDictionary<*const std::ffi::c_void, *const std::ffi::c_void> = unsafe { CFDictionary::wrap_under_get_rule(value as CFDictionaryRef) };

        let number_key = cfstr("kCGWindowNumber");
        let owner_name_key = cfstr("kCGWindowOwnerName");
        let name_key = cfstr("kCGWindowName");
        let layer_key = cfstr("kCGWindowLayer");
        let bounds_key = cfstr("kCGWindowBounds");

        let window_number: Option<i64> = unsafe {
            let mut out: *const c_void = std::ptr::null();
            let found = CFDictionaryGetValueIfPresent(
                dict.as_concrete_TypeRef(),
                number_key.as_concrete_TypeRef() as *const c_void,
                &mut out,
            );
            if found != 0 && !out.is_null() {
                CFNumber::wrap_under_get_rule(out as CFNumberRef).to_i64()
            } else {
                None
            }
        };

        let layer: Option<i64> = unsafe {
            let mut out: *const c_void = std::ptr::null();
            let found = CFDictionaryGetValueIfPresent(
                dict.as_concrete_TypeRef(),
                layer_key.as_concrete_TypeRef() as *const c_void,
                &mut out,
            );
            if found != 0 && !out.is_null() {
                CFNumber::wrap_under_get_rule(out as CFNumberRef).to_i64()
            } else {
                None
            }
        };

        // Only consider layer 0 (normal app windows)
        if layer != Some(0) {
            continue;
        }

        let owner_name: Option<String> = unsafe {
            let mut out: *const c_void = std::ptr::null();
            let found = CFDictionaryGetValueIfPresent(
                dict.as_concrete_TypeRef(),
                owner_name_key.as_concrete_TypeRef() as *const c_void,
                &mut out,
            );
            if found != 0 && !out.is_null() {
                Some(CFString::wrap_under_get_rule(out as CFStringRef).to_string())
            } else {
                None
            }
        };

        let window_name: Option<String> = unsafe {
            let mut out: *const c_void = std::ptr::null();
            let found = CFDictionaryGetValueIfPresent(
                dict.as_concrete_TypeRef(),
                name_key.as_concrete_TypeRef() as *const c_void,
                &mut out,
            );
            if found != 0 && !out.is_null() {
                Some(CFString::wrap_under_get_rule(out as CFStringRef).to_string())
            } else {
                None
            }
        };

        // Bounds dictionary contains X, Y, Width, Height
        let bounds_dict_ptr = unsafe {
            let mut out: *const c_void = std::ptr::null();
            let found = CFDictionaryGetValueIfPresent(
                dict.as_concrete_TypeRef(),
                bounds_key.as_concrete_TypeRef() as *const c_void,
                &mut out,
            );
            if found != 0 && !out.is_null() {
                Some(out as CFDictionaryRef)
            } else {
                None
            }
        };

        let mut rect = CGRect::new(&core_graphics::geometry::CGPoint::new(0.0, 0.0), &core_graphics::geometry::CGSize::new(0.0, 0.0));
        if let Some(bounds_dict) = bounds_dict_ptr {
            let bounds: CFDictionary<*const std::ffi::c_void, *const std::ffi::c_void> = unsafe { CFDictionary::wrap_under_get_rule(bounds_dict) };
            let x = unsafe {
                let mut out: *const c_void = std::ptr::null();
                let found = CFDictionaryGetValueIfPresent(
                    bounds.as_concrete_TypeRef(),
                    cfstr("X").as_concrete_TypeRef() as *const c_void,
                    &mut out,
                );
                if found != 0 && !out.is_null() {
                    CFNumber::wrap_under_get_rule(out as CFNumberRef).to_f64()
                } else { None }
            }.unwrap_or(0.0);
            let y = unsafe {
                let mut out: *const c_void = std::ptr::null();
                let found = CFDictionaryGetValueIfPresent(
                    bounds.as_concrete_TypeRef(),
                    cfstr("Y").as_concrete_TypeRef() as *const c_void,
                    &mut out,
                );
                if found != 0 && !out.is_null() {
                    CFNumber::wrap_under_get_rule(out as CFNumberRef).to_f64()
                } else { None }
            }.unwrap_or(0.0);
            let w = unsafe {
                let mut out: *const c_void = std::ptr::null();
                let found = CFDictionaryGetValueIfPresent(
                    bounds.as_concrete_TypeRef(),
                    cfstr("Width").as_concrete_TypeRef() as *const c_void,
                    &mut out,
                );
                if found != 0 && !out.is_null() {
                    CFNumber::wrap_under_get_rule(out as CFNumberRef).to_f64()
                } else { None }
            }.unwrap_or(0.0);
            let h = unsafe {
                let mut out: *const c_void = std::ptr::null();
                let found = CFDictionaryGetValueIfPresent(
                    bounds.as_concrete_TypeRef(),
                    cfstr("Height").as_concrete_TypeRef() as *const c_void,
                    &mut out,
                );
                if found != 0 && !out.is_null() {
                    CFNumber::wrap_under_get_rule(out as CFNumberRef).to_f64()
                } else { None }
            }.unwrap_or(0.0);
            rect = CGRect::new(
                &core_graphics::geometry::CGPoint::new(x, y),
                &core_graphics::geometry::CGSize::new(w, h),
            );
        }

        if let Some(id) = window_number {
            let owner = owner_name.unwrap_or_default();
            let title = window_name.unwrap_or_default();
            if owner.is_empty() && title.is_empty() {
                continue;
            }
            result.push(WindowInfo {
                window_id: id as u64,
                owner_name: owner,
                window_title: title,
                x: rect.origin.x as i32,
                y: rect.origin.y as i32,
                width: rect.size.width as i32,
                height: rect.size.height as i32,
            });
        }
    }

    // Sort for stable display
    result.sort_by(|a, b| a.owner_name.cmp(&b.owner_name).then(a.window_title.cmp(&b.window_title)));
    Ok(result)
}

pub fn has_screen_capture_access() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

pub fn request_screen_capture_access() -> bool {
    unsafe { CGRequestScreenCaptureAccess() }
}

pub fn capture_window_image(window_id: u64) -> Option<(Vec<u8>, usize, usize)> {
    // Capture the window image  
    let cg_null_rect = core_graphics::geometry::CGRect::new(
        &core_graphics::geometry::CGPoint::new(0.0, 0.0),
        &core_graphics::geometry::CGSize::new(0.0, 0.0),
    );
    
    let image_ptr = unsafe {
        CGWindowListCreateImage(
            cg_null_rect,
            K_CG_WINDOW_LIST_OPTION_INCLUDING_WINDOW, // Capture only this specific window
            window_id as u32,
            K_CG_WINDOW_IMAGE_BOUNDS_IGNORE_FRAMING,
        )
    };
    
    if image_ptr.is_null() {
        return None;
    }
    
    // Get image dimensions
    let width = unsafe { CGImageGetWidth(image_ptr) };
    let height = unsafe { CGImageGetHeight(image_ptr) };
    
    if width == 0 || height == 0 {
        unsafe { CGImageRelease(image_ptr) };
        return None;
    }
    
    // Create bitmap context to render the image into RGBA format
    let bytes_per_row = width * 4;
    let mut buffer = vec![0u8; bytes_per_row * height];
    
    unsafe {
        let color_space = CGColorSpaceCreateDeviceRGB();
        let ctx = CGBitmapContextCreate(
            buffer.as_mut_ptr() as *mut std::ffi::c_void,
            width,
            height,
            8,
            bytes_per_row,
            color_space,
            K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST,
        );
        
        if ctx.is_null() {
            CGColorSpaceRelease(color_space);
            CGImageRelease(image_ptr);
            return None;
        }
        
        // Draw the captured image into our bitmap context
        let rect = core_graphics::geometry::CGRect::new(
            &core_graphics::geometry::CGPoint::new(0.0, 0.0),
            &core_graphics::geometry::CGSize::new(width as f64, height as f64),
        );
        
        CGContextDrawImage(ctx, rect, image_ptr);
        
        // Cleanup
        CGContextRelease(ctx);
        CGColorSpaceRelease(color_space);
        CGImageRelease(image_ptr);
    }
    
    Some((buffer, width, height))
}

