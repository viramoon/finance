//! Path boundary shared by the market projection index and watermark readers.

use std::{
    fs::{self, Metadata},
    io::{self, ErrorKind},
    path::{Path, PathBuf},
};

use finance_market_protocol::Token;

pub enum ProjectionArtifact {
    Index,
    Watermark,
}

fn invalid_path(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidInput,
        format!("invalid projection path {}: {reason}", path.display()),
    )
}

fn metadata(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn checked_path(path: &Path, expected_type: fn(&Metadata) -> bool) -> io::Result<bool> {
    let Some(metadata) = metadata(path)? else {
        return Ok(false);
    };
    if metadata.is_symlink() {
        return Err(invalid_path(path, "symbolic links are forbidden"));
    }
    if !expected_type(&metadata) {
        return Err(invalid_path(path, "unexpected file type"));
    }
    Ok(true)
}

fn reject_checkout(directory: &Path) -> io::Result<()> {
    let marker = directory.join(".git");
    let Some(mut entry) = metadata(&marker)? else {
        return Ok(());
    };
    if entry.is_dir() {
        let Some(head) = metadata(&marker.join("HEAD"))? else {
            return Ok(());
        };
        entry = head;
    }
    if entry.is_file() || entry.is_symlink() {
        return Err(invalid_path(directory, "path is in a Git working tree"));
    }
    Ok(())
}

/// Return the checked, absolute path of an existing projection artifact.
pub fn projection_read_path(
    data_root: &Path,
    source: &Token,
    product: &Token,
    artifact: ProjectionArtifact,
) -> io::Result<Option<PathBuf>> {
    let data_root: PathBuf = data_root.components().collect();
    if !checked_path(&data_root, Metadata::is_dir)? {
        return Ok(None);
    }
    let mut path = data_root.canonicalize()?;
    for directory in path.ancestors() {
        reject_checkout(directory)?;
    }
    for component in [
        "finance-state",
        "market-projection",
        source.as_str(),
        product.as_str(),
    ] {
        path.push(component);
        if !checked_path(&path, Metadata::is_dir)? {
            return Ok(None);
        }
        reject_checkout(&path)?;
    }
    let name = match artifact {
        ProjectionArtifact::Index => "projection.sqlite3",
        ProjectionArtifact::Watermark => "watermark.json",
    };
    let file = path.join(name);
    if !checked_path(&file, Metadata::is_file)? {
        return Ok(None);
    }
    if matches!(artifact, ProjectionArtifact::Index) {
        for sidecar in [
            "projection.sqlite3-journal",
            "projection.sqlite3-wal",
            "projection.sqlite3-shm",
        ] {
            checked_path(&path.join(sidecar), Metadata::is_file)?;
        }
    }
    Ok(Some(file))
}
