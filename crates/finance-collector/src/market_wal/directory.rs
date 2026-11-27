use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path},
};

use anyhow::{Context, Result};
use rustix::{
    fs::{AtFlags, Dir, Mode, OFlags, mkdirat, open, openat, renameat, unlinkat},
    io::Errno,
};
use serde::Serialize;

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Keeps WAL operations relative to an opened product directory.
pub(crate) struct WalDirectory {
    file: File,
    #[cfg(test)]
    path: std::path::PathBuf,
}

fn open_anchor(path: &Path) -> io::Result<File> {
    let start = if path.is_absolute() { "/" } else { "." };
    let mut dir = File::from(open(start, DIRECTORY_FLAGS, Mode::empty())?);
    for component in path.components() {
        if !matches!(component, Component::RootDir | Component::CurDir) {
            dir = File::from(openat(
                &dir,
                component.as_os_str(),
                DIRECTORY_FLAGS,
                Mode::empty(),
            )?);
        }
    }
    Ok(dir)
}

impl WalDirectory {
    fn new(file: File, _anchor: &Path, _components: &[&str]) -> Self {
        Self {
            file,
            #[cfg(test)]
            path: _components
                .iter()
                .fold(_anchor.to_owned(), |path, component| path.join(component)),
        }
    }

    #[cfg(test)]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn open(data_root: &Path, components: &[&str]) -> Result<Self> {
        let mut dir = open_anchor(data_root).context("opening WAL restore data root")?;
        for &component in components {
            let child = match openat(&dir, component, DIRECTORY_FLAGS, Mode::empty()) {
                Err(Errno::NOENT) => {
                    match mkdirat(&dir, component, Mode::from_bits_truncate(0o777)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(error) => return Err(error).context("creating WAL restore directory"),
                    }
                    openat(&dir, component, DIRECTORY_FLAGS, Mode::empty())
                }
                opened => opened,
            }
            .with_context(|| format!("opening WAL restore directory {component}"))?;
            // Also sync directories created when the sealed files were copied.
            dir.sync_all()?;
            dir = File::from(child);
        }
        Ok(Self::new(dir, data_root, components))
    }

    pub(crate) fn open_existing(anchor: &Path, components: &[&str]) -> Result<Option<Self>> {
        let result: io::Result<_> = (|| {
            let mut dir = open_anchor(anchor)?;
            for &component in components {
                dir = File::from(openat(&dir, component, DIRECTORY_FLAGS, Mode::empty())?);
            }
            Ok(dir)
        })();
        match result {
            Ok(dir) => Ok(Some(Self::new(dir, anchor, components))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("opening WAL directory"),
        }
    }

    pub(super) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            #[cfg(test)]
            path: self.path.clone(),
        })
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
            &self.file,
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

    pub(super) fn resume_segment(&self, name: &str) -> Result<File> {
        self.open_regular(name, OFlags::RDWR, 0)
            .context("opening active WAL segment")
    }

    pub(super) fn create_segment(&self, name: &str) -> Result<File> {
        self.open_regular(name, OFlags::RDWR | OFlags::CREATE | OFlags::EXCL, 0o666)
            .context("creating active WAL segment")
    }

    pub(super) fn discard_segment(&self, name: &str) -> Result<()> {
        match unlinkat(&self.file, name, AtFlags::empty()) {
            Ok(()) => self.sync()?,
            Err(Errno::NOENT) => {}
            Err(error) => return Err(error).context("removing uncommitted WAL segment"),
        }
        Ok(())
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
        for entry in Dir::read_from(&self.file).context("enumerating WAL restore segments")? {
            let entry = entry.context("enumerating WAL restore segments")?;
            let name = Path::new(OsStr::from_bytes(entry.file_name().to_bytes()));
            if name.extension().is_some_and(|extension| extension == "wal") {
                count += 1;
            }
        }
        Ok(count)
    }

    pub(super) fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// Atomically replace metadata while holding the product lock.
    pub(crate) fn publish_json(&self, name: &str, value: &impl Serialize) -> Result<()> {
        let temp = format!(".{name}.restore");
        match unlinkat(&self.file, temp.as_str(), AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(error) => return Err(error).context("removing interrupted WAL restore metadata"),
        }
        let mut file =
            self.open_regular(&temp, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, 0o600)?;
        let result: Result<()> = (|| {
            serde_json::to_writer(&mut file, value)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            renameat(&self.file, temp.as_str(), &self.file, name)?;
            self.sync()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(&self.file, temp.as_str(), AtFlags::empty());
        }
        result.with_context(|| format!("publishing WAL restore {name}"))
    }
}
