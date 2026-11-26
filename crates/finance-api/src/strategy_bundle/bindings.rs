use rusqlite::{Connection, TransactionBehavior, params};
use std::collections::{BTreeMap, btree_map::Entry};
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub enum Binding {
    Bound,
    Conflict { existing_sha256: String },
}

pub type BindingError = Box<dyn std::error::Error + Send + Sync>;

pub trait VersionBindings {
    /// Atomically bind a new tuple, accept its existing hash, or report a conflict
    /// without changing the stored hash.
    fn bind(
        &mut self,
        strategy_id: &str,
        version: &str,
        content_sha256: &str,
    ) -> Result<Binding, BindingError>;
}

#[derive(Default)]
pub struct MemoryBindings(BTreeMap<(String, String), String>);

impl VersionBindings for MemoryBindings {
    fn bind(&mut self, id: &str, version: &str, hash: &str) -> Result<Binding, BindingError> {
        Ok(match self.0.entry((id.to_owned(), version.to_owned())) {
            Entry::Vacant(entry) => {
                entry.insert(hash.to_owned());
                Binding::Bound
            }
            Entry::Occupied(entry) if entry.get() == hash => Binding::Bound,
            Entry::Occupied(entry) => Binding::Conflict {
                existing_sha256: entry.get().clone(),
            },
        })
    }
}

/// Local binding store for explicit standalone CLI use.
pub struct SqliteBindings(Connection);

impl SqliteBindings {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS strategy_bundle_bindings (
                strategy_id TEXT NOT NULL,
                version TEXT NOT NULL,
                content_sha256 TEXT NOT NULL,
                PRIMARY KEY (strategy_id, version)
            ) STRICT;",
        )?;
        Ok(Self(db))
    }
}

impl VersionBindings for SqliteBindings {
    fn bind(&mut self, id: &str, version: &str, hash: &str) -> Result<Binding, BindingError> {
        let txn = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        txn.execute(
            "INSERT INTO strategy_bundle_bindings VALUES (?1, ?2, ?3)
             ON CONFLICT (strategy_id, version) DO NOTHING",
            params![id, version, hash],
        )?;
        let existing_sha256: String = txn.query_row(
            "SELECT content_sha256 FROM strategy_bundle_bindings WHERE strategy_id=?1 AND version=?2",
            params![id, version],
            |row| row.get(0),
        )?;
        txn.commit()?;
        Ok(if existing_sha256 == hash {
            Binding::Bound
        } else {
            Binding::Conflict { existing_sha256 }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn concurrent_sqlite_bindings_keep_the_first_hash() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bindings.sqlite3");
        let mut reader = SqliteBindings::open(&path).unwrap();
        let barrier = Barrier::new(2);
        let (left, right) = std::thread::scope(|scope| {
            let bind = |hash| {
                let mut store = SqliteBindings::open(&path).unwrap();
                barrier.wait();
                store.bind("example.strategy", "1.0.0", hash).unwrap()
            };
            let first = scope.spawn(move || bind("first"));
            let second = scope.spawn(move || bind("second"));
            (first.join().unwrap(), second.join().unwrap())
        });
        let (winner, loser) = match (left, right) {
            (Binding::Bound, Binding::Conflict { existing_sha256 }) => {
                assert_eq!(existing_sha256, "first");
                ("first", "second")
            }
            (Binding::Conflict { existing_sha256 }, Binding::Bound) => {
                assert_eq!(existing_sha256, "second");
                ("second", "first")
            }
            result => panic!("expected one successful bind and one conflict: {result:?}"),
        };
        assert_eq!(
            reader.bind("example.strategy", "1.0.0", winner).unwrap(),
            Binding::Bound
        );
        assert_eq!(
            reader.bind("example.strategy", "1.0.0", loser).unwrap(),
            Binding::Conflict {
                existing_sha256: winner.into()
            }
        );
    }
}
