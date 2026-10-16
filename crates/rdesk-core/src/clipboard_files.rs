//! Copying files through the clipboard (MS-RDPECLIP file streams).
//!
//! Local → remote: copied files (and the contents of copied folders) are described to the remote,
//! which then reads sizes and byte ranges from them on paste.
//!
//! Remote → local: a copy on the remote only brings the file list. Contents are downloaded on
//! paste, one file after another in 1 MiB requests, into a folder the UI picks.

use std::fs::{self, File};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use ironrdp::cliprdr::pdu::{
    ClipboardFileAttributes, FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor,
};
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// Bytes asked for per request when downloading.
const CHUNK: u32 = 1024 * 1024;
/// More entries than this in one copy are not offered (a copied home folder, say).
const MAX_ENTRIES: usize = 10_000;
/// Seconds between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET: u64 = 11_644_473_600;

/// A file or folder the remote copied, as shown before pasting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    pub name: String,
    /// Folder inside the copy, `\`-separated; `None` for top-level items.
    pub relative_path: Option<String>,
    pub size: Option<u64>,
    pub is_dir: bool,
}

impl RemoteFile {
    pub(crate) fn from_descriptor(descriptor: &FileDescriptor) -> Self {
        Self {
            name: descriptor.name.clone(),
            relative_path: descriptor.relative_path.clone().filter(|p| !p.is_empty()),
            size: descriptor.file_size,
            is_dir: is_dir(descriptor),
        }
    }
}

fn is_dir(descriptor: &FileDescriptor) -> bool {
    descriptor
        .attributes
        .is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY))
}

/// Pasting the remote's files finished: the top-level items, now local; or why it failed.
pub type FetchResult = Result<Vec<PathBuf>, String>;

// ── Local → remote ───────────────────────────────────────────────────────────

#[derive(Debug)]
struct LocalEntry {
    path: PathBuf,
    size: u64,
    is_dir: bool,
}

/// Local files on offer, in the order they were described to the remote.
#[derive(Debug, Default)]
pub(crate) struct LocalFiles {
    entries: Vec<LocalEntry>,
}

impl LocalFiles {
    /// Describes `paths` (walking into folders) for `initiate_file_copy`.
    pub(crate) fn describe(paths: &[PathBuf]) -> (Self, Vec<FileDescriptor>) {
        let mut files = Self::default();
        let mut descriptors = Vec::new();
        for path in paths {
            files.add(path, None, &mut descriptors);
        }
        if files.entries.len() >= MAX_ENTRIES {
            warn!(
                limit = MAX_ENTRIES,
                "Too many files copied; only the first ones are offered"
            );
        }
        (files, descriptors)
    }

    fn add(&mut self, path: &Path, relative: Option<&str>, descriptors: &mut Vec<FileDescriptor>) {
        if self.entries.len() >= MAX_ENTRIES {
            return;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        // Symlinks are followed for content but loops are avoided by not descending into them.
        let Ok(metadata) = fs::metadata(path) else { return };
        let is_dir = metadata.is_dir();
        let mut descriptor = FileDescriptor::new(name).with_last_write_time(filetime(&metadata));
        descriptor = if is_dir {
            descriptor.with_attributes(ClipboardFileAttributes::DIRECTORY)
        } else {
            descriptor
                .with_attributes(ClipboardFileAttributes::ARCHIVE)
                .with_file_size(metadata.len())
        };
        if let Some(relative) = relative {
            descriptor = descriptor.with_relative_path(relative);
        }
        descriptors.push(descriptor);
        self.entries.push(LocalEntry {
            path: path.to_owned(),
            size: if is_dir { 0 } else { metadata.len() },
            is_dir,
        });
        let symlink = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
        if is_dir && !symlink {
            let Ok(children) = fs::read_dir(path) else { return };
            let inner = match relative {
                Some(relative) => format!("{relative}\\{name}"),
                None => name.to_owned(),
            };
            let mut children: Vec<PathBuf> = children.filter_map(|e| e.ok().map(|e| e.path())).collect();
            children.sort();
            for child in children {
                self.add(&child, Some(&inner), descriptors);
            }
        }
    }

    /// Answers the remote reading one of our files.
    pub(crate) fn serve(&self, request: &FileContentsRequest) -> FileContentsResponse<'static> {
        let entry = usize::try_from(request.index).ok().and_then(|i| self.entries.get(i));
        let Some(entry) = entry.filter(|e| !e.is_dir) else {
            return FileContentsResponse::new_error(request.stream_id);
        };
        if request.flags.contains(FileContentsFlags::SIZE) {
            return FileContentsResponse::new_size_response(request.stream_id, entry.size);
        }
        let read = (|| {
            let mut file = File::open(&entry.path)?;
            file.seek(SeekFrom::Start(request.position))?;
            let mut data = Vec::with_capacity(request.requested_size.min(CHUNK * 8) as usize);
            file.take(u64::from(request.requested_size)).read_to_end(&mut data)?;
            Ok::<_, std::io::Error>(data)
        })();
        match read {
            Ok(data) => FileContentsResponse::new_data_response(request.stream_id, data),
            Err(e) => {
                warn!(path = %entry.path.display(), error = %e, "Couldn't read a copied file");
                FileContentsResponse::new_error(request.stream_id)
            }
        }
    }
}

fn filetime(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| {
            (d.as_secs() + FILETIME_UNIX_OFFSET) * 10_000_000 + u64::from(d.subsec_nanos()) / 100
        })
}

// ── Remote → local ───────────────────────────────────────────────────────────

/// Downloads the remote's copied files into `dir`.
pub(crate) struct Download {
    dir: PathBuf,
    files: Vec<FileDescriptor>,
    /// Local paths, matching `files`.
    targets: Vec<PathBuf>,
    index: usize,
    position: u64,
    size: Option<u64>,
    file: Option<File>,
    stream_id: u32,
    reply: Option<oneshot::Sender<FetchResult>>,
}

impl Download {
    pub(crate) fn start(
        dir: PathBuf,
        files: Vec<FileDescriptor>,
        reply: oneshot::Sender<FetchResult>,
    ) -> Result<Self, (oneshot::Sender<FetchResult>, String)> {
        let targets: Option<Vec<PathBuf>> = files.iter().map(|f| local_path(&dir, f)).collect();
        let Some(targets) = targets else {
            return Err((reply, "the copied files have unusable names".to_owned()));
        };
        if let Err(e) = fs::create_dir_all(&dir) {
            return Err((reply, e.to_string()));
        }
        debug!(files = files.len(), dir = %dir.display(), "Downloading copied files");
        Ok(Self {
            dir,
            files,
            targets,
            index: 0,
            position: 0,
            size: None,
            file: None,
            stream_id: 0,
            reply: Some(reply),
        })
    }

    /// The next request to send, or `None` once everything is downloaded (or it failed).
    pub(crate) fn next_request(&mut self, stream_id: u32) -> Option<FileContentsRequest> {
        loop {
            let descriptor = self.files.get(self.index)?;
            let target = &self.targets[self.index];
            if is_dir(descriptor) {
                if let Err(e) = fs::create_dir_all(target) {
                    self.fail(format!("{}: {e}", target.display()));
                    return None;
                }
                self.index += 1;
                continue;
            }
            if self.file.is_none() {
                let created = target
                    .parent()
                    .map_or(Ok(()), fs::create_dir_all)
                    .and_then(|()| File::create(target));
                match created {
                    Ok(file) => self.file = Some(file),
                    Err(e) => {
                        self.fail(format!("{}: {e}", target.display()));
                        return None;
                    }
                }
                self.position = 0;
                self.size = descriptor.file_size;
            }
            self.stream_id = stream_id;
            let request = |flags, position, requested_size| FileContentsRequest {
                stream_id,
                index: self.index as i32,
                flags,
                position,
                requested_size,
                data_id: None,
            };
            return match self.size {
                None => Some(request(FileContentsFlags::SIZE, 0, 8)),
                Some(size) if self.position < size => {
                    let left = u32::try_from(size - self.position).unwrap_or(u32::MAX);
                    Some(request(FileContentsFlags::RANGE, self.position, left.min(CHUNK)))
                }
                Some(_) => {
                    self.file = None;
                    self.index += 1;
                    continue;
                }
            };
        }
    }

    /// Handles a response; returns whether it belonged to this download.
    pub(crate) fn on_response(&mut self, stream_id: u32, data: Option<Vec<u8>>) -> bool {
        if stream_id != self.stream_id || self.reply.is_none() {
            return false;
        }
        let Some(data) = data else {
            let name = self.files.get(self.index).map_or("?", |f| f.name.as_str()).to_owned();
            self.fail(format!("the PC couldn't send {name}"));
            return true;
        };
        if self.size.is_none() {
            let size = data.get(..8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes);
            match size {
                Some(size) => self.size = Some(size),
                None => self.fail("the PC sent an invalid file size".to_owned()),
            }
            return true;
        }
        if data.is_empty() {
            // The file ended early; keep what arrived.
            self.size = Some(self.position);
            return true;
        }
        if let Some(file) = &mut self.file
            && let Err(e) = file.write_all(&data)
        {
            self.fail(e.to_string());
            return true;
        }
        self.position += data.len() as u64;
        true
    }

    /// Reports success once every file is done; `true` if the download is over.
    pub(crate) fn finish_if_done(&mut self) -> bool {
        if self.reply.is_none() {
            return true;
        }
        if self.index < self.files.len() {
            return false;
        }
        let top = self
            .files
            .iter()
            .zip(&self.targets)
            .filter(|(f, _)| f.relative_path.as_deref().is_none_or(str::is_empty))
            .map(|(_, target)| target.clone())
            .collect();
        debug!(dir = %self.dir.display(), "Copied files downloaded");
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Ok(top));
        }
        true
    }

    fn fail(&mut self, error: String) {
        warn!(%error, "Downloading copied files failed");
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err(error));
        }
    }
}

/// Where a remote file goes under `dir`; `None` if any part of its path is unsafe.
fn local_path(dir: &Path, descriptor: &FileDescriptor) -> Option<PathBuf> {
    let mut path = dir.to_path_buf();
    let parts = descriptor
        .relative_path
        .iter()
        .flat_map(|p| p.split('\\'))
        .chain(std::iter::once(descriptor.name.as_str()));
    for part in parts.filter(|p| !p.is_empty()) {
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => path.push(part),
            _ => return None,
        }
    }
    (path != dir).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_files_are_described_and_served() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Docs");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::write(root.join("sub/b.bin"), vec![7; 10]).unwrap();
        let single = dir.path().join("single.txt");
        fs::write(&single, b"x").unwrap();

        let (files, descriptors) = LocalFiles::describe(&[root.clone(), single]);
        let names: Vec<_> = descriptors
            .iter()
            .map(|d| (d.relative_path.clone(), d.name.clone(), is_dir(d)))
            .collect();
        assert_eq!(
            names,
            [
                (None, "Docs".to_owned(), true),
                (Some("Docs".to_owned()), "a.txt".to_owned(), false),
                (Some("Docs".to_owned()), "sub".to_owned(), true),
                (Some("Docs\\sub".to_owned()), "b.bin".to_owned(), false),
                (None, "single.txt".to_owned(), false),
            ]
        );

        let request = |index, flags, position, requested_size| FileContentsRequest {
            stream_id: 9,
            index,
            flags,
            position,
            requested_size,
            data_id: None,
        };
        assert_eq!(
            files
                .serve(&request(1, FileContentsFlags::SIZE, 0, 8))
                .data_as_size()
                .unwrap(),
            5
        );
        assert_eq!(files.serve(&request(1, FileContentsFlags::RANGE, 1, 3)).data(), b"ell");
        assert!(files.serve(&request(0, FileContentsFlags::RANGE, 0, 3)).is_error());
        assert!(files.serve(&request(99, FileContentsFlags::RANGE, 0, 3)).is_error());
    }

    #[test]
    fn downloads_into_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            FileDescriptor::new("Pics").with_attributes(ClipboardFileAttributes::DIRECTORY),
            FileDescriptor::new("p.png")
                .with_relative_path("Pics")
                .with_file_size(3),
            FileDescriptor::new("unknown-size.txt"),
        ];
        let (reply, result) = oneshot::channel();
        let mut download = Download::start(dir.path().join("paste"), files, reply).ok().unwrap();

        let first = download.next_request(1).unwrap();
        assert_eq!(
            (first.index, first.flags, first.requested_size),
            (1, FileContentsFlags::RANGE, 3)
        );
        assert!(download.on_response(1, Some(b"abc".to_vec())));
        let size = download.next_request(2).unwrap();
        assert_eq!((size.index, size.flags), (2, FileContentsFlags::SIZE));
        assert!(!download.on_response(1, Some(vec![])), "stale stream");
        assert!(download.on_response(2, Some(2u64.to_le_bytes().to_vec())));
        let range = download.next_request(3).unwrap();
        assert_eq!((range.position, range.requested_size), (0, 2));
        assert!(download.on_response(3, Some(b"hi".to_vec())));
        assert!(download.next_request(4).is_none());
        assert!(download.finish_if_done());

        let top = result.blocking_recv().unwrap().unwrap();
        let paste = dir.path().join("paste");
        assert_eq!(top, [paste.join("Pics"), paste.join("unknown-size.txt")]);
        assert_eq!(fs::read(paste.join("Pics/p.png")).unwrap(), b"abc");
        assert_eq!(fs::read(paste.join("unknown-size.txt")).unwrap(), b"hi");
    }

    #[test]
    fn unsafe_names_are_refused() {
        let dir = Path::new("/tmp/x");
        assert_eq!(local_path(dir, &FileDescriptor::new("a.txt")), Some(dir.join("a.txt")));
        assert_eq!(local_path(dir, &FileDescriptor::new("..")), None);
        assert_eq!(
            local_path(dir, &FileDescriptor::new("a").with_relative_path("..\\..")),
            None
        );
        assert_eq!(local_path(dir, &FileDescriptor::new("/etc/passwd")), None);
    }
}
