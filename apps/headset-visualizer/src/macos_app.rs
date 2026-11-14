//! Things that only matter when vr-space runs as a macOS app bundle (`vr-space.app`, made by
//! `scripts/bundle-macos.sh`) rather than from a terminal: where the log goes, who holds the
//! Screen Recording permission, and opening at login.
//!
//! Nothing here runs per frame.

use std::ffi::{c_char, CStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use log::{info, warn};
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject};

// SMAppService (macOS 13+) lives here. Linking it is harmless on macOS 12: the class is just
// missing, and `open_at_login()` returns `None`.
#[link(name = "ServiceManagement", kind = "framework")]
extern "C" {}

/// The `.app` folder if `exe` is the executable inside a bundle (`X.app/Contents/MacOS/exe`).
pub fn bundle_root(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let is_bundle = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app";
    is_bundle.then_some(app)
}

/// Whether this process is running from an app bundle. Checked once.
pub fn in_bundle() -> bool {
    static IN_BUNDLE: OnceLock<bool> = OnceLock::new();
    *IN_BUNDLE.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.canonicalize().ok())
            .is_some_and(|exe| bundle_root(&exe).is_some())
    })
}

/// Drop the `-psn_…` process serial number argument that older macOS versions pass to apps
/// opened from Finder, so the command-line parser doesn't reject it.
pub fn without_psn_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    args.into_iter()
        .filter(|a| !a.to_string_lossy().starts_with("-psn_"))
        .collect()
}

/// Where the log goes when the app was opened from Finder or at login, so there's no terminal
/// to show it: `~/Library/Logs/vr-space.log`.
pub fn log_file_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join("Library/Logs/vr-space.log"))
}

/// Who macOS gives the Screen Recording permission to: the app itself when it runs as a
/// bundle, otherwise the terminal it was started from.
pub fn permission_holder() -> &'static str {
    if in_bundle() {
        "vr-space"
    } else {
        "your terminal app"
    }
}

/// What to quit and reopen after granting Screen Recording.
pub fn restart_target() -> &'static str {
    if in_bundle() {
        "vr-space"
    } else {
        "the terminal"
    }
}

/// `SMAppService.mainAppService`, if this macOS has it and we run from a bundle.
fn main_app_service() -> Option<&'static AnyObject> {
    if !in_bundle() {
        return None;
    }
    let class = AnyClass::get(c"SMAppService")?;
    // SAFETY: +[SMAppService mainAppService] returns a shared object that lives for the
    // whole process; we only send it messages on the main thread.
    unsafe {
        let service: *const AnyObject = msg_send![class, mainAppService];
        service.as_ref()
    }
}

/// SMAppService status, preserved for command-line callers.
#[derive(serde::Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoginStatus {
    Unavailable,
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
}

fn login_status_from_native(status: isize) -> LoginStatus {
    match status {
        0 => LoginStatus::NotRegistered,
        1 => LoginStatus::Enabled,
        2 => LoginStatus::RequiresApproval,
        3 => LoginStatus::NotFound,
        // Unknown native states must not be represented as off.
        _ => LoginStatus::RequiresApproval,
    }
}

fn service_login_status(service: &AnyObject) -> LoginStatus {
    // SAFETY: `status` is a plain property getter returning an NSInteger.
    let status: isize = unsafe { msg_send![service, status] };
    login_status_from_native(status)
}

/// Full SMAppService status; unavailable outside a supported app bundle.
pub fn login_status() -> LoginStatus {
    main_app_service().map_or(LoginStatus::Unavailable, service_login_status)
}

/// Preserve the existing menu API. Pending approval remains checked.
pub fn open_at_login() -> Option<bool> {
    match login_status() {
        LoginStatus::Unavailable => None,
        LoginStatus::NotRegistered | LoginStatus::NotFound => Some(false),
        LoginStatus::Enabled | LoginStatus::RequiresApproval => Some(true),
    }
}

fn set_login_enabled_with(
    on: bool,
    status: LoginStatus,
    mut register: impl FnMut() -> Result<()>,
    mut unregister: impl FnMut() -> Result<()>,
    mut current_status: impl FnMut() -> LoginStatus,
) -> Result<LoginStatus> {
    match (on, status) {
        (true, LoginStatus::NotRegistered | LoginStatus::NotFound) => register()?,
        (false, LoginStatus::Enabled | LoginStatus::RequiresApproval) => unregister()?,
        // In particular, don't re-register while approval is pending.
        _ => return Ok(status),
    }
    Ok(current_status())
}

/// Set Open at Login and return the native status. Registration errors are returned to callers.
pub fn set_login_enabled(on: bool) -> Result<LoginStatus> {
    let Some(service) = main_app_service() else {
        return Ok(LoginStatus::Unavailable);
    };
    let status = service_login_status(service);
    let mut error: *mut AnyObject = std::ptr::null_mut();
    let mut action = || {
        // SAFETY: both methods take an `NSError **` out-parameter and return a BOOL.
        let ok: bool = unsafe {
            if on {
                msg_send![service, registerAndReturnError: &mut error as *mut *mut AnyObject]
            } else {
                msg_send![service, unregisterAndReturnError: &mut error as *mut *mut AnyObject]
            }
        };
        if ok {
            Ok(())
        } else {
            Err(anyhow!("{}", describe_error(error)))
        }
    };
    let result = if on {
        set_login_enabled_with(
            on,
            status,
            &mut action,
            || Ok(()),
            || service_login_status(service),
        )
    } else {
        set_login_enabled_with(
            on,
            status,
            || Ok(()),
            &mut action,
            || service_login_status(service),
        )
    };
    match &result {
        Ok(LoginStatus::RequiresApproval) => {
            info!("Allow vr-space in System Settings > General > Login Items to finish turning it on.");
        }
        Ok(status) => info!("Open at login is {status:?}"),
        Err(error) => warn!(
            "Couldn't turn open at login {}: {error}",
            if on { "on" } else { "off" }
        ),
    }
    result
}

/// Legacy menu setter. Returns the updated checkmark state, or `None` if unavailable or failed.
pub fn set_open_at_login(on: bool) -> Option<bool> {
    let _ = set_login_enabled(on);
    open_at_login()
}

fn describe_error(error: *mut AnyObject) -> String {
    // SAFETY: `error` is null or an autoreleased NSError from the call above.
    unsafe {
        let Some(error) = error.as_ref() else {
            return "unknown error".into();
        };
        let desc: *const AnyObject = msg_send![error, localizedDescription];
        let Some(desc) = desc.as_ref() else {
            return "unknown error".into();
        };
        let utf8: *const c_char = msg_send![desc, UTF8String];
        if utf8.is_null() {
            return "unknown error".into();
        }
        CStr::from_ptr(utf8).to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_native_login_statuses_without_treating_unknown_as_off() {
        assert_eq!(login_status_from_native(0), LoginStatus::NotRegistered);
        assert_eq!(login_status_from_native(1), LoginStatus::Enabled);
        assert_eq!(login_status_from_native(2), LoginStatus::RequiresApproval);
        assert_eq!(login_status_from_native(3), LoginStatus::NotFound);
        assert_eq!(login_status_from_native(99), LoginStatus::RequiresApproval);
        assert_eq!(
            serde_json::to_string(&LoginStatus::RequiresApproval).unwrap(),
            "\"requires_approval\""
        );
    }

    #[test]
    fn pending_approval_setter_is_idempotent_and_native_free() {
        let mut registered = false;
        let mut unregistered = false;
        let status = set_login_enabled_with(
            true,
            LoginStatus::RequiresApproval,
            || {
                registered = true;
                Ok(())
            },
            || {
                unregistered = true;
                Ok(())
            },
            || LoginStatus::Enabled,
        )
        .unwrap();
        assert_eq!(status, LoginStatus::RequiresApproval);
        assert!(!registered && !unregistered);
    }

    #[test]
    fn already_set_login_states_are_idempotent() {
        for (on, status) in [
            (true, LoginStatus::Enabled),
            (false, LoginStatus::NotRegistered),
        ] {
            assert_eq!(
                set_login_enabled_with(
                    on,
                    status,
                    || panic!("register called"),
                    || panic!("unregister called"),
                    || panic!("status reread")
                )
                .unwrap(),
                status,
            );
        }
    }

    #[test]
    fn missing_login_registration_is_retried() {
        let mut registered = false;
        let status = set_login_enabled_with(
            true,
            LoginStatus::NotFound,
            || {
                registered = true;
                Ok(())
            },
            || panic!("unregister called"),
            || LoginStatus::RequiresApproval,
        )
        .unwrap();
        assert!(registered);
        assert_eq!(status, LoginStatus::RequiresApproval);
    }

    #[test]
    fn setter_propagates_native_registration_errors() {
        let result = set_login_enabled_with(
            true,
            LoginStatus::NotRegistered,
            || Err(anyhow!("registration denied")),
            || Ok(()),
            || LoginStatus::Enabled,
        );
        assert_eq!(result.unwrap_err().to_string(), "registration denied");
    }

    #[test]
    fn finds_the_bundle_around_its_executable() {
        let exe = Path::new("/Applications/vr-space.app/Contents/MacOS/vr-space");
        assert_eq!(
            bundle_root(exe),
            Some(Path::new("/Applications/vr-space.app"))
        );
    }

    #[test]
    fn a_plain_binary_is_not_in_a_bundle() {
        assert_eq!(
            bundle_root(Path::new("/repo/target/release/headset-visualizer")),
            None
        );
        assert_eq!(bundle_root(Path::new("/x/Contents/MacOS/vr-space")), None);
        assert_eq!(
            bundle_root(Path::new("/x.app/Contents/Resources/vr-space")),
            None
        );
        assert_eq!(bundle_root(Path::new("vr-space")), None);
    }

    #[test]
    fn drops_the_finder_process_serial_number() {
        let args = ["vr-space", "-psn_0_12345", "--fullscreen", "2"].map(OsString::from);
        let kept = without_psn_args(args);
        assert_eq!(kept, ["vr-space", "--fullscreen", "2"].map(OsString::from));
    }
}
