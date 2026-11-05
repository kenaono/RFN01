//! Move a directory entry without clobbering a name that appeared meanwhile.
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    MOVE_FILE_FLAGS, MoveFileExW,
};
use windows::core::PCWSTR;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Version {
    volume: u32,
    index: (u32, u32),
    created: (u32, u32),
    modified: (u32, u32),
    size: (u32, u32),
}

impl Version {
    pub(crate) fn read(path: &Path) -> io::Result<Self> {
        let entry = OpenOptions::new()
            .access_mode(0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .open(path)?;
        Self::of(&entry)
    }

    pub(crate) fn of(file: &File) -> io::Result<Self> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the file owns a valid handle and info is writable.
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }?;
        let time = |value: windows::Win32::Foundation::FILETIME| {
            (value.dwHighDateTime, value.dwLowDateTime)
        };
        Ok(Self {
            volume: info.dwVolumeSerialNumber,
            index: (info.nFileIndexHigh, info.nFileIndexLow),
            created: time(info.ftCreationTime),
            modified: time(info.ftLastWriteTime),
            size: (info.nFileSizeHigh, info.nFileSizeLow),
        })
    }
}

pub(crate) fn changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        crate::i18n::pick(
            "移動先または置換対象が外部で変更されました",
            "The destination or replacement target changed outside RFN Edit",
        ),
    )
}

pub(crate) fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        // Resolve only the parent. The final component may be a link or a
        // new spelling for an existing name, and is the entry being moved.
        let path = std::path::absolute(path)?;
        let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
        Ok(parent
            .canonicalize()?
            .join(name)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect())
    }
    let from = wide(from)?;
    let to = wide(to)?;
    // SAFETY: both paths are NUL-terminated and live for the call.
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVE_FILE_FLAGS(0),
        )
    }?;
    Ok(())
}

/// A private directory on the destination volume. A nonempty one is retained,
/// so failed rollback or disposal never removes the only copy of old data.
pub(crate) struct Stage(PathBuf);

impl Stage {
    pub(crate) fn beside(target: &Path) -> io::Result<Self> {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        loop {
            let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
            let path =
                target.with_file_name(format!(".rfnedit-move-{}-{serial}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) fn incoming(&self) -> PathBuf {
        self.0.join("incoming")
    }

    /// On success, return the previous entry, which the caller can dispose of.
    pub(crate) fn replace(
        &self,
        prepared: &Path,
        target: &Path,
        expected: Version,
    ) -> io::Result<PathBuf> {
        let old = self.0.join("previous");
        fs::create_dir(&old)?;
        let name = target.file_name().ok_or(io::ErrorKind::InvalidInput)?;
        let previous = old.join(name);
        let outcome = self.install(prepared, target, expected, &previous);
        if outcome.is_err() {
            let _ = fs::remove_dir(&old);
        }
        outcome.map(|()| previous)
    }

    fn install(
        &self,
        prepared: &Path,
        target: &Path,
        expected: Version,
        previous: &Path,
    ) -> io::Result<()> {
        fs::symlink_metadata(prepared)?;
        if Version::read(target)? != expected {
            return Err(changed());
        }
        rename_new(target, previous)?;
        // An external rename could have won between the comparison and move.
        let result = match Version::read(previous) {
            Ok(version) if version == expected => rename_new(prepared, target),
            Ok(_) => Err(changed()),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            if let Err(restore) = rename_new(previous, target) {
                let retained = previous.display();
                return Err(io::Error::other(crate::say!(
                    "{error}。元に戻せなかったデータの場所: {retained} ({restore})",
                    "{error}. Could not restore the previous entry. It is kept at {retained} ({restore})"
                )));
            }
            return Err(error);
        }
        Ok(())
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir(self.0.join("previous"));
        let _ = fs::remove_dir(&self.0);
    }
}
