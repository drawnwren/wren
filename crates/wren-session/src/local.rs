use std::ffi::OsString;
use std::fs::{self, File, Metadata, Permissions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;
use thiserror::Error;
pub use wren_types::FileIdentity;
use wren_types::{DocumentClass, DocumentProfile};
use xattr::FileExt as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentEncoding {
    Utf8,
    InvalidUtf8,
    Binary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
    Cr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub identity: FileIdentity,
    pub content_hash: [u8; 32],
    pub len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedDocument {
    pub text: String,
    pub encoding: DocumentEncoding,
    pub class: DocumentClass,
    pub mixed_line_endings: bool,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveWarning {
    HardLinkReplaced { links: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveReport {
    pub bytes_written: usize,
    pub warning: Option<SaveWarning>,
    pub stamp: FileStamp,
}

#[derive(Debug, Error)]
pub enum SaveError {
    #[error("document has no path; use save-as")]
    NoPath,
    #[error("{path} is read-only because its encoding is {encoding:?}")]
    ReadOnly { path: PathBuf, encoding: DocumentEncoding },
    #[error("refusing to overwrite externally changed file {path}: {reason}")]
    ExternalChange { path: PathBuf, reason: String },
    #[error("file operation for {path} failed: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug)]
pub struct LocalDocument {
    presentation_path: Option<PathBuf>,
    resolved_path: Option<PathBuf>,
    stamp: Option<FileStamp>,
    permissions: Option<Permissions>,
    accessed: Option<filetime::FileTime>,
    extended_attributes: Vec<(OsString, Vec<u8>)>,
    line_endings: Vec<LineEnding>,
    default_line_ending: LineEnding,
    encoding: DocumentEncoding,
}

impl LocalDocument {
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, OpenedDocument), SaveError> {
        let presentation_path = path.as_ref().to_path_buf();
        let resolved_path = fs::canonicalize(&presentation_path).map_err(|source| io_error(&presentation_path, source))?;
        let bytes = fs::read(&resolved_path).map_err(|source| io_error(&resolved_path, source))?;
        let metadata = fs::metadata(&resolved_path).map_err(|source| io_error(&resolved_path, source))?;
        let stamp = stamp(&metadata, &bytes);
        let extended_attributes = read_extended_attributes(&resolved_path).map_err(|source| io_error(&resolved_path, source))?;
        let decoded = decode(&bytes);
        let document = Self {
            presentation_path: Some(presentation_path),
            resolved_path: Some(resolved_path),
            stamp: Some(stamp),
            permissions: Some(metadata.permissions()),
            accessed: metadata.accessed().ok().map(filetime::FileTime::from_system_time),
            extended_attributes,
            line_endings: decoded.line_endings,
            default_line_ending: decoded.default_line_ending,
            encoding: decoded.encoding,
        };
        Ok((document, decoded.opened))
    }

    pub fn open_or_new(path: impl AsRef<Path>) -> Result<(Self, OpenedDocument), SaveError> {
        match Self::open(path.as_ref()) {
            Ok(opened) => Ok(opened),
            Err(SaveError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => Ok(Self::new_at(path)),
            Err(error) => Err(error),
        }
    }

    #[must_use]
    pub fn unnamed() -> (Self, OpenedDocument) {
        Self::new(None)
    }

    #[must_use]
    pub fn new_at(path: impl AsRef<Path>) -> (Self, OpenedDocument) {
        Self::new(Some(path.as_ref().to_path_buf()))
    }

    fn new(path: Option<PathBuf>) -> (Self, OpenedDocument) {
        (
            Self {
                presentation_path: path.clone(),
                resolved_path: path,
                stamp: None,
                permissions: None,
                accessed: None,
                extended_attributes: Vec::new(),
                line_endings: Vec::new(),
                default_line_ending: LineEnding::Lf,
                encoding: DocumentEncoding::Utf8,
            },
            OpenedDocument { text: String::new(), encoding: DocumentEncoding::Utf8, class: DocumentClass::Normal, mixed_line_endings: false, read_only: false },
        )
    }

    #[must_use]
    pub fn presentation_path(&self) -> Option<&Path> {
        self.presentation_path.as_deref()
    }

    #[must_use]
    pub const fn stamp(&self) -> Option<&FileStamp> {
        self.stamp.as_ref()
    }

    #[must_use]
    pub fn base_hash(&self) -> [u8; 32] {
        self.stamp.as_ref().map_or(*blake3::hash(b"").as_bytes(), |stamp| stamp.content_hash)
    }

    #[must_use]
    pub const fn encoding(&self) -> DocumentEncoding {
        self.encoding
    }

    /// Explicitly converts a byte-oriented document into editable UTF-8.
    /// Valid UTF-8 spans are retained and each invalid byte becomes a visible
    /// `\\xNN` escape, so the conversion is deliberate and auditable.
    pub fn convert_to_utf8(&mut self) -> Result<String, SaveError> {
        if self.encoding == DocumentEncoding::Utf8 {
            return self
                .resolved_path
                .as_ref()
                .map_or_else(|| Ok(String::new()), |path| fs::read(path).map_err(|source| io_error(path, source)).map(|bytes| decode(&bytes).opened.text));
        }
        let path = self.resolved_path.clone().ok_or(SaveError::NoPath)?;
        self.validate_precondition(&path, false)?;
        let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
        let escaped = escape_invalid_utf8(&bytes);
        let (line_endings, default_line_ending, normalized) = normalize_line_endings(&escaped);
        self.encoding = DocumentEncoding::Utf8;
        self.line_endings = line_endings;
        self.default_line_ending = default_line_ending;
        Ok(normalized)
    }

    pub fn save(&mut self, text: &str) -> Result<SaveReport, SaveError> {
        let path = self.resolved_path.clone().ok_or(SaveError::NoPath)?;
        self.save_to_path(&path, text, false)
    }

    pub fn save_as(&mut self, path: impl AsRef<Path>, text: &str) -> Result<SaveReport, SaveError> {
        self.save_to_path(path.as_ref(), text, true)
    }

    fn save_to_path(&mut self, requested_path: &Path, text: &str, save_as: bool) -> Result<SaveReport, SaveError> {
        if self.encoding != DocumentEncoding::Utf8 {
            return Err(SaveError::ReadOnly { path: requested_path.to_path_buf(), encoding: self.encoding });
        }

        let path = if save_as && requested_path.exists() {
            fs::canonicalize(requested_path).map_err(|source| io_error(requested_path, source))?
        } else {
            requested_path.to_path_buf()
        };
        self.validate_precondition(&path, save_as)?;
        let bytes = self.encode(text);
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut temporary = NamedTempFile::new_in(parent).map_err(|source| io_error(&path, source))?;
        if let Some(permissions) = &self.permissions {
            temporary.as_file().set_permissions(permissions.clone()).map_err(|source| io_error(&path, source))?;
        }
        for (name, value) in &self.extended_attributes {
            temporary.as_file().set_xattr(name, value).map_err(|source| io_error(&path, source))?;
        }
        temporary.write_all(&bytes).and_then(|()| temporary.flush()).map_err(|source| io_error(&path, source))?;
        filetime::set_file_handle_times(temporary.as_file(), self.accessed, None).map_err(|source| io_error(&path, source))?;
        // One sync after all data and metadata updates establishes the same
        // durable file frontier without paying for an intermediate flush.
        temporary.as_file().sync_all().map_err(|source| io_error(&path, source))?;

        self.validate_precondition(&path, save_as)?;
        let warning = hard_link_warning(&path).map_err(|source| io_error(&path, source))?;
        temporary.persist(&path).map_err(|error| io_error(&path, error.error))?;
        sync_directory(parent).map_err(|source| io_error(parent, source))?;

        let metadata = fs::metadata(&path).map_err(|source| io_error(&path, source))?;
        let new_stamp = stamp(&metadata, &bytes);
        self.presentation_path = Some(requested_path.to_path_buf());
        self.permissions = Some(metadata.permissions());
        self.accessed = metadata.accessed().ok().map(filetime::FileTime::from_system_time);
        self.extended_attributes = read_extended_attributes(&path).map_err(|source| io_error(&path, source))?;
        self.resolved_path = Some(path);
        self.stamp = Some(new_stamp.clone());
        let persisted = decode(&bytes);
        self.line_endings = persisted.line_endings;
        self.default_line_ending = persisted.default_line_ending;
        Ok(SaveReport { bytes_written: bytes.len(), warning, stamp: new_stamp })
    }

    fn validate_precondition(&self, path: &Path, save_as: bool) -> Result<(), SaveError> {
        if save_as {
            if path.exists() {
                return Err(SaveError::ExternalChange { path: path.to_path_buf(), reason: "save-as target already exists".to_owned() });
            }
            return Ok(());
        }
        match &self.stamp {
            Some(expected) => {
                let bytes = fs::read(path).map_err(|source| io_error(path, source))?;
                let metadata = fs::metadata(path).map_err(|source| io_error(path, source))?;
                let current = stamp(&metadata, &bytes);
                if current.identity != expected.identity {
                    return Err(SaveError::ExternalChange { path: path.to_path_buf(), reason: "file identity changed".to_owned() });
                }
                if current.content_hash != expected.content_hash {
                    return Err(SaveError::ExternalChange { path: path.to_path_buf(), reason: "content hash changed".to_owned() });
                }
                Ok(())
            }
            None if path.exists() => Err(SaveError::ExternalChange { path: path.to_path_buf(), reason: "new-file target appeared after open".to_owned() }),
            None => Ok(()),
        }
    }

    fn encode(&self, text: &str) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(text.len());
        let mut line = 0;
        for segment in text.split_inclusive('\n') {
            if let Some(body) = segment.strip_suffix('\n') {
                encoded.extend_from_slice(body.as_bytes());
                let ending = self.line_endings.get(line).copied().unwrap_or(self.default_line_ending);
                encoded.extend_from_slice(ending.bytes());
                line += 1;
            } else {
                encoded.extend_from_slice(segment.as_bytes());
            }
        }
        encoded
    }
}

fn read_extended_attributes(path: &Path) -> io::Result<Vec<(OsString, Vec<u8>)>> {
    if !xattr::SUPPORTED_PLATFORM {
        return Ok(Vec::new());
    }
    let mut attributes = Vec::new();
    for name in xattr::list(path)? {
        if let Some(value) = xattr::get(path, &name)? {
            attributes.push((name, value));
        }
    }
    attributes.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(attributes)
}

impl LineEnding {
    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Lf => b"\n",
            Self::Crlf => b"\r\n",
            Self::Cr => b"\r",
        }
    }
}

struct Decoded {
    opened: OpenedDocument,
    encoding: DocumentEncoding,
    line_endings: Vec<LineEnding>,
    default_line_ending: LineEnding,
}

fn decode(bytes: &[u8]) -> Decoded {
    let nul_count = bytes.iter().filter(|byte| **byte == 0).count();
    let binary = nul_count > 0 && nul_count.saturating_mul(100) >= bytes.len().max(1);
    if binary {
        return Decoded {
            opened: OpenedDocument {
                text: escaped_bytes(bytes),
                encoding: DocumentEncoding::Binary,
                class: DocumentClass::Pathological,
                mixed_line_endings: false,
                read_only: true,
            },
            encoding: DocumentEncoding::Binary,
            line_endings: Vec::new(),
            default_line_ending: LineEnding::Lf,
        };
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Decoded {
            opened: OpenedDocument {
                text: escaped_bytes(bytes),
                encoding: DocumentEncoding::InvalidUtf8,
                class: classify(bytes.len(), longest_line(bytes)),
                mixed_line_endings: false,
                read_only: true,
            },
            encoding: DocumentEncoding::InvalidUtf8,
            line_endings: Vec::new(),
            default_line_ending: LineEnding::Lf,
        };
    };
    let (line_endings, default_line_ending, normalized) = normalize_line_endings(text);
    let mixed_line_endings = line_endings.first().is_some_and(|first| line_endings.iter().any(|ending| ending != first));
    Decoded {
        opened: OpenedDocument {
            text: normalized,
            encoding: DocumentEncoding::Utf8,
            class: classify(bytes.len(), longest_line(bytes)),
            mixed_line_endings,
            read_only: false,
        },
        encoding: DocumentEncoding::Utf8,
        line_endings,
        default_line_ending,
    }
}

fn normalize_line_endings(text: &str) -> (Vec<LineEnding>, LineEnding, String) {
    let mut endings = Vec::new();
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                normalized.push('\n');
                endings.push(LineEnding::Crlf);
            }
            '\r' => {
                normalized.push('\n');
                endings.push(LineEnding::Cr);
            }
            '\n' => {
                normalized.push('\n');
                endings.push(LineEnding::Lf);
            }
            _ => normalized.push(character),
        }
    }
    let default = dominant_line_ending(&endings);
    (endings, default, normalized)
}

fn dominant_line_ending(endings: &[LineEnding]) -> LineEnding {
    if endings.is_empty() {
        return LineEnding::Lf;
    }
    let counts = [LineEnding::Lf, LineEnding::Crlf, LineEnding::Cr].map(|candidate| endings.iter().filter(|ending| **ending == candidate).count());
    let mut index = 0;
    for candidate in 1..counts.len() {
        if counts[candidate] > counts[index] {
            index = candidate;
        }
    }
    [LineEnding::Lf, LineEnding::Crlf, LineEnding::Cr][index]
}

fn classify(bytes: usize, longest_line: usize) -> DocumentClass {
    DocumentClass::classify(DocumentProfile {
        byte_length: u64::try_from(bytes).unwrap_or(u64::MAX),
        longest_line_estimate: u64::try_from(longest_line).unwrap_or(u64::MAX),
        parse_bytes_per_millisecond: None,
        generated_file: false,
    })
}

fn longest_line(bytes: &[u8]) -> usize {
    bytes.split(|byte| matches!(byte, b'\n' | b'\r')).map(<[u8]>::len).max().unwrap_or(0)
}

fn escaped_bytes(bytes: &[u8]) -> String {
    let mut escaped = String::new();
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && index % 16 == 0 {
            escaped.push('\n');
        }
        escaped.push_str(&format!("{byte:02x} "));
    }
    escaped
}

fn escape_invalid_utf8(bytes: &[u8]) -> String {
    let mut output = String::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                output.push_str(valid);
                break;
            }
            Err(error) => {
                let valid = &rest[..error.valid_up_to()];
                // SAFETY is unnecessary: `valid_up_to` is the UTF-8 API's
                // guarantee, and this branch still validates through from_utf8.
                if let Ok(valid) = std::str::from_utf8(valid) {
                    output.push_str(valid);
                }
                let invalid_len = error.error_len().unwrap_or(1).min(rest.len());
                for byte in &rest[error.valid_up_to()..error.valid_up_to() + invalid_len] {
                    output.push_str(&format!("\\x{byte:02X}"));
                }
                rest = &rest[error.valid_up_to() + invalid_len..];
            }
        }
    }
    output
}

fn stamp(metadata: &Metadata, bytes: &[u8]) -> FileStamp {
    FileStamp { identity: file_identity(metadata), content_hash: *blake3::hash(bytes).as_bytes(), len: metadata.len() }
}

#[cfg(unix)]
fn file_identity(metadata: &Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;

    FileIdentity { device: metadata.dev(), file: metadata.ino(), generation: metadata.len() }
}

#[cfg(not(unix))]
fn file_identity(metadata: &Metadata) -> FileIdentity {
    let modified = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |duration| duration.as_nanos() as u64);
    FileIdentity { device: metadata.len(), file: modified, generation: metadata.len() }
}

#[cfg(unix)]
fn hard_link_warning(path: &Path) -> io::Result<Option<SaveWarning>> {
    use std::os::unix::fs::MetadataExt;

    match fs::metadata(path) {
        Ok(metadata) if metadata.nlink() > 1 => Ok(Some(SaveWarning::HardLinkReplaced { links: metadata.nlink() })),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn hard_link_warning(_path: &Path) -> io::Result<Option<SaveWarning>> {
    Ok(None)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn io_error(path: &Path, source: io::Error) -> SaveError {
    SaveError::Io { path: path.to_path_buf(), source }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn preserves_mixed_line_endings_on_save() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("mixed.txt");
        fs::write(&path, b"a\r\nb\nc\r").expect("fixture");
        let (mut document, opened) = LocalDocument::open(&path).expect("open");
        assert_eq!(opened.text, "a\nb\nc\n");
        assert!(opened.mixed_line_endings);
        document.save(&opened.text).expect("save");
        document.save(&opened.text).expect("save again");
        assert_eq!(fs::read(&path).expect("read"), b"a\r\nb\nc\r");
    }

    #[test]
    fn refuses_to_overwrite_external_changes() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("race.txt");
        fs::write(&path, "original").expect("fixture");
        let (mut document, _) = LocalDocument::open(&path).expect("open");
        fs::write(&path, "external").expect("external write");
        assert!(matches!(document.save("editor"), Err(SaveError::ExternalChange { .. })));
        assert_eq!(fs::read_to_string(&path).expect("read"), "external");
    }

    #[test]
    fn invalid_utf8_and_binary_are_byte_preserving_read_only_views() {
        let directory = tempdir().expect("temporary directory");
        let invalid = directory.path().join("invalid");
        fs::write(&invalid, [0xff, b'a']).expect("fixture");
        let (mut document, opened) = LocalDocument::open(&invalid).expect("open");
        assert_eq!(opened.encoding, DocumentEncoding::InvalidUtf8);
        assert!(opened.read_only);
        assert!(matches!(document.save("changed"), Err(SaveError::ReadOnly { .. })));
        let converted = document.convert_to_utf8().expect("explicit conversion");
        assert_eq!(converted, "\\xFFa");
        document.save(&converted).expect("save converted text");
        assert_eq!(fs::read(&invalid).expect("converted bytes"), b"\\xFFa");

        let binary = directory.path().join("binary");
        fs::write(&binary, [0, 1, 2]).expect("fixture");
        let (_, opened) = LocalDocument::open(&binary).expect("open");
        assert_eq!(opened.encoding, DocumentEncoding::Binary);
    }

    #[test]
    fn new_files_use_atomic_save_and_then_track_identity() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("new.rs");
        let (mut document, _) = LocalDocument::open_or_new(&path).expect("new document");
        document.save("fn main() {}\n").expect("first save");
        document.save("fn main() { }\n").expect("second save");
        assert_eq!(fs::read_to_string(path).expect("read"), "fn main() { }\n");
    }

    #[cfg(unix)]
    #[test]
    fn saving_a_symlink_updates_target_and_preserves_link() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("temporary directory");
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        fs::write(&target, "old").expect("target");
        symlink(&target, &link).expect("link");
        let (mut document, _) = LocalDocument::open(&link).expect("open link");
        document.save("new").expect("save target");
        assert_eq!(fs::read_to_string(&target).expect("target read"), "new");
        assert!(fs::symlink_metadata(&link).expect("link metadata").file_type().is_symlink());
    }

    #[test]
    fn hard_links_surface_replacement_warning() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("one");
        let link = directory.path().join("two");
        let mut fixture = File::create(&path).expect("fixture");
        fixture.write_all(b"old").expect("write");
        fs::hard_link(&path, &link).expect("hard link");
        let (mut document, _) = LocalDocument::open(&path).expect("open");
        let report = document.save("new").expect("save");
        assert!(matches!(report.warning, Some(SaveWarning::HardLinkReplaced { links: 2 })));
        assert_eq!(fs::read_to_string(&link).expect("other link"), "old");
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_mode_and_supported_extended_attributes() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("metadata");
        fs::write(&path, "old").expect("fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("mode");
        let attribute_supported = xattr::set(&path, "user.wren-test", b"preserved").is_ok();
        let (mut document, _) = LocalDocument::open(&path).expect("open");
        document.save("new").expect("save");
        assert_eq!(fs::metadata(&path).expect("metadata").permissions().mode() & 0o777, 0o640);
        if attribute_supported {
            assert_eq!(xattr::get(&path, "user.wren-test").expect("xattr"), Some(b"preserved".to_vec()));
        }
    }
}
