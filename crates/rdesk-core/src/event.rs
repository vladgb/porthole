use std::fmt;
use std::sync::Arc;

use tokio::sync::oneshot;

pub use ironrdp::graphics::pointer::DecodedPointer;

/// Events from a session task to the UI, in order.
#[derive(Debug)]
pub enum SessionEvent {
    /// The server certificate could not be verified. The session waits until the prompt is answered or dropped.
    CertificatePrompt(CertificatePrompt),
    /// Sign-in and activation finished; the framebuffer has this size.
    Connected {
        width: u16,
        height: u16,
    },
    /// The server changed the desktop size (deactivation-reactivation). The framebuffer was reallocated.
    DesktopResized {
        width: u16,
        height: u16,
    },
    /// New pixels are in the framebuffer. Sent once until the UI takes the damage with
    /// [`crate::Framebuffer::take_damage`], so it never queues up.
    FrameReady,
    Pointer(PointerEvent),
    /// The remote PC copied text; put it on the local clipboard.
    ClipboardText(String),
    /// Always the last event of a session.
    Disconnected(DisconnectReason),
}

#[derive(Debug, Clone)]
pub enum PointerEvent {
    /// Use the system default arrow.
    Default,
    Hidden,
    /// The server moved the pointer (e.g. "snap to default button").
    Position {
        x: u16,
        y: u16,
    },
    /// RGBA, not premultiplied.
    Bitmap(Arc<DecodedPointer>),
}

/// Why a session ended, worded for people.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DisconnectReason {
    #[error("You disconnected")]
    UserRequested,
    #[error("The remote PC ended the session: {0}")]
    ServerEnded(String),
    #[error("Couldn't find the PC “{0}”. Check the name, or the network connection.")]
    HostNotFound(String),
    #[error("Couldn't connect to the PC: {0}")]
    Unreachable(String),
    #[error("The PC took too long to respond while {0}")]
    Timeout(&'static str),
    #[error("The connection was cancelled because the PC's certificate wasn't trusted")]
    CertificateRejected,
    #[error("{0}")]
    Authentication(AuthFailure),
    #[error(
        "The PC doesn't support Network Level Authentication (NLA). If you trust the network, you can turn off NLA in this PC's settings."
    )]
    NlaNotSupported,
    #[error("The PC requires Network Level Authentication (NLA). Turn on NLA in this PC's settings.")]
    NlaRequired,
    #[error("The PC and this app couldn't agree on security settings: {0}")]
    Negotiation(String),
    #[error("The connection to the PC was lost: {0}")]
    ConnectionLost(String),
    #[error("The connection failed because of a protocol error: {0}")]
    Protocol(String),
    #[error("An internal error ended the session: {0}")]
    Internal(String),
}

impl DisconnectReason {
    /// Whether retrying automatically is safe. Never retry sign-in failures: each attempt counts
    /// towards the account lockout threshold.
    pub fn may_retry(&self) -> bool {
        matches!(
            self,
            Self::ConnectionLost(_) | Self::Unreachable(_) | Self::Timeout(_) | Self::HostNotFound(_)
        )
    }
}

/// Sign-in failures reported by the server during NLA (from the CredSSP NTSTATUS).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthFailure {
    #[error("The user name or password is incorrect")]
    WrongCredentials,
    #[error("The account is locked out. Wait for it to unlock, or ask an administrator.")]
    AccountLockedOut,
    #[error("The account is disabled")]
    AccountDisabled,
    #[error("The password has expired")]
    PasswordExpired,
    #[error("The password must be changed before signing in")]
    PasswordMustChange,
    #[error("The account isn't allowed to sign in right now (logon hours or workstation restrictions)")]
    AccountRestriction,
    #[error("The account isn't allowed to sign in remotely")]
    LogonNotGranted,
    #[error("Sign-in failed: {0}")]
    Other(String),
}

/// What the trust dialog shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateInfo {
    /// The host name we connected to.
    pub host: String,
    pub subject: String,
    pub issuer: String,
    pub not_before: String,
    pub not_after: String,
    /// Uppercase hex, colon separated.
    pub sha256_fingerprint: String,
    /// DER encoding, for storing or exporting.
    pub der: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertificateProblem {
    /// Self-signed or issued by an authority this computer doesn't trust (the usual case for RDP).
    UntrustedIssuer,
    /// Valid, but for a different name than the one we connected to.
    NameMismatch,
    Expired,
    NotYetValid,
    Revoked,
    Other(String),
}

impl fmt::Display for CertificateProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UntrustedIssuer => f.write_str("The certificate isn't from a trusted certifying authority"),
            Self::NameMismatch => f.write_str("The certificate was issued for a different name"),
            Self::Expired => f.write_str("The certificate has expired"),
            Self::NotYetValid => f.write_str("The certificate isn't valid yet"),
            Self::Revoked => f.write_str("The certificate has been revoked"),
            Self::Other(e) => write!(f, "The certificate couldn't be verified: {e}"),
        }
    }
}

/// A pending question to the user. Dropping it rejects the certificate.
pub struct CertificatePrompt {
    pub certificate: CertificateInfo,
    pub problem: CertificateProblem,
    pub(crate) reply: oneshot::Sender<bool>,
}

impl CertificatePrompt {
    pub fn accept(self) {
        let _ = self.reply.send(true);
    }

    pub fn reject(self) {
        let _ = self.reply.send(false);
    }
}

impl fmt::Debug for CertificatePrompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertificatePrompt")
            .field("certificate", &self.certificate)
            .field("problem", &self.problem)
            .finish_non_exhaustive()
    }
}
