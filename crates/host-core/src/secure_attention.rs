//! Delivering the secure attention sequence (Ctrl+Alt+Del) on the host.
//!
//! A viewer sends `Control::RequestSecureAttention` and the host calls
//! [`deliver`]. On Windows the security screen is a protected desktop that an
//! ordinary process cannot reach; reaching it is only possible from the
//! helper service that runs at the sign-in screen (see
//! `docs/WINDOWS_SERVICE.md`). [`deliver`] uses the `SendSAS` broker when the
//! host was launched by that service and reports [`Unsupported`] otherwise.

/// Why the secure attention sequence could not be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecureAttentionError {
    /// This platform has no secure attention sequence, or OpenDesk cannot
    /// reach it yet from where it runs.
    Unsupported,
}

impl std::fmt::Display for SecureAttentionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => write!(f, "secure attention is not available on this host"),
        }
    }
}

impl std::error::Error for SecureAttentionError {}

#[cfg(target_os = "windows")]
fn helper_active() -> bool {
    crate::windows_service::helper_active()
}

/// Whether this host can deliver the secure attention sequence. The host only
/// offers the viewer a Ctrl+Alt+Del control when it is the helper-launched
/// host on Windows.
pub fn available() -> bool {
    #[cfg(target_os = "windows")]
    { helper_active() }
    #[cfg(not(target_os = "windows"))]
    { false }
}

/// Deliver Ctrl+Alt+Del on this host. Succeeds only where OpenDesk can reach
/// the secure attention sequence; everywhere else it returns [`Unsupported`]
/// and the caller simply ignores the request.
pub fn deliver() -> Result<(), SecureAttentionError> {
    #[cfg(target_os = "windows")]
    {
        if !helper_active() {
            return Err(SecureAttentionError::Unsupported);
        }
        return crate::windows_service::send_secure_attention();
    }
    #[cfg(not(target_os = "windows"))]
    { Err(SecureAttentionError::Unsupported) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_windows_has_no_secure_attention() {
        #[cfg(not(target_os = "windows"))]
        assert_eq!(deliver(), Err(SecureAttentionError::Unsupported));
    }
}
