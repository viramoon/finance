use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{self, Component, Path},
};

use anyhow::{Context, Result};
use rustix::{
    fs::{AtFlags, Dir, FileType, Mode, OFlags, mkdirat, open, openat, renameat, statat, unlinkat},
    io::Errno,
};
use serde::Serialize;

#[cfg(test)]
type SnapshotHook = Box<dyn FnOnce(&File)>;

#[cfg(test)]
thread_local! {
    static AFTER_SNAPSHOT_OPEN: std::cell::RefCell<Option<SnapshotHook>> = const {
        std::cell::RefCell::new(None)
    };
}

enum Links {
    Attached,
    Snapshot,
}

fn check_regular_file(file: File, links: Links) -> io::Result<File> {
    let metadata = file.metadata()?;
    let valid_links = match links {
        Links::Attached => metadata.nlink() == 1,
        // An atomic replacement can unlink the old inode while a reader
        // still holds its immutable contents through the open descriptor.
        Links::Snapshot => metadata.nlink() <= 1,
    };
    if !metadata.is_file() || !valid_links {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WAL requires a regular file with an admissible link count",
        ));
    }
    Ok(file)
}

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

fn require_outside_worktree(dir: &File) -> io::Result<()> {
    match statat(dir, ".git", AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(marker) if FileType::from_raw_mode(marker.st_mode) == FileType::Directory => {
            let git = openat(dir, ".git", DIRECTORY_FLAGS, Mode::empty())?;
            match statat(&git, "HEAD", AtFlags::SYMLINK_NOFOLLOW) {
                Err(Errno::NOENT) => return Ok(()),
                Err(error) => return Err(error.into()),
                Ok(_) => {}
            }
        }
        Ok(_) => {}
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "WAL data must be recorded outside Git worktrees",
    ))
}

fn open_anchor(path: &Path, for_write: bool) -> io::Result<File> {
    let start = if path.is_absolute() { "/" } else { "." };
    let mut dir = File::from(open(start, DIRECTORY_FLAGS, Mode::empty())?);
    if for_write {
        require_outside_worktree(&dir)?;
    }
    for component in path.components() {
        if !matches!(component, Component::RootDir | Component::CurDir) {
            dir = File::from(openat(
                &dir,
                component.as_os_str(),
                DIRECTORY_FLAGS,
                Mode::empty(),
            )?);
            if for_write {
                require_outside_worktree(&dir)?;
            }
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
        let absolute = path::absolute(data_root).context("resolving WAL data root")?;
        let mut dir = open_anchor(&absolute, true).context("opening WAL restore data root")?;
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
            let child = File::from(child);
            require_outside_worktree(&child)
                .with_context(|| format!("checking WAL directory {component}"))?;
            // Persist the directory entries, including those created when sealed files were copied.
            dir.sync_all()?;
            dir = child;
        }
        Ok(Self::new(dir, data_root, components))
    }

    pub(crate) fn open_existing(anchor: &Path, components: &[&str]) -> Result<Option<Self>> {
        let result: io::Result<_> = (|| {
            let mut dir = open_anchor(anchor, false)?;
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

    fn open_file(&self, name: &str, flags: OFlags, permissions: u32) -> io::Result<File> {
        Ok(File::from(openat(
            &self.file,
            name,
            flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from_bits_truncate(permissions),
        )?))
    }

    fn open_regular(&self, name: &str, flags: OFlags, permissions: u32) -> io::Result<File> {
        check_regular_file(self.open_file(name, flags, permissions)?, Links::Attached)
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
        self.open_optional_metadata(name, Links::Attached)
    }

    pub(crate) fn open_optional_snapshot(&self, name: &str) -> Result<Option<File>> {
        self.open_optional_metadata(name, Links::Snapshot)
    }

    fn open_optional_metadata(&self, name: &str, links: Links) -> Result<Option<File>> {
        match self.open_file(name, OFlags::RDONLY, 0) {
            Ok(file) => {
                #[cfg(test)]
                if matches!(links, Links::Snapshot)
                    && let Some(hook) = AFTER_SNAPSHOT_OPEN.with(|slot| slot.borrow_mut().take())
                {
                    hook(&file);
                }
                check_regular_file(file, links)
                    .map(Some)
                    .with_context(|| format!("opening WAL restore {name}"))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("opening WAL restore {name}")),
        }
    }

    #[cfg(test)]
    pub(super) fn after_next_snapshot_open(hook: impl FnOnce(&File) + 'static) {
        AFTER_SNAPSHOT_OPEN.with(|slot| {
            assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
        });
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
