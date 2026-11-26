use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rustix::{
    fs::{AtFlags, Dir, Mode, OFlags, mkdirat, open, openat, renameat, unlinkat},
    io::Errno,
};
use serde::Serialize;

/// Keeps every restore operation relative to the validated product directory.
pub(crate) struct RestoreDirectory(File);

impl RestoreDirectory {
    pub(super) fn open(data_root: &Path, components: &[&str]) -> Result<Self> {
        // Normalize trailing slashes before O_NOFOLLOW checks the anchor itself.
        let anchor: PathBuf = data_root.components().collect();
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut dir = File::from(
            open(anchor, flags, Mode::empty()).context("opening WAL restore data root")?,
        );
        for &component in components {
            let child = match openat(&dir, component, flags, Mode::empty()) {
                Err(Errno::NOENT) => {
                    match mkdirat(&dir, component, Mode::from_bits_truncate(0o777)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(error) => return Err(error).context("creating WAL restore directory"),
                    }
                    openat(&dir, component, flags, Mode::empty())
                }
                opened => opened,
            }
            .with_context(|| format!("opening WAL restore directory {component}"))?;
            // Also sync directories created when the sealed files were copied.
            dir.sync_all()?;
            dir = File::from(child);
        }
        Ok(Self(dir))
    }

    pub(super) fn lock(&self) -> Result<File> {
        let file = self
            .open_regular("writer.lock", OFlags::RDWR | OFlags::CREATE, 0o666)
            .context("opening WAL restore writer lock")?;
        file.try_lock()
            .context("WAL product already has a writer")?;
        Ok(file)
    }

    fn open_regular(&self, name: &str, flags: OFlags, permissions: u32) -> io::Result<File> {
        let file = File::from(openat(
            &self.0,
            name,
            flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from_bits_truncate(permissions),
        )?);
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WAL restore requires a regular file with exactly one link",
            ));
        }
        Ok(file)
    }

    pub(super) fn open_segment(&self, name: &str) -> Result<File> {
        self.open_regular(name, OFlags::RDONLY, 0)
            .context("opening WAL restore segment")
    }

    pub(crate) fn open_optional(&self, name: &str) -> Result<Option<File>> {
        match self.open_regular(name, OFlags::RDONLY, 0) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("opening WAL restore {name}")),
        }
    }

    pub(super) fn segment_count(&self) -> Result<usize> {
        let mut count = 0;
        for entry in Dir::read_from(&self.0).context("enumerating WAL restore segments")? {
            let entry = entry.context("enumerating WAL restore segments")?;
            let name = Path::new(OsStr::from_bytes(entry.file_name().to_bytes()));
            if name.extension().is_some_and(|extension| extension == "wal") {
                count += 1;
            }
        }
        Ok(count)
    }

    pub(super) fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }

    /// Publish restore metadata atomically while holding the product lock.
    pub(crate) fn publish_json(&self, name: &str, value: &impl Serialize) -> Result<()> {
        let temp = format!(".{name}.restore");
        match unlinkat(&self.0, temp.as_str(), AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(error) => return Err(error).context("removing interrupted WAL restore metadata"),
        }
        let mut file =
            self.open_regular(&temp, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, 0o600)?;
        let result: Result<()> = (|| {
            serde_json::to_writer(&mut file, value)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            renameat(&self.0, temp.as_str(), &self.0, name)?;
            self.sync()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(&self.0, temp.as_str(), AtFlags::empty());
        }
        result.with_context(|| format!("publishing WAL restore {name}"))
    }
}
