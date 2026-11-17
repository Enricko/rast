//! One HOME per user (ADR 0005): File Station requests of a one-HOME account
//! are answered by that account's own file agent ([`agent`]), which runs as
//! `rast-u<id>` with its root at the HOME. The API layer
//! (`crate::api::homefs`) keeps doing every RAST check and forwards.
//!
//! A user is in one-HOME mode when the HOME directory is owned by the uid of
//! their Linux identity; a HOME owned by the `rast` service is a legacy home,
//! served in-process as before. No database column records the mode.

use std::collections::BTreeMap;

use axum::http::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::fileagent::{SHARES_HEADER, SHARE_FREE_HEADER};
use crate::storage::safe_path::validate_component;
use crate::storage::shares::{rw_group_name, MAX_SHARE_NAME};

pub mod agent;
pub mod client;
pub mod fsat;
pub mod links;
pub mod watch;

/// Prefix of chunked upload ids that belong to a per-user agent (the root
/// agent uses `sys-`).
pub const UPLOAD_PREFIX: &str = "usr-";
/// Folder in HOME that holds the `~/Shared/<name>` links to shared folders.
/// They are for apps only: the agent never follows them (ADR 0005).
pub const SHARED_DIR: &str = "Shared";
/// Largest [`SHARES_HEADER`] value an agent accepts.
pub const MAX_SHARES_HEADER: usize = 64 * 1024;

/// One shared folder of a request, as the server's database records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareRoot {
    /// The share id, a UUID (it names the share's Linux groups).
    pub id: String,
    /// The display name.
    pub name: String,
    /// The container: a direct child of the shared root.
    pub container: String,
}

/// The shared folders that the paths of one request name, keyed by the
/// name in those paths (`/shared/<name>/…`). The server fills it in from its
/// database after its access checks and sends it in [`SHARES_HEADER`]
/// (hex-encoded JSON); the agent opens each container directly below the
/// shared root and never follows `~/Shared/<name>` (ADR 0005, section 2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShareRoots(BTreeMap<String, ShareRoot>);

impl ShareRoots {
    /// Record the share that `/shared/<path_name>` names.
    pub fn insert(&mut self, path_name: &str, root: ShareRoot) -> AppResult<()> {
        if !valid(path_name, &root) {
            return Err(AppError::internal("invalid shared folder record"));
        }
        self.0.insert(path_name.to_string(), root);
        Ok(())
    }

    /// The share that `/shared/<path_name>` names in this request.
    pub fn get(&self, path_name: &str) -> Option<&ShareRoot> {
        self.0.get(path_name)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every share, by the name in its paths.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &ShareRoot)> {
        self.0.iter()
    }

    /// The header value (`None` when the request names no share).
    pub fn header_value(&self) -> AppResult<Option<HeaderValue>> {
        if self.0.is_empty() {
            return Ok(None);
        }
        let json = serde_json::to_vec(&self.0)
            .map_err(|error| AppError::internal(format!("cannot encode shares: {error}")))?;
        let value = hex::encode(json);
        if value.len() > MAX_SHARES_HEADER {
            return Err(AppError::bad_request(
                "the request names too many shared folders",
            ));
        }
        HeaderValue::from_str(&value)
            .map(Some)
            .map_err(|_| AppError::internal("cannot encode shares"))
    }

    /// Put the header on an agent request (replacing any other value).
    pub fn apply(&self, headers: &mut HeaderMap) -> AppResult<()> {
        headers.remove(SHARES_HEADER);
        if let Some(value) = self.header_value()? {
            headers.insert(SHARES_HEADER, value);
        }
        Ok(())
    }

    /// The shares of a request, as the agent reads them: strictly, every
    /// field checked.
    pub fn from_headers(headers: &HeaderMap) -> AppResult<Self> {
        let mut values = headers.get_all(SHARES_HEADER).iter();
        let Some(value) = values.next() else {
            return Ok(Self::default());
        };
        let bad = || AppError::bad_request("invalid shared folder header");
        if values.next().is_some() || value.len() > MAX_SHARES_HEADER {
            return Err(bad());
        }
        let json = hex::decode(value.as_bytes()).map_err(|_| bad())?;
        let map: BTreeMap<String, ShareRoot> = serde_json::from_slice(&json).map_err(|_| bad())?;
        if map.iter().any(|(name, root)| !valid(name, root)) {
            return Err(bad());
        }
        Ok(Self(map))
    }
}

/// Most shared folders one [`SHARE_FREE_HEADER`] names.
pub const MAX_SHARE_FREE: usize = 1024;

/// The bytes still free below the size limits of shared folders, by share
/// id, for a restore from a recycle bin (a share without a limit is not
/// named). The server measures them and sends them in
/// [`SHARE_FREE_HEADER`] as `id=bytes` pairs separated by commas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShareFree(BTreeMap<String, u64>);

impl ShareFree {
    /// Record the free bytes of a share.
    pub fn insert(&mut self, share_id: &str, free: u64) -> AppResult<()> {
        if rw_group_name(share_id).is_none() || self.0.len() >= MAX_SHARE_FREE {
            return Err(AppError::internal("invalid shared folder limit"));
        }
        self.0.insert(share_id.to_string(), free);
        Ok(())
    }

    /// The free bytes of a share, `None` when it has no limit.
    pub fn get(&self, share_id: &str) -> Option<u64> {
        self.0.get(share_id).copied()
    }

    /// Put the header on an agent request (replacing any other value).
    pub fn apply(&self, headers: &mut HeaderMap) -> AppResult<()> {
        headers.remove(SHARE_FREE_HEADER);
        if self.0.is_empty() {
            return Ok(());
        }
        let value = self
            .0
            .iter()
            .map(|(id, free)| format!("{id}={free}"))
            .collect::<Vec<_>>()
            .join(",");
        let value = HeaderValue::from_str(&value)
            .map_err(|_| AppError::internal("cannot encode shared folder limits"))?;
        headers.insert(SHARE_FREE_HEADER, value);
        Ok(())
    }

    /// The limits of a request, as the agent reads them: strictly.
    pub fn from_headers(headers: &HeaderMap) -> AppResult<Self> {
        let mut values = headers.get_all(SHARE_FREE_HEADER).iter();
        let Some(value) = values.next() else {
            return Ok(Self::default());
        };
        let bad = || AppError::bad_request("invalid shared folder limit header");
        if values.next().is_some() || value.len() > MAX_SHARES_HEADER {
            return Err(bad());
        }
        let text = value.to_str().map_err(|_| bad())?;
        let mut free = Self::default();
        for pair in text.split(',') {
            let (id, bytes) = pair.split_once('=').ok_or_else(bad)?;
            if bytes.is_empty() || !bytes.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(bad());
            }
            let bytes: u64 = bytes.parse().map_err(|_| bad())?;
            if free.0.contains_key(id) {
                return Err(bad());
            }
            free.insert(id, bytes).map_err(|_| bad())?;
        }
        Ok(free)
    }
}

fn valid(path_name: &str, root: &ShareRoot) -> bool {
    validate_component(path_name).is_ok()
        && validate_component(&root.container).is_ok()
        && rw_group_name(&root.id).is_some()
        && !root.name.is_empty()
        && root.name.chars().count() <= MAX_SHARE_NAME
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "00000000-0000-4000-8000-000000000001";

    fn root(container: &str) -> ShareRoot {
        ShareRoot {
            id: ID.to_string(),
            name: "Team".to_string(),
            container: container.to_string(),
        }
    }

    #[test]
    fn shares_travel_in_one_strictly_checked_header() {
        let mut shares = ShareRoots::default();
        shares.insert("team", root("team-1")).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(SHARES_HEADER, HeaderValue::from_static("planted"));
        shares.apply(&mut headers).unwrap();
        assert_eq!(headers.get_all(SHARES_HEADER).iter().count(), 1);
        assert_eq!(ShareRoots::from_headers(&headers).unwrap(), shares);
        // No share: no header, and none left over.
        ShareRoots::default().apply(&mut headers).unwrap();
        assert!(headers.get(SHARES_HEADER).is_none());
        assert!(ShareRoots::from_headers(&headers).unwrap().is_empty());

        let encode = |value: serde_json::Value| {
            let mut headers = HeaderMap::new();
            headers.insert(
                SHARES_HEADER,
                HeaderValue::from_str(&hex::encode(value.to_string())).unwrap(),
            );
            ShareRoots::from_headers(&headers)
        };
        let good = serde_json::json!({"team": {"id": ID, "name": "Team", "container": "team-1"}});
        assert!(encode(good).is_ok());
        for bad in [
            serde_json::json!({"team": {"id": ID, "name": "Team", "container": ".."}}),
            serde_json::json!({"team": {"id": ID, "name": "Team", "container": "a/b"}}),
            serde_json::json!({"..": {"id": ID, "name": "Team", "container": "team-1"}}),
            serde_json::json!({"team": {"id": "x", "name": "Team", "container": "team-1"}}),
            serde_json::json!({"team": {"id": ID, "name": "", "container": "team-1"}}),
            serde_json::json!({"team": {"id": ID, "name": "Team", "container": "team-1", "extra": 1}}),
            serde_json::json!({"team": {"id": ID, "name": "Team"}}),
            serde_json::json!(["team"]),
        ] {
            assert!(encode(bad.clone()).is_err(), "{bad}");
        }
        let mut headers = HeaderMap::new();
        headers.insert(SHARES_HEADER, HeaderValue::from_static("zz"));
        assert!(ShareRoots::from_headers(&headers).is_err());
        let mut twice = HeaderMap::new();
        twice.append(SHARES_HEADER, HeaderValue::from_static("7b7d"));
        twice.append(SHARES_HEADER, HeaderValue::from_static("7b7d"));
        assert!(ShareRoots::from_headers(&twice).is_err());
        // The server never records an invalid share.
        assert!(ShareRoots::default().insert("team", root("../x")).is_err());
    }

    #[test]
    fn free_bytes_of_shared_folders_travel_in_one_strict_header() {
        const OTHER: &str = "00000000-0000-4000-8000-000000000002";
        let mut free = ShareFree::default();
        free.insert(ID, 10).unwrap();
        free.insert(OTHER, 0).unwrap();
        assert!(free.insert("x", 1).is_err());
        let mut headers = HeaderMap::new();
        headers.insert(SHARE_FREE_HEADER, HeaderValue::from_static("planted"));
        free.apply(&mut headers).unwrap();
        assert_eq!(headers.get_all(SHARE_FREE_HEADER).iter().count(), 1);
        let read = ShareFree::from_headers(&headers).unwrap();
        assert_eq!(read, free);
        assert_eq!(read.get(ID), Some(10));
        assert_eq!(read.get(OTHER), Some(0));
        assert_eq!(read.get("00000000-0000-4000-8000-000000000003"), None);
        ShareFree::default().apply(&mut headers).unwrap();
        assert!(headers.get(SHARE_FREE_HEADER).is_none());
        assert_eq!(
            ShareFree::from_headers(&headers).unwrap(),
            ShareFree::default()
        );
        for bad in [
            "",
            "x=1",
            ID,
            &format!("{ID}="),
            &format!("{ID}=-1"),
            &format!("{ID}=+1"),
            &format!("{ID}=1x"),
            &format!("{ID}=18446744073709551616"),
            &format!("{ID}=1,{ID}=2"),
            &format!("{ID}=1,"),
            &format!("{ID}=1;{OTHER}=2"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(SHARE_FREE_HEADER, HeaderValue::from_str(bad).unwrap());
            assert!(ShareFree::from_headers(&headers).is_err(), "{bad}");
        }
        let mut twice = HeaderMap::new();
        twice.append(
            SHARE_FREE_HEADER,
            HeaderValue::from_str(&format!("{ID}=1")).unwrap(),
        );
        twice.append(
            SHARE_FREE_HEADER,
            HeaderValue::from_str(&format!("{ID}=1")).unwrap(),
        );
        assert!(ShareFree::from_headers(&twice).is_err());
    }

    #[test]
    fn the_read_write_group_is_named_after_the_share_id() {
        assert_eq!(
            rw_group_name("0123abcd-4567-4ef0-8123-456789abcdef").as_deref(),
            Some("rast-s0123abcd45674ef0-rw")
        );
        assert_eq!(rw_group_name("not-a-uuid"), None);
    }
}
