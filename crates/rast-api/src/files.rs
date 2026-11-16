//! File Station DTOs (`/api/files`).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A file or directory entry as exposed by the File Station API.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct EntryDto {
    pub name: String,
    pub path: String,
    #[ts(type = "\"file\" | \"dir\"")]
    pub kind: String,
    #[ts(type = "number")]
    pub size: i64,
    #[ts(type = "number")]
    pub modified_at: i64,
    pub mime: String,
    pub readable: bool,
    pub writable: bool,
    pub trashed: bool,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BreadcrumbDto {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct QuotaDto {
    #[ts(type = "number")]
    pub quota_bytes: i64,
    #[ts(type = "number")]
    pub used_bytes: i64,
    #[ts(type = "number")]
    pub free_bytes: i64,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ListingDto {
    pub path: String,
    pub breadcrumbs: Vec<BreadcrumbDto>,
    pub items: Vec<EntryDto>,
    #[ts(type = "number")]
    pub page: i64,
    #[ts(type = "number")]
    pub per_page: i64,
    #[ts(type = "number")]
    pub total: i64,
    pub quota: QuotaDto,
}
