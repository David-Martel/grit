use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

use super::lock_store::{LockEntry, LockResult, LockStore};

/// Translate a raw SQLite foreign-key failure on `locks.symbol_id` into an
/// actionable message. The lock table references `symbols(id)`, so claiming a
/// symbol that was never indexed surfaces as a bare "FOREIGN KEY constraint
/// failed" otherwise (issue #21).
fn unknown_symbol_error(symbol_id: &str, err: rusqlite::Error) -> anyhow::Error {
    if let rusqlite::Error::SqliteFailure(e, _) = &err {
        if e.code == rusqlite::ErrorCode::ConstraintViolation {
            return anyhow::anyhow!(
                "symbol '{}' is not in the registry. Run `grit symbols` to list \
                 indexed symbols (re-run `grit init` if the codebase changed).",
                symbol_id
            );
        }
    }
    err.into()
}

/// SQLite-backed lock store (local coordination)
pub struct SqliteLockStore {
    conn: Mutex<Connection>,
}

impl SqliteLockStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        crate::db::configure_connection(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Acquire the connection mutex, converting poison errors.
    fn conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Database lock poisoned: {}", e))
    }
}

impl LockStore for SqliteLockStore {
    fn try_lock(
        &self,
        symbol_id: &str,
        agent_id: &str,
        intent: &str,
        ttl_seconds: u64,
        mode: &str,
    ) -> Result<LockResult> {
        let mut conn = self.conn()?;

        // The whole check-then-set MUST be atomic across processes. Each `grit`
        // invocation is a separate process with its own connection, so the
        // struct Mutex only serializes threads within one process. Without a
        // transaction that takes the write lock up front, two processes could
        // both read "no conflicting lock" and both INSERT a write lock on the
        // same symbol (distinct (symbol_id, agent_id) primary keys, so neither
        // INSERT fails) — granting two agents a write lock on the same symbol.
        // BEGIN IMMEDIATE acquires the database write lock before the SELECT, so
        // a concurrent process blocks (busy_timeout) until we commit and then
        // observes our lock.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Clean up expired locks on this symbol. COALESCE matches
        // gc_expired_locks so legacy rows with a NULL ttl_seconds are still
        // collectable here instead of lingering forever.
        tx.execute(
            "DELETE FROM locks WHERE symbol_id = ?1
             AND (julianday('now') - julianday(locked_at)) * 86400 > COALESCE(ttl_seconds, 600)",
            params![symbol_id],
        )?;

        // Check existing locks on this symbol (there can be multiple read locks)
        let existing: Vec<(String, String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT agent_id, intent, COALESCE(mode, 'write') FROM locks WHERE symbol_id = ?1",
            )?;
            let rows = stmt
                .query_map(params![symbol_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };

        let result = if existing.is_empty() {
            tx.execute(
                "INSERT INTO locks (symbol_id, agent_id, intent, mode, ttl_seconds) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![symbol_id, agent_id, intent, mode, ttl_seconds],
            )
            .map_err(|e| unknown_symbol_error(symbol_id, e))?;
            LockResult::Granted
        } else if existing.iter().any(|(a, _, _)| a == agent_id) {
            // Same agent already holds a lock on this symbol -- update it
            tx.execute(
                "UPDATE locks SET intent = ?1, mode = ?2, locked_at = datetime('now'), ttl_seconds = ?5
                 WHERE symbol_id = ?3 AND agent_id = ?4",
                params![intent, mode, symbol_id, agent_id, ttl_seconds],
            )?;
            LockResult::Granted
        } else if mode == "read" {
            // Read request: blocked only by a WRITE lock from another agent.
            if let Some((by_agent, by_intent, _)) = existing.iter().find(|(_, _, m)| m == "write") {
                LockResult::Blocked {
                    by_agent: by_agent.clone(),
                    by_intent: by_intent.clone(),
                }
            } else {
                // All existing locks are read -- grant (insert new read lock row)
                tx.execute(
                    "INSERT INTO locks (symbol_id, agent_id, intent, mode, ttl_seconds) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![symbol_id, agent_id, intent, mode, ttl_seconds],
                )
                .map_err(|e| unknown_symbol_error(symbol_id, e))?;
                LockResult::Granted
            }
        } else {
            // Write lock requested -- blocked by any other agent's lock.
            let (by_agent, by_intent, _) = &existing[0];
            LockResult::Blocked {
                by_agent: by_agent.clone(),
                by_intent: by_intent.clone(),
            }
        };

        tx.commit()?;
        Ok(result)
    }

    fn release(&self, symbol_id: &str, agent_id: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM locks WHERE symbol_id = ?1 AND agent_id = ?2",
            params![symbol_id, agent_id],
        )?;
        Ok(())
    }

    fn release_all(&self, agent_id: &str) -> Result<usize> {
        let conn = self.conn()?;
        let count = conn.execute("DELETE FROM locks WHERE agent_id = ?1", params![agent_id])?;
        Ok(count)
    }

    fn all_locks(&self) -> Result<Vec<LockEntry>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT symbol_id, agent_id, intent, locked_at, COALESCE(ttl_seconds, 600), COALESCE(mode, 'write')
             FROM locks ORDER BY agent_id, symbol_id"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(LockEntry {
                symbol_id: row.get(0)?,
                agent_id: row.get(1)?,
                intent: row.get(2)?,
                locked_at: row.get(3)?,
                ttl_seconds: row.get::<_, i64>(4)? as u64,
                mode: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn locks_for_agent(&self, agent_id: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT symbol_id, intent FROM locks WHERE agent_id = ?1")?;
        let rows = stmt.query_map(params![agent_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn gc_expired_locks(&self) -> Result<usize> {
        let conn = self.conn()?;
        let count = conn.execute(
            "DELETE FROM locks
             WHERE (julianday('now') - julianday(locked_at)) * 86400 > COALESCE(ttl_seconds, 600)",
            [],
        )?;
        Ok(count)
    }

    fn refresh_ttl(&self, agent_id: &str, ttl_seconds: u64) -> Result<usize> {
        let conn = self.conn()?;
        let count = conn.execute(
            "UPDATE locks SET locked_at = datetime('now'), ttl_seconds = ?1 WHERE agent_id = ?2",
            params![ttl_seconds, agent_id],
        )?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::super::lock_store::{LockResult, LockStore};
    use super::*;
    use std::sync::Arc;

    /// Create a temporary SQLite database with the locks table and return the store.
    fn setup() -> (tempfile::TempDir, SqliteLockStore) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let db_path = dir.path().join("test.db");

        // Create schema directly — avoids needing the full Database struct and symbols table FK
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA busy_timeout=5000;
                 CREATE TABLE IF NOT EXISTS locks (
                     symbol_id   TEXT NOT NULL,
                     agent_id    TEXT NOT NULL,
                     intent      TEXT,
                     mode        TEXT DEFAULT 'write',
                     locked_at   TEXT DEFAULT (datetime('now')),
                     ttl_seconds INTEGER DEFAULT 600,
                     PRIMARY KEY (symbol_id, agent_id)
                 );
                 CREATE INDEX IF NOT EXISTS idx_locks_agent ON locks(agent_id);",
            )
            .unwrap();
        }

        let store = SqliteLockStore::open(&db_path).expect("failed to open store");
        (dir, store)
    }

    #[test]
    fn test_lock_and_release() {
        let (_dir, store) = setup();

        let result = store
            .try_lock("sym::foo", "agent-1", "editing foo", 600, "write")
            .unwrap();
        assert!(matches!(result, LockResult::Granted));

        let locks = store.all_locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].symbol_id, "sym::foo");
        assert_eq!(locks[0].agent_id, "agent-1");
        assert_eq!(locks[0].mode, "write");

        store.release("sym::foo", "agent-1").unwrap();
        let locks = store.all_locks().unwrap();
        assert!(locks.is_empty());
    }

    #[test]
    fn test_lock_blocked_by_other_agent() {
        let (_dir, store) = setup();

        let result = store
            .try_lock("sym::bar", "agent-A", "refactoring", 600, "write")
            .unwrap();
        assert!(matches!(result, LockResult::Granted));

        let result = store
            .try_lock("sym::bar", "agent-B", "also refactoring", 600, "write")
            .unwrap();
        match result {
            LockResult::Blocked {
                by_agent,
                by_intent,
            } => {
                assert_eq!(by_agent, "agent-A");
                assert_eq!(by_intent, "refactoring");
            }
            LockResult::Granted => panic!("expected Blocked, got Granted"),
        }
    }

    #[test]
    fn test_same_agent_relock() {
        let (_dir, store) = setup();

        let result = store
            .try_lock("sym::baz", "agent-A", "first pass", 300, "write")
            .unwrap();
        assert!(matches!(result, LockResult::Granted));

        let result = store
            .try_lock("sym::baz", "agent-A", "second pass", 900, "write")
            .unwrap();
        assert!(matches!(result, LockResult::Granted));

        let locks = store.all_locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].ttl_seconds, 900);
        assert_eq!(locks[0].intent, "second pass");
    }

    #[test]
    fn test_release_all() {
        let (_dir, store) = setup();

        store
            .try_lock("sym::a", "agent-1", "intent-a", 600, "write")
            .unwrap();
        store
            .try_lock("sym::b", "agent-1", "intent-b", 600, "write")
            .unwrap();
        store
            .try_lock("sym::c", "agent-1", "intent-c", 600, "write")
            .unwrap();
        store
            .try_lock("sym::d", "agent-2", "intent-d", 600, "write")
            .unwrap();

        let count = store.release_all("agent-1").unwrap();
        assert_eq!(count, 3);

        let locks = store.all_locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].agent_id, "agent-2");
    }

    #[test]
    fn test_all_locks() {
        let (_dir, store) = setup();

        store
            .try_lock("sym::x", "agent-A", "ix", 600, "write")
            .unwrap();
        store
            .try_lock("sym::y", "agent-A", "iy", 600, "write")
            .unwrap();
        store
            .try_lock("sym::z", "agent-B", "iz", 600, "write")
            .unwrap();

        let locks = store.all_locks().unwrap();
        assert_eq!(locks.len(), 3);

        let ids: Vec<(&str, &str)> = locks
            .iter()
            .map(|l| (l.agent_id.as_str(), l.symbol_id.as_str()))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("agent-A", "sym::x"),
                ("agent-A", "sym::y"),
                ("agent-B", "sym::z")
            ]
        );
    }

    #[test]
    fn test_locks_for_agent() {
        let (_dir, store) = setup();

        store
            .try_lock("sym::p", "agent-1", "ip", 600, "write")
            .unwrap();
        store
            .try_lock("sym::q", "agent-1", "iq", 600, "write")
            .unwrap();
        store
            .try_lock("sym::r", "agent-2", "ir", 600, "write")
            .unwrap();

        let agent1_locks = store.locks_for_agent("agent-1").unwrap();
        assert_eq!(agent1_locks.len(), 2);
        let symbols: Vec<&str> = agent1_locks.iter().map(|(s, _)| s.as_str()).collect();
        assert!(symbols.contains(&"sym::p"));
        assert!(symbols.contains(&"sym::q"));

        let agent2_locks = store.locks_for_agent("agent-2").unwrap();
        assert_eq!(agent2_locks.len(), 1);
        assert_eq!(agent2_locks[0].0, "sym::r");
    }

    #[test]
    fn test_gc_expired_locks() {
        let (_dir, store) = setup();

        store
            .try_lock("sym::expire", "agent-1", "short-lived", 1, "write")
            .unwrap();
        assert_eq!(store.all_locks().unwrap().len(), 1);

        std::thread::sleep(std::time::Duration::from_secs(2));

        let cleaned = store.gc_expired_locks().unwrap();
        assert_eq!(cleaned, 1);
        assert!(store.all_locks().unwrap().is_empty());
    }

    #[test]
    fn test_refresh_ttl() {
        let (_dir, store) = setup();

        store
            .try_lock("sym::m", "agent-1", "im", 300, "write")
            .unwrap();
        store
            .try_lock("sym::n", "agent-1", "in", 300, "write")
            .unwrap();

        let count = store.refresh_ttl("agent-1", 900).unwrap();
        assert_eq!(count, 2);

        let locks = store.all_locks().unwrap();
        for lock in &locks {
            assert_eq!(lock.ttl_seconds, 900);
        }
    }

    #[test]
    fn test_concurrent_access() {
        let (_dir, store) = setup();
        let store = Arc::new(store);
        let mut handles = Vec::new();

        for i in 0..10 {
            let store = Arc::clone(&store);
            let handle = std::thread::spawn(move || {
                let agent = format!("agent-{}", i);
                store
                    .try_lock("sym::contested", &agent, "racing", 600, "write")
                    .unwrap()
            });
            handles.push(handle);
        }

        let results: Vec<LockResult> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let granted = results
            .iter()
            .filter(|r| matches!(r, LockResult::Granted))
            .count();
        let blocked = results
            .iter()
            .filter(|r| matches!(r, LockResult::Blocked { .. }))
            .count();

        assert_eq!(granted, 1, "expected exactly 1 Granted, got {}", granted);
        assert_eq!(blocked, 9, "expected exactly 9 Blocked, got {}", blocked);
    }

    /// Regression test for the cross-process double-write-lock race: each thread
    /// opens its OWN `SqliteLockStore` (a separate connection, exactly like a
    /// separate `grit` process), so the per-store Mutex provides no mutual
    /// exclusion between them. Only the `BEGIN IMMEDIATE` transaction in
    /// try_lock keeps the check-then-set atomic. Without it, several agents
    /// would each be Granted a write lock on the same symbol.
    #[test]
    fn test_concurrent_access_separate_connections() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let db_path = dir.path().join("race.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA busy_timeout=5000;
                 CREATE TABLE IF NOT EXISTS locks (
                     symbol_id   TEXT NOT NULL,
                     agent_id    TEXT NOT NULL,
                     intent      TEXT,
                     mode        TEXT DEFAULT 'write',
                     locked_at   TEXT DEFAULT (datetime('now')),
                     ttl_seconds INTEGER DEFAULT 600,
                     PRIMARY KEY (symbol_id, agent_id)
                 );
                 CREATE INDEX IF NOT EXISTS idx_locks_agent ON locks(agent_id);",
            )
            .unwrap();
        }

        let mut handles = Vec::new();
        for i in 0..16 {
            let path = db_path.clone();
            handles.push(std::thread::spawn(move || {
                let store = SqliteLockStore::open(&path).expect("open");
                let agent = format!("agent-{}", i);
                store
                    .try_lock("sym::contested", &agent, "racing", 600, "write")
                    .unwrap()
            }));
        }

        let results: Vec<LockResult> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let granted = results
            .iter()
            .filter(|r| matches!(r, LockResult::Granted))
            .count();

        assert_eq!(
            granted, 1,
            "exactly one agent must win the write lock across separate connections, got {}",
            granted
        );

        // And the database must agree: a single write lock row for the symbol.
        let store = SqliteLockStore::open(&db_path).unwrap();
        let rows: Vec<_> = store
            .all_locks()
            .unwrap()
            .into_iter()
            .filter(|l| l.symbol_id == "sym::contested")
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "exactly one lock row expected, got {}",
            rows.len()
        );
    }

    // ── Read/write lock mode tests ──

    #[test]
    fn test_read_lock_allows_multiple_readers() {
        let (_dir, store) = setup();

        let r1 = store
            .try_lock("sym::shared", "agent-1", "reading", 600, "read")
            .unwrap();
        assert!(matches!(r1, LockResult::Granted));

        let r2 = store
            .try_lock("sym::shared", "agent-2", "also reading", 600, "read")
            .unwrap();
        assert!(matches!(r2, LockResult::Granted));

        let r3 = store
            .try_lock("sym::shared", "agent-3", "reading too", 600, "read")
            .unwrap();
        assert!(matches!(r3, LockResult::Granted));

        let locks = store.all_locks().unwrap();
        assert_eq!(locks.len(), 3);
        for lock in &locks {
            assert_eq!(lock.symbol_id, "sym::shared");
            assert_eq!(lock.mode, "read");
        }
    }

    #[test]
    fn test_write_lock_blocks_readers() {
        let (_dir, store) = setup();

        let r1 = store
            .try_lock("sym::exclusive", "agent-1", "editing", 600, "write")
            .unwrap();
        assert!(matches!(r1, LockResult::Granted));

        let r2 = store
            .try_lock("sym::exclusive", "agent-2", "reading", 600, "read")
            .unwrap();
        assert!(matches!(r2, LockResult::Blocked { .. }));
    }

    #[test]
    fn test_read_lock_blocks_writers() {
        let (_dir, store) = setup();

        let r1 = store
            .try_lock("sym::guarded", "agent-1", "reading", 600, "read")
            .unwrap();
        assert!(matches!(r1, LockResult::Granted));

        let r2 = store
            .try_lock("sym::guarded", "agent-2", "editing", 600, "write")
            .unwrap();
        assert!(matches!(r2, LockResult::Blocked { .. }));
    }

    #[test]
    fn test_read_lock_does_not_block_readers() {
        let (_dir, store) = setup();

        let r1 = store
            .try_lock("sym::open", "agent-1", "context", 600, "read")
            .unwrap();
        assert!(matches!(r1, LockResult::Granted));

        let r2 = store
            .try_lock("sym::open", "agent-2", "context", 600, "read")
            .unwrap();
        assert!(matches!(r2, LockResult::Granted));

        // But a write should be blocked
        let r3 = store
            .try_lock("sym::open", "agent-3", "refactor", 600, "write")
            .unwrap();
        assert!(matches!(r3, LockResult::Blocked { .. }));
    }

    #[test]
    fn test_mode_stored_in_lock_entry() {
        let (_dir, store) = setup();

        store
            .try_lock("sym::a", "agent-1", "reading", 600, "read")
            .unwrap();
        store
            .try_lock("sym::b", "agent-2", "writing", 600, "write")
            .unwrap();

        let locks = store.all_locks().unwrap();
        let read_lock = locks.iter().find(|l| l.symbol_id == "sym::a").unwrap();
        assert_eq!(read_lock.mode, "read");

        let write_lock = locks.iter().find(|l| l.symbol_id == "sym::b").unwrap();
        assert_eq!(write_lock.mode, "write");
    }
}
