use rusqlite::{Connection, params};

// Minimal synthesized stored request. The list reader only needs the plan identity.
pub const BUNDLE: &str = r#"{"strategy":"StrategyBundle","start":"2025-01-02T00:00:00Z","end":"2025-03-04T00:00:00Z","plan":{"content_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","bundle":{"metadata":{"id":"synthetic.bundle","version":"1.0.0"}}},"snapshot_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#;

pub fn insert_list_fixture(db: &Connection, id: i64, body: &str) {
    db.execute(
        "INSERT INTO jobs(id, idem, request, status, created_at)
         VALUES (?1, ?2, ?3, 'failed', '2025-01-02T00:00:00Z')",
        params![id, format!("synthetic-{id}"), body],
    )
    .unwrap();
}
