use rusqlite::{Connection, OpenFlags};
use rustix::{
    fs::{AtFlags, FileType, Mode, OFlags, mkdirat, open as open_dir, openat, statat},
    io::Errno,
};
use std::{
    fs::{self, File},
    io,
    path::{Component, Path, PathBuf},
    time::Duration,
};

struct Directory {
    fd: File,
    path: PathBuf,
}

fn directory(root: &Path, relative: &Path, create: bool) -> io::Result<Option<Directory>> {
    // The configured root itself is trusted. Descend from its descriptor so
    // opening or creating a child cannot cross a linked parent.
    let mut path = match fs::canonicalize(root) {
        Ok(path) => path,
        Err(err) if !create && err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut fd = File::from(open_dir(&path, flags, Mode::empty())?);
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(invalid_path());
        };
        let child = match openat(&fd, name, flags, Mode::empty()) {
            Err(Errno::NOENT) if create => {
                match mkdirat(&fd, name, Mode::from_bits_truncate(0o777)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(err) => return Err(err.into()),
                }
                openat(&fd, name, flags, Mode::empty())
            }
            Err(Errno::NOENT) => return Ok(None),
            opened => opened,
        }?;
        fd = File::from(child);
        path.push(name);
    }
    Ok(Some(Directory { fd, path }))
}

fn invalid_path() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "expected a regular local file")
}

impl Directory {
    fn split<'a>(
        root: &Path,
        relative: &'a Path,
        create: bool,
    ) -> io::Result<Option<(Self, &'a std::ffi::OsStr)>> {
        let parent = relative.parent().ok_or_else(invalid_path)?;
        let name = relative.file_name().ok_or_else(invalid_path)?;
        Ok(directory(root, parent, create)?.map(|dir| (dir, name)))
    }

    fn regular(&self, name: &std::ffi::OsStr) -> io::Result<Option<File>> {
        let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let file = match openat(&self.fd, name, flags, Mode::empty()) {
            Ok(fd) => File::from(fd),
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP) => return Err(invalid_path()),
            Err(err) => return Err(err.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(invalid_path());
        }
        Ok(Some(file))
    }
}

pub(crate) fn open_file(root: &Path, relative: &Path) -> io::Result<Option<File>> {
    let Some((dir, name)) = Directory::split(root, relative, false)? else {
        return Ok(None);
    };
    dir.regular(name)
}

fn sqlite(
    root: &Path,
    relative: &Path,
    create: bool,
) -> Result<Option<Connection>, Box<dyn std::error::Error>> {
    let Some((dir, name)) = Directory::split(root, relative, create)? else {
        return Ok(None);
    };
    // Inspect SQLite without opening a second descriptor for its inode.
    // Closing such a descriptor can release this process's SQLite locks.
    match statat(&dir.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {}
        Ok(_) => return Err(invalid_path().into()),
        Err(Errno::NOENT) if !create => return Ok(None),
        Err(Errno::NOENT) => {}
        Err(err) => return Err(err.into()),
    }
    let flags = if create {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    // The bundled Unix SQLite VFS also rejects symlinks while resolving
    // the entire filename for the connection.
    let conn =
        Connection::open_with_flags(dir.path.join(name), flags | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    conn.busy_timeout(Duration::from_secs(3))?;
    Ok(Some(conn))
}

pub(crate) fn open(
    root: &Path,
    relative: &Path,
) -> Result<Option<Connection>, Box<dyn std::error::Error>> {
    sqlite(root, relative, false)
}

pub(crate) fn open_queue(
    root: &Path,
    relative: &Path,
) -> Result<Connection, Box<dyn std::error::Error>> {
    sqlite(root, relative, true)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing backtest data root").into())
}

pub(crate) fn has_table(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )
}
