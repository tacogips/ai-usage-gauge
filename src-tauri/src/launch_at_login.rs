use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchAtLoginState {
    Off,
    On,
    RequiresApproval,
    Unavailable,
}

static E2E_MOCK_ENABLED: AtomicBool = AtomicBool::new(false);

fn is_e2e_mock() -> bool {
    std::env::var("CCUSAGE_GAUGE_E2E_OPEN_MENU").ok().as_deref() == Some("1")
}

pub struct LaunchAtLoginController;

impl LaunchAtLoginController {
    pub fn status() -> LaunchAtLoginState {
        if is_e2e_mock() {
            return if E2E_MOCK_ENABLED.load(Ordering::SeqCst) {
                LaunchAtLoginState::On
            } else {
                LaunchAtLoginState::Off
            };
        }
        #[cfg(target_os = "macos")]
        {
            macos::status()
        }
        #[cfg(not(target_os = "macos"))]
        {
            LaunchAtLoginState::Unavailable
        }
    }

    pub fn set_enabled(enabled: bool) -> Result<LaunchAtLoginState, String> {
        if is_e2e_mock() {
            E2E_MOCK_ENABLED.store(enabled, Ordering::SeqCst);
            return Ok(Self::status());
        }
        #[cfg(target_os = "macos")]
        {
            macos::set_enabled(enabled)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = enabled;
            Ok(LaunchAtLoginState::Unavailable)
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::LaunchAtLoginState;
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    #[link(name = "ServiceManagement", kind = "framework")]
    extern "C" {}

    pub fn status() -> LaunchAtLoginState {
        let Some(cls) = AnyClass::get(c"SMAppService") else {
            return LaunchAtLoginState::Unavailable;
        };
        unsafe {
            let service: *mut AnyObject = msg_send![cls, mainAppService];
            if service.is_null() {
                return LaunchAtLoginState::Unavailable;
            }
            let raw: isize = msg_send![service, status];
            match raw {
                1 => LaunchAtLoginState::On,
                2 => LaunchAtLoginState::RequiresApproval,
                0 => LaunchAtLoginState::Off,
                _ => LaunchAtLoginState::Unavailable,
            }
        }
    }

    pub fn set_enabled(enabled: bool) -> Result<LaunchAtLoginState, String> {
        let Some(cls) = AnyClass::get(c"SMAppService") else {
            return Ok(LaunchAtLoginState::Unavailable);
        };
        unsafe {
            let service: *mut AnyObject = msg_send![cls, mainAppService];
            if service.is_null() {
                return Ok(LaunchAtLoginState::Unavailable);
            }
            let mut err: *mut AnyObject = std::ptr::null_mut();
            let ok: bool = if enabled {
                msg_send![service, registerAndReturnError: &mut err]
            } else {
                msg_send![service, unregisterAndReturnError: &mut err]
            };
            if ok {
                Ok(status())
            } else {
                Err("SMAppService registration call failed".into())
            }
        }
    }
}
