//! Saved passwords, in the desktop's keyring (Secret Service, or the secret portal in Flatpak).
//!
//! Entries are keyed by PC id and user name, so a changed user name never gets the old password.
//! Needs a Tokio runtime.

use std::collections::HashMap;

use uuid::Uuid;
use zeroize::Zeroizing;

const APPLICATION: &str = "io.github.vladgb.Porthole";
/// Earlier app IDs: passwords saved under them are found and moved over.
const LEGACY_APPLICATIONS: [&str; 2] = ["io.github.porthole.Porthole", "io.github.rdesk.Rdesk"];

#[derive(Debug, thiserror::Error)]
#[error("the keyring is unavailable: {0}")]
pub struct KeyringError(Box<oo7::Error>);

impl From<oo7::Error> for KeyringError {
    fn from(e: oo7::Error) -> Self {
        Self(Box::new(e))
    }
}

fn attributes(pc: Uuid, username: Option<&str>) -> HashMap<&'static str, String> {
    attributes_for(APPLICATION, pc, username)
}

fn attributes_for(application: &str, pc: Uuid, username: Option<&str>) -> HashMap<&'static str, String> {
    let mut attributes = HashMap::from([("application", application.to_owned()), ("pc", pc.to_string())]);
    if let Some(username) = username {
        attributes.insert("username", username.to_owned());
    }
    attributes
}

/// The saved password for `username` on the PC, if any.
pub async fn password(pc: Uuid, username: &str) -> Result<Option<Zeroizing<String>>, KeyringError> {
    let keyring = oo7::Keyring::new().await?;
    let mut items = keyring.search_items(&attributes(pc, Some(username))).await?;
    let mut legacy = None;
    for application in LEGACY_APPLICATIONS {
        if !items.is_empty() {
            break;
        }
        items = keyring
            .search_items(&attributes_for(application, pc, Some(username)))
            .await?;
        legacy = Some(application);
    }
    let Some(item) = items.first() else { return Ok(None) };
    item.unlock().await?;
    let secret = item.secret().await?;
    let password = Zeroizing::new(String::from_utf8_lossy(secret.as_bytes()).into_owned());
    if let Some(application) = legacy {
        let label = item.label().await?;
        keyring
            .create_item(&label, &attributes(pc, Some(username)), password.as_str(), true)
            .await?;
        keyring.delete(&attributes_for(application, pc, None)).await?;
    }
    Ok(Some(password))
}

/// Saves the password for `username`, replacing any saved for the PC (under any user name).
pub async fn store_password(pc: Uuid, username: &str, label: &str, password: &str) -> Result<(), KeyringError> {
    let keyring = oo7::Keyring::new().await?;
    forget_all(&keyring, pc).await?;
    keyring
        .create_item(label, &attributes(pc, Some(username)), password, true)
        .await?;
    Ok(())
}

/// Removes any saved password for the PC.
pub async fn forget_password(pc: Uuid) -> Result<(), KeyringError> {
    let keyring = oo7::Keyring::new().await?;
    forget_all(&keyring, pc).await
}

/// Deletes the PC's passwords under the current and earlier app IDs.
async fn forget_all(keyring: &oo7::Keyring, pc: Uuid) -> Result<(), KeyringError> {
    keyring.delete(&attributes(pc, None)).await?;
    for application in LEGACY_APPLICATIONS {
        keyring.delete(&attributes_for(application, pc, None)).await?;
    }
    Ok(())
}
