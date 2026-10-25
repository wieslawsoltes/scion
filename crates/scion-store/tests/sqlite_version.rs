#[test]
fn bundled_sqlite_contains_the_concurrent_wal_reset_fix() {
    // LocalStore, working copies and caches allow concurrent WAL connections.
    // https://sqlite.org/releaselog/3_51_3.html fixes a corruption race between
    // writers and checkpointers. Keep the linked engine above the affected
    // release range, including when build environments override bundled SQLite.
    assert!(
        rusqlite::version_number() >= 3_051_003,
        "Scion requires SQLite >= 3.51.3 (the WAL-reset fix baseline); linked version is {}",
        rusqlite::version()
    );
}
