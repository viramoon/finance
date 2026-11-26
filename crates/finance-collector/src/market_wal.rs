//! Product-scoped durable market log. An atomic checkpoint commits a byte
//! prefix, so a valid frame left by an interrupted batch is still unconfirmed.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use finance_market_protocol::{Event, Token, U64Text, decode_json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const VERSION: u32 = 1;
const CHECKPOINT: &str = "checkpoint.json";
const MAGIC: &[u8; 8] = b"FINWAL1\n";
const FRAME_HEADER: usize = 4 + 32;
const FRAME_POSITION: usize = 8 + 8;
const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
const PAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_REPLAY_RECORDS: usize = 4096;

/// A committed event and its WAL-local position and writer generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalRecord {
    pub position: U64Text,
    pub generation: U64Text,
    pub event: Event,
}

/// One immutable file covered by a WAL backup cutoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalSegment {
    pub id: String,
    pub path: String,
    pub first: U64Text,
    pub last: U64Text,
    pub bytes: U64Text,
    pub sha256: String,
}

/// Complete sealed prefix of one source/product, ordered by local position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalManifest {
    pub version: u32,
    pub source: Token,
    pub product: Token,
    pub sealed_through: U64Text,
    pub segments: Vec<WalSegment>,
}

impl WalManifest {
    fn empty(source: Token, product: Token) -> Self {
        Self {
            version: VERSION,
            source,
            product,
            sealed_through: 0.into(),
            segments: Vec::new(),
        }
    }

    fn validate(&self, source: &Token, product: &Token) -> Result<()> {
        ensure!(self.version == VERSION, "unsupported WAL manifest version");
        ensure!(
            &self.source == source && &self.product == product,
            "WAL manifest belongs to a different source/product"
        );
        let mut last = 0_u64;
        for segment in &self.segments {
            let first = segment.first.get();
            ensure!(
                last.checked_add(1) == Some(first)
                    && segment.last.get() >= first
                    && valid_segment_size(segment.bytes.get())
                    && segment.id == segment_id(first)
                    && segment.path == segment_path(source, product, first)
                    && segment.sha256.len() == 64
                    && segment
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "invalid sealed WAL segment descriptor"
            );
            last = segment.last.get();
        }
        ensure!(
            last == self.sealed_through.get(),
            "WAL manifest cutoff differs from its sealed segment range"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    generation: U64Text,
    durable_position: U64Text,
    active_bytes: U64Text,
    manifest: WalManifest,
}

impl Checkpoint {
    fn empty(source: Token, product: Token) -> Self {
        Self {
            generation: 0.into(),
            durable_position: 0.into(),
            active_bytes: 0.into(),
            manifest: WalManifest::empty(source, product),
        }
    }

    fn load(dir: &Path, source: &Token, product: &Token) -> Result<Option<Self>> {
        let file = match File::open(dir.join(CHECKPOINT)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("opening WAL checkpoint"),
        };
        let state: Self =
            serde_json::from_reader(BufReader::new(file)).context("decoding WAL checkpoint")?;
        state.manifest.validate(source, product)?;
        let sealed = state.manifest.sealed_through.get();
        let durable = state.durable_position.get();
        let active = state.active_bytes.get();
        ensure!(
            (durable == 0 || state.generation.get() > 0)
                && ((sealed == durable && active == 0)
                    || (sealed < durable && valid_segment_size(active))),
            "invalid WAL checkpoint range"
        );
        Ok(Some(state))
    }

    fn publish(&self, dir: &Path) -> Result<()> {
        let mut temp = NamedTempFile::new_in(dir).context("creating WAL checkpoint")?;
        serde_json::to_writer(&mut temp, self).context("encoding WAL checkpoint")?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(dir.join(CHECKPOINT))?;
        sync_dir(dir).context("persisting WAL checkpoint")
    }

    fn active(&self) -> Option<SegmentRange<'_>> {
        (self.active_bytes.get() > 0).then(|| SegmentRange {
            first: self.manifest.sealed_through.get() + 1,
            last: self.durable_position.get(),
            bytes: self.active_bytes.get(),
            sha256: None,
        })
    }

    fn ranges(&self) -> impl Iterator<Item = SegmentRange<'_>> {
        self.manifest
            .segments
            .iter()
            .map(SegmentRange::from)
            .chain(self.active())
    }

    fn verify_files(&self, dir: &Path, expected_license: Option<&Token>) -> Result<u64> {
        let mut generation = 0;
        for range in self.ranges() {
            let path = dir.join(segment_filename(range.first));
            if let Some(expected) = range.sha256 {
                ensure!(
                    file_digest(&path, range.bytes)? == expected,
                    "sealed WAL digest mismatch"
                );
            }
            let mut reader = SegmentReader::open(&path, range)?;
            while let Some((record, _)) = reader.next_record()? {
                ensure!(
                    record.generation.get() >= generation,
                    "WAL writer generation regressed"
                );
                self.check_product(&record.event)?;
                if let Some(license) = expected_license {
                    ensure!(
                        &record.event.license == license,
                        "WAL event license differs from restore policy"
                    );
                }
                generation = record.generation.get();
            }
        }
        Ok(generation)
    }

    fn check_product(&self, event: &Event) -> Result<()> {
        ensure!(
            event.source == self.manifest.source && event.product == self.manifest.product,
            "WAL event belongs to a different source/product"
        );
        Ok(())
    }
}

fn segment_id(first: u64) -> String {
    format!("{first:020}")
}

fn segment_filename(first: u64) -> String {
    format!("{}.wal", segment_id(first))
}

fn segment_path(source: &Token, product: &Token, first: u64) -> String {
    format!("{source}/{product}/{}", segment_filename(first))
}

fn valid_segment_size(bytes: u64) -> bool {
    (MAGIC.len() as u64 + FRAME_HEADER as u64 + FRAME_POSITION as u64 + 1..=MAX_SEGMENT_BYTES)
        .contains(&bytes)
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn ensure_dir(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() || path.is_dir() {
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure_dir(parent)?;
    match fs::create_dir(path) {
        Ok(()) => sync_dir(parent),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {
            sync_dir(parent)
        }
        Err(error) => Err(error),
    }
}

fn file_digest(path: &Path, bytes: u64) -> Result<String> {
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.len() == bytes,
        "sealed WAL file length mismatch"
    );
    let mut digest = Sha256::new();
    let read = io::copy(&mut file, &mut digest)?;
    ensure!(
        read == bytes,
        "sealed WAL file length changed during verification"
    );
    Ok(format!("{:x}", digest.finalize()))
}

fn lock_product(dir: &Path) -> Result<File> {
    ensure_dir(dir)?;
    // These names may have been created by a concurrent opener or a backup
    // copy, so ensure they are durable even when they already existed.
    for ancestor in dir.ancestors().skip(1).take(3) {
        if ancestor.as_os_str().is_empty() {
            sync_dir(Path::new("."))?;
        } else {
            sync_dir(ancestor)?;
        }
    }
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join("writer.lock"))?;
    lock.try_lock()
        .context("WAL product already has a writer")?;
    Ok(lock)
}

fn segment_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "wal") {
            files.push(path);
        }
    }
    Ok(files)
}

/// Single-owner raw WAL writer. All public ingestion goes through admission.
pub(crate) struct WalWriter {
    dir: PathBuf,
    state: Checkpoint,
    active: Option<File>,
    staged_position: u64,
    staged_bytes: u64,
    poisoned: bool,
    _lock: File,
}

impl WalWriter {
    pub(crate) fn open(root: &Path, source: &str, product: &str) -> Result<Self> {
        let source: Token = source.parse()?;
        let product: Token = product.parse()?;
        let dir = root.join(source.as_str()).join(product.as_str());
        let lock = lock_product(&dir)?;
        let mut state = match Checkpoint::load(&dir, &source, &product)? {
            Some(state) => state,
            None => {
                ensure!(
                    segment_files(&dir)?.is_empty(),
                    "WAL files exist without a checkpoint"
                );
                Checkpoint::empty(source, product)
            }
        };
        let observed_generation = state.verify_files(&dir, None)?;
        ensure!(
            state.generation.get() >= observed_generation,
            "WAL checkpoint predates a committed writer generation"
        );
        let staged_position = state.durable_position.get();
        let staged_bytes = state.active_bytes.get();
        let mut active = None;
        if let Some(first) = state.manifest.sealed_through.get().checked_add(1) {
            let path = dir.join(segment_filename(first));
            if staged_bytes > 0 {
                let mut file = OpenOptions::new().read(true).write(true).open(path)?;
                if file.metadata()?.len() > staged_bytes {
                    file.set_len(staged_bytes)?;
                    file.sync_all()?;
                }
                file.seek(SeekFrom::Start(staged_bytes))?;
                active = Some(file);
            } else {
                match fs::remove_file(path) {
                    Ok(()) => sync_dir(&dir)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("removing uncommitted WAL segment"),
                }
            }
        }
        state.generation = state
            .generation
            .get()
            .checked_add(1)
            .context("WAL writer generation exhausted")?
            .into();
        state.publish(&dir)?;
        Ok(Self {
            dir,
            state,
            active,
            staged_position,
            staged_bytes,
            poisoned: false,
            _lock: lock,
        })
    }

    pub(crate) fn product_dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn generation(&self) -> u64 {
        self.state.generation.get()
    }

    pub(crate) fn durable_position(&self) -> u64 {
        self.state.durable_position.get()
    }

    fn check_writeable(&self) -> Result<()> {
        ensure!(
            !self.poisoned,
            "WAL writer failed an I/O operation and must be reopened"
        );
        Ok(())
    }

    /// Stages an event. Its position becomes confirmed only on a successful
    /// flush. A rollover may first commit the preceding staged batch.
    pub(crate) fn append(&mut self, event: &Event) -> Result<u64> {
        self.check_writeable()?;
        self.state.check_product(event)?;
        event.validate()?;
        let payload = serde_json::to_vec(event)?;
        ensure!(
            payload.len() <= MAX_EVENT_BYTES,
            "market event exceeds the WAL frame size limit"
        );
        let position = self
            .staged_position
            .checked_add(1)
            .context("WAL position exhausted")?;
        let frame_bytes = FRAME_HEADER + FRAME_POSITION + payload.len();
        if self.staged_bytes + frame_bytes as u64 > MAX_SEGMENT_BYTES {
            self.seal()?;
        }
        self.poisoned = true;
        if self.active.is_none() {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(self.dir.join(segment_filename(position)))?;
            file.write_all(MAGIC)?;
            self.active = Some(file);
            self.staged_bytes = MAGIC.len() as u64;
        }
        let length = ((FRAME_POSITION + payload.len()) as u32).to_le_bytes();
        let mut coordinates = [0; FRAME_POSITION];
        coordinates[..8].copy_from_slice(&position.to_le_bytes());
        coordinates[8..].copy_from_slice(&self.generation().to_le_bytes());
        let mut digest = Sha256::new();
        digest.update(length);
        digest.update(coordinates);
        digest.update(&payload);
        let file = self.active.as_mut().unwrap();
        file.write_all(&length)?;
        file.write_all(&digest.finalize())?;
        file.write_all(&coordinates)?;
        file.write_all(&payload)?;
        self.staged_bytes += frame_bytes as u64;
        self.staged_position = position;
        self.poisoned = false;
        Ok(position)
    }

    pub(crate) fn flush(&mut self) -> Result<u64> {
        self.check_writeable()?;
        if self.staged_position != self.durable_position() {
            self.poisoned = true;
            self.active.as_ref().unwrap().sync_all()?;
            // The checkpoint's directory sync must not be the first sync of a
            // newly linked segment. The two names may persist independently.
            if self.state.active_bytes.get() == 0 {
                sync_dir(&self.dir)?;
            }
            let mut next = self.state.clone();
            next.durable_position = self.staged_position.into();
            next.active_bytes = self.staged_bytes.into();
            next.publish(&self.dir)?;
            self.state = next;
            self.poisoned = false;
        }
        Ok(self.durable_position())
    }

    pub(crate) fn seal(&mut self) -> Result<WalManifest> {
        self.flush()?;
        if let Some(range) = self.state.active() {
            self.poisoned = true;
            let first = range.first;
            let last = range.last;
            let bytes = range.bytes;
            let digest = file_digest(&self.dir.join(segment_filename(first)), bytes)?;
            let mut next = self.state.clone();
            next.manifest.segments.push(WalSegment {
                id: segment_id(first),
                path: segment_path(&next.manifest.source, &next.manifest.product, first),
                first: first.into(),
                last: last.into(),
                bytes: bytes.into(),
                sha256: digest,
            });
            next.manifest.sealed_through = last.into();
            next.active_bytes = 0.into();
            next.publish(&self.dir)?;
            self.state = next;
            self.active = None;
            self.staged_bytes = 0;
            self.poisoned = false;
        }
        Ok(self.state.manifest.clone())
    }

    /// Bootstraps an isolated root from copied, verified sealed files.
    pub(crate) fn restore_sealed(
        root: &Path,
        manifest: &WalManifest,
        expected_license: &Token,
        before_checkpoint: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<()> {
        let dir = root
            .join(manifest.source.as_str())
            .join(manifest.product.as_str());
        manifest.validate(&manifest.source, &manifest.product)?;
        let _lock = lock_product(&dir)?;
        let prior = Checkpoint::load(&dir, &manifest.source, &manifest.product)?;
        if let Some(prior) = &prior {
            ensure!(
                prior.active_bytes.get() == 0 && &prior.manifest == manifest,
                "WAL restore would overwrite an existing checkpoint"
            );
        }
        let files = segment_files(&dir)?;
        ensure!(
            files.len() == manifest.segments.len(),
            "WAL restore directory contains an unexpected segment set"
        );
        let mut restored = Checkpoint {
            generation: 0.into(),
            durable_position: manifest.sealed_through,
            active_bytes: 0.into(),
            manifest: manifest.clone(),
        };
        // A failed verification leaves no checkpoint and can be retried.
        restored.generation = restored.verify_files(&dir, Some(expected_license))?.into();
        for segment in &manifest.segments {
            File::open(root.join(&segment.path))?.sync_all()?;
        }
        sync_dir(&dir)?;
        if let Some(prior) = prior {
            ensure!(
                prior.generation >= restored.generation,
                "WAL checkpoint predates restored writer generation"
            );
            return before_checkpoint(&dir);
        }
        before_checkpoint(&dir)?;
        restored.publish(&dir)
    }
}

#[derive(Clone, Copy)]
struct SegmentRange<'a> {
    first: u64,
    last: u64,
    bytes: u64,
    sha256: Option<&'a str>,
}

impl<'a> From<&'a WalSegment> for SegmentRange<'a> {
    fn from(segment: &'a WalSegment) -> Self {
        Self {
            first: segment.first.get(),
            last: segment.last.get(),
            bytes: segment.bytes.get(),
            sha256: Some(&segment.sha256),
        }
    }
}

struct SegmentReader {
    reader: BufReader<io::Take<File>>,
    remaining: u64,
    next: Option<u64>,
    last: u64,
}

impl SegmentReader {
    fn open(path: &Path, range: SegmentRange<'_>) -> Result<Self> {
        let file = File::open(path).context("opening committed WAL segment")?;
        let size = file.metadata()?.len();
        ensure!(
            size >= range.bytes && (range.sha256.is_none() || size == range.bytes),
            "committed WAL segment length mismatch"
        );
        let mut reader = BufReader::new(file.take(range.bytes));
        let mut magic = [0; MAGIC.len()];
        reader.read_exact(&mut magic)?;
        ensure!(magic == *MAGIC, "unsupported WAL segment header");
        Ok(Self {
            reader,
            remaining: range.bytes - MAGIC.len() as u64,
            next: Some(range.first),
            last: range.last,
        })
    }

    fn next_record(&mut self) -> Result<Option<(WalRecord, usize)>> {
        let Some(expected) = self.next else {
            return Ok(None);
        };
        ensure!(
            self.remaining >= FRAME_HEADER as u64,
            "incomplete committed WAL frame header"
        );
        let mut header = [0; FRAME_HEADER];
        self.reader.read_exact(&mut header)?;
        let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let frame_bytes = FRAME_HEADER + len;
        ensure!(
            (FRAME_POSITION + 1..=FRAME_POSITION + MAX_EVENT_BYTES).contains(&len)
                && frame_bytes as u64 <= self.remaining,
            "invalid committed WAL frame length"
        );
        let mut body = vec![0; len];
        self.reader.read_exact(&mut body)?;
        let mut digest = Sha256::new();
        digest.update(&header[..4]);
        digest.update(&body);
        ensure!(
            digest.finalize()[..] == header[4..],
            "committed WAL frame checksum mismatch"
        );
        let position = u64::from_le_bytes(body[..8].try_into().unwrap());
        let generation = u64::from_le_bytes(body[8..16].try_into().unwrap());
        ensure!(
            position == expected && generation > 0,
            "invalid committed WAL frame coordinates"
        );
        self.remaining -= frame_bytes as u64;
        ensure!(
            (position == self.last) == (self.remaining == 0),
            "committed WAL frame range does not match checkpoint"
        );
        self.next = (position < self.last).then(|| position + 1);
        let event = decode_json::<Event>(std::str::from_utf8(&body[FRAME_POSITION..])?)
            .context("invalid committed WAL event")?;
        Ok(Some((
            WalRecord {
                position: position.into(),
                generation: generation.into(),
                event,
            },
            frame_bytes,
        )))
    }
}

/// Read-only snapshot of one product. Reopen to observe newer commits/seals.
pub struct WalReader {
    dir: PathBuf,
    state: Checkpoint,
}

impl WalReader {
    pub fn open(root: &Path, source: &str, product: &str) -> Result<Self> {
        let source: Token = source.parse()?;
        let product: Token = product.parse()?;
        let dir = root.join(source.as_str()).join(product.as_str());
        let state = match Checkpoint::load(&dir, &source, &product)? {
            Some(state) => {
                // A reader can race the rename before the writer syncs the
                // directory. Make the visible prefix durable before a reader
                // can commit a projection cursor that depends on it. A
                // concurrent replacement only extends that committed prefix.
                sync_dir(&dir).context("stabilizing WAL checkpoint for replay")?;
                state
            }
            None => Checkpoint::empty(source, product),
        };
        Ok(Self { dir, state })
    }

    /// Enumerates products with a published checkpoint, in lexical order.
    pub fn products(root: &Path) -> Result<Vec<(Token, Token)>> {
        let mut products = Vec::new();
        let sources = match fs::read_dir(root) {
            Ok(sources) => sources,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(products),
            Err(error) => return Err(error.into()),
        };
        for entry in sources {
            let entry = entry?;
            let Some(source) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<Token>().ok())
            else {
                continue;
            };
            if !entry.file_type()?.is_dir() {
                continue;
            }
            for product in fs::read_dir(entry.path())? {
                let product = product?;
                if product.file_type()?.is_dir()
                    && product.path().join(CHECKPOINT).try_exists()?
                    && let Some(name) = product
                        .file_name()
                        .to_str()
                        .and_then(|name| name.parse::<Token>().ok())
                {
                    products.push((source.clone(), name));
                }
            }
        }
        products.sort_by(|(source_a, product_a), (source_b, product_b)| {
            (source_a.as_str(), product_a.as_str()).cmp(&(source_b.as_str(), product_b.as_str()))
        });
        Ok(products)
    }

    pub fn durable_position(&self) -> u64 {
        self.state.durable_position.get()
    }

    pub fn manifest(&self) -> WalManifest {
        self.state.manifest.clone()
    }

    /// Returns committed records in WAL order after an exclusive local cursor.
    /// Pages stop at the requested count or after roughly 16 MiB of frames.
    pub fn read_after(&self, after: u64, limit: usize) -> Result<Vec<WalRecord>> {
        ensure!(
            (1..=MAX_REPLAY_RECORDS).contains(&limit),
            "WAL replay limit out of range"
        );
        ensure!(
            after <= self.durable_position(),
            "WAL cursor is ahead of the durable cutoff"
        );
        let mut records = Vec::new();
        let mut bytes = 0;
        for range in self.state.ranges().filter(|range| range.last > after) {
            let mut reader =
                SegmentReader::open(&self.dir.join(segment_filename(range.first)), range)?;
            while let Some((record, frame_bytes)) = reader.next_record()? {
                self.state.check_product(&record.event)?;
                ensure!(
                    record.generation <= self.state.generation,
                    "WAL checkpoint predates a committed writer generation"
                );
                if record.position.get() > after {
                    records.push(record);
                    bytes += frame_bytes;
                    if records.len() == limit || bytes >= PAGE_BYTES {
                        return Ok(records);
                    }
                }
            }
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::{
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    // Reuse the F1 synthetic fixture. The first two trades deliberately have
    // identical exchange timestamps and distinct native IDs and F1 positions.
    const EXAMPLES: &str = include_str!("../../../protocol/v1/examples.json");
    const SOURCE: &str = "fixture";
    const PRODUCT: &str = "us-etf-delayed";

    fn event(index: usize) -> Event {
        let examples: Value = serde_json::from_str(EXAMPLES).unwrap();
        decode_json(&examples["events"][index].to_string()).unwrap()
    }

    fn writer(root: &Path) -> WalWriter {
        WalWriter::open(root, SOURCE, PRODUCT).unwrap()
    }

    fn reader(root: &Path) -> WalReader {
        WalReader::open(root, SOURCE, PRODUCT).unwrap()
    }

    #[test]
    fn committed_snapshot_and_sealed_manifest_survive_later_appends() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = writer(root.path());
        assert_eq!(writer.generation(), 1);
        assert!(WalWriter::open(root.path(), SOURCE, PRODUCT).is_err());
        assert_eq!(writer.append(&event(0)).unwrap(), 1);
        let empty = reader(root.path());
        assert_eq!(empty.durable_position(), 0);
        assert!(empty.read_after(0, 10).unwrap().is_empty());
        assert_eq!(writer.flush().unwrap(), 1);
        let snapshot = reader(root.path());
        assert_eq!(snapshot.durable_position(), 1);
        assert!(snapshot.manifest().segments.is_empty());

        writer.append(&event(1)).unwrap();
        let manifest = writer.seal().unwrap();
        assert_eq!(manifest.sealed_through.get(), 2);
        assert_eq!(manifest.segments.len(), 1);
        assert_eq!(writer.seal().unwrap(), manifest);
        let first = &manifest.segments[0];
        let file = root.path().join(&first.path);
        let bytes = fs::read(&file).unwrap();
        assert_eq!(first.first.get(), 1);
        assert_eq!(first.last.get(), 2);
        assert_eq!(first.bytes.get(), bytes.len() as u64);
        assert_eq!(first.sha256, format!("{:x}", Sha256::digest(&bytes)));
        assert_eq!(
            serde_json::to_value(&manifest).unwrap()["sealed_through"],
            "2"
        );

        writer.append(&event(2)).unwrap();
        writer.flush().unwrap();
        assert_eq!(fs::read(file).unwrap(), bytes);
        let old = snapshot.read_after(0, 10).unwrap();
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].event, event(0));
        assert!(snapshot.read_after(2, 10).is_err());
        let current = reader(root.path());
        let one = current.read_after(1, 1).unwrap();
        assert_eq!(one[0].event, event(1));
        assert_eq!(one[0].position.get(), 2);
        let two = current.read_after(1, 2).unwrap();
        assert_eq!(two[1].event, event(2));
        assert_eq!(two[1].position.get(), 3);
        assert_eq!(two[1].generation.get(), 1);
        assert_eq!(current.manifest(), manifest);
        assert_eq!(
            WalReader::products(root.path()).unwrap(),
            [(SOURCE.parse().unwrap(), PRODUCT.parse().unwrap())]
        );
    }

    #[test]
    fn partial_tail_is_truncated_without_skipping_the_next_position() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = writer(root.path());
        writer.append(&event(0)).unwrap();
        writer.flush().unwrap();
        let segment = writer.product_dir().join(segment_filename(1));
        let committed_bytes = fs::metadata(&segment).unwrap().len();
        drop(writer);
        let mut file = OpenOptions::new().append(true).open(&segment).unwrap();
        file.write_all(&400_u32.to_le_bytes()).unwrap();
        file.write_all(b"half-a-frame").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let before = reader(root.path());
        assert_eq!(before.read_after(0, 10).unwrap().len(), 1);
        let mut restarted = self::writer(root.path());
        assert_eq!(fs::metadata(&segment).unwrap().len(), committed_bytes);
        assert_eq!(restarted.generation(), 2);
        assert_eq!(restarted.append(&event(1)).unwrap(), 2);
        assert_eq!(restarted.flush().unwrap(), 2);
        let rows = reader(root.path()).read_after(0, 10).unwrap();
        assert_eq!((rows[0].generation.get(), rows[1].generation.get()), (1, 2));
    }

    #[test]
    fn unconfirmed_first_segment_does_not_become_a_backup_object() {
        let root = tempfile::tempdir().unwrap();
        let mut original = writer(root.path());
        assert_eq!(original.append(&event(0)).unwrap(), 1);
        let segment = original.product_dir().join(segment_filename(1));
        assert!(segment.exists());
        drop(original);

        let mut restarted = writer(root.path());
        assert!(!segment.exists());
        assert!(restarted.seal().unwrap().segments.is_empty());
        assert_eq!(restarted.append(&event(1)).unwrap(), 1);
        assert_eq!(restarted.flush().unwrap(), 1);
        let rows = reader(root.path()).read_after(0, 10).unwrap();
        assert_eq!(rows[0].event, event(1));
    }

    #[test]
    fn committed_corruption_fails_closed_and_is_not_truncated() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = writer(root.path());
        writer.append(&event(0)).unwrap();
        writer.flush().unwrap();
        let path = writer.product_dir().join(segment_filename(1));
        drop(writer);
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, &bytes).unwrap();
        assert!(reader(root.path()).read_after(0, 10).is_err());
        assert!(WalWriter::open(root.path(), SOURCE, PRODUCT).is_err());
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn restore_accepts_only_the_verified_sealed_prefix_and_resumes_positions() {
        let source = tempfile::tempdir().unwrap();
        let mut writer = writer(source.path());
        writer.append(&event(0)).unwrap();
        let first = writer.seal().unwrap();
        writer.append(&event(1)).unwrap();
        let both = writer.seal().unwrap();
        assert_eq!(both.segments[..1], first.segments);
        writer.append(&event(2)).unwrap();
        writer.flush().unwrap();

        let target = tempfile::tempdir().unwrap();
        for segment in &both.segments {
            let destination = target.path().join(&segment.path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(source.path().join(&segment.path), destination).unwrap();
        }

        let mut wrong_digest = both.clone();
        wrong_digest.segments[1].sha256 = "0".repeat(64);
        let license = event(0).license;
        let wrong_license = "fixture.other".parse().unwrap();
        assert!(
            WalWriter::restore_sealed(target.path(), &wrong_digest, &license, |_| {
                panic!("policy callback must follow successful verification")
            })
            .is_err()
        );
        assert!(
            WalWriter::restore_sealed(target.path(), &both, &wrong_license, |_| Ok(())).is_err()
        );
        assert_eq!(reader(target.path()).durable_position(), 0);
        assert!(WalWriter::open(target.path(), SOURCE, PRODUCT).is_err());
        WalWriter::restore_sealed(target.path(), &both, &license, |directory| {
            assert!(!directory.join(CHECKPOINT).exists());
            Ok(())
        })
        .unwrap();
        WalWriter::restore_sealed(target.path(), &both, &license, |_| Ok(())).unwrap();
        let restored = reader(target.path());
        assert_eq!(restored.manifest(), both);
        let events: Vec<_> = restored
            .read_after(0, 10)
            .unwrap()
            .into_iter()
            .map(|row| row.event)
            .collect();
        assert_eq!(events, vec![event(0), event(1)]);

        let mut new_writer = self::writer(target.path());
        assert_eq!(new_writer.generation(), 2);
        assert_eq!(new_writer.append(&event(2)).unwrap(), 3);
        assert_eq!(new_writer.flush().unwrap(), 3);
    }

    #[test]
    #[cfg(unix)]
    fn sigkill_replays_acknowledged_and_discards_unconfirmed() {
        use std::os::unix::process::ExitStatusExt;

        const CHILD_ROOT: &str = "FIN32_WAL_SIGKILL_TEST_CHILD_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let mut writer = writer(&root);
            writer.append(&event(0)).unwrap();
            writer.append(&event(1)).unwrap();
            assert_eq!(writer.flush().unwrap(), 2);
            writer.append(&event(2)).unwrap();
            fs::write(root.join(".kill-ready"), b"2").unwrap();
            fs::rename(root.join(".kill-ready"), root.join("kill-ready")).unwrap();
            loop {
                thread::park();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "market_wal::tests::sigkill_replays_acknowledged_and_discards_unconfirmed",
            ])
            .env(CHILD_ROOT, root.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root.path().join("kill-ready").exists() && Instant::now() < deadline {
            assert!(child.try_wait().unwrap().is_none());
            thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();
        assert_eq!(child.wait().unwrap().signal(), Some(9));
        assert_eq!(fs::read(root.path().join("kill-ready")).unwrap(), b"2");

        let mut recovered = writer(root.path());
        assert_eq!(recovered.durable_position(), 2);
        let rows = reader(root.path()).read_after(0, 10).unwrap();
        assert_eq!(
            rows.iter().map(|row| &row.event).collect::<Vec<_>>(),
            [&event(0), &event(1)]
        );
        assert_eq!(recovered.append(&event(2)).unwrap(), 3);
        assert_eq!(recovered.flush().unwrap(), 3);
    }
}
