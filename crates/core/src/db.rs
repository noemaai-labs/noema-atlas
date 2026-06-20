use crate::cas::BlobMeta;
use crate::error::Result;
use crate::manifest::{Manifest, RedistributionClass};
use crate::sign::VerificationReport;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS manifests (
    manifest_id     TEXT PRIMARY KEY,
    schema_version  TEXT NOT NULL,
    publisher_id    TEXT NOT NULL,
    model_name      TEXT NOT NULL,
    revision        TEXT,
    license_spdx    TEXT NOT NULL,
    redistribution  TEXT NOT NULL,
    gated           INTEGER NOT NULL,
    signed          INTEGER NOT NULL,
    json            TEXT NOT NULL,
    imported_at     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS manifest_signatures (
    manifest_id  TEXT NOT NULL REFERENCES manifests(manifest_id) ON DELETE CASCADE,
    key_id       TEXT NOT NULL,
    algorithm    TEXT NOT NULL,
    valid        INTEGER NOT NULL,
    PRIMARY KEY (manifest_id, key_id)
);

CREATE TABLE IF NOT EXISTS artifacts (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    manifest_id  TEXT NOT NULL REFERENCES manifests(manifest_id) ON DELETE CASCADE,
    path         TEXT NOT NULL,
    role         TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    blake3       TEXT NOT NULL,
    sha256       TEXT NOT NULL,
    format       TEXT
);
CREATE INDEX IF NOT EXISTS idx_artifacts_manifest ON artifacts(manifest_id);
CREATE INDEX IF NOT EXISTS idx_artifacts_blake3 ON artifacts(blake3);

CREATE TABLE IF NOT EXISTS artifact_sources (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    artifact_id           INTEGER NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    source_type           TEXT NOT NULL,
    source_id             TEXT NOT NULL,
    locator               TEXT NOT NULL,
    auth                  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS cache_blobs (
    blake3        TEXT PRIMARY KEY,
    sha256        TEXT NOT NULL,
    size_bytes    INTEGER NOT NULL,
    state         TEXT NOT NULL,
    committed_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS install_views (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    manifest_id    TEXT NOT NULL,
    artifact_path  TEXT NOT NULL,
    dest_path      TEXT NOT NULL,
    blake3         TEXT NOT NULL,
    link_kind      TEXT NOT NULL,
    created_at     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS downloads (
    download_id    TEXT PRIMARY KEY,
    manifest_id    TEXT NOT NULL,
    artifact_path  TEXT NOT NULL,
    state          TEXT NOT NULL,
    bytes_total    INTEGER NOT NULL,
    bytes_done     INTEGER NOT NULL,
    source_id      TEXT,
    started_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS source_health (
    source_id          TEXT PRIMARY KEY,
    success_count      INTEGER NOT NULL DEFAULT 0,
    failure_count      INTEGER NOT NULL DEFAULT 0,
    integrity_failures INTEGER NOT NULL DEFAULT 0,
    last_latency_ms    INTEGER,
    banned             INTEGER NOT NULL DEFAULT 0,
    updated_at         TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS policy_events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    manifest_id  TEXT,
    decision     TEXT NOT NULL,
    reason       TEXT NOT NULL,
    created_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS quarantine_records (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    download_id    TEXT NOT NULL,
    artifact_path  TEXT NOT NULL,
    source_id      TEXT,
    reason         TEXT NOT NULL,
    path           TEXT NOT NULL,
    created_at     TEXT NOT NULL
);

-- Per-file share override. By default every model in the library is seeded to
-- the public mesh (so anyone can find it in Explore); this table records an
-- explicit user choice to deviate: shared=0 stops sharing a model, shared=1
-- shares one that's off by default (e.g. a gated/token-walled model).
CREATE TABLE IF NOT EXISTS share_overrides (
    blake3        TEXT PRIMARY KEY,
    sha256        TEXT NOT NULL,
    shared        INTEGER NOT NULL,
    created_at    TEXT NOT NULL
);
"#;

pub struct Db {
    conn: Mutex<Connection>,
    /// Live opt-in for gated/token-walled/restrictively-licensed auto-share.
    /// Per-model overrides still win. Atomic so Settings applies without restart.
    share_gated: AtomicBool,
}

#[derive(Debug, Clone)]
pub struct ManifestSummary {
    pub manifest_id: String,
    pub model_name: String,
    pub revision: Option<String>,
    pub license_spdx: String,
    pub redistribution: RedistributionClass,
    pub gated: bool,
    pub signed: bool,
    pub imported_at: String,
}

#[derive(Debug, Clone)]
pub struct CacheBlobRow {
    pub blake3: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub state: String,
    pub committed_at: String,
}

#[derive(Debug, Clone)]
pub struct InstallRow {
    pub manifest_id: String,
    pub artifact_path: String,
    pub dest_path: String,
    pub blake3: String,
    pub link_kind: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct DownloadRow {
    pub download_id: String,
    pub manifest_id: String,
    pub artifact_path: String,
    pub state: String,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub source_id: Option<String>,
    pub started_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct SourceHealth {
    pub source_id: String,
    pub success_count: i64,
    pub failure_count: i64,
    pub integrity_failures: i64,
    pub last_latency_ms: Option<i64>,
    pub banned: bool,
}

#[derive(Debug, Clone)]
pub struct QuarantineRow {
    pub download_id: String,
    pub artifact_path: String,
    pub source_id: Option<String>,
    pub reason: String,
    pub path: String,
    pub created_at: String,
}

impl Db {
    /// Open (creating if needed) the index at `path` and run migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db {
            conn: Mutex::new(conn),
            share_gated: AtomicBool::new(false),
        })
    }

    /// Open an in-memory index (tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db {
            conn: Mutex::new(conn),
            share_gated: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("db mutex poisoned")
    }

    /// Enable/disable auto-sharing of gated/restrictive *public* models (the
    pub fn set_share_gated(&self, on: bool) {
        self.share_gated.store(on, Ordering::Relaxed);
    }

    /// Whether gated/restrictive public models are currently auto-shared.
    pub fn share_gated(&self) -> bool {
        self.share_gated.load(Ordering::Relaxed)
    }

    pub fn insert_manifest(&self, m: &Manifest, report: &VerificationReport) -> Result<()> {
        let json = m.to_json_pretty()?;
        let signed = report.is_signed();
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO manifests
             (manifest_id, schema_version, publisher_id, model_name, revision,
              license_spdx, redistribution, gated, signed, json, imported_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                m.manifest_id,
                m.schema_version,
                m.publisher.id,
                m.model.name,
                m.model.revision,
                m.license.spdx,
                m.license.redistribution.as_str(),
                m.access.gated as i64,
                signed as i64,
                json,
                crate::util::now_rfc3339(),
            ],
        )?;
        // Replace child rows.
        tx.execute(
            "DELETE FROM manifest_signatures WHERE manifest_id = ?1",
            params![m.manifest_id],
        )?;
        tx.execute(
            "DELETE FROM artifacts WHERE manifest_id = ?1",
            params![m.manifest_id],
        )?;
        for key_id in &report.valid_signatures {
            tx.execute(
                "INSERT OR REPLACE INTO manifest_signatures (manifest_id, key_id, algorithm, valid)
                 VALUES (?1,?2,'ed25519',1)",
                params![m.manifest_id, key_id],
            )?;
        }
        for key_id in &report.invalid_signatures {
            tx.execute(
                "INSERT OR REPLACE INTO manifest_signatures (manifest_id, key_id, algorithm, valid)
                 VALUES (?1,?2,'ed25519',0)",
                params![m.manifest_id, key_id],
            )?;
        }
        for art in &m.artifacts {
            tx.execute(
                "INSERT INTO artifacts (manifest_id, path, role, size_bytes, blake3, sha256, format)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    m.manifest_id,
                    art.path,
                    art.role,
                    art.size_bytes as i64,
                    art.hashes.blake3,
                    art.hashes.sha256,
                    art.format,
                ],
            )?;
            let artifact_id = tx.last_insert_rowid();
            for src in &art.sources {
                let locator = serde_json::to_string(src)?;
                tx.execute(
                    "INSERT INTO artifact_sources (artifact_id, source_type, source_id, locator, auth)
                     VALUES (?1,?2,?3,?4,?5)",
                    params![
                        artifact_id,
                        format!("{:?}", src.class()),
                        src.source_id(),
                        locator,
                        format!("{:?}", src.auth()),
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_manifest(&self, manifest_id: &str) -> Result<Option<Manifest>> {
        let conn = self.lock();
        let json: Option<String> = conn
            .query_row(
                "SELECT json FROM manifests WHERE manifest_id = ?1",
                params![manifest_id],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(j) => Ok(Some(Manifest::from_json(j.as_bytes())?)),
            None => Ok(None),
        }
    }

    pub fn list_manifests(&self) -> Result<Vec<ManifestSummary>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT manifest_id, model_name, revision, license_spdx, redistribution, gated, signed, imported_at
             FROM manifests ORDER BY imported_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let redistribution: String = row.get(4)?;
            Ok(ManifestSummary {
                manifest_id: row.get(0)?,
                model_name: row.get(1)?,
                revision: row.get(2)?,
                license_spdx: row.get(3)?,
                redistribution: RedistributionClass::from_str_opt(&redistribution)
                    .unwrap_or(RedistributionClass::PublicDownloadOnly),
                gated: row.get::<_, i64>(5)? != 0,
                signed: row.get::<_, i64>(6)? != 0,
                imported_at: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn delete_manifest(&self, manifest_id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM manifests WHERE manifest_id = ?1",
            params![manifest_id],
        )?;
        Ok(())
    }

    /// Whether a blob is shared to the mesh (LAN serve + worldwide announce).
    ///
    /// mesh, or a public mirror) is shared so anyone can find it in Explore —
    /// auto-shared, so a personal download isn't silently broadcast). A privately
    /// imported file (publisher `local`, no public source) is NOT auto-shared.
    /// A per-model override (this table) **always wins, in either direction** —
    /// Atlas is a content-addressed P2P service that verifies bytes, not
    /// licenses, so the operator decides what to share (including gated/
    /// restrictive content they explicitly opt in). A blob of unknown provenance
    pub fn is_blob_shareable(&self, blake3: &str) -> Result<bool> {
        if let Some(forced) = self.share_override(blake3)? {
            return Ok(forced);
        }
        let (_has_manifest, _gated, shareable) = self.blob_provenance(blake3)?;
        Ok(shareable)
    }

    /// `(has_manifest, gated, shareable_by_default)` for a cached blob, derived
    /// from ALL its containing manifest(s). `gated` is true if any is
    /// access-controlled (surfaced as a Library badge); `shareable_by_default` is
    /// true if at least one manifest is publicly auto-shareable — openly-licensed
    /// public models always, plus gated/restrictive public models when the
    /// `share_gated` opt-in is on (Atlas verifies content, not licenses). Matches
    /// artifacts by blake3 OR the blob's sha256 (HF-synth manifests are
    /// sha256-only until first download).
    pub fn blob_provenance(&self, blake3: &str) -> Result<(bool, bool, bool)> {
        let include_gated = self.share_gated();
        let conn = self.lock();
        let sha256: String = conn
            .query_row(
                "SELECT sha256 FROM cache_blobs WHERE blake3 = ?1",
                params![blake3],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_default();
        let mut stmt = conn.prepare(
            "SELECT m.json
             FROM artifacts a JOIN manifests m ON a.manifest_id = m.manifest_id
             WHERE a.blake3 = ?1 OR (?2 <> '' AND a.sha256 = ?2)",
        )?;
        let jsons: Vec<String> = stmt
            .query_map(params![blake3, sha256], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        if jsons.is_empty() {
            return Ok((false, false, false));
        }
        let mut gated = false;
        let mut any_auto = false;
        for j in &jsons {
            if let Ok(m) = Manifest::from_json(j.as_bytes()) {
                if m.is_gated() {
                    gated = true;
                }
                if m.auto_shareable(include_gated) {
                    any_auto = true;
                }
            }
        }
        Ok((true, gated, any_auto))
    }

    /// Browse metadata for a cached blob from its containing manifest(s):
    /// `(model_name, license_spdx, quant)`. `quant` is the model's quantization
    /// variant (e.g. `Q4_K_M`), parsed from the manifest. Matches artifacts by
    /// blake3 OR sha256 (HF-synth manifests are sha256-only).
    pub fn blob_catalog_meta(
        &self,
        blake3: &str,
        sha256: &str,
    ) -> Result<Option<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT m.model_name, m.license_spdx, m.json
             FROM artifacts a JOIN manifests m ON a.manifest_id = m.manifest_id
             WHERE a.blake3 = ?1 OR (?2 <> '' AND a.sha256 = ?2)",
        )?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map(params![blake3, sha256], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        if rows.is_empty() {
            return Ok(None);
        }
        let name = rows
            .iter()
            .map(|r| r.0.clone())
            .find(|s| !s.is_empty())
            .unwrap_or_default();
        let license = rows
            .iter()
            .map(|r| r.1.clone())
            .find(|s| !s.is_empty())
            .unwrap_or_default();
        let quant = rows
            .iter()
            .find_map(|r| {
                Manifest::from_json(r.2.as_bytes())
                    .ok()
                    .and_then(|m| m.model.quantization)
                    .filter(|q| !q.is_empty())
            })
            .unwrap_or_default();
        Ok(Some((name, license, quant)))
    }

    /// The user's explicit per-model share choice, if any: `Some(true)` = force
    /// share, `Some(false)` = stop sharing, `None` = use the default.
    pub fn share_override(&self, blake3: &str) -> Result<Option<bool>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT shared FROM share_overrides WHERE blake3 = ?1",
                params![blake3],
                |r| {
                    let v: i64 = r.get(0)?;
                    Ok(v != 0)
                },
            )
            .optional()?)
    }

    /// Record an explicit per-model share choice (overrides the default).
    pub fn set_share_override(&self, blake3: &str, sha256: &str, shared: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO share_overrides (blake3, sha256, shared, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![blake3, sha256, shared as i64, crate::util::now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn upsert_cache_blob(&self, meta: &BlobMeta, state: &str) -> Result<()> {
        let conn = self.lock();
        // Keyed on blake3. On re-upsert, never clobber a known sha256/size with an
        // empty/zero one: a blake3-only manifest (or a bare Content-ID add) can
        // re-touch a blob whose sha256 we already learned, and that sha256 is now
        // load-bearing for `is_blob_shareable`'s sha256 match.
        conn.execute(
            "INSERT INTO cache_blobs (blake3, sha256, size_bytes, state, committed_at)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(blake3) DO UPDATE SET
                sha256 = CASE WHEN excluded.sha256 <> '' THEN excluded.sha256
                              ELSE cache_blobs.sha256 END,
                size_bytes = CASE WHEN excluded.size_bytes > 0 THEN excluded.size_bytes
                                  ELSE cache_blobs.size_bytes END,
                state = excluded.state,
                committed_at = excluded.committed_at",
            params![
                meta.blake3,
                meta.sha256,
                meta.size_bytes as i64,
                state,
                meta.committed_at,
            ],
        )?;
        Ok(())
    }

    /// Resolve a known blake3 for a sha256, if we've cached that content before
    pub fn blake3_for_sha256(&self, sha256: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let v: Option<String> = conn
            .query_row(
                "SELECT blake3 FROM cache_blobs WHERE sha256 = ?1 LIMIT 1",
                params![sha256],
                |row| row.get(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Whether any cached blob has this sha256 (LAN seeder check by HF oid).
    pub fn has_blob_with_sha256(&self, sha256: &str) -> Result<bool> {
        Ok(self.blake3_for_sha256(sha256)?.is_some())
    }

    pub fn has_cache_blob(&self, blake3: &str) -> Result<bool> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(1) FROM cache_blobs WHERE blake3 = ?1",
            params![blake3],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn list_cache_blobs(&self) -> Result<Vec<CacheBlobRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT blake3, sha256, size_bytes, state, committed_at FROM cache_blobs ORDER BY committed_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(CacheBlobRow {
                blake3: row.get(0)?,
                sha256: row.get(1)?,
                size_bytes: row.get::<_, i64>(2)? as u64,
                state: row.get(3)?,
                committed_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn delete_cache_blob(&self, blake3: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM cache_blobs WHERE blake3 = ?1", params![blake3])?;
        // Drop any share override too, so a later re-download starts from the
        // default (e.g. a gated model goes back to private rather than silently
        // re-sharing because of a stale opt-in).
        conn.execute(
            "DELETE FROM share_overrides WHERE blake3 = ?1",
            params![blake3],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_install(
        &self,
        manifest_id: &str,
        artifact_path: &str,
        dest_path: &str,
        blake3: &str,
        link_kind: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO install_views (manifest_id, artifact_path, dest_path, blake3, link_kind, created_at)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![manifest_id, artifact_path, dest_path, blake3, link_kind, crate::util::now_rfc3339()],
        )?;
        Ok(())
    }

    /// Remove install-view rows pointing at a destination path (reconcile).
    pub fn delete_install_by_dest(&self, dest_path: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM install_views WHERE dest_path = ?1",
            params![dest_path],
        )?;
        Ok(())
    }

    pub fn list_installs(&self) -> Result<Vec<InstallRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT manifest_id, artifact_path, dest_path, blake3, link_kind, created_at
             FROM install_views ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(InstallRow {
                manifest_id: row.get(0)?,
                artifact_path: row.get(1)?,
                dest_path: row.get(2)?,
                blake3: row.get(3)?,
                link_kind: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn upsert_download(
        &self,
        download_id: &str,
        manifest_id: &str,
        artifact_path: &str,
        state: &str,
        bytes_total: u64,
    ) -> Result<()> {
        let now = crate::util::now_rfc3339();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO downloads (download_id, manifest_id, artifact_path, state, bytes_total, bytes_done, started_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,0,?6,?6)
             ON CONFLICT(download_id) DO UPDATE SET state=?4, bytes_total=?5, updated_at=?6",
            params![download_id, manifest_id, artifact_path, state, bytes_total as i64, now],
        )?;
        Ok(())
    }

    pub fn update_download_progress(
        &self,
        download_id: &str,
        bytes_done: u64,
        source_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE downloads SET bytes_done=?2, source_id=?3, updated_at=?4 WHERE download_id=?1",
            params![
                download_id,
                bytes_done as i64,
                source_id,
                crate::util::now_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn set_download_state(&self, download_id: &str, state: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE downloads SET state=?2, updated_at=?3 WHERE download_id=?1",
            params![download_id, state, crate::util::now_rfc3339()],
        )?;
        Ok(())
    }

    /// Drop a download row entirely — used by a user Stop, which discards the
    /// partial transfer so the next attempt starts clean (no `paused` row to
    /// resume from). No-op if the row doesn't exist.
    pub fn delete_download(&self, download_id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM downloads WHERE download_id=?1",
            params![download_id],
        )?;
        Ok(())
    }

    pub fn list_downloads(&self) -> Result<Vec<DownloadRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT download_id, manifest_id, artifact_path, state, bytes_total, bytes_done, source_id, started_at, updated_at
             FROM downloads ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DownloadRow {
                download_id: row.get(0)?,
                manifest_id: row.get(1)?,
                artifact_path: row.get(2)?,
                state: row.get(3)?,
                bytes_total: row.get::<_, i64>(4)? as u64,
                bytes_done: row.get::<_, i64>(5)? as u64,
                source_id: row.get(6)?,
                started_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn get_source_health(&self, source_id: &str) -> Result<SourceHealth> {
        let conn = self.lock();
        let h = conn
            .query_row(
                "SELECT source_id, success_count, failure_count, integrity_failures, last_latency_ms, banned
                 FROM source_health WHERE source_id = ?1",
                params![source_id],
                |row| {
                    Ok(SourceHealth {
                        source_id: row.get(0)?,
                        success_count: row.get(1)?,
                        failure_count: row.get(2)?,
                        integrity_failures: row.get(3)?,
                        last_latency_ms: row.get(4)?,
                        banned: row.get::<_, i64>(5)? != 0,
                    })
                },
            )
            .optional()?;
        Ok(h.unwrap_or_else(|| SourceHealth {
            source_id: source_id.to_string(),
            ..Default::default()
        }))
    }

    pub fn record_source_result(
        &self,
        source_id: &str,
        success: bool,
        integrity_failure: bool,
        latency_ms: Option<i64>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO source_health (source_id, success_count, failure_count, integrity_failures, last_latency_ms, banned, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(source_id) DO UPDATE SET
               success_count = success_count + ?2,
               failure_count = failure_count + ?3,
               integrity_failures = integrity_failures + ?4,
               last_latency_ms = COALESCE(?5, last_latency_ms),
               banned = banned | ?6,
               updated_at = ?7",
            params![
                source_id,
                success as i64,
                (!success) as i64,
                integrity_failure as i64,
                latency_ms,
                integrity_failure as i64, // an integrity failure bans the source
                crate::util::now_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_source_health(&self) -> Result<Vec<SourceHealth>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT source_id, success_count, failure_count, integrity_failures, last_latency_ms, banned
             FROM source_health ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SourceHealth {
                source_id: row.get(0)?,
                success_count: row.get(1)?,
                failure_count: row.get(2)?,
                integrity_failures: row.get(3)?,
                last_latency_ms: row.get(4)?,
                banned: row.get::<_, i64>(5)? != 0,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn record_policy_event(
        &self,
        manifest_id: Option<&str>,
        decision: &str,
        reason: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO policy_events (manifest_id, decision, reason, created_at) VALUES (?1,?2,?3,?4)",
            params![manifest_id, decision, reason, crate::util::now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn record_quarantine(
        &self,
        download_id: &str,
        artifact_path: &str,
        source_id: Option<&str>,
        reason: &str,
        path: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO quarantine_records (download_id, artifact_path, source_id, reason, path, created_at)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![download_id, artifact_path, source_id, reason, path, crate::util::now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn list_quarantine(&self) -> Result<Vec<QuarantineRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT download_id, artifact_path, source_id, reason, path, created_at
             FROM quarantine_records ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(QuarantineRow {
                download_id: row.get(0)?,
                artifact_path: row.get(1)?,
                source_id: row.get(2)?,
                reason: row.get(3)?,
                path: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests_support::sample_manifest;
    use crate::sign::KeyPair;

    #[test]
    fn manifest_insert_and_fetch() {
        let db = Db::open_in_memory().unwrap();
        let mut m = sample_manifest();
        let kp = KeyPair::generate();
        kp.sign_manifest(&mut m).unwrap();
        let report = crate::sign::verify_manifest(&m).unwrap();

        db.insert_manifest(&m, &report).unwrap();
        let got = db.get_manifest(&m.manifest_id).unwrap().unwrap();
        assert_eq!(got, m);

        let summaries = db.list_manifests().unwrap();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].signed);
    }

    #[test]
    fn source_health_bans_on_integrity_failure() {
        let db = Db::open_in_memory().unwrap();
        db.record_source_result("src1", false, true, None).unwrap();
        let h = db.get_source_health("src1").unwrap();
        assert!(h.banned);
        assert_eq!(h.integrity_failures, 1);

        db.record_source_result("src2", true, false, Some(42))
            .unwrap();
        let h2 = db.get_source_health("src2").unwrap();
        assert!(!h2.banned);
        assert_eq!(h2.success_count, 1);
        assert_eq!(h2.last_latency_ms, Some(42));
    }

    #[test]
    fn blob_shareable_default_public() {
        use crate::manifest::{AuthPolicy, Source};
        let db = Db::open_in_memory().unwrap();
        let kp = KeyPair::generate();

        // Any non-gated model is shared by DEFAULT (open mesh), regardless of its
        // license class — even a "download-only"-licensed one.
        let mut openm = sample_manifest();
        openm.license.redistribution = crate::manifest::RedistributionClass::PublicDownloadOnly;
        let open_b3 = openm.artifacts[0].hashes.blake3.clone();
        let open_sha = openm.artifacts[0].hashes.sha256.clone();
        kp.sign_manifest(&mut openm).unwrap();
        db.insert_manifest(&openm, &crate::sign::verify_manifest(&openm).unwrap())
            .unwrap();
        assert!(db.is_blob_shareable(&open_b3).unwrap());

        // The user can opt a model OUT.
        db.set_share_override(&open_b3, &open_sha, false).unwrap();
        assert!(!db.is_blob_shareable(&open_b3).unwrap());
        db.set_share_override(&open_b3, &open_sha, true).unwrap();
        assert!(db.is_blob_shareable(&open_b3).unwrap());

        // A gated (access.gated) HF model is NOT auto-shared by default — only
        // openly-licensed public models are. The operator can opt gated content
        // in globally (`set_share_gated`) or per-model (override).
        let mut gated = sample_manifest();
        gated.manifest_id = "mdl_gated".into();
        gated.access.gated = true;
        gated.artifacts[0].path = "gated.gguf".into();
        gated.artifacts[0].hashes.blake3 = "ab".repeat(32);
        let gated_b3 = gated.artifacts[0].hashes.blake3.clone();
        let gated_sha = gated.artifacts[0].hashes.sha256.clone();
        kp.sign_manifest(&mut gated).unwrap();
        db.insert_manifest(&gated, &crate::sign::verify_manifest(&gated).unwrap())
            .unwrap();
        assert!(!db.is_blob_shareable(&gated_b3).unwrap());
        // Global "also share gated/licensed" opt-in makes it auto-share…
        db.set_share_gated(true);
        assert!(db.is_blob_shareable(&gated_b3).unwrap());
        db.set_share_gated(false);
        assert!(!db.is_blob_shareable(&gated_b3).unwrap());
        // …or a per-model override force-shares it regardless of the global flag.
        db.set_share_override(&gated_b3, &gated_sha, true).unwrap();
        assert!(db.is_blob_shareable(&gated_b3).unwrap());

        // A token-walled HF source is also off by default (publicly sourced but
        // gated), and follows the same global opt-in.
        let mut tok = sample_manifest();
        tok.manifest_id = "mdl_token".into();
        tok.artifacts[0].path = "tok.gguf".into();
        tok.artifacts[0].hashes.blake3 = "cd".repeat(32);
        tok.artifacts[0].sources = vec![Source::Huggingface {
            repo_id: "meta-llama/x".into(),
            revision: "main".into(),
            path: "tok.gguf".into(),
            auth: AuthPolicy::Token,
        }];
        let tok_b3 = tok.artifacts[0].hashes.blake3.clone();
        kp.sign_manifest(&mut tok).unwrap();
        db.insert_manifest(&tok, &crate::sign::verify_manifest(&tok).unwrap())
            .unwrap();
        assert!(!db.is_blob_shareable(&tok_b3).unwrap());
        db.set_share_gated(true);
        assert!(db.is_blob_shareable(&tok_b3).unwrap());
        db.set_share_gated(false);

        // Unknown blob (no containing manifest) => not shared.
        assert!(!db.is_blob_shareable(&"ee".repeat(32)).unwrap());

        // sha256-robust match: a manifest whose artifact has an EMPTY blake3
        // (HF-synth, sha256-only) is shared once its blob is cached.
        let mut sha_only = sample_manifest();
        sha_only.manifest_id = "mdl_sha_only".into();
        sha_only.artifacts[0].path = "shaonly.gguf".into();
        sha_only.artifacts[0].hashes.blake3 = String::new();
        sha_only.artifacts[0].hashes.sha256 = "12".repeat(32);
        let real_b3 = "34".repeat(32);
        let real_sha = sha_only.artifacts[0].hashes.sha256.clone();
        kp.sign_manifest(&mut sha_only).unwrap();
        db.insert_manifest(&sha_only, &crate::sign::verify_manifest(&sha_only).unwrap())
            .unwrap();
        assert!(!db.is_blob_shareable(&real_b3).unwrap());
        db.upsert_cache_blob(
            &BlobMeta {
                blake3: real_b3.clone(),
                sha256: real_sha.clone(),
                size_bytes: 1,
                committed_at: crate::util::now_rfc3339(),
            },
            "ready",
        )
        .unwrap();
        assert!(db.is_blob_shareable(&real_b3).unwrap());

        // A privately-imported file (publisher "local", no public source) is NOT
        // shared by default — only after the user explicitly opts it in.
        let mut localm = sample_manifest();
        localm.manifest_id = "mdl_local_x".into();
        localm.publisher.id = "local".into();
        localm.artifacts[0].path = "local.gguf".into();
        localm.artifacts[0].hashes.blake3 = "fa".repeat(32);
        localm.artifacts[0].sources = vec![];
        let local_b3 = localm.artifacts[0].hashes.blake3.clone();
        let local_sha = localm.artifacts[0].hashes.sha256.clone();
        kp.sign_manifest(&mut localm).unwrap();
        db.insert_manifest(&localm, &crate::sign::verify_manifest(&localm).unwrap())
            .unwrap();
        assert!(!db.is_blob_shareable(&local_b3).unwrap());
        db.set_share_override(&local_b3, &local_sha, true).unwrap();
        assert!(db.is_blob_shareable(&local_b3).unwrap());
        // Deleting the blob clears the override (no stale re-share on re-download).
        db.delete_cache_blob(&local_b3).unwrap();
        assert!(db.share_override(&local_b3).unwrap().is_none());
    }

    #[test]
    fn cache_blob_lifecycle() {
        let db = Db::open_in_memory().unwrap();
        let meta = BlobMeta {
            blake3: "aa".repeat(32),
            sha256: "bb".repeat(32),
            size_bytes: 100,
            committed_at: crate::util::now_rfc3339(),
        };
        db.upsert_cache_blob(&meta, "ready").unwrap();
        assert!(db.has_cache_blob(&meta.blake3).unwrap());
        assert_eq!(db.list_cache_blobs().unwrap().len(), 1);
        db.delete_cache_blob(&meta.blake3).unwrap();
        assert!(!db.has_cache_blob(&meta.blake3).unwrap());
    }
}
