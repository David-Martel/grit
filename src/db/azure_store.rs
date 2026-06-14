use anyhow::{Context, Result};
use azure_core::request_options::IfMatchCondition;
use azure_storage::prelude::*;
use azure_storage_blobs::prelude::*;
use chrono::Utc;

use super::lock_store::{LockEntry, LockResult, LockStore};

/// Azure Blob Storage backend (native API).
///
/// Uses `If-None-Match: *` on PUT for atomic lock acquisition.
///
/// Note: grit does not emit events itself. Blob create/delete can be observed
/// in real time only if an Azure Event Grid subscription is configured on the
/// container out of band — `grit watch` then consumes those events.
pub struct AzureLockStore {
    client: ContainerClient,
    prefix: String,
    _runtime: tokio::runtime::Runtime,
    rt: tokio::runtime::Handle,
}

/// Configuration for Azure Blob Storage backend
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AzureConfig {
    /// Storage account name
    pub account: String,
    /// Access key
    pub access_key: String,
    /// Container name
    pub container: String,
    /// Key prefix (default: ".grit/locks/")
    pub prefix: Option<String>,
}

const DEFAULT_PREFIX: &str = ".grit/locks/";

/// Outcome of reserving the exclusive write slot for a symbol.
enum WriteReserve {
    /// We now hold the write slot and must still verify reader compatibility.
    Reserved,
    /// A terminal result was reached (own-lock refresh granted, or blocked).
    Terminal(LockResult),
}

impl AzureLockStore {
    pub fn from_config(config: &AzureConfig) -> Result<Self> {
        let rt = tokio::runtime::Runtime::new()?;

        let storage_credentials =
            StorageCredentials::access_key(&config.account, config.access_key.clone());
        let service_client = BlobServiceClient::new(&config.account, storage_credentials);
        let container_client = service_client.container_client(&config.container);

        let handle = rt.handle().clone();
        Ok(Self {
            client: container_client,
            prefix: config
                .prefix
                .clone()
                .unwrap_or_else(|| DEFAULT_PREFIX.to_string()),
            _runtime: rt,
            rt: handle,
        })
    }

    fn lock_key(&self, symbol_id: &str) -> String {
        format!("{}{}", self.prefix, urlencoding::encode(symbol_id))
    }

    /// Key for a per-agent shared READ lock. Each reader gets its own blob so
    /// multiple readers of a symbol coexist and are tracked, released and
    /// refreshed independently. Write locks keep the canonical `lock_key`.
    fn reader_key(&self, symbol_id: &str, agent_id: &str) -> String {
        format!(
            "{}r/{}/{}",
            self.prefix,
            urlencoding::encode(symbol_id),
            urlencoding::encode(agent_id)
        )
    }

    /// The blob key that actually stores `entry` (reader keyspace for reads).
    fn key_for_entry(&self, entry: &LockEntry) -> String {
        if entry.mode == "read" {
            self.reader_key(&entry.symbol_id, &entry.agent_id)
        } else {
            self.lock_key(&entry.symbol_id)
        }
    }

    /// PUT a blob at an explicit key (unconditional).
    fn put_at(&self, key: &str, entry: &LockEntry) -> Result<()> {
        let body = serde_json::to_vec(entry)?;
        let blob = self.client.blob_client(key);
        self.rt
            .block_on(async {
                blob.put_block_blob(body)
                    .content_type("application/json")
                    .await
            })
            .context("Azure PUT failed")?;
        Ok(())
    }

    /// DELETE a blob at an explicit key (tolerates a missing blob).
    fn delete_at(&self, key: &str) -> Result<()> {
        let blob = self.client.blob_client(key);
        let result = self.rt.block_on(async { blob.delete().await });
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                let s = e.to_string();
                if s.contains("BlobNotFound") || s.contains("404") || s.contains("not found") {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("Azure DELETE failed: {}", e))
                }
            }
        }
    }

    /// GET and parse the blob at an explicit key, None if absent.
    fn get_at(&self, key: &str) -> Result<Option<LockEntry>> {
        let blob = self.client.blob_client(key);
        let result = self.rt.block_on(async { blob.get_content().await });
        match result {
            Ok(data) => Ok(Some(
                serde_json::from_slice(&data).context("Failed to parse lock entry")?,
            )),
            Err(e) => {
                let s = e.to_string();
                if s.contains("BlobNotFound") || s.contains("404") || s.contains("not found") {
                    Ok(None)
                } else {
                    Err(anyhow::anyhow!("Azure GET failed: {}", e))
                }
            }
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

    /// Reserve the exclusive write slot for `entry`, reclaiming an expired
    /// holder and refreshing our own existing write lock.
    fn reserve_write_slot(&self, entry: &LockEntry) -> Result<WriteReserve> {
        for _attempt in 0..3 {
            if self.put_lock_if_absent(entry)? {
                return Ok(WriteReserve::Reserved);
            }
            match self.get_lock(&entry.symbol_id)? {
                Some(existing) if existing.agent_id == entry.agent_id => {
                    self.put_lock(entry)?;
                    return Ok(WriteReserve::Terminal(LockResult::Granted));
                }
                Some(existing) if Self::is_entry_expired(&existing) => {
                    self.delete_lock(&entry.symbol_id)?;
                }
                Some(existing) => {
                    return Ok(WriteReserve::Terminal(LockResult::Blocked {
                        by_agent: existing.agent_id,
                        by_intent: existing.intent,
                    }));
                }
                None => {}
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
    /// other agent holds an active read lock.
    fn try_write_lock(&self, entry: &LockEntry) -> Result<LockResult> {
        match self.reserve_write_slot(entry)? {
            WriteReserve::Terminal(result) => Ok(result),
            WriteReserve::Reserved => {
                let readers = self.other_active_readers(&entry.symbol_id, &entry.agent_id)?;
                if let Some(r) = readers.into_iter().next() {
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
    /// lock. Persists a per-agent reader blob, then re-verifies no writer
    /// slipped in.
    fn try_read_lock(&self, entry: &LockEntry) -> Result<LockResult> {
        let symbol_id = &entry.symbol_id;
        let agent_id = &entry.agent_id;

        if let Some(w) = self.get_lock(symbol_id)? {
            if w.agent_id == *agent_id {
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

        self.put_at(&self.reader_key(symbol_id, agent_id), entry)?;

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

    fn get_lock(&self, symbol_id: &str) -> Result<Option<LockEntry>> {
        let key = self.lock_key(symbol_id);
        let blob = self.client.blob_client(&key);

        let result = self.rt.block_on(async { blob.get_content().await });

        match result {
            Ok(data) => {
                let entry: LockEntry =
                    serde_json::from_slice(&data).context("Failed to parse lock entry")?;
                Ok(Some(entry))
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("BlobNotFound")
                    || err_str.contains("404")
                    || err_str.contains("not found")
                {
                    Ok(None)
                } else {
                    Err(anyhow::anyhow!("Azure GET failed: {}", e))
                }
            }
        }
    }

    fn put_lock(&self, entry: &LockEntry) -> Result<()> {
        let key = self.lock_key(&entry.symbol_id);
        let body = serde_json::to_vec(entry)?;
        let blob = self.client.blob_client(&key);

        self.rt
            .block_on(async {
                blob.put_block_blob(body)
                    .content_type("application/json")
                    .await
            })
            .context("Azure PUT failed")?;

        Ok(())
    }

    /// Atomic PUT — only succeeds if blob does NOT exist.
    /// Uses `If-None-Match: *` (Azure native support).
    fn put_lock_if_absent(&self, entry: &LockEntry) -> Result<bool> {
        let key = self.lock_key(&entry.symbol_id);
        let body = serde_json::to_vec(entry)?;
        let blob = self.client.blob_client(&key);

        let result = self.rt.block_on(async {
            blob.put_block_blob(body)
                .content_type("application/json")
                .if_match(IfMatchCondition::NotMatch("*".to_string()))
                .await
        });

        match result {
            Ok(_) => Ok(true),
            Err(e) => {
                let err_str = e.to_string();
                // 409 Conflict or 412 Precondition Failed = blob already exists
                if err_str.contains("409")
                    || err_str.contains("412")
                    || err_str.contains("BlobAlreadyExists")
                    || err_str.contains("ConditionNotMet")
                {
                    Ok(false)
                } else {
                    Err(anyhow::anyhow!("Azure conditional PUT failed: {}", e))
                }
            }
        }
    }

    fn delete_lock(&self, symbol_id: &str) -> Result<()> {
        let key = self.lock_key(symbol_id);
        let blob = self.client.blob_client(&key);

        let result = self.rt.block_on(async { blob.delete().await });

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("BlobNotFound") || err_str.contains("404") {
                    Ok(()) // Already deleted
                } else {
                    Err(anyhow::anyhow!("Azure DELETE failed: {}", e))
                }
            }
        }
    }

    fn is_entry_expired(entry: &LockEntry) -> bool {
        if let Ok(locked_at) = chrono::DateTime::parse_from_rfc3339(&entry.locked_at) {
            let elapsed = Utc::now().signed_duration_since(locked_at);
            elapsed.num_seconds() as u64 > entry.ttl_seconds
        } else {
            true
        }
    }

    fn list_all_locks(&self) -> Result<Vec<LockEntry>> {
        self.list_under_prefix(&self.prefix)
    }

    /// List and parse all lock blobs under an explicit key prefix.
    fn list_under_prefix(&self, prefix: &str) -> Result<Vec<LockEntry>> {
        let result = self.rt.block_on(async {
            let mut entries = Vec::new();
            let mut stream = self
                .client
                .list_blobs()
                .prefix(prefix.to_string())
                .into_stream();

            use futures::StreamExt;
            while let Some(page) = stream.next().await {
                match page {
                    Ok(response) => {
                        for blob in response.blobs.blobs() {
                            let blob_client = self.client.blob_client(&blob.name);
                            if let Ok(data) = blob_client.get_content().await {
                                if let Ok(entry) = serde_json::from_slice::<LockEntry>(&data) {
                                    entries.push(entry);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Azure list error: {}", e);
                        break;
                    }
                }
            }
            entries
        });

        Ok(result)
    }
}

impl LockStore for AzureLockStore {
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
        // Release whichever lock the agent owns (per-agent read blob or the
        // canonical write blob). Reader deletion is idempotent.
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
                self.delete_at(&self.key_for_entry(entry))?;
                count += 1;
            }
        }
        Ok(count)
    }

    fn all_locks(&self) -> Result<Vec<LockEntry>> {
        let all = self.list_all_locks()?;
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
                // Re-GET before refreshing so a concurrently released/stolen
                // lock is not resurrected by an unconditional PUT. Only refresh
                // when it still exists and is still ours.
                match self.get_at(&key)? {
                    Some(current) if current.agent_id == agent_id => {
                        let updated = LockEntry {
                            locked_at: now.clone(),
                            ttl_seconds,
                            ..entry
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
