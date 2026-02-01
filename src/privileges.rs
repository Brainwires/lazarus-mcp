//! Privilege Management
//!
//! Handles dropping root privileges when running with elevated permissions.
//! By default, lazarus-mcp drops to the original user before spawning coding agents
//! to prevent accidental damage from privileged operations.

use anyhow::{anyhow, Context, Result};
use nix::unistd::{setgid, setuid, Gid, Uid};
use std::env;
use tracing::info;

/// Check if the current process is running as root
pub fn is_root() -> bool {
    Uid::effective().is_root()
}

/// Drop root privileges to the original user who ran sudo.
///
/// This function:
/// 1. Checks if we're running as root
/// 2. Reads SUDO_UID and SUDO_GID environment variables
/// 3. Drops group privileges first (required order)
/// 4. Drops user privileges
///
/// # Errors
///
/// Returns an error if:
/// - Running as root but SUDO_UID/SUDO_GID are not set (ran as root directly, not via sudo)
/// - Failed to drop privileges (permission denied, etc.)
pub fn drop_privileges() -> Result<()> {
    if !is_root() {
        return Ok(()); // Not root, nothing to do
    }

    // Get original user from SUDO_UID/SUDO_GID
    let uid = env::var("SUDO_UID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(Uid::from_raw)
        .ok_or_else(|| {
            anyhow!(
                "Running as root but SUDO_UID not set. Use 'sudo' to run as root."
            )
        })?;

    let gid = env::var("SUDO_GID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(Gid::from_raw)
        .ok_or_else(|| {
            anyhow!(
                "Running as root but SUDO_GID not set. Use 'sudo' to run as root."
            )
        })?;

    // Drop privileges - must do gid first, then uid
    // (can't change gid after dropping uid)
    setgid(gid).context("Failed to drop group privileges")?;
    setuid(uid).context("Failed to drop user privileges")?;

    info!("Dropped privileges to uid={}, gid={}", uid, gid);
    Ok(())
}

/// Get information about the current privilege state
pub fn privilege_info() -> PrivilegeInfo {
    let effective_uid = Uid::effective();
    let effective_gid = Gid::effective();
    let is_root = effective_uid.is_root();

    let sudo_user = env::var("SUDO_USER").ok();
    let sudo_uid = env::var("SUDO_UID").ok().and_then(|s| s.parse().ok());
    let sudo_gid = env::var("SUDO_GID").ok().and_then(|s| s.parse().ok());

    PrivilegeInfo {
        effective_uid: effective_uid.as_raw(),
        effective_gid: effective_gid.as_raw(),
        is_root,
        sudo_user,
        sudo_uid,
        sudo_gid,
    }
}

/// Information about the current privilege state
#[derive(Debug, Clone)]
pub struct PrivilegeInfo {
    pub effective_uid: u32,
    pub effective_gid: u32,
    pub is_root: bool,
    pub sudo_user: Option<String>,
    pub sudo_uid: Option<u32>,
    pub sudo_gid: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_privilege_info() {
        let info = privilege_info();
        // Basic sanity check - we should have valid UIDs
        assert!(info.effective_uid < 65534 || info.effective_uid == 65534);
    }

    #[test]
    fn test_is_root_returns_correct_value() {
        let info = privilege_info();
        assert_eq!(is_root(), info.is_root);
    }
}
