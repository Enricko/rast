//! SQLite access for installed packages, consumers, approved sources,
//! GitHub inspections and package job history.

use serde::Serialize;
use sqlx::{Row, SqlitePool};

use super::manifest::{self, Manifest};
use crate::{
    db,
    error::{AppError, AppResult},
};

#[derive(Debug, Clone)]
pub struct PackageRow {
    pub id: String,
    pub source: String,
    pub source_repo: Option<String>,
    pub publisher_id: String,
    pub publisher_name: String,
    pub name: String,
    pub default_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VersionRow {
    pub package_id: String,
    pub version: String,
    pub revision: u32,
    pub target: String,
    pub manifest: Manifest,
    pub artifact_url: String,
    pub artifact_sha256: String,
    pub release_tag: Option<String>,
    pub install_path: String,
    pub size_bytes: i64,
    pub check_output: String,
    pub installed_by: String,
    pub installed_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumerRow {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub package_id: String,
    /// `None` = follows the package default.
    pub version: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

pub async fn packages(pool: &SqlitePool) -> AppResult<Vec<PackageRow>> {
    let rows = sqlx::query(
        "SELECT id, source, source_repo, publisher_id, publisher_name, name, default_version
         FROM packages ORDER BY name COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(package_from_row).collect())
}

pub async fn package(pool: &SqlitePool, id: &str) -> AppResult<Option<PackageRow>> {
    let row = sqlx::query(
        "SELECT id, source, source_repo, publisher_id, publisher_name, name, default_version
         FROM packages WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(package_from_row))
}

fn package_from_row(row: &sqlx::sqlite::SqliteRow) -> PackageRow {
    PackageRow {
        id: row.get("id"),
        source: row.get("source"),
        source_repo: row.get("source_repo"),
        publisher_id: row.get("publisher_id"),
        publisher_name: row.get("publisher_name"),
        name: row.get("name"),
        default_version: row.get("default_version"),
    }
}

pub async fn versions(pool: &SqlitePool, package_id: Option<&str>) -> AppResult<Vec<VersionRow>> {
    let rows = sqlx::query(
        "SELECT package_id, version, revision, target, manifest_json, artifact_url, artifact_sha256,
                release_tag, install_path, size_bytes, check_output, installed_by, installed_at
         FROM package_versions WHERE (? IS NULL OR package_id = ?)",
    )
    .bind(package_id)
    .bind(package_id)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for row in rows {
        let json: String = row.get("manifest_json");
        // Stored manifests were validated at install time; skip a row only if
        // it was corrupted outside RAST.
        let Ok(manifest) = manifest::parse(json.as_bytes()) else {
            tracing::warn!(package = %row.get::<String, _>("package_id"), "stored package manifest is invalid");
            continue;
        };
        out.push(VersionRow {
            package_id: row.get("package_id"),
            version: row.get("version"),
            revision: row.get::<i64, _>("revision") as u32,
            target: row.get("target"),
            manifest,
            artifact_url: row.get("artifact_url"),
            artifact_sha256: row.get("artifact_sha256"),
            release_tag: row.get("release_tag"),
            install_path: row.get("install_path"),
            size_bytes: row.get("size_bytes"),
            check_output: row.get("check_output"),
            installed_by: row.get("installed_by"),
            installed_at: row.get("installed_at"),
        });
    }
    out.sort_by(|a, b| {
        a.package_id
            .cmp(&b.package_id)
            .then(super::version::compare(&b.version, &a.version))
    });
    Ok(out)
}

pub struct NewVersion<'a> {
    pub manifest: &'a Manifest,
    pub source: &'a str,
    pub source_repo: Option<&'a str>,
    pub target: &'a str,
    pub artifact_url: &'a str,
    pub artifact_sha256: &'a str,
    pub release_tag: Option<&'a str>,
    pub install_path: &'a str,
    pub size_bytes: u64,
    pub check_output: &'a str,
    pub installed_by: &'a str,
}

/// Whether a package row belongs to this source: the same kind of source,
/// the same GitHub repository (case-insensitive) and the same publisher.
pub fn same_owner(
    row: &PackageRow,
    source: &str,
    source_repo: Option<&str>,
    publisher_id: &str,
) -> bool {
    row.source == source
        && match (row.source_repo.as_deref(), source_repo) {
            (None, None) => true,
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        }
        && row.publisher_id == publisher_id
}

/// Record an installed version (and its package row) in one transaction.
/// Refuses (and changes nothing) when the package id belongs to another
/// source or publisher.
pub async fn record_install(pool: &SqlitePool, v: NewVersion<'_>) -> AppResult<()> {
    let now = db::now_ms();
    let manifest_json = serde_json::to_string(v.manifest).unwrap_or_default();
    let mut tx = pool.begin().await?;
    let owner = sqlx::query("SELECT source, source_repo, publisher_id FROM packages WHERE id = ?")
        .bind(&v.manifest.id)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(owner) = owner {
        let row = PackageRow {
            id: v.manifest.id.clone(),
            source: owner.get("source"),
            source_repo: owner.get("source_repo"),
            publisher_id: owner.get("publisher_id"),
            publisher_name: String::new(),
            name: String::new(),
            default_version: None,
        };
        if !same_owner(&row, v.source, v.source_repo, &v.manifest.publisher.id) {
            return Err(AppError::conflict(format!(
                "Package id {:?} belongs to another source.",
                v.manifest.id
            )));
        }
    }
    sqlx::query(
        "INSERT INTO packages (id, type, name, source, source_repo, publisher_id, publisher_name,
                               default_version, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, publisher_name = excluded.publisher_name,
                                       updated_at = excluded.updated_at",
    )
    .bind(&v.manifest.id)
    .bind(&v.manifest.package_type)
    .bind(&v.manifest.name)
    .bind(v.source)
    .bind(v.source_repo)
    .bind(&v.manifest.publisher.id)
    .bind(&v.manifest.publisher.name)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO package_versions (package_id, version, revision, target, manifest_json, artifact_url,
                                       artifact_sha256, release_tag, install_path, size_bytes,
                                       check_output, installed_by, installed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(package_id, version) DO UPDATE SET
            revision = excluded.revision, target = excluded.target, manifest_json = excluded.manifest_json,
            artifact_url = excluded.artifact_url, artifact_sha256 = excluded.artifact_sha256,
            release_tag = excluded.release_tag, install_path = excluded.install_path,
            size_bytes = excluded.size_bytes, check_output = excluded.check_output,
            installed_by = excluded.installed_by, installed_at = excluded.installed_at",
    )
    .bind(&v.manifest.id)
    .bind(&v.manifest.version)
    .bind(v.manifest.revision as i64)
    .bind(v.target)
    .bind(manifest_json)
    .bind(v.artifact_url)
    .bind(v.artifact_sha256)
    .bind(v.release_tag)
    .bind(v.install_path)
    .bind(v.size_bytes as i64)
    .bind(v.check_output)
    .bind(v.installed_by)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_default(pool: &SqlitePool, id: &str, version: Option<&str>) -> AppResult<()> {
    sqlx::query("UPDATE packages SET default_version = ?, updated_at = ? WHERE id = ?")
        .bind(version)
        .bind(db::now_ms())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete one version; drops the package row when it was the last one.
pub async fn delete_version(pool: &SqlitePool, id: &str, version: &str) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM package_versions WHERE package_id = ? AND version = ?")
        .bind(id)
        .bind(version)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE packages SET default_version = NULL WHERE id = ? AND default_version = ?")
        .bind(id)
        .bind(version)
        .execute(&mut *tx)
        .await?;
    let left: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM package_versions WHERE package_id = ?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let removed_package = left == 0;
    if removed_package {
        sqlx::query("DELETE FROM packages WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(removed_package)
}

pub async fn consumers(pool: &SqlitePool) -> AppResult<Vec<ConsumerRow>> {
    let rows = sqlx::query(
        "SELECT id, name, description, package_id, version, created_by, created_at, updated_at
         FROM package_consumers ORDER BY name COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| ConsumerRow {
            id: row.get("id"),
            name: row.get("name"),
            description: row.get("description"),
            package_id: row.get("package_id"),
            version: row.get("version"),
            created_by: row.get("created_by"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
        .collect())
}

pub async fn consumer(pool: &SqlitePool, id: i64) -> AppResult<Option<ConsumerRow>> {
    Ok(consumers(pool).await?.into_iter().find(|c| c.id == id))
}

pub async fn approved_source(
    pool: &SqlitePool,
    repo: &str,
) -> AppResult<Option<(String, String, i64)>> {
    let row = sqlx::query(
        "SELECT publisher_id, approved_by, approved_at FROM package_sources WHERE repo = ?",
    )
    .bind(repo)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        (
            r.get("publisher_id"),
            r.get("approved_by"),
            r.get("approved_at"),
        )
    }))
}

pub async fn approve_source(
    pool: &SqlitePool,
    repo: &str,
    publisher_id: &str,
    by: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO package_sources (repo, publisher_id, approved_by, approved_at) VALUES (?, ?, ?, ?)
         ON CONFLICT(repo) DO NOTHING",
    )
    .bind(repo)
    .bind(publisher_id)
    .bind(by)
    .bind(db::now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn link_job(
    pool: &SqlitePool,
    job_id: &str,
    package_id: &str,
    version: &str,
    action: &str,
) -> AppResult<()> {
    sqlx::query("INSERT INTO package_jobs (job_id, package_id, version, action, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(job_id)
        .bind(package_id)
        .bind(version)
        .bind(action)
        .bind(db::now_ms())
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobHistory {
    pub job_id: String,
    pub package_id: String,
    pub version: String,
    pub action: String,
    pub status: String,
    pub progress: i64,
    pub total: i64,
    pub message: String,
    pub error: Option<String>,
    pub username: String,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

pub async fn job_history(
    pool: &SqlitePool,
    package_id: Option<&str>,
    limit: i64,
) -> AppResult<Vec<JobHistory>> {
    let rows = sqlx::query(
        "SELECT pj.job_id, pj.package_id, pj.version, pj.action, j.status, j.progress, j.total,
                j.message, j.error, j.username, j.created_at, j.finished_at
         FROM package_jobs pj JOIN jobs j ON j.id = pj.job_id
         WHERE (? IS NULL OR pj.package_id = ?)
         ORDER BY j.created_at DESC LIMIT ?",
    )
    .bind(package_id)
    .bind(package_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| JobHistory {
            job_id: row.get("job_id"),
            package_id: row.get("package_id"),
            version: row.get("version"),
            action: row.get("action"),
            status: row.get("status"),
            progress: row.get("progress"),
            total: row.get("total"),
            message: row.get("message"),
            error: row.get("error"),
            username: row.get("username"),
            created_at: row.get("created_at"),
            finished_at: row.get("finished_at"),
        })
        .collect())
}

/// Running or queued package jobs (one per package at most).
pub async fn active_jobs(pool: &SqlitePool) -> AppResult<Vec<JobHistory>> {
    let mut all = job_history(pool, None, 200).await?;
    all.retain(|j| j.status == "pending" || j.status == "running");
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(publisher: &str) -> Manifest {
        let mut v = crate::packages::manifest::tests::sample();
        v["id"] = "demo".into();
        v["publisher"]["id"] = publisher.into();
        serde_json::from_value(v).unwrap()
    }

    fn new_version<'a>(m: &'a Manifest, source: &'a str, repo: Option<&'a str>) -> NewVersion<'a> {
        NewVersion {
            manifest: m,
            source,
            source_repo: repo,
            target: "linux-x86_64",
            artifact_url: "https://example.test/a.tar.gz",
            artifact_sha256: "00",
            release_tag: None,
            install_path: "/nonexistent",
            size_bytes: 1,
            check_output: "",
            installed_by: "admin",
        }
    }

    #[tokio::test]
    async fn a_package_id_stays_with_its_source_and_publisher() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::connect(&dir.path().join("p.db"), 2).await.unwrap();
        let github = manifest("someone");
        record_install(&pool, new_version(&github, "github", Some("Someone/Demo")))
            .await
            .unwrap();
        // Same repository (any case) and publisher: a reinstall is fine.
        record_install(&pool, new_version(&github, "github", Some("someone/demo")))
            .await
            .unwrap();
        // Another repository, the official catalog, or another publisher of
        // the same repository cannot take the id over.
        for (m, source, repo) in [
            (manifest("someone"), "github", Some("other/demo")),
            (manifest("someone"), "official", None),
            (manifest("intruder"), "github", Some("someone/demo")),
        ] {
            let err = record_install(&pool, new_version(&m, source, repo))
                .await
                .unwrap_err();
            assert!(
                err.public_message().contains("another source"),
                "{source} {repo:?}"
            );
        }
        let row = package(&pool, "demo").await.unwrap().unwrap();
        assert_eq!(
            (
                row.source.as_str(),
                row.source_repo.as_deref(),
                row.publisher_id.as_str()
            ),
            ("github", Some("Someone/Demo"), "someone")
        );
    }
}
