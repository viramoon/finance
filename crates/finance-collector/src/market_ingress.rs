use std::{
    collections::VecDeque,
    fs::File,
    io::{BufReader, BufWriter, ErrorKind, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use finance_market_protocol::{Capability, Event, Instrument, InstrumentId, Token, U64Text};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tempfile::NamedTempFile;

use crate::market_wal::{WalManifest, WalReader, WalWriter};

const DISPLAY_TTL: Duration = Duration::from_secs(5);
const DISPLAY_EVENTS: usize = 256;
const DISPLAY_BYTES: usize = 16 * 1024 * 1024;

pub fn wal_root(data_root: &Path) -> PathBuf {
    data_root.join("finance-state/market-wal")
}

fn product_dir(data_root: &Path, source: &Token, product: &Token) -> PathBuf {
    wal_root(data_root)
        .join(source.as_str())
        .join(product.as_str())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductPolicy {
    pub source: Token,
    pub product: Token,
    pub license: Token,
    pub display_live: bool,
    pub record_local: bool,
    pub derive_non_display: bool,
    pub backup_personal_drive: bool,
}

impl ProductPolicy {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.backup_personal_drive || self.record_local,
            "WAL backup requires permission to record locally"
        );
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PublishResult {
    /// A local delivery position which must be covered by a successful flush or seal.
    Staged(u64),
    DisplayOnly,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceObservation {
    pub channel: Token,
    pub instrument_id: InstrumentId,
    pub epoch: U64Text,
    pub seq: U64Text,
    pub exchange_ts: Option<U64Text>,
    pub receive_ts: U64Text,
}

impl From<&Event> for SourceObservation {
    fn from(event: &Event) -> Self {
        Self {
            channel: event.channel.clone(),
            instrument_id: event.instrument_id.clone(),
            epoch: event.epoch,
            seq: event.seq,
            exchange_ts: event.exchange_ts,
            receive_ts: event.receive_ts,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Watermarks {
    pub source_latest: Option<SourceObservation>,
    pub local_durable: U64Text,
    pub projected: U64Text,
    pub remote_verified: Option<U64Text>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapCause {
    LicenseSkip,
    Rotation,
    SourceRateLimit,
    Disconnected,
    SourceGap,
    StorageUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOutcome {
    TickBackfilled,
    BarBackfilled,
    BookRestoredFromSnapshot,
    HistoryUnrecoverable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityReport {
    pub channel: Token,
    pub instrument_id: InstrumentId,
    pub from_ts: U64Text,
    pub to_ts: U64Text,
    pub cause: GapCause,
    /// None denotes a gap with recovery still pending.
    pub outcome: Option<RecoveryOutcome>,
    /// Number of recovered items, or number known missing for an unrecoverable interval.
    pub count: Option<U64Text>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupManifest {
    pub access: ProductPolicy,
    pub wal: WalManifest,
}

struct DisplayEvent {
    expires: Instant,
    bytes: usize,
    event: Event,
}

/// Ingress for a configured source/product. Use one instance per product.
pub struct MarketIngress {
    data_root: PathBuf,
    policy: ProductPolicy,
    writer: Option<WalWriter>,
    source_latest: Option<SourceObservation>,
    display: VecDeque<DisplayEvent>,
    display_bytes: usize,
    live_quality: VecDeque<QualityReport>,
}

impl MarketIngress {
    pub fn open(data_root: &Path, policy: ProductPolicy) -> Result<Self> {
        policy.validate()?;
        let writer = if policy.record_local {
            ensure!(
                data_root.is_dir(),
                "WAL data root must be an existing directory"
            );
            if let Some(existing) = recording_policy(data_root, &policy.source, &policy.product)? {
                ensure!(
                    existing == policy,
                    "WAL policy binding changed; use a separate data root"
                );
            }
            let writer = WalWriter::open(
                &wal_root(data_root),
                policy.source.as_str(),
                policy.product.as_str(),
            )?;
            bind_policy(
                writer.product_dir(),
                &policy,
                writer.durable_position() == 0,
            )?;
            Some(writer)
        } else {
            None
        };
        Ok(Self {
            data_root: data_root.to_owned(),
            policy,
            writer,
            source_latest: None,
            display: VecDeque::new(),
            display_bytes: 0,
            live_quality: VecDeque::new(),
        })
    }

    pub fn publish(
        &mut self,
        event: Event,
        instrument: &Instrument,
        capability: &Capability,
    ) -> Result<PublishResult> {
        ensure!(
            event.source == self.policy.source
                && event.product == self.policy.product
                && event.license == self.policy.license,
            "event does not match the source, product and license of the ingress"
        );
        event.validate_against(instrument, capability)?;
        self.source_latest = Some(SourceObservation::from(&event));

        let result = if let Some(writer) = &mut self.writer {
            PublishResult::Staged(writer.append(&event)?)
        } else if self.policy.display_live {
            PublishResult::DisplayOnly
        } else {
            PublishResult::Denied
        };
        if self.policy.display_live {
            self.retain_display(event)?;
        }
        Ok(result)
    }

    pub fn flush(&mut self) -> Result<Option<u64>> {
        self.writer.as_mut().map(WalWriter::flush).transpose()
    }

    pub fn seal(&mut self) -> Result<WalManifest> {
        self.writer
            .as_mut()
            .context("local WAL recording is disabled")?
            .seal()
    }

    pub fn seal_for_backup(&mut self) -> Result<BackupManifest> {
        ensure_backup(&self.policy)?;
        Ok(BackupManifest {
            access: self.policy.clone(),
            wal: self.seal()?,
        })
    }

    pub fn writer_generation(&self) -> Option<u64> {
        self.writer.as_ref().map(WalWriter::generation)
    }

    pub fn watermarks(&self) -> Result<Watermarks> {
        let mut marks = if self.writer.is_some() {
            read_watermarks(&self.data_root, &self.policy.source, &self.policy.product)?
        } else {
            empty_watermarks()
        };
        if self.source_latest.is_some() {
            marks.source_latest.clone_from(&self.source_latest);
        }
        Ok(marks)
    }

    /// Report a recovery observation. Repeating an identical report is idempotent.
    pub fn report_quality(&mut self, report: QualityReport) -> Result<()> {
        ensure!(report.from_ts <= report.to_ts, "reversed quality interval");
        if let Some(writer) = &self.writer {
            let path = writer.product_dir().join("quality.json");
            let mut reports: Vec<QualityReport> = read_json(&path)?.unwrap_or_default();
            if !reports.contains(&report) {
                reports.push(report);
                atomic_json(&path, &reports)?;
            }
        } else if !self.live_quality.contains(&report) {
            if self.live_quality.len() == DISPLAY_EVENTS {
                self.live_quality.pop_front();
            }
            self.live_quality.push_back(report);
        }
        Ok(())
    }

    pub fn quality(&self) -> Result<Vec<QualityReport>> {
        if self.writer.is_some() {
            read_quality(&self.data_root, &self.policy.source, &self.policy.product)
        } else {
            Ok(self.live_quality.iter().cloned().collect())
        }
    }

    pub fn display_events(&mut self) -> impl Iterator<Item = &Event> {
        self.expire_display();
        self.display.iter().map(|item| &item.event)
    }

    /// An adapter's idle timer can also call this to release expired memory.
    pub fn expire_display(&mut self) {
        let now = Instant::now();
        while self.display.front().is_some_and(|item| item.expires <= now) {
            self.remove_display_front();
        }
    }

    fn retain_display(&mut self, event: Event) -> Result<()> {
        self.expire_display();
        let bytes = serde_json::to_vec(&event)?.len();
        if bytes <= DISPLAY_BYTES {
            while self.display.len() >= DISPLAY_EVENTS || self.display_bytes + bytes > DISPLAY_BYTES
            {
                self.remove_display_front();
            }
            self.display.push_back(DisplayEvent {
                expires: Instant::now() + DISPLAY_TTL,
                bytes,
                event,
            });
            self.display_bytes += bytes;
        }
        Ok(())
    }

    fn remove_display_front(&mut self) {
        if let Some(item) = self.display.pop_front() {
            self.display_bytes -= item.bytes;
        }
    }
}

fn empty_watermarks() -> Watermarks {
    Watermarks {
        source_latest: None,
        local_durable: 0.into(),
        projected: 0.into(),
        remote_verified: None,
    }
}

pub fn read_watermarks(data_root: &Path, source: &Token, product: &Token) -> Result<Watermarks> {
    if recording_policy(data_root, source, product)?.is_none() {
        return Ok(empty_watermarks());
    }

    #[derive(Deserialize)]
    struct ProjectionWatermark {
        projected_through: U64Text,
    }

    // Read the projection position first to avoid comparing a new projector cursor to
    // an older WAL checkpoint during concurrent commits.
    let projected_path = data_root
        .join("finance-state/market-projection")
        .join(source.as_str())
        .join(product.as_str())
        .join("watermark.json");
    let projected = read_json::<ProjectionWatermark>(&projected_path)?
        .map_or(0, |mark| mark.projected_through.get());
    let reader = WalReader::open(&wal_root(data_root), source.as_str(), product.as_str())?;
    let durable = reader.durable_position();
    ensure!(
        projected <= durable,
        "projection watermark is ahead of the WAL"
    );
    let source_latest = if durable == 0 {
        None
    } else {
        let records = reader.read_after(durable - 1, 1)?;
        let record = records
            .first()
            .context("missing last committed WAL record")?;
        Some(SourceObservation::from(&record.event))
    };
    Ok(Watermarks {
        source_latest,
        local_durable: durable.into(),
        projected: projected.into(),
        remote_verified: None,
    })
}

pub fn read_quality(
    data_root: &Path,
    source: &Token,
    product: &Token,
) -> Result<Vec<QualityReport>> {
    if recording_policy(data_root, source, product)?.is_none() {
        return Ok(Vec::new());
    }
    Ok(
        read_json(&product_dir(data_root, source, product).join("quality.json"))?
            .unwrap_or_default(),
    )
}

pub fn recording_policy(
    data_root: &Path,
    source: &Token,
    product: &Token,
) -> Result<Option<ProductPolicy>> {
    let path = product_dir(data_root, source, product).join("access.json");
    let policy: Option<ProductPolicy> = read_json(&path)?;
    if let Some(policy) = &policy {
        policy.validate()?;
        ensure!(
            policy.record_local && policy.source == *source && policy.product == *product,
            "invalid WAL policy binding"
        );
    }
    Ok(policy)
}

pub fn backup_manifest(
    data_root: &Path,
    source: &Token,
    product: &Token,
) -> Result<BackupManifest> {
    let access = recording_policy(data_root, source, product)?.context("WAL policy is missing")?;
    ensure_backup(&access)?;
    let reader = WalReader::open(&wal_root(data_root), source.as_str(), product.as_str())?;
    Ok(BackupManifest {
        access,
        wal: reader.manifest(),
    })
}

/// In an isolated data root, activate the files enumerated by a backup manifest.
pub fn restore_backup(data_root: &Path, manifest: &BackupManifest) -> Result<()> {
    let access = &manifest.access;
    access.validate()?;
    ensure_backup(access)?;
    ensure!(
        access.source == manifest.wal.source && access.product == manifest.wal.product,
        "backup policy does not match its WAL manifest"
    );
    WalWriter::restore_sealed(
        &wal_root(data_root),
        &manifest.wal,
        &access.license,
        |dir| bind_policy(dir, access, true),
    )
}

fn ensure_backup(policy: &ProductPolicy) -> Result<()> {
    ensure!(
        policy.backup_personal_drive,
        "the product policy does not authorize personal Drive backup"
    );
    Ok(())
}

fn bind_policy(dir: &Path, policy: &ProductPolicy, allow_new: bool) -> Result<()> {
    let path = dir.join("access.json");
    if let Some(existing) = read_json::<ProductPolicy>(&path)? {
        ensure!(
            existing == *policy,
            "WAL policy binding changed; use a separate data root"
        );
        return Ok(());
    }
    ensure!(allow_new, "committed WAL has lost its policy binding");
    atomic_json(&path, policy)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match File::open(path) {
        Ok(file) => Ok(Some(serde_json::from_reader(BufReader::new(file))?)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .context("metadata file needs a parent directory")?;
    let mut temp = NamedTempFile::new_in(parent)?;
    {
        let mut output = BufWriter::new(temp.as_file_mut());
        serde_json::to_writer(&mut output, value)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    // F1's checked-in examples are deterministic, hand-authored synthetic data.
    fn fixture() -> (Instrument, Capability, Event, ProductPolicy) {
        let examples: serde_json::Value =
            serde_json::from_str(include_str!("../../../protocol/v1/examples.json")).unwrap();
        let event: Event = serde_json::from_value(examples["events"][0].clone()).unwrap();
        let instrument = examples["instruments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["instrument_id"] == event.instrument_id.as_str())
            .unwrap();
        let capability = examples["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| {
                row["source"] == event.source.as_str() && row["product"] == event.product.as_str()
            })
            .unwrap();
        let policy = ProductPolicy {
            source: event.source.clone(),
            product: event.product.clone(),
            license: event.license.clone(),
            display_live: true,
            record_local: false,
            derive_non_display: false,
            backup_personal_drive: false,
        };
        (
            serde_json::from_value(instrument.clone()).unwrap(),
            serde_json::from_value(capability.clone()).unwrap(),
            event,
            policy,
        )
    }

    #[test]
    fn display_only_is_bounded_and_does_not_create_any_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("absent");
        let (instrument, capability, event, policy) = fixture();
        let mut ingress = MarketIngress::open(&root, policy.clone()).unwrap();
        for _ in 0..=DISPLAY_EVENTS {
            assert_eq!(
                ingress
                    .publish(event.clone(), &instrument, &capability)
                    .unwrap(),
                PublishResult::DisplayOnly
            );
        }
        assert_eq!(ingress.display_events().count(), DISPLAY_EVENTS);
        for item in &mut ingress.display {
            item.expires = Instant::now();
        }
        assert!(ingress.display_events().next().is_none());
        assert_eq!(ingress.display_bytes, 0);
        assert_eq!(ingress.flush().unwrap(), None);
        assert!(ingress.seal().is_err());

        let live = ingress.watermarks().unwrap();
        assert_eq!(live.source_latest, Some(SourceObservation::from(&event)));
        assert_eq!(live.local_durable.get(), 0);
        assert_eq!(live.projected.get(), 0);
        assert!(live.remote_verified.is_none());
        ingress
            .report_quality(QualityReport {
                channel: event.channel,
                instrument_id: event.instrument_id,
                from_ts: 0.into(),
                to_ts: 1.into(),
                cause: GapCause::LicenseSkip,
                outcome: None,
                count: None,
            })
            .unwrap();
        assert_eq!(ingress.quality().unwrap().len(), 1);
        assert!(
            read_watermarks(&root, &policy.source, &policy.product)
                .unwrap()
                .source_latest
                .is_none()
        );
        assert!(!root.exists());
    }

    #[test]
    fn rejects_mismatched_license_and_missing_existing_policy() {
        let temp = tempfile::tempdir().unwrap();
        let (instrument, capability, mut event, mut policy) = fixture();
        policy.record_local = true;
        let missing_root = temp.path().join("missing");
        assert!(MarketIngress::open(&missing_root, policy.clone()).is_err());
        assert!(!missing_root.exists());
        let mut ingress = MarketIngress::open(temp.path(), policy.clone()).unwrap();
        event.license = "other".parse().unwrap();
        assert!(
            ingress
                .publish(event.clone(), &instrument, &capability)
                .is_err()
        );
        assert_eq!(ingress.flush().unwrap(), Some(0));
        event.license = policy.license.clone();
        assert_eq!(
            ingress.publish(event, &instrument, &capability).unwrap(),
            PublishResult::Staged(1)
        );
        assert_eq!(ingress.flush().unwrap(), Some(1));
        drop(ingress);
        let mut changed = policy.clone();
        changed.backup_personal_drive = true;
        assert!(MarketIngress::open(temp.path(), changed.clone()).is_err());
        assert!(backup_manifest(temp.path(), &policy.source, &policy.product).is_err());
        fs::remove_file(
            product_dir(temp.path(), &policy.source, &policy.product).join("access.json"),
        )
        .unwrap();
        assert!(MarketIngress::open(temp.path(), changed).is_err());
    }

    #[test]
    fn quality_recovery_keeps_intervals_counts_and_categories_on_restart() {
        let temp = tempfile::tempdir().unwrap();
        let (_instrument, _capability, event, mut policy) = fixture();
        policy.record_local = true;
        let mut ingress = MarketIngress::open(temp.path(), policy.clone()).unwrap();
        let outcomes = [
            None,
            Some(RecoveryOutcome::TickBackfilled),
            Some(RecoveryOutcome::BarBackfilled),
            Some(RecoveryOutcome::BookRestoredFromSnapshot),
            Some(RecoveryOutcome::HistoryUnrecoverable),
        ];
        let reports: Vec<_> = outcomes
            .into_iter()
            .enumerate()
            .map(|(index, outcome)| QualityReport {
                channel: event.channel.clone(),
                instrument_id: event.instrument_id.clone(),
                from_ts: (9_007_199_254_740_993 + index as u64).into(),
                to_ts: (9_007_199_254_741_000 + index as u64).into(),
                cause: GapCause::Disconnected,
                outcome,
                count: outcome.map(|_| (index as u64).into()),
            })
            .collect();
        for report in &reports {
            ingress.report_quality(report.clone()).unwrap();
            ingress.report_quality(report.clone()).unwrap();
        }
        let mut invalid = reports[0].clone();
        invalid.from_ts = u64::MAX.into();
        assert!(ingress.report_quality(invalid).is_err());
        drop(ingress);
        let ingress = MarketIngress::open(temp.path(), policy).unwrap();
        assert_eq!(ingress.quality().unwrap(), reports);
    }
}
