use anyhow::{Context, Result};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use chrono::Utc;

use super::lock_store::{LockEntry, LockResult, LockStore};

/// S3-compatible lock store (works with AWS S3, Cloudflare R2, GCS, Azure Blob via S3 API, MinIO)
///
/// Each lock is an S3 object:
///   Key:  {prefix}{url_encoded_symbol_id}
///   Body: JSON LockEntry
///
/// Atomic acquisition via conditional PUT (If-None-Match: *)
pub struct S3LockStore {
    client: Client,
    bucket: String,
    prefix: String,
    _runtime: tokio::runtime::Runtime,
    rt: tokio::runtime::Handle,
}

const DEFAULT_LOCK_PREFIX: &str = ".grit/locks/";

/// Outcome of reserving the exclusive write slot for a symbol.
enum WriteReserve {
    /// We now hold the write slot and must still verify reader compatibility.
    Reserved,
    /// A terminal result was reached (own-lock refresh granted, or blocked).
    Terminal(LockResult),
}

impl S3LockStore {
    /// Build from config
    pub fn from_config(config: &S3Config) -> Result<Self> {
        let rt = tokio::runtime::Runtime::new()?;
        let client = rt.block_on(async {
            let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());

            if let Some(ref endpoint) = config.endpoint {
                loader = loader.endpoint_url(endpoint);
            }
            if let Some(ref region) = config.region {
                loader = loader.region(aws_config::Region::new(region.clone()));
            }

            let sdk_config = loader.load().await;

            // Force path-style for R2/MinIO/GCS compatibility
            // Set reasonable timeouts for CLI usage
            let timeout_config = aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                .operation_timeout(std::time::Duration::from_secs(10))
                .operation_attempt_timeout(std::time::Duration::from_secs(5))
                .build();

            let retry_config =
                aws_sdk_s3::config::retry::RetryConfig::standard().with_max_attempts(3);

            let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
                .force_path_style(true)
                .timeout_config(timeout_config)
                .retry_config(retry_config)
                .build();

            Client::from_conf(s3_config)
        });

        let handle = rt.handle().clone();
        Ok(Self {
            client,
            bucket: config.bucket.clone(),
            prefix: config
                .prefix
                .clone()
                .unwrap_or_else(|| DEFAULT_LOCK_PREFIX.to_string()),
            _runtime: rt,
            rt: handle,
        })
    }

    fn lock_key(&self, symbol_id: &str) -> String {
        format!("{}{}", self.prefix, urlencoding::encode(symbol_id))
    }

    /// Key for a per-agent shared READ lock. Each reader gets its own object so
    /// multiple readers of the same symbol coexist and are tracked, released
    /// and refreshed independently (single-key-per-symbol cannot represent
    /// multiple holders). Write locks keep the canonical `lock_key`.
    fn reader_key(&self, symbol_id: &str, agent_id: &str) -> String {
        format!(
            "{}r/{}/{}",
            self.prefix,
            urlencoding::encode(symbol_id),
            urlencoding::encode(agent_id)
        )
    }

    /// The object key that actually stores `entry`, picking the reader keyspace
    /// for read locks and the canonical key for write locks.
    fn key_for_entry(&self, entry: &LockEntry) -> String {
        if entry.mode == "read" {
            self.reader_key(&entry.symbol_id, &entry.agent_id)
        } else {
            self.lock_key(&entry.symbol_id)
        }
    }

    /// PUT an object at an explicit key (unconditional).
    fn put_at(&self, key: &str, entry: &LockEntry) -> Result<()> {
        let body = serde_json::to_vec(entry)?;
        self.rt
            .block_on(async {
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .body(ByteStream::from(body))
                    .content_type("application/json")
                    .send()
                    .await
            })
            .context("S3 PUT failed")?;
        Ok(())
    }

    /// DELETE an object at an explicit key.
    fn delete_at(&self, key: &str) -> Result<()> {
        self.rt
            .block_on(async {
                self.client
                    .delete_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .send()
                    .await
            })
            .context("S3 DELETE failed")?;
        Ok(())
    }

    /// GET and parse the object at an explicit key, None if absent.
    fn get_at(&self, key: &str) -> Result<Option<LockEntry>> {
        let result = self.rt.block_on(async {
            self.client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
        });
        match result {
            Ok(output) => {
                let body = self
                    .rt
                    .block_on(async { output.body.collect().await.map(|b| b.to_vec()) })?;
                Ok(Some(self.parse_entry(&body)?))
            }
            Err(SdkError::ServiceError(ref e)) if e.err().is_no_such_key() => Ok(None),
            Err(SdkError::ServiceError(ref e)) if e.raw().status().as_u16() == 404 => Ok(None),
            Err(err) => Err(anyhow::anyhow!("S3 GET failed: {}", err)),
        }
    }

    /// List the non-expired READ locks held on `symbol_id` by other agents.
    fn other_active_readers(&self, symbol_id: &str, agent_id: &str) -> Result<Vec<LockEntry>> {
        let prefix = format!("{}r/{}/", self.prefix, urlencoding::encode(symbol_id));
        let entries = self.list_under_prefix(&prefix)?;
        Ok(entries
            .into_iter()
            .filter(|e| e.agent_id != agent_id && !Self::is_entry_expired(e))
            .collect())
    }

    /// Reserve the exclusive write slot for `entry` (the canonical lock key),
    /// reclaiming an expired holder and refreshing our own existing write lock.
    fn reserve_write_slot(&self, entry: &LockEntry) -> Result<WriteReserve> {
        for _attempt in 0..3 {
            if self.put_lock_if_absent(entry)? {
                return Ok(WriteReserve::Reserved);
            }
            match self.get_lock(&entry.symbol_id)? {
                Some(existing) if existing.agent_id == entry.agent_id => {
                    // Refresh our own write lock.
                    self.put_lock(entry)?;
                    return Ok(WriteReserve::Terminal(LockResult::Granted));
                }
                Some(existing) if Self::is_entry_expired(&existing) => {
                    // Reclaim an expired holder, then retry the reservation.
                    self.delete_lock(&entry.symbol_id)?;
                }
                Some(existing) => {
                    return Ok(WriteReserve::Terminal(LockResult::Blocked {
                        by_agent: existing.agent_id,
                        by_intent: existing.intent,
                    }));
                }
                None => {
                    // Vanished between conditional PUT and GET — retry.
                }
            }
        }
        if let Some(holder) = self.get_lock(&entry.symbol_id)? {
            return Ok(WriteReserve::Terminal(LockResult::Blocked {
                by_agent: holder.agent_id,
                by_intent: holder.intent,
            }));
        }
        anyhow::bail!("Failed to acquire write lock after retries")
    }

    /// Acquire an exclusive write lock: reserve the slot, then back off if any
    /// other agent holds an active read lock (a writer is incompatible with
    /// readers). The reserve-then-verify ordering guarantees we never grant a
    /// writer while a reader is live; under a tie both sides back off and the
    /// caller retries (no double grant).
    fn try_write_lock(&self, entry: &LockEntry) -> Result<LockResult> {
        match self.reserve_write_slot(entry)? {
            WriteReserve::Terminal(result) => Ok(result),
            WriteReserve::Reserved => {
                let readers = self.other_active_readers(&entry.symbol_id, &entry.agent_id)?;
                if let Some(r) = readers.into_iter().next() {
                    // Release our reservation so the live readers can finish.
                    self.delete_lock(&entry.symbol_id)?;
                    Ok(LockResult::Blocked {
                        by_agent: r.agent_id,
                        by_intent: r.intent,
                    })
                } else {
                    Ok(LockResult::Granted)
                }
            }
        }
    }

    /// Acquire a shared read lock: blocked only by another agent's active write
    /// lock. Each reader persists its own per-agent object, then re-verifies no
    /// writer slipped in (and backs off if one did).
    fn try_read_lock(&self, entry: &LockEntry) -> Result<LockResult> {
        let symbol_id = &entry.symbol_id;
        let agent_id = &entry.agent_id;

        // 1. A non-expired write lock by another agent blocks reads.
        if let Some(w) = self.get_lock(symbol_id)? {
            if w.agent_id == *agent_id {
                // We already hold the write lock; a read is redundant — grant.
                return Ok(LockResult::Granted);
            }
            if Self::is_entry_expired(&w) {
                self.delete_lock(symbol_id)?;
            } else {
                return Ok(LockResult::Blocked {
                    by_agent: w.agent_id,
                    by_intent: w.intent,
                });
            }
        }

        // 2. Persist our per-agent reader object (readers never conflict).
        self.put_at(&self.reader_key(symbol_id, agent_id), entry)?;

        // 3. Re-verify no writer reserved the slot after our check; back off if so.
        if let Some(w) = self.get_lock(symbol_id)? {
            if w.agent_id != *agent_id && !Self::is_entry_expired(&w) {
                let _ = self.delete_at(&self.reader_key(symbol_id, agent_id));
                return Ok(LockResult::Blocked {
                    by_agent: w.agent_id,
                    by_intent: w.intent,
                });
            }
        }

        Ok(LockResult::Granted)
    }

    fn parse_entry(&self, body: &[u8]) -> Result<LockEntry> {
        serde_json::from_slice(body).context("Failed to parse lock entry JSON")
    }

    fn is_entry_expired(entry: &LockEntry) -> bool {
        if let Ok(locked_at) = chrono::DateTime::parse_from_rfc3339(&entry.locked_at) {
            let elapsed = Utc::now().signed_duration_since(locked_at);
            elapsed.num_seconds() as u64 > entry.ttl_seconds
        } else {
            // Can't parse timestamp, treat as expired
            true
        }
    }

    /// GET a lock object, returns None if not found
    fn get_lock(&self, symbol_id: &str) -> Result<Option<LockEntry>> {
        let key = self.lock_key(symbol_id);
        let result = self.rt.block_on(async {
            self.client
                .get_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
        });

        match result {
            Ok(output) => {
                let body = self
                    .rt
                    .block_on(async { output.body.collect().await.map(|b| b.to_vec()) })?;
                let entry = self.parse_entry(&body)?;
                Ok(Some(entry))
            }
            Err(SdkError::ServiceError(ref service_err)) if service_err.err().is_no_such_key() => {
                Ok(None)
            }
            Err(SdkError::ServiceError(ref service_err))
                if service_err.raw().status().as_u16() == 404 =>
            {
                Ok(None)
            }
            Err(err) => Err(anyhow::anyhow!("S3 GET failed: {}", err)),
        }
    }

    /// PUT a lock object (unconditional — caller must handle atomicity)
    fn put_lock(&self, entry: &LockEntry) -> Result<()> {
        let key = self.lock_key(&entry.symbol_id);
        let body = serde_json::to_vec(entry)?;

        self.rt
            .block_on(async {
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(&key)
                    .body(ByteStream::from(body))
                    .content_type("application/json")
                    .send()
                    .await
            })
            .context("S3 PUT failed")?;

        Ok(())
    }

    /// Conditional PUT — only succeeds if key does NOT exist.
    /// Returns true if created, false if already exists.
    ///
    /// Uses If-None-Match on AWS S3, falls back to GET-then-PUT for
    /// providers that don't support conditional writes (MinIO, GCS, Azure).
    fn put_lock_if_absent(&self, entry: &LockEntry) -> Result<bool> {
        let key = self.lock_key(&entry.symbol_id);
        let body = serde_json::to_vec(entry)?;

        // First try conditional PUT (native S3 / R2)
        let result = self.rt.block_on(async {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(ByteStream::from(body.clone()))
                .content_type("application/json")
                .if_none_match("*")
                .send()
                .await
        });

        match result {
            Ok(_) => return Ok(true),
            Err(SdkError::ServiceError(ref service_err)) => {
                let status = service_err.raw().status().as_u16();
                // 412 Precondition Failed = object already exists (AWS S3 / R2).
                if status == 412 {
                    return Ok(false);
                }
                // Only treat the request as "conditional PUT unsupported" for the
                // specific signals a provider gives when it does not implement
                // If-None-Match (MinIO / older S3 clones): 501 Not Implemented, or
                // a 400 carrying a NotImplemented / PreconditionNotSupported code.
                // For ANY other service error (timeout-as-5xx, throttling, auth,
                // transient 500) we must NOT silently degrade to non-atomic
                // last-writer-wins — that would let two agents win the same write
                // lock. Fail closed instead.
                let code = aws_sdk_s3::error::ProvideErrorMetadata::code(service_err.err())
                    .unwrap_or_default();
                let unsupported = status == 501
                    || (status == 400
                        && (code == "NotImplemented" || code == "PreconditionNotSupported"));
                if !unsupported {
                    anyhow::bail!(
                        "S3 conditional PUT failed (status {status}, code '{code}'); \
                         refusing to fall back to non-atomic locking to avoid granting \
                         two agents the same lock"
                    );
                }
                // else: fall through to the GET-first fallback below.
            }
            // Non-service errors (dispatch / timeout / construction) are operational
            // failures, not "feature unsupported": fail closed.
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context("S3 conditional PUT failed; refusing non-atomic fallback"));
            }
        }

        // Fallback ONLY for providers that genuinely lack conditional PUT (MinIO,
        // GCS via S3 API). Do a GET-first compare-and-set instead of an
        // unconditional overwrite: if any lock object already exists, report
        // not-acquired and let the caller's expiry/compatibility logic decide.
        // This no longer clobbers a live lock held by another agent. A small
        // TOCTOU window remains (inherent to stores without atomic conditional
        // writes) and is closed by the post-write ownership re-read.
        if self.get_lock(&entry.symbol_id)?.is_some() {
            return Ok(false);
        }

        self.put_lock(entry)?;

        // Brief pause to let the write propagate (GCS is strongly consistent since 2021,
        // but MinIO clusters may have replication lag)
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Re-read to verify we own the lock
        match self.get_lock(&entry.symbol_id)? {
            Some(stored) if stored.agent_id == entry.agent_id => Ok(true),
            Some(_) => {
                // Another agent raced us and wrote last — we lost.
                Ok(false)
            }
            None => Ok(true), // Lock vanished, treat as success
        }
    }

    /// DELETE a lock object
    fn delete_lock(&self, symbol_id: &str) -> Result<()> {
        let key = self.lock_key(symbol_id);
        self.rt
            .block_on(async {
                self.client
                    .delete_object()
                    .bucket(&self.bucket)
                    .key(&key)
                    .send()
                    .await
            })
            .context("S3 DELETE failed")?;
        Ok(())
    }

    /// LIST all lock objects, fetching bodies in parallel
    fn list_all_locks(&self) -> Result<Vec<LockEntry>> {
        self.list_under_prefix(&self.prefix)
    }

    /// List and parse all lock objects under an explicit key prefix.
    fn list_under_prefix(&self, prefix: &str) -> Result<Vec<LockEntry>> {
        let mut all_keys: Vec<String> = Vec::new();
        let mut continuation_token: Option<String> = None;

        // Phase 1: collect all keys
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);

            if let Some(ref token) = continuation_token {
                req = req.continuation_token(token);
            }

            let output = self
                .rt
                .block_on(async { req.send().await })
                .context("S3 LIST failed")?;

            for obj in output.contents() {
                if let Some(key) = obj.key() {
                    all_keys.push(key.to_string());
                }
            }

            if output.is_truncated() == Some(true) {
                continuation_token = output.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }

        if all_keys.is_empty() {
            return Ok(Vec::new());
        }

        // Phase 2: GET all objects in parallel using tokio JoinSet
        let entries: Vec<LockEntry> = self.rt.block_on(async {
            let mut set: tokio::task::JoinSet<Option<LockEntry>> = tokio::task::JoinSet::new();
            for key in all_keys {
                let client = self.client.clone();
                let bucket = self.bucket.clone();
                set.spawn(async move {
                    let get_result = client.get_object().bucket(&bucket).key(&key).send().await;
                    if let Ok(get_output) = get_result {
                        if let Ok(body) = get_output.body.collect().await.map(|b| b.to_vec()) {
                            return serde_json::from_slice::<LockEntry>(&body).ok();
                        }
                    }
                    None
                });
            }
            let mut results = Vec::new();
            while let Some(Ok(entry)) = set.join_next().await {
                if let Some(e) = entry {
                    results.push(e);
                }
            }
            results
        });

        Ok(entries)
    }
}

impl LockStore for S3LockStore {
    fn try_lock(
        &self,
        symbol_id: &str,
        agent_id: &str,
        intent: &str,
        ttl_seconds: u64,
        mode: &str,
    ) -> Result<LockResult> {
        let entry = LockEntry {
            symbol_id: symbol_id.to_string(),
            agent_id: agent_id.to_string(),
            intent: intent.to_string(),
            locked_at: Utc::now().to_rfc3339(),
            ttl_seconds,
            mode: mode.to_string(),
        };

        if mode == "read" {
            self.try_read_lock(&entry)
        } else {
            self.try_write_lock(&entry)
        }
    }

    fn release(&self, symbol_id: &str, agent_id: &str) -> Result<()> {
        // An agent may hold either a write lock (canonical key) or a per-agent
        // read lock — release whichever it owns. Reader deletion is idempotent.
        let _ = self.delete_at(&self.reader_key(symbol_id, agent_id));
        if let Some(entry) = self.get_lock(symbol_id)? {
            if entry.agent_id == agent_id {
                self.delete_lock(symbol_id)?;
            }
        }
        Ok(())
    }

    fn release_all(&self, agent_id: &str) -> Result<usize> {
        let all = self.list_all_locks()?;
        let mut count = 0;
        for entry in &all {
            if entry.agent_id == agent_id {
                // Delete at the entry's real key (reader keyspace for read locks,
                // canonical key for write locks).
                self.delete_at(&self.key_for_entry(entry))?;
                count += 1;
            }
        }
        Ok(count)
    }

    fn all_locks(&self) -> Result<Vec<LockEntry>> {
        let all = self.list_all_locks()?;
        // Filter out expired
        Ok(all
            .into_iter()
            .filter(|e| !Self::is_entry_expired(e))
            .collect())
    }

    fn locks_for_agent(&self, agent_id: &str) -> Result<Vec<(String, String)>> {
        let all = self.list_all_locks()?;
        Ok(all
            .into_iter()
            .filter(|e| e.agent_id == agent_id && !Self::is_entry_expired(e))
            .map(|e| (e.symbol_id, e.intent))
            .collect())
    }

    fn gc_expired_locks(&self) -> Result<usize> {
        let all = self.list_all_locks()?;
        let mut count = 0;
        for entry in &all {
            if Self::is_entry_expired(entry) {
                self.delete_at(&self.key_for_entry(entry))?;
                count += 1;
            }
        }
        Ok(count)
    }

    fn refresh_ttl(&self, agent_id: &str, ttl_seconds: u64) -> Result<usize> {
        let all = self.list_all_locks()?;
        let now = Utc::now().to_rfc3339();
        let mut count = 0;
        for entry in all {
            if entry.agent_id == agent_id {
                let key = self.key_for_entry(&entry);
                // Re-GET before refreshing: if the lock was released or taken
                // over by another agent since the listing, an unconditional PUT
                // would resurrect/steal it. Only refresh when it still exists
                // and is still ours. (A small TOCTOU window remains, inherent to
                // stores without compare-and-set on PUT.)
                match self.get_at(&key)? {
                    Some(current) if current.agent_id == agent_id => {
                        let updated = LockEntry {
                            symbol_id: entry.symbol_id,
                            agent_id: entry.agent_id,
                            intent: entry.intent,
                            locked_at: now.clone(),
                            ttl_seconds,
                            mode: entry.mode,
                        };
                        self.put_at(&key, &updated)?;
                        count += 1;
                    }
                    _ => {
                        // Released or taken over — do not resurrect.
                    }
                }
            }
        }
        Ok(count)
    }
}

/// Configuration for S3-compatible backend
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct S3Config {
    pub bucket: String,
    /// Custom endpoint (for R2, GCS S3-compat, Azure S3-compat, MinIO)
    pub endpoint: Option<String>,
    /// Region (default: "auto" for R2, "us-east-1" for AWS)
    pub region: Option<String>,
    /// Key prefix (default: ".grit/locks/")
    pub prefix: Option<String>,
}
