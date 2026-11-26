use crate::{Error, Result};
use rustix::{
    fs::{Mode, OFlags, open},
    time::{ClockId, clock_gettime},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, TryLockError},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::NamedTempFile;

const GAP: Duration = Duration::from_secs(16);
const MAX_STATE_SIZE: u64 = 512;

#[derive(Deserialize, Serialize)]
struct Stamp {
    boot_id: String,
    monotonic_ns: u64,
}

impl Stamp {
    fn now() -> Result<Self> {
        let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(failure)?;
        let clock = clock_gettime(ClockId::Monotonic);
        let too_large = || Error::Recovery("IB history pacing clock out of range".into());
        let seconds = u64::try_from(clock.tv_sec).map_err(|_| too_large())?;
        let nanos = u64::try_from(clock.tv_nsec).map_err(|_| too_large())?;
        let monotonic_ns = seconds
            .checked_mul(1_000_000_000)
            .and_then(|seconds| seconds.checked_add(nanos))
            .ok_or_else(too_large)?;
        Ok(Self {
            boot_id: boot_id.trim().into(),
            monotonic_ns,
        })
    }
}

#[derive(Deserialize, Serialize)]
enum Reservation {
    Pending,
    Completed(Stamp),
}

impl Reservation {
    fn wait_at(&self, now: &Stamp) -> Duration {
        match self {
            Self::Completed(previous) if previous.boot_id == now.boot_id => {
                let elapsed = now.monotonic_ns.saturating_sub(previous.monotonic_ns);
                GAP.saturating_sub(Duration::from_nanos(elapsed))
            }
            // An interrupted process could have just sent a request before it
            // released the OS lock. The same conservative bound covers reboots.
            _ => GAP,
        }
    }
}

pub(super) struct Pacer {
    dir: PathBuf,
    _lock: File,
}

impl Pacer {
    pub(super) async fn acquire(root: &Path) -> Result<Self> {
        let dir = state_dir(root)?;
        let lock = File::from(
            open(
                dir.join("ibkr-pacing.lock"),
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(failure)?,
        );
        if !lock.metadata().map_err(failure)?.is_file() {
            return Err(failure("lock must be a regular file"));
        }
        loop {
            match lock.try_lock() {
                Ok(()) => return Ok(Self { dir, _lock: lock }),
                Err(TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(TryLockError::Error(error)) => return Err(failure(error)),
            }
        }
    }

    pub(super) async fn wait(&self) -> Result<()> {
        let delay = self.delay_at(&Stamp::now()?)?;
        tokio::time::sleep(delay).await;
        Ok(())
    }

    fn delay_at(&self, now: &Stamp) -> Result<Duration> {
        let fd = match open(
            self.dir.join("ibkr-pacing.json"),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(Duration::ZERO),
            Err(error) => return Err(failure(error)),
        };
        let file = File::from(fd);
        let metadata = file.metadata().map_err(failure)?;
        if !metadata.is_file() || metadata.len() > MAX_STATE_SIZE {
            return Err(failure("state must be a small regular file"));
        }
        let previous: Reservation =
            serde_json::from_reader(file.take(MAX_STATE_SIZE)).map_err(failure)?;
        Ok(previous.wait_at(now))
    }

    pub(super) fn begin(&self) -> Result<()> {
        self.save(&Reservation::Pending)
    }

    pub(super) fn finish(&self) -> Result<()> {
        self.save(&Reservation::Completed(Stamp::now()?))
    }

    fn save(&self, reservation: &Reservation) -> Result<()> {
        let temporary = NamedTempFile::new_in(&self.dir).map_err(failure)?;
        serde_json::to_writer(temporary.as_file(), reservation).map_err(failure)?;
        temporary.as_file().sync_all().map_err(failure)?;
        temporary
            .persist(self.dir.join("ibkr-pacing.json"))
            .map_err(failure)?;
        File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .map_err(failure)
    }
}

fn state_dir(root: &Path) -> Result<PathBuf> {
    if !root.is_absolute() {
        return Err(Error::Config("FERTILE_DATA_ROOT must be absolute".into()));
    }
    let root = root.canonicalize().map_err(failure)?;
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !root.is_dir()
        || root.ancestors().any(is_checkout)
        || source
            .canonicalize()
            .is_ok_and(|path| root.starts_with(path))
    {
        return Err(Error::Config(
            "FERTILE_DATA_ROOT must be a directory outside the worktree".into(),
        ));
    }
    let mut ancestor = root;
    for name in ["finance-state", "historical"] {
        let path = ancestor.join(name);
        match fs::create_dir(&path) {
            Ok(()) => File::open(&ancestor)
                .and_then(|dir| dir.sync_all())
                .map_err(failure)?,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => (),
            Err(error) => return Err(failure(error)),
        }
        if !path.symlink_metadata().map_err(failure)?.is_dir() || is_checkout(&path) {
            return Err(failure("state path must be a directory with no symlinks"));
        }
        ancestor = path;
    }
    Ok(ancestor)
}

fn is_checkout(path: &Path) -> bool {
    let marker = path.join(".git");
    marker.is_file() || marker.join("HEAD").is_file()
}

fn failure(error: impl std::fmt::Display) -> Error {
    Error::Recovery(format!("IB history pacing: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(seconds: u64) -> Stamp {
        Stamp {
            boot_id: "synthetic-boot".into(),
            monotonic_ns: seconds * 1_000_000_000,
        }
    }

    #[tokio::test]
    async fn persistent_pacing_at_most_60_requests_in_every_10_minutes() {
        let root = tempfile::tempdir().unwrap();
        let mut requests = Vec::new();
        let mut elapsed = 0;
        for _ in 0..130 {
            let pacer = Pacer::acquire(root.path()).await.unwrap();
            elapsed += pacer.delay_at(&stamp(elapsed)).unwrap().as_secs();
            requests.push(elapsed);
            pacer.begin().unwrap();
            pacer.save(&Reservation::Completed(stamp(elapsed))).unwrap();
        }
        assert!(requests.windows(2).all(|pair| pair[1] - pair[0] >= 16));
        for time in &requests {
            assert!(
                requests
                    .iter()
                    .filter(|t| *t >= time && **t < time + 600)
                    .count()
                    <= 60
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn interrupted_reservation_survives_reopen_and_waits() {
        let root = tempfile::tempdir().unwrap();
        {
            let pacer = Pacer::acquire(root.path()).await.unwrap();
            pacer.begin().unwrap();
        }
        let pacer = Pacer::acquire(root.path()).await.unwrap();
        let start = tokio::time::Instant::now();
        pacer.wait().await.unwrap();
        assert!(start.elapsed() >= GAP);
        pacer.finish().unwrap();
        assert!(pacer.delay_at(&Stamp::now().unwrap()).unwrap() <= GAP);
        assert!(
            pacer
                .delay_at(&Stamp {
                    boot_id: "reboot".into(),
                    ..stamp(0)
                })
                .unwrap()
                == GAP
        );
    }

    #[tokio::test]
    async fn lock_serializes_callers_and_rejects_escape() {
        let root = tempfile::tempdir().unwrap();
        let lock = Pacer::acquire(root.path()).await.unwrap();
        let waiting = Pacer::acquire(root.path());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), waiting)
                .await
                .is_err()
        );
        drop(lock);
        assert!(Pacer::acquire(root.path()).await.is_ok());

        let linked = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), linked.path().join("finance-state")).unwrap();
        assert!(Pacer::acquire(linked.path()).await.is_err());
        assert!(outside.path().read_dir().unwrap().next().is_none());
        assert!(
            Pacer::acquire(Path::new(env!("CARGO_MANIFEST_DIR")))
                .await
                .is_err()
        );
    }
}
