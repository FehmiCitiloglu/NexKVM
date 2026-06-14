//! macOS Accessibility trust checks and prompt trigger.
//!
//! The rest of the platform backend consumes the safe [`AccessibilityStatus`]
//! trait. Raw ApplicationServices/CoreFoundation FFI stays isolated here.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::fmt::Debug;
use std::ptr;

/// Source of macOS Accessibility trust state.
pub trait AccessibilityStatus: Debug + Send + Sync {
    /// Whether the current process is already trusted for Accessibility.
    fn is_trusted(&self) -> bool;

    /// Trigger the system prompt if needed, then return the current trust state.
    fn prompt_and_check(&self) -> bool;
}

/// Accessibility status backed by macOS ApplicationServices.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemAccessibility;

impl AccessibilityStatus for SystemAccessibility {
    fn is_trusted(&self) -> bool {
        ax_is_process_trusted()
    }

    fn prompt_and_check(&self) -> bool {
        ax_prompt_and_check()
    }
}

type Boolean = u8;
type CFIndex = isize;
type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFDictionaryRef = *const c_void;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    static kAXTrustedCheckOptionPrompt: CFTypeRef;

    fn AXIsProcessTrusted() -> Boolean;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> Boolean;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFTypeRef;

    fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;

    fn CFRelease(cf: CFTypeRef);
}

fn ax_is_process_trusted() -> bool {
    // SAFETY: `AXIsProcessTrusted` has no parameters and only queries process
    // trust state from ApplicationServices.
    unsafe { AXIsProcessTrusted() != 0 }
}

fn ax_prompt_and_check() -> bool {
    let options = prompt_options();
    if options.is_null() {
        return ax_is_process_trusted();
    }

    // SAFETY: `options` is a valid immutable CFDictionary created below and
    // retained for the duration of the call.
    let trusted = unsafe { AXIsProcessTrustedWithOptions(options) != 0 };

    // SAFETY: `options` follows CoreFoundation create/copy ownership rules and
    // is released exactly once after use.
    unsafe {
        CFRelease(options);
    }

    trusted
}

fn prompt_options() -> CFDictionaryRef {
    // SAFETY: CoreFoundation receives one key/value pair. The key is the
    // ApplicationServices prompt option constant and the value is kCFBooleanTrue.
    unsafe {
        let key = kAXTrustedCheckOptionPrompt;
        let value = kCFBooleanTrue;
        CFDictionaryCreate(
            ptr::null(),
            ptr::addr_of!(key),
            ptr::addr_of!(value),
            1,
            ptr::null(),
            ptr::null(),
        )
    }
}
