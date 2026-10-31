//! macOS privacy permissions needed for decoding. Other platforms need none.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    ScreenRecording,
    Accessibility,
}

impl Permission {
    pub fn name(self) -> &'static str {
        match self {
            Self::ScreenRecording => "Screen Recording",
            Self::Accessibility => "Accessibility",
        }
    }

    pub fn purpose(self) -> &'static str {
        match self {
            Self::ScreenRecording => "to take screenshots of the QR codes",
            Self::Accessibility => "to press the forward key",
        }
    }
}

/// Permissions that are required on this platform but not granted yet.
pub fn missing() -> Vec<Permission> {
    platform::REQUIRED
        .iter()
        .copied()
        .filter(|permission| !platform::is_granted(*permission))
        .collect()
}

/// Whether [`reset_and_request`] is possible, i.e. the app runs from a macOS app bundle.
pub fn can_reset() -> bool {
    platform::can_reset()
}

/// Removes the stored decision for `permission`, then requests it again.
///
/// macOS ties a grant to the exact code signature of the build that was approved, so an
/// ad-hoc signed rebuild shows up as enabled in System Settings but is not actually trusted.
pub fn reset_and_request(permission: Permission) {
    platform::reset(permission);
    platform::request(permission);
}

/// Registers the app for `permission` (showing the system prompt the first time)
/// and opens the matching System Settings pane.
pub fn request(permission: Permission) {
    platform::request(permission);
}

#[cfg(target_os = "macos")]
mod platform {
    use core_foundation::{
        base::TCFType,
        boolean::CFBoolean,
        bundle::CFBundle,
        dictionary::{CFDictionary, CFDictionaryRef},
        string::{CFString, CFStringRef},
    };

    use super::Permission;

    pub const REQUIRED: &[Permission] = &[Permission::ScreenRecording, Permission::Accessibility];

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        static kAXTrustedCheckOptionPrompt: CFStringRef;
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    }

    pub fn is_granted(permission: Permission) -> bool {
        // SAFETY: both functions take no arguments and only query the TCC database.
        unsafe {
            match permission {
                Permission::ScreenRecording => CGPreflightScreenCaptureAccess(),
                Permission::Accessibility => AXIsProcessTrusted(),
            }
        }
    }

    fn bundle_identifier() -> Option<String> {
        CFBundle::main_bundle()
            .info_dictionary()
            .find(CFString::from_static_string("CFBundleIdentifier"))
            .and_then(|identifier| identifier.downcast::<CFString>())
            .map(|identifier| identifier.to_string())
    }

    pub fn can_reset() -> bool {
        bundle_identifier().is_some()
    }

    pub fn reset(permission: Permission) {
        let Some(identifier) = bundle_identifier() else {
            return;
        };
        let service = match permission {
            Permission::ScreenRecording => "ScreenCapture",
            Permission::Accessibility => "Accessibility",
        };
        match std::process::Command::new("tccutil")
            .args(["reset", service, &identifier])
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => log::warn!("tccutil reset {service} {identifier} failed: {status}"),
            Err(error) => log::warn!("failed to run tccutil: {error}"),
        }
    }

    pub fn request(permission: Permission) {
        let pane = match permission {
            Permission::ScreenRecording => {
                // SAFETY: takes no arguments; shows the system prompt at most once.
                unsafe { CGRequestScreenCaptureAccess() };
                "Privacy_ScreenCapture"
            }
            Permission::Accessibility => {
                // SAFETY: the key is a constant owned by the framework (get rule), and the
                // dictionary outlives the call.
                let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
                let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
                unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) };
                "Privacy_Accessibility"
            }
        };
        let url = format!("x-apple.systempreferences:com.apple.preference.security?{pane}");
        if let Err(error) = std::process::Command::new("open").arg(&url).spawn() {
            log::warn!("failed to open {url}: {error}");
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::Permission;

    pub const REQUIRED: &[Permission] = &[];

    pub fn is_granted(_permission: Permission) -> bool {
        true
    }

    pub fn can_reset() -> bool {
        false
    }

    pub fn reset(_permission: Permission) {}

    pub fn request(_permission: Permission) {}
}
