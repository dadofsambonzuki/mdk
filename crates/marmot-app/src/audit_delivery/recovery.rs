use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::state::{AuditDeliveryError, MAX_METADATA_BYTES};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FaultPoint {
    Write,
    FileSync,
    Rename,
    DirectorySync,
    PayloadSync,
}

#[derive(Default)]
pub(super) struct Faults {
    #[cfg(test)]
    next: Option<(FaultPoint, u8)>,
}

impl Faults {
    #[cfg(test)]
    pub(super) fn fail_next(&mut self, point: FaultPoint) {
        self.fail_after(point, 0);
    }

    #[cfg(test)]
    pub(super) fn fail_after(&mut self, point: FaultPoint, matching_calls_to_skip: u8) {
        self.next = Some((point, matching_calls_to_skip));
    }

    pub(super) fn check(&mut self, point: FaultPoint) -> Result<(), AuditDeliveryError> {
        #[cfg(test)]
        if let Some((expected, remaining)) = &mut self.next
            && *expected == point
        {
            if *remaining > 0 {
                *remaining -= 1;
            } else {
                self.next = None;
                if point == FaultPoint::DirectorySync {
                    return Err(AuditDeliveryError::UncertainPublication {
                        source: io::Error::other("injected audit-delivery fault"),
                    });
                }
                return Err(AuditDeliveryError::Filesystem {
                    operation: point.operation(),
                    source: io::Error::other("injected audit-delivery fault"),
                });
            }
        }
        let _ = point;
        Ok(())
    }
}

#[cfg(test)]
impl FaultPoint {
    fn operation(self) -> &'static str {
        match self {
            Self::Write => "write metadata",
            Self::FileSync => "sync metadata file",
            Self::Rename => "publish metadata",
            Self::DirectorySync => "sync metadata directory",
            Self::PayloadSync => "sync payload prefix",
        }
    }
}

pub(super) fn validate_single_component(value: &str) -> Result<(), AuditDeliveryError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(AuditDeliveryError::UnsafeIdentifier);
    }
    let mut components = Path::new(value).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) {
        return Err(AuditDeliveryError::UnsafeIdentifier);
    }
    Ok(())
}

pub(super) fn create_private_directory(path: &Path) -> Result<(), AuditDeliveryError> {
    #[cfg(unix)]
    {
        fs_private::prepare_directory_path(
            path,
            fs_private::PRIVATE_DIR_MODE,
            fs_private::ExistingDirectoryMode::Enforce,
        )
        .map(|_| ())
        .map_err(|source| AuditDeliveryError::Filesystem {
            operation: "create private journal directory",
            source,
        })
    }
    #[cfg(not(unix))]
    {
        fs_private::create_dir_all_private(path).map_err(|source| AuditDeliveryError::Filesystem {
            operation: "create private journal directory",
            source,
        })
    }
}

pub(super) fn create_generation_directory(path: &Path) -> Result<(), AuditDeliveryError> {
    let parent = path.parent().ok_or(AuditDeliveryError::UnsafePath)?;
    create_private_directory(parent)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(fs_private::PRIVATE_DIR_MODE);
    }
    let result = builder.create(path);
    match result {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            Err(AuditDeliveryError::GenerationCollision)
        }
        Err(source) => Err(AuditDeliveryError::Filesystem {
            operation: "create journal generation directory",
            source,
        }),
    }
}

pub(super) fn open_regular_read(path: &Path) -> Result<File, AuditDeliveryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(AuditDeliveryError::UnsafePath);
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(AuditDeliveryError::Filesystem {
                operation: "inspect journal artifact path",
                source,
            });
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    fs_private::set_private_file_mode(&mut options);
    let file = options
        .open(path)
        .map_err(|source| AuditDeliveryError::Filesystem {
            operation: "open journal artifact",
            source,
        })?;
    let metadata = file
        .metadata()
        .map_err(|source| AuditDeliveryError::Filesystem {
            operation: "inspect journal artifact",
            source,
        })?;
    if !metadata.file_type().is_file() {
        return Err(AuditDeliveryError::UnsafePath);
    }
    Ok(file)
}

pub(super) fn open_segment_read(path: &Path) -> Result<File, AuditDeliveryError> {
    match open_regular_read(path) {
        Err(AuditDeliveryError::Filesystem { source, .. })
            if source.kind() == io::ErrorKind::NotFound =>
        {
            Err(AuditDeliveryError::MissingSegment)
        }
        result => result,
    }
}

pub(super) fn read_bounded(path: &Path) -> Result<Vec<u8>, AuditDeliveryError> {
    let file = open_regular_read(path)?;
    let length = file
        .metadata()
        .map_err(|source| AuditDeliveryError::Filesystem {
            operation: "inspect metadata file",
            source,
        })?
        .len();
    if length > MAX_METADATA_BYTES as u64 {
        return Err(AuditDeliveryError::MetadataTooLarge);
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(MAX_METADATA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| AuditDeliveryError::Filesystem {
            operation: "read metadata file",
            source,
        })?;
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(AuditDeliveryError::MetadataTooLarge);
    }
    Ok(bytes)
}

pub(super) fn atomic_replace(
    path: &Path,
    bytes: &[u8],
    faults: &mut Faults,
) -> Result<(), AuditDeliveryError> {
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(AuditDeliveryError::MetadataTooLarge);
    }
    let parent = path.parent().ok_or(AuditDeliveryError::UnsafePath)?;
    validate_existing_target(path)?;
    let file_name = path.file_name().ok_or(AuditDeliveryError::UnsafePath)?;
    let (mut file, temporary) = create_temporary(parent, file_name)?;
    let result = (|| {
        faults.check(FaultPoint::Write)?;
        file.write_all(bytes)
            .map_err(|source| AuditDeliveryError::Filesystem {
                operation: "write metadata",
                source,
            })?;
        faults.check(FaultPoint::FileSync)?;
        file.sync_all()
            .map_err(|source| AuditDeliveryError::Filesystem {
                operation: "sync metadata file",
                source,
            })?;
        drop(file);
        faults.check(FaultPoint::Rename)?;
        fs::rename(&temporary, path).map_err(|source| AuditDeliveryError::Filesystem {
            operation: "publish metadata",
            source,
        })?;
        faults.check(FaultPoint::DirectorySync)?;
        sync_directory(parent).map_err(|source| AuditDeliveryError::UncertainPublication { source })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn validate_existing_target(path: &Path) -> Result<(), AuditDeliveryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(AuditDeliveryError::UnsafePath),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AuditDeliveryError::Filesystem {
            operation: "inspect metadata target",
            source,
        }),
    }
}

fn create_temporary(
    parent: &Path,
    target: &std::ffi::OsStr,
) -> Result<(File, PathBuf), AuditDeliveryError> {
    for _ in 0..32 {
        let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut name = OsString::from(".");
        name.push(target);
        name.push(format!(".tmp-{}-{nonce}", std::process::id()));
        let path = parent.join(name);
        match fs_private::create_new_private(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(AuditDeliveryError::Filesystem {
                    operation: "create metadata staging file",
                    source,
                });
            }
        }
    }
    Err(AuditDeliveryError::TemporaryNameExhausted)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}
