//! Shared folders: local directories that appear on the PC as drives (RDPDR file system,
//! MS-RDPEFS).
//!
//! The server sends Windows-style file operations; each runs against the shared directory. Paths
//! are confined to it: `..` is rejected and existing paths are resolved (following symlinks) and
//! must stay under the share's root.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ironrdp::core::impl_as_any;
use ironrdp::pdu::PduResult;
use ironrdp::rdpdr::backend::RdpdrBackend;
use ironrdp::rdpdr::pdu::RdpdrPdu;
use ironrdp::rdpdr::pdu::efs::{
    Boolean, Characteristics, ClientDriveQueryDirectoryResponse, ClientDriveQueryInformationResponse,
    ClientDriveQueryVolumeInformationResponse, ClientDriveSetInformationResponse, CreateDisposition, CreateOptions,
    DesiredAccess, DeviceCloseResponse, DeviceControlRequest, DeviceControlResponse, DeviceCreateRequest,
    DeviceCreateResponse, DeviceIoRequest, DeviceIoResponse, DeviceReadResponse, DeviceWriteResponse,
    FileAttributeTagInformation, FileAttributes, FileBasicInformation, FileBothDirectoryInformation,
    FileDirectoryInformation, FileFsAttributeInformation, FileFsDeviceInformation, FileFsFullSizeInformation,
    FileFsSizeInformation, FileFsVolumeInformation, FileFullDirectoryInformation, FileInformationClass,
    FileInformationClassLevel, FileNamesInformation, FileStandardInformation, FileSystemAttributes,
    FileSystemInformationClass, FileSystemInformationClassLevel, Information, NtStatus, ServerDeviceAnnounceResponse,
    ServerDriveIoRequest, ServerDriveQueryDirectoryRequest, ServerDriveQueryInformationRequest,
    ServerDriveQueryVolumeInformationRequest, ServerDriveSetInformationRequest,
};
use ironrdp::rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
use ironrdp::svc::SvcMessage;
use tracing::{debug, warn};

/// A local directory offered to the PC as a drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFolder {
    /// Shown on the PC as "<name> on <this computer>".
    pub name: String,
    pub path: PathBuf,
}

const OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
const OBJECT_PATH_NOT_FOUND: u32 = 0xC000_003A;
const FILE_IS_A_DIRECTORY: u32 = 0xC000_00BA;
const END_OF_FILE: u32 = 0xC000_0011;
const DISK_FULL: u32 = 0xC000_007F;
/// FILE_DEVICE_DISK
const DEVICE_TYPE_DISK: u32 = 0x0000_0007;

/// Device ids for shared folders (the first is used by nothing else).
pub(crate) const FIRST_DEVICE_ID: u32 = 1;

#[derive(Debug)]
struct Open {
    path: PathBuf,
    file: Option<File>,
    delete_on_close: bool,
    /// Directory listing in progress (QueryDirectory), with the next entry to return.
    listing: Option<(Vec<Entry>, usize)>,
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    metadata: fs::Metadata,
}

#[derive(Debug)]
pub(crate) struct DriveBackend {
    /// Device id → (share name, canonical root).
    roots: HashMap<u32, (String, PathBuf)>,
    opens: HashMap<u32, Open>,
    next_file_id: u32,
}

impl_as_any!(DriveBackend);

impl DriveBackend {
    /// Returns the backend and the (device id, name) pairs to announce.
    pub(crate) fn new(folders: &[SharedFolder]) -> (Self, Vec<(u32, String)>) {
        let mut roots = HashMap::new();
        let mut announce = Vec::new();
        for (i, folder) in folders.iter().enumerate() {
            let id = FIRST_DEVICE_ID + i as u32;
            match fs::canonicalize(&folder.path) {
                Ok(root) if root.is_dir() => {
                    roots.insert(id, (folder.name.clone(), root));
                    announce.push((id, dos_name(&folder.name)));
                }
                _ => warn!(path = %folder.path.display(), "Shared folder isn't a readable directory; skipped"),
            }
        }
        let backend = Self {
            roots,
            opens: HashMap::new(),
            next_file_id: 1,
        };
        (backend, announce)
    }

    fn create(&mut self, req: DeviceCreateRequest) -> RdpdrPdu {
        let io = req.device_io_request.clone();
        let reply = |status: NtStatus, file_id: u32, information: Information| {
            RdpdrPdu::DeviceCreateResponse(DeviceCreateResponse {
                device_io_reply: DeviceIoResponse::new(io.clone(), status),
                file_id,
                information,
            })
        };
        let Some((_, root)) = self.roots.get(&io.device_id) else {
            return reply(NtStatus::NO_SUCH_FILE, 0, Information::empty());
        };
        let Some(path) = resolve(root, &req.path) else {
            return reply(NtStatus::ACCESS_DENIED, 0, Information::empty());
        };
        let result = open(&path, &req);
        debug!(
            path = %req.path,
            disposition = ?req.create_disposition,
            options = ?req.create_options,
            ok = result.is_ok(),
            "Create"
        );
        match result {
            Ok((file, information)) => {
                let file_id = self.next_file_id;
                self.next_file_id = self.next_file_id.wrapping_add(1).max(1);
                let delete_on_close = req.create_options.contains(CreateOptions::FILE_DELETE_ON_CLOSE);
                self.opens.insert(
                    file_id,
                    Open {
                        path,
                        file,
                        delete_on_close,
                        listing: None,
                    },
                );
                reply(NtStatus::SUCCESS, file_id, information)
            }
            Err(status) => reply(status, 0, Information::empty()),
        }
    }

    fn close(&mut self, io: DeviceIoRequest) -> RdpdrPdu {
        if let Some(open) = self.opens.remove(&io.file_id) {
            drop(open.file);
            if open.delete_on_close {
                let removed = if open.path.is_dir() {
                    fs::remove_dir(&open.path)
                } else {
                    fs::remove_file(&open.path)
                };
                if let Err(e) = removed {
                    debug!(path = %open.path.display(), error = %e, "Delete on close failed");
                }
            }
        }
        RdpdrPdu::DeviceCloseResponse(DeviceCloseResponse {
            device_io_response: DeviceIoResponse::new(io, NtStatus::SUCCESS),
        })
    }

    fn read(&mut self, io: DeviceIoRequest, offset: u64, length: u32) -> RdpdrPdu {
        let result = (|| {
            let file = self
                .opens
                .get_mut(&io.file_id)
                .and_then(|o| o.file.as_mut())
                .ok_or(NtStatus::from(FILE_IS_A_DIRECTORY))?;
            file.seek(SeekFrom::Start(offset)).map_err(|e| status(&e))?;
            // Windows reads in chunks of at most a few MB; cap anyway.
            let mut data = vec![0; length.min(16 * 1024 * 1024) as usize];
            let mut filled = 0;
            while filled < data.len() {
                match file.read(&mut data[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(status(&e)),
                }
            }
            data.truncate(filled);
            Ok(data)
        })();
        let (status, read_data) = match result {
            Ok(data) if data.is_empty() && length > 0 => (NtStatus::from(END_OF_FILE), data),
            Ok(data) => (NtStatus::SUCCESS, data),
            Err(status) => (status, Vec::new()),
        };
        RdpdrPdu::DeviceReadResponse(DeviceReadResponse {
            device_io_reply: DeviceIoResponse::new(io, status),
            read_data,
        })
    }

    fn write(&mut self, io: DeviceIoRequest, offset: u64, data: &[u8]) -> RdpdrPdu {
        let result = (|| {
            let file = self
                .opens
                .get_mut(&io.file_id)
                .and_then(|o| o.file.as_mut())
                .ok_or(NtStatus::from(FILE_IS_A_DIRECTORY))?;
            file.seek(SeekFrom::Start(offset)).map_err(|e| status(&e))?;
            file.write_all(data).map_err(|e| status(&e))?;
            Ok(data.len() as u32)
        })();
        let (status, length) = match result {
            Ok(n) => (NtStatus::SUCCESS, n),
            Err(status) => (status, 0),
        };
        RdpdrPdu::DeviceWriteResponse(DeviceWriteResponse {
            device_io_reply: DeviceIoResponse::new(io, status),
            length,
        })
    }

    fn query_information(&mut self, req: ServerDriveQueryInformationRequest) -> RdpdrPdu {
        let io = req.device_io_request;
        let info = self
            .opens
            .get(&io.file_id)
            .ok_or(NtStatus::UNSUCCESSFUL)
            .and_then(|open| fs::metadata(&open.path).map(|m| (open, m)).map_err(|e| status(&e)))
            .and_then(|(open, metadata)| {
                let name = open.path.file_name().map(|n| n.to_string_lossy().into_owned());
                let attributes = attributes(&metadata, name.as_deref().unwrap_or_default());
                let (created, accessed, written, changed) = times(&metadata);
                Ok(match req.file_info_class_lvl {
                    FileInformationClassLevel::FILE_BASIC_INFORMATION => {
                        FileInformationClass::Basic(FileBasicInformation {
                            creation_time: created,
                            last_access_time: accessed,
                            last_write_time: written,
                            change_time: changed,
                            file_attributes: attributes,
                        })
                    }
                    FileInformationClassLevel::FILE_STANDARD_INFORMATION => {
                        FileInformationClass::Standard(FileStandardInformation {
                            allocation_size: allocation(&metadata),
                            end_of_file: metadata.len() as i64,
                            number_of_links: 1,
                            delete_pending: boolean(open.delete_on_close),
                            directory: boolean(metadata.is_dir()),
                        })
                    }
                    FileInformationClassLevel::FILE_ATTRIBUTE_TAG_INFORMATION => {
                        FileInformationClass::AttributeTag(FileAttributeTagInformation {
                            file_attributes: attributes,
                            reparse_tag: 0,
                        })
                    }
                    other => {
                        debug!(class = ?other, "Unsupported file information class");
                        return Err(NtStatus::NOT_SUPPORTED);
                    }
                })
            });
        let (status, buffer) = match info {
            Ok(info) => (NtStatus::SUCCESS, Some(info)),
            Err(status) => (status, None),
        };
        RdpdrPdu::ClientDriveQueryInformationResponse(ClientDriveQueryInformationResponse {
            device_io_response: DeviceIoResponse::new(io, status),
            buffer,
        })
    }

    fn query_directory(&mut self, req: ServerDriveQueryDirectoryRequest) -> RdpdrPdu {
        let io = req.device_io_request.clone();
        let reply = |status: NtStatus, buffer: Option<FileInformationClass>| {
            RdpdrPdu::ClientDriveQueryDirectoryResponse(ClientDriveQueryDirectoryResponse {
                device_io_reply: DeviceIoResponse::new(io.clone(), status),
                buffer,
            })
        };
        let Some(open) = self.opens.get_mut(&io.file_id) else {
            return reply(NtStatus::UNSUCCESSFUL, None);
        };
        if req.initial_query != 0 {
            // The path ends with the pattern: "\dir\*" or "\dir\name.txt".
            let pattern = req.path.rsplit('\\').next().unwrap_or("*").to_owned();
            let entries = match list(&open.path, &pattern) {
                Ok(entries) => entries,
                Err(e) => return reply(status(&e), None),
            };
            if entries.is_empty() {
                return reply(NtStatus::NO_SUCH_FILE, None);
            }
            open.listing = Some((entries, 0));
        }
        let Some((entries, next)) = open.listing.as_mut() else {
            return reply(NtStatus::NO_MORE_FILES, None);
        };
        let Some(entry) = entries.get(*next).cloned() else {
            return reply(NtStatus::NO_MORE_FILES, None);
        };
        *next += 1;
        let attributes = attributes(&entry.metadata, &entry.name);
        let (created, accessed, written, changed) = times(&entry.metadata);
        let size = if entry.metadata.is_dir() {
            0
        } else {
            entry.metadata.len() as i64
        };
        let info = match req.file_info_class_lvl {
            FileInformationClassLevel::FILE_BOTH_DIRECTORY_INFORMATION => FileInformationClass::BothDirectory(
                FileBothDirectoryInformation::new(created, accessed, written, changed, size, attributes, entry.name),
            ),
            FileInformationClassLevel::FILE_FULL_DIRECTORY_INFORMATION => FileInformationClass::FullDirectory(
                FileFullDirectoryInformation::new(created, accessed, written, changed, size, attributes, entry.name),
            ),
            FileInformationClassLevel::FILE_DIRECTORY_INFORMATION => FileInformationClass::Directory(
                FileDirectoryInformation::new(created, accessed, written, changed, size, attributes, entry.name),
            ),
            FileInformationClassLevel::FILE_NAMES_INFORMATION => {
                FileInformationClass::Names(FileNamesInformation::new(entry.name))
            }
            _ => return reply(NtStatus::NOT_SUPPORTED, None),
        };
        reply(NtStatus::SUCCESS, Some(info))
    }

    fn query_volume(&mut self, req: ServerDriveQueryVolumeInformationRequest) -> RdpdrPdu {
        let io = req.device_io_request;
        let Some((name, root)) = self.roots.get(&io.device_id) else {
            return volume_reply(io, NtStatus::UNSUCCESSFUL, None);
        };
        let space = rustix::fs::statvfs(root.as_path()).ok();
        let (block, total, free) = space.map_or((4096, 0, 0), |s| {
            (s.f_frsize.max(512), s.f_blocks as i64, s.f_bavail as i64)
        });
        // Report allocation units of `block` bytes as one sector each.
        let info = match req.fs_info_class_lvl {
            FileSystemInformationClassLevel::FILE_FS_VOLUME_INFORMATION => {
                FileSystemInformationClass::FileFsVolumeInformation(FileFsVolumeInformation {
                    volume_creation_time: 0,
                    volume_serial_number: serial(name),
                    supports_objects: Boolean::False,
                    volume_label: name.clone(),
                })
            }
            FileSystemInformationClassLevel::FILE_FS_SIZE_INFORMATION => {
                FileSystemInformationClass::FileFsSizeInformation(FileFsSizeInformation {
                    total_alloc_units: total,
                    available_alloc_units: free,
                    sectors_per_alloc_unit: 1,
                    bytes_per_sector: block as u32,
                })
            }
            FileSystemInformationClassLevel::FILE_FS_FULL_SIZE_INFORMATION => {
                FileSystemInformationClass::FileFsFullSizeInformation(FileFsFullSizeInformation {
                    total_alloc_units: total,
                    caller_available_alloc_units: free,
                    actual_available_alloc_units: free,
                    sectors_per_alloc_unit: 1,
                    bytes_per_sector: block as u32,
                })
            }
            FileSystemInformationClassLevel::FILE_FS_ATTRIBUTE_INFORMATION => {
                FileSystemInformationClass::FileFsAttributeInformation(FileFsAttributeInformation {
                    file_system_attributes: FileSystemAttributes::FILE_CASE_SENSITIVE_SEARCH
                        | FileSystemAttributes::FILE_CASE_PRESERVED_NAMES
                        | FileSystemAttributes::FILE_UNICODE_ON_DISK,
                    max_component_name_len: 255,
                    file_system_name: "NTFS".to_owned(),
                })
            }
            FileSystemInformationClassLevel::FILE_FS_DEVICE_INFORMATION => {
                FileSystemInformationClass::FileFsDeviceInformation(FileFsDeviceInformation {
                    device_type: DEVICE_TYPE_DISK,
                    characteristics: Characteristics::empty(),
                })
            }
            _ => return volume_reply(io, NtStatus::NOT_SUPPORTED, None),
        };
        volume_reply(io, NtStatus::SUCCESS, Some(info))
    }

    fn set_information(&mut self, req: ServerDriveSetInformationRequest) -> PduResult<RdpdrPdu> {
        let status = self.apply(&req).err().unwrap_or(NtStatus::SUCCESS);
        let response = ClientDriveSetInformationResponse::new(&req, status)
            .map_err(|e| ironrdp::pdu::pdu_other_err!("set information response", source: e))?;
        Ok(RdpdrPdu::ClientDriveSetInformationResponse(response))
    }

    fn apply(&mut self, req: &ServerDriveSetInformationRequest) -> Result<(), NtStatus> {
        let io = &req.device_io_request;
        let root = self.roots.get(&io.device_id).map(|(_, root)| root.clone());
        let open = self.opens.get_mut(&io.file_id).ok_or(NtStatus::UNSUCCESSFUL)?;
        match &req.set_buffer {
            FileInformationClass::Basic(basic) => {
                let mut times = fs::FileTimes::new();
                if let Some(t) = from_filetime(basic.last_write_time) {
                    times = times.set_modified(t);
                }
                if let Some(t) = from_filetime(basic.last_access_time) {
                    times = times.set_accessed(t);
                }
                if let Some(file) = &open.file {
                    file.set_times(times).map_err(|e| status(&e))?;
                }
                Ok(())
            }
            FileInformationClass::EndOfFile(eof) => set_len(open, eof.end_of_file),
            FileInformationClass::Allocation(allocation) => {
                // Only shrinking matters: growing an allocation doesn't change the content.
                let len = open.file.as_ref().map_or(0, |f| f.metadata().map_or(0, |m| m.len()));
                if (allocation.allocation_size as u64) < len {
                    set_len(open, allocation.allocation_size)?;
                }
                Ok(())
            }
            FileInformationClass::Disposition(disposition) => {
                let delete = disposition.delete_pending != 0;
                if delete && open.path.is_dir() && fs::read_dir(&open.path).is_ok_and(|mut d| d.next().is_some()) {
                    return Err(NtStatus::DIRECTORY_NOT_EMPTY);
                }
                open.delete_on_close = delete;
                Ok(())
            }
            FileInformationClass::Rename(rename) => {
                let root = root.ok_or(NtStatus::UNSUCCESSFUL)?;
                let target = resolve(&root, &rename.file_name).ok_or(NtStatus::ACCESS_DENIED)?;
                if target.exists() && rename.replace_if_exists == Boolean::False {
                    return Err(NtStatus::OBJECT_NAME_COLLISION);
                }
                fs::rename(&open.path, &target).map_err(|e| status(&e))?;
                open.path = target;
                Ok(())
            }
            other => {
                debug!(class = ?other, "Unsupported set information class");
                Err(NtStatus::NOT_SUPPORTED)
            }
        }
    }
}

fn set_len(open: &mut Open, len: i64) -> Result<(), NtStatus> {
    let file = open.file.as_ref().ok_or(NtStatus::from(FILE_IS_A_DIRECTORY))?;
    file.set_len(u64::try_from(len).map_err(|_| NtStatus::UNSUCCESSFUL)?)
        .map_err(|e| status(&e))
}

fn volume_reply(io: DeviceIoRequest, status: NtStatus, buffer: Option<FileSystemInformationClass>) -> RdpdrPdu {
    RdpdrPdu::ClientDriveQueryVolumeInformationResponse(ClientDriveQueryVolumeInformationResponse::new(
        io, status, buffer,
    ))
}

impl RdpdrBackend for DriveBackend {
    fn handle_server_device_announce_response(&mut self, pdu: ServerDeviceAnnounceResponse) -> PduResult<()> {
        debug!(?pdu, "Device announced");
        Ok(())
    }

    fn handle_scard_call(&mut self, _req: DeviceControlRequest<ScardIoCtlCode>, _call: ScardCall) -> PduResult<()> {
        Ok(())
    }

    fn handle_drive_io_request(&mut self, req: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
        let reply = match req {
            ServerDriveIoRequest::ServerCreateDriveRequest(req) => self.create(req),
            ServerDriveIoRequest::DeviceCloseRequest(req) => self.close(req.device_io_request),
            ServerDriveIoRequest::DeviceReadRequest(req) => self.read(req.device_io_request, req.offset, req.length),
            ServerDriveIoRequest::DeviceWriteRequest(req) => {
                self.write(req.device_io_request, req.offset, &req.write_data)
            }
            ServerDriveIoRequest::ServerDriveQueryInformationRequest(req) => self.query_information(req),
            ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(req) => self.query_directory(req),
            ServerDriveIoRequest::ServerDriveQueryVolumeInformationRequest(req) => self.query_volume(req),
            ServerDriveIoRequest::ServerDriveSetInformationRequest(req) => self.set_information(req)?,
            // Change notifications stay pending, as in other clients; Windows re-reads instead.
            ServerDriveIoRequest::ServerDriveNotifyChangeDirectoryRequest(_) => return Ok(Vec::new()),
            ServerDriveIoRequest::DeviceControlRequest(req) => {
                RdpdrPdu::DeviceControlResponse(DeviceControlResponse::new(req, NtStatus::SUCCESS, None))
            }
            // Byte-range locks are granted: the share is only used from this session.
            ServerDriveIoRequest::ServerDriveLockControlRequest(req) => {
                RdpdrPdu::DeviceCloseResponse(DeviceCloseResponse {
                    device_io_response: DeviceIoResponse::new(req.device_io_request, NtStatus::SUCCESS),
                })
            }
        };
        Ok(vec![SvcMessage::from(reply)])
    }
}

/// `\dir\file.txt` (relative to the share) → a local path under `root`, or `None` if it would
/// leave the share.
fn resolve(root: &Path, windows_path: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in windows_path.split(['\\', '/']).filter(|p| !p.is_empty() && *p != ".") {
        let component = Path::new(part);
        if part == ".." || !matches!(component.components().next(), Some(Component::Normal(_))) {
            return None;
        }
        path.push(part);
    }
    // Symlinks must not lead outside the share; check the deepest part that exists.
    let mut existing = path.as_path();
    while !existing.exists() {
        existing = existing.parent()?;
    }
    fs::canonicalize(existing).ok()?.starts_with(root).then_some(path)
}

fn open(path: &Path, req: &DeviceCreateRequest) -> Result<(Option<File>, Information), NtStatus> {
    let exists = path.exists();
    let want_dir = req.create_options.contains(CreateOptions::FILE_DIRECTORY_FILE);
    let disposition = req.create_disposition;
    if !exists {
        if !path.parent().is_some_and(Path::is_dir) {
            return Err(NtStatus::from(OBJECT_PATH_NOT_FOUND));
        }
        if disposition == CreateDisposition::FILE_OPEN || disposition == CreateDisposition::FILE_OVERWRITE {
            return Err(NtStatus::from(OBJECT_NAME_NOT_FOUND));
        }
    } else if disposition == CreateDisposition::FILE_CREATE {
        return Err(NtStatus::OBJECT_NAME_COLLISION);
    }

    if path.is_dir() || (!exists && want_dir) {
        if req.create_options.contains(CreateOptions::FILE_NON_DIRECTORY_FILE) {
            return Err(NtStatus::from(FILE_IS_A_DIRECTORY));
        }
        if !exists {
            fs::create_dir(path).map_err(|e| status(&e))?;
            return Ok((None, Information::FILE_SUPERSEDED));
        }
        return Ok((None, Information::FILE_OPENED));
    }
    if want_dir && exists {
        return Err(NtStatus::NOT_A_DIRECTORY);
    }

    let write = req.desired_access.intersects(
        DesiredAccess::GENERIC_WRITE
            | DesiredAccess::GENERIC_ALL
            | DesiredAccess::FILE_WRITE_DATA_OR_FILE_ADD_FILE
            | DesiredAccess::FILE_APPEND_DATA_OR_FILE_ADD_SUBDIRECTORY,
    );
    let truncate = exists
        && [
            CreateDisposition::FILE_SUPERSEDE,
            CreateDisposition::FILE_OVERWRITE,
            CreateDisposition::FILE_OVERWRITE_IF,
        ]
        .contains(&disposition);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write || !exists || truncate)
        .create(!exists)
        .truncate(truncate);
    let file = match options.open(path) {
        Ok(file) => file,
        // Read-only files can still be opened for reading.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && exists && !write && !truncate => {
            OpenOptions::new().read(true).open(path).map_err(|e| status(&e))?
        }
        Err(e) => return Err(status(&e)),
    };
    let information = match (exists, truncate) {
        (false, _) => Information::FILE_SUPERSEDED,
        (true, true) => Information::FILE_OVERWRITTEN,
        (true, false) => Information::FILE_OPENED,
    };
    Ok((Some(file), information))
}

fn list(dir: &Path, pattern: &str) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for name in [".", ".."] {
        if matches_pattern(name, pattern) {
            entries.push(Entry {
                name: name.to_owned(),
                metadata: fs::metadata(dir)?,
            });
        }
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches_pattern(&name, pattern) {
            continue;
        }
        // Follow symlinks for their type and size; broken ones are skipped.
        if let Ok(metadata) = fs::metadata(entry.path()) {
            entries.push(Entry { name, metadata });
        }
    }
    Ok(entries)
}

/// Windows wildcards: `*` (any run), `?` (one character); case-insensitive.
fn matches_pattern(name: &str, pattern: &str) -> bool {
    if pattern == "*" || pattern == "*.*" {
        return true;
    }
    let name: Vec<char> = name.to_lowercase().chars().collect();
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let (mut n, mut p) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                n += 1;
                p += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

fn attributes(metadata: &fs::Metadata, name: &str) -> FileAttributes {
    let mut attributes = if metadata.is_dir() {
        FileAttributes::FILE_ATTRIBUTE_DIRECTORY
    } else {
        FileAttributes::FILE_ATTRIBUTE_ARCHIVE
    };
    if metadata.permissions().readonly() {
        attributes |= FileAttributes::FILE_ATTRIBUTE_READONLY;
    }
    if name.starts_with('.') && name != "." && name != ".." {
        attributes |= FileAttributes::FILE_ATTRIBUTE_HIDDEN;
    }
    attributes
}

fn allocation(metadata: &fs::Metadata) -> i64 {
    if metadata.is_dir() {
        0
    } else {
        metadata.len().next_multiple_of(4096) as i64
    }
}

/// Seconds between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET: u64 = 11_644_473_600;

fn filetime(time: io::Result<SystemTime>) -> i64 {
    time.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| {
            ((d.as_secs() + FILETIME_UNIX_OFFSET) * 10_000_000 + u64::from(d.subsec_nanos()) / 100) as i64
        })
}

/// FILETIME → time; 0 and -1 mean "don't change".
fn from_filetime(value: i64) -> Option<SystemTime> {
    let ticks = u64::try_from(value).ok().filter(|&t| t > 0)?;
    let since_unix = Duration::from_nanos(ticks * 100).checked_sub(Duration::from_secs(FILETIME_UNIX_OFFSET))?;
    UNIX_EPOCH.checked_add(since_unix)
}

/// (created, accessed, written, changed) as FILETIME.
fn times(metadata: &fs::Metadata) -> (i64, i64, i64, i64) {
    let written = filetime(metadata.modified());
    let created = match filetime(metadata.created()) {
        0 => written,
        t => t,
    };
    (created, filetime(metadata.accessed()), written, written)
}

fn boolean(value: bool) -> Boolean {
    if value { Boolean::True } else { Boolean::False }
}

fn serial(name: &str) -> u32 {
    name.bytes()
        .fold(0x811C_9DC5_u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
}

/// The 8-character ASCII name in the device announcement.
fn dos_name(name: &str) -> String {
    let ascii: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(7)
        .collect();
    if ascii.is_empty() { "SHARE".to_owned() } else { ascii }
}

fn status(e: &io::Error) -> NtStatus {
    NtStatus::from(match e.kind() {
        io::ErrorKind::NotFound => OBJECT_NAME_NOT_FOUND,
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => u32::from(NtStatus::ACCESS_DENIED),
        io::ErrorKind::AlreadyExists => u32::from(NtStatus::OBJECT_NAME_COLLISION),
        io::ErrorKind::NotADirectory => u32::from(NtStatus::NOT_A_DIRECTORY),
        io::ErrorKind::IsADirectory => FILE_IS_A_DIRECTORY,
        io::ErrorKind::DirectoryNotEmpty => u32::from(NtStatus::DIRECTORY_NOT_EMPTY),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => DISK_FULL,
        _ => u32::from(NtStatus::UNSUCCESSFUL),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_share() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        assert_eq!(resolve(&root, "\\sub\\new.txt"), Some(root.join("sub/new.txt")));
        assert_eq!(resolve(&root, ""), Some(root.clone()));
        assert_eq!(resolve(&root, "\\..\\etc\\passwd"), None);
        assert_eq!(resolve(&root, "\\sub\\..\\..\\x"), None);
        std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
        assert_eq!(resolve(&root, "\\escape\\passwd"), None);
    }

    #[test]
    fn wildcards() {
        assert!(matches_pattern("Report.DOCX", "*.docx"));
        assert!(matches_pattern("a.txt", "?.txt"));
        assert!(!matches_pattern("ab.txt", "?.txt"));
        assert!(matches_pattern("anything", "*"));
        assert!(matches_pattern("x", "*.*"));
        assert!(!matches_pattern("notes.md", "*.txt"));
        assert!(matches_pattern("a1b2", "a*b*"));
    }

    #[test]
    fn filetimes_round_trip() {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(from_filetime(filetime(Ok(t))), Some(t));
        assert_eq!(from_filetime(0), None);
        assert_eq!(from_filetime(-1), None);
    }

    #[test]
    fn dos_names() {
        assert_eq!(dos_name("Documents"), "Documen");
        assert_eq!(dos_name("Mes fichiers"), "Mesfich");
        assert_eq!(dos_name("☃"), "SHARE");
    }
}
