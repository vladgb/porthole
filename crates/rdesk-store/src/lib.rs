//! Saved PCs and trusted certificates, kept as TOML files in `$XDG_CONFIG_HOME/rdesk/`.
//!
//! Every change is written straight away (to a temporary file that replaces the old one), so a
//! crash never leaves a half-written file. Passwords are never stored here.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::warn;
pub use uuid::Uuid;

const PCS_FILE: &str = "pcs.toml";
const CERTIFICATES_FILE: &str = "trusted-certificates.toml";
const SETTINGS_FILE: &str = "settings.toml";
const FORMAT_VERSION: u32 = 1;
const DEFAULT_PORT: u16 = 3389;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("couldn't find the configuration folder (neither XDG_CONFIG_HOME nor HOME is set)")]
    NoConfigDir,
    #[error("couldn't read {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("couldn't save {}: {source}", path.display())]
    Write { path: PathBuf, source: io::Error },
    #[error("{} was written by a newer version of this app (format {found})", path.display())]
    NewerFormat { path: PathBuf, found: u32 },
}

/// A saved PC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pc {
    pub id: Uuid,
    /// Friendly name; empty means "show the host name".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Groups PCs in the main window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `DOMAIN\user`, `user@domain` or `user`. Asked for when connecting if missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Network Level Authentication. Off only for PCs that don't support it: the PC then shows its
    /// own sign-in screen, and credentials aren't checked before the session starts.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub nla: bool,
    /// Share clipboard text with the PC.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub clipboard: bool,
    /// Unix time of the last successful connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected: Option<u64>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn yes() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl Pc {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: String::new(),
            host: host.into(),
            port,
            group: None,
            username: None,
            nla: true,
            clipboard: true,
            last_connected: None,
        }
    }

    /// The friendly name, or the address when there is none.
    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() {
            self.address()
        } else {
            self.name.clone()
        }
    }

    /// `host`, `host:port`, `[v6]:port` — the form a user would type.
    pub fn address(&self) -> String {
        match (self.port, self.host.contains(':')) {
            (DEFAULT_PORT, _) => self.host.clone(),
            (port, true) => format!("[{}]:{port}", self.host),
            (port, false) => format!("{}:{port}", self.host),
        }
    }
}

/// What we know about a server certificate that failed normal verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertificateTrust {
    /// Never trusted for this PC.
    Unknown,
    /// The user chose to always trust exactly this certificate for this PC.
    Trusted,
    /// The user trusted a different certificate for this PC; this one is new.
    Changed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TrustedCertificate {
    host: String,
    port: u16,
    /// Uppercase, colon-separated hex (as shown to the user).
    sha256: String,
    added: u64,
}

/// How the main window shows saved PCs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    #[default]
    Grid,
    List,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    #[default]
    Name,
    /// Most recently connected first.
    Recent,
}

/// App preferences.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub layout: Layout,
    pub sort: SortOrder,
}

#[derive(Serialize, Deserialize)]
struct SettingsFile {
    version: u32,
    #[serde(default)]
    view: Settings,
}

#[derive(Serialize, Deserialize)]
struct PcsFile {
    version: u32,
    #[serde(default, rename = "pc")]
    pcs: Vec<Pc>,
}

#[derive(Serialize, Deserialize)]
struct CertificatesFile {
    version: u32,
    #[serde(default, rename = "certificate")]
    certificates: Vec<TrustedCertificate>,
}

/// A file that couldn't be parsed. It was moved aside and the app started with an empty list.
#[derive(Clone, Debug)]
pub struct Recovered {
    pub path: PathBuf,
    pub backup: PathBuf,
    pub error: String,
}

#[derive(Debug)]
pub struct Store {
    /// `None`: nothing is saved (used when the configuration can't be read safely).
    dir: Option<PathBuf>,
    /// Thumbnails; `None`: not kept.
    cache_dir: Option<PathBuf>,
    pcs: Vec<Pc>,
    certificates: Vec<TrustedCertificate>,
    settings: Settings,
    recovered: Vec<Recovered>,
}

impl Store {
    /// Opens the store in `$XDG_CONFIG_HOME/rdesk` (or `~/.config/rdesk`).
    /// Thumbnails go to `$XDG_CACHE_HOME/rdesk/thumbnails` (or `~/.cache/...`).
    pub fn open_default() -> Result<Self, StoreError> {
        let dir = xdg_dir("XDG_CONFIG_HOME", ".config").ok_or(StoreError::NoConfigDir)?;
        Self::open(dir, xdg_dir("XDG_CACHE_HOME", ".cache").map(|d| d.join("thumbnails")))
    }

    /// Opens the store in `dir`. Missing files are fine; unreadable TOML is moved aside.
    pub fn open(dir: impl Into<PathBuf>, cache_dir: Option<PathBuf>) -> Result<Self, StoreError> {
        let dir = dir.into();
        let mut recovered = Vec::new();
        let pcs: Option<PcsFile> = load(&dir.join(PCS_FILE), &mut recovered)?;
        let certificates: Option<CertificatesFile> = load(&dir.join(CERTIFICATES_FILE), &mut recovered)?;
        let settings: Option<SettingsFile> = load(&dir.join(SETTINGS_FILE), &mut recovered)?;
        for (version, file) in [
            (pcs.as_ref().map(|f| f.version), PCS_FILE),
            (certificates.as_ref().map(|f| f.version), CERTIFICATES_FILE),
            (settings.as_ref().map(|f| f.version), SETTINGS_FILE),
        ] {
            if let Some(found) = version.filter(|&v| v > FORMAT_VERSION) {
                return Err(StoreError::NewerFormat {
                    path: dir.join(file),
                    found,
                });
            }
        }
        let mut pcs = pcs.map(|f| f.pcs).unwrap_or_default();
        sort(&mut pcs);
        Ok(Self {
            dir: Some(dir),
            cache_dir,
            pcs,
            certificates: certificates.map(|f| f.certificates).unwrap_or_default(),
            settings: settings.map(|f| f.view).unwrap_or_default(),
            recovered,
        })
    }

    /// A store that keeps everything in memory only.
    pub fn in_memory() -> Self {
        Self {
            dir: None,
            cache_dir: None,
            pcs: Vec::new(),
            certificates: Vec::new(),
            settings: Settings::default(),
            recovered: Vec::new(),
        }
    }

    /// Files that were unreadable when the store was opened.
    pub fn recovered(&self) -> &[Recovered] {
        &self.recovered
    }

    /// Saved PCs, sorted by name.
    pub fn pcs(&self) -> &[Pc] {
        &self.pcs
    }

    pub fn pc(&self, id: Uuid) -> Option<&Pc> {
        self.pcs.iter().find(|pc| pc.id == id)
    }

    /// Adds `pc`, or replaces the saved PC with the same id.
    pub fn save_pc(&mut self, pc: Pc) -> Result<(), StoreError> {
        match self.pcs.iter_mut().find(|p| p.id == pc.id) {
            Some(existing) => *existing = pc,
            None => self.pcs.push(pc),
        }
        sort(&mut self.pcs);
        self.write_pcs()
    }

    pub fn remove_pc(&mut self, id: Uuid) -> Result<(), StoreError> {
        let before = self.pcs.len();
        self.pcs.retain(|pc| pc.id != id);
        if self.pcs.len() == before {
            return Ok(());
        }
        if let Some(thumbnail) = self.thumbnail_path(id) {
            let _ = fs::remove_file(thumbnail);
        }
        self.write_pcs()
    }

    /// Group names in use, sorted.
    pub fn groups(&self) -> Vec<String> {
        let mut groups: Vec<String> = self.pcs.iter().filter_map(|pc| pc.group.clone()).collect();
        groups.sort_by_key(|g| g.to_lowercase());
        groups.dedup();
        groups
    }

    /// Where the PC's last-session thumbnail (PNG) is kept, if thumbnails are kept at all.
    pub fn thumbnail_path(&self, id: Uuid) -> Option<PathBuf> {
        Some(self.cache_dir.as_ref()?.join(format!("{id}.png")))
    }

    pub fn save_thumbnail(&self, id: Uuid, png: &[u8]) -> Result<(), StoreError> {
        let (Some(dir), Some(path)) = (&self.cache_dir, self.thumbnail_path(id)) else {
            return Ok(());
        };
        write_atomic(dir, &path, png).map_err(|source| StoreError::Write { path, source })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn save_settings(&mut self, settings: Settings) -> Result<(), StoreError> {
        if self.settings == settings {
            return Ok(());
        }
        self.settings = settings;
        let file = SettingsFile {
            version: FORMAT_VERSION,
            view: self.settings.clone(),
        };
        self.write(SETTINGS_FILE, &file)
    }

    /// Records a successful connection.
    pub fn mark_connected(&mut self, id: Uuid) -> Result<(), StoreError> {
        let Some(pc) = self.pcs.iter_mut().find(|p| p.id == id) else {
            return Ok(());
        };
        pc.last_connected = Some(now());
        self.write_pcs()
    }

    pub fn certificate_trust(&self, host: &str, port: u16, sha256: &str) -> CertificateTrust {
        let host = normalize_host(host);
        let mut known = self
            .certificates
            .iter()
            .filter(|c| c.host == host && c.port == port)
            .peekable();
        if known.peek().is_none() {
            CertificateTrust::Unknown
        } else if known.any(|c| c.sha256.eq_ignore_ascii_case(sha256)) {
            CertificateTrust::Trusted
        } else {
            CertificateTrust::Changed
        }
    }

    /// Always trusts this certificate for `host:port`, replacing any certificate trusted before.
    pub fn trust_certificate(&mut self, host: &str, port: u16, sha256: &str) -> Result<(), StoreError> {
        let host = normalize_host(host);
        self.certificates.retain(|c| !(c.host == host && c.port == port));
        self.certificates.push(TrustedCertificate {
            host,
            port,
            sha256: sha256.to_ascii_uppercase(),
            added: now(),
        });
        self.write_certificates()
    }

    fn write_pcs(&self) -> Result<(), StoreError> {
        let file = PcsFile {
            version: FORMAT_VERSION,
            pcs: self.pcs.clone(),
        };
        self.write(PCS_FILE, &file)
    }

    fn write_certificates(&self) -> Result<(), StoreError> {
        let file = CertificatesFile {
            version: FORMAT_VERSION,
            certificates: self.certificates.clone(),
        };
        self.write(CERTIFICATES_FILE, &file)
    }

    fn write(&self, name: &str, value: &impl Serialize) -> Result<(), StoreError> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let path = dir.join(name);
        let text = toml::to_string_pretty(value).expect("store types always serialize");
        write_atomic(dir, &path, text.as_bytes()).map_err(|source| StoreError::Write { path, source })
    }
}

/// `$VAR/rdesk`, or `~/<fallback>/rdesk` when the variable is unset or not absolute.
fn xdg_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    let base = std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(fallback)))?;
    Some(base.join("rdesk"))
}

fn load<T: DeserializeOwned>(path: &Path, recovered: &mut Vec<Recovered>) -> Result<Option<T>, StoreError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StoreError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    match toml::from_str(&text) {
        Ok(value) => Ok(Some(value)),
        Err(e) => {
            // Keep the user's file for manual repair instead of overwriting it on the next save.
            let backup = path.with_extension(format!("toml.broken-{}", now()));
            fs::rename(path, &backup).map_err(|source| StoreError::Write {
                path: backup.clone(),
                source,
            })?;
            warn!(path = %path.display(), backup = %backup.display(), error = %e, "Unreadable file moved aside");
            recovered.push(Recovered {
                path: path.to_owned(),
                backup,
                error: e.message().to_owned(),
            });
            Ok(None)
        }
    }
}

fn write_atomic(dir: &Path, path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

fn sort(pcs: &mut [Pc]) {
    pcs.sort_by_cached_key(|pc| (pc.display_name().to_lowercase(), pc.id));
}

fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP_A: &str = "AA:BB";
    const FP_B: &str = "CC:DD";

    #[test]
    fn pcs_round_trip_sorted_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path(), None).unwrap();
        assert!(store.pcs().is_empty());

        let mut office = Pc::new("office.example", 3389);
        office.name = "office".into();
        office.username = Some(r"CORP\me".into());
        let mut lab = Pc::new("10.0.0.5", 3390);
        lab.nla = false;
        store.save_pc(office.clone()).unwrap();
        store.save_pc(lab.clone()).unwrap();
        store.mark_connected(lab.id).unwrap();

        let reopened = Store::open(dir.path(), None).unwrap();
        let names: Vec<_> = reopened.pcs().iter().map(Pc::display_name).collect();
        assert_eq!(names, ["10.0.0.5:3390", "office"]);
        assert_eq!(reopened.pc(office.id), Some(&office));
        assert!(reopened.pc(lab.id).unwrap().last_connected.is_some());
        assert!(!reopened.pc(lab.id).unwrap().nla);

        let mut store = reopened;
        store.remove_pc(office.id).unwrap();
        assert_eq!(Store::open(dir.path(), None).unwrap().pcs().len(), 1);
    }

    #[test]
    fn editing_keeps_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path(), None).unwrap();
        let mut pc = Pc::new("a", 3389);
        store.save_pc(pc.clone()).unwrap();
        pc.host = "b".into();
        store.save_pc(pc.clone()).unwrap();
        assert_eq!(store.pcs(), [pc]);
    }

    #[test]
    fn certificate_trust_per_host_and_port() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path(), None).unwrap();
        assert_eq!(store.certificate_trust("pc", 3389, FP_A), CertificateTrust::Unknown);

        store.trust_certificate("PC.", 3389, FP_A).unwrap();
        let store = Store::open(dir.path(), None).unwrap();
        assert_eq!(store.certificate_trust("pc", 3389, "aa:bb"), CertificateTrust::Trusted);
        assert_eq!(store.certificate_trust("pc", 3389, FP_B), CertificateTrust::Changed);
        assert_eq!(store.certificate_trust("pc", 3390, FP_A), CertificateTrust::Unknown);

        let mut store = store;
        store.trust_certificate("pc", 3389, FP_B).unwrap();
        assert_eq!(store.certificate_trust("pc", 3389, FP_A), CertificateTrust::Changed);
        assert_eq!(store.certificate_trust("pc", 3389, FP_B), CertificateTrust::Trusted);
    }

    #[test]
    fn broken_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(PCS_FILE), "this is [not toml").unwrap();
        let store = Store::open(dir.path(), None).unwrap();
        assert!(store.pcs().is_empty());
        let [recovered] = store.recovered() else { panic!() };
        assert!(recovered.backup.exists());
        assert!(!dir.path().join(PCS_FILE).exists());
    }

    #[test]
    fn newer_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(PCS_FILE), "version = 2\n").unwrap();
        assert!(matches!(
            Store::open(dir.path(), None),
            Err(StoreError::NewerFormat { found: 2, .. })
        ));
    }

    #[test]
    fn files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("rdesk");
        Store::open(&config, None).unwrap().save_pc(Pc::new("a", 3389)).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&config), 0o700);
        assert_eq!(mode(&config.join(PCS_FILE)), 0o600);
    }

    #[test]
    fn settings_and_thumbnails() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let mut store = Store::open(dir.path(), Some(cache.clone())).unwrap();
        assert_eq!(store.settings(), &Settings::default());
        let settings = Settings {
            layout: Layout::List,
            sort: SortOrder::Recent,
        };
        store.save_settings(settings.clone()).unwrap();

        let mut pc = Pc::new("a", 3389);
        pc.group = Some("Lab".into());
        store.save_pc(pc.clone()).unwrap();
        store.save_thumbnail(pc.id, b"png").unwrap();
        let thumbnail = store.thumbnail_path(pc.id).unwrap();
        assert!(thumbnail.exists());

        let mut store = Store::open(dir.path(), Some(cache)).unwrap();
        assert_eq!(store.settings(), &settings);
        assert_eq!(store.groups(), ["Lab"]);
        store.remove_pc(pc.id).unwrap();
        assert!(!thumbnail.exists());
    }

    #[test]
    fn addresses() {
        assert_eq!(Pc::new("pc", 3389).address(), "pc");
        assert_eq!(Pc::new("pc", 3390).address(), "pc:3390");
        assert_eq!(Pc::new("fe80::1", 3389).address(), "fe80::1");
        assert_eq!(Pc::new("fe80::1", 3390).address(), "[fe80::1]:3390");
    }
}
