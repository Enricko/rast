//! Groups and app access: `/api/groups`, `/api/users/{id}/access` and
//! `/api/access/me`. Super Admins change groups and rules; Admins may read
//! them; everyone may read their own access.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::access::{self, Choice, Rule};
use crate::api::{ApiError, ApiResult, admin};
use crate::db::{self, Group, GroupChange, RuleChanges, User};
use crate::{AppState, Role, auth};

const MAX_GROUP_NAME: usize = 48;
const MAX_DESCRIPTION: usize = 200;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GroupDto {
    id: i64,
    name: String,
    description: String,
    member_ids: Vec<i64>,
    apps: BTreeMap<String, Rule>,
    created_at: i64,
    updated_at: i64,
}

impl From<Group> for GroupDto {
    fn from(group: Group) -> Self {
        Self {
            id: group.id,
            name: group.name,
            description: group.description,
            member_ids: group.member_ids,
            apps: group.apps,
            created_at: group.created_at,
            updated_at: group.updated_at,
        }
    }
}

fn catalog() -> Value {
    access::APPS
        .iter()
        .map(|app| {
            json!({
                "id": app.id,
                "label": app.label,
                "defaultAllowed": app.default_allowed,
            })
        })
        .collect()
}

async fn super_admin(state: &AppState, headers: &HeaderMap) -> ApiResult<User> {
    let user = admin(state, headers).await?;
    if user.role != Role::SuperAdmin {
        return Err(ApiError::forbidden(
            "Only a Super Admin can change groups and app access.",
        ));
    }
    Ok(user)
}

fn group_gone() -> ApiError {
    ApiError::not_found_with("That group no longer exists.")
}

fn validate_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("Give the group a name."));
    }
    if name.chars().count() > MAX_GROUP_NAME || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request(format!(
            "Use up to {MAX_GROUP_NAME} characters for the group name, without line breaks."
        )));
    }
    Ok(name.to_owned())
}

fn validate_description(text: &str) -> ApiResult<String> {
    let text = text.trim();
    if text.chars().count() > MAX_DESCRIPTION || text.chars().any(char::is_control) {
        return Err(ApiError::bad_request(format!(
            "Use up to {MAX_DESCRIPTION} characters for the description, on one line."
        )));
    }
    Ok(text.to_owned())
}

fn rule_changes(apps: BTreeMap<String, Choice>) -> ApiResult<RuleChanges> {
    apps.into_iter()
        .map(|(id, choice)| match access::app(&id) {
            Some(app) => Ok((app.id.to_owned(), choice.rule())),
            None => Err(ApiError::bad_request(format!(
                "There is no app called {id}."
            ))),
        })
        .collect()
}

fn describe(changes: &RuleChanges) -> Vec<String> {
    changes
        .iter()
        .map(|(app, rule)| format!("{app} {}", rule.map_or("inherit", Rule::as_str)))
        .collect()
}

/// Sorted, without duplicates, and every id must exist.
fn checked_ids(mut ids: Vec<i64>, known: &BTreeSet<i64>, what: &str) -> ApiResult<Vec<i64>> {
    ids.sort_unstable();
    ids.dedup();
    if ids.iter().all(|id| known.contains(id)) {
        Ok(ids)
    } else {
        Err(ApiError::bad_request(format!(
            "One of those {what} no longer exists. Reload and try again."
        )))
    }
}

// --- your own access ------------------------------------------------------------------

pub(crate) async fn my_access(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let signed = auth::require(&state, &headers).await?;
    access_body(&state, &signed.user).await.map(Json)
}

async fn access_body(state: &AppState, user: &User) -> ApiResult<Value> {
    let apps = access::effective(&state.pool, user).await?;
    let groups: Vec<Value> = db::user_groups(&state.pool, user.id)
        .await?
        .into_iter()
        .map(|(id, name)| json!({ "id": id, "name": name }))
        .collect();
    let rules = db::user_rules(&state.pool, user.id).await?;
    Ok(json!({
        "groups": groups,
        "groupIds": groups.iter().filter_map(|g| g["id"].as_i64()).collect::<Vec<_>>(),
        "rules": rules,
        "apps": apps,
    }))
}

// --- groups ---------------------------------------------------------------------------

pub(crate) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    admin(&state, &headers).await?;
    let groups: Vec<GroupDto> = db::groups(&state.pool)
        .await?
        .into_iter()
        .map(GroupDto::from)
        .collect();
    Ok(Json(json!({ "groups": groups, "apps": catalog() })))
}

#[derive(Deserialize)]
pub(crate) struct CreateBody {
    name: String,
    #[serde(default)]
    description: String,
}

pub(crate) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let actor = super_admin(&state, &headers).await?;
    let name = validate_name(&body.name)?;
    let description = validate_description(&body.description)?;
    let Some(id) = db::insert_group(&state.pool, &name, &description).await? else {
        return Err(ApiError::conflict(format!(
            "There is already a group called {name}."
        )));
    };
    db::audit(&state.pool, Some(actor.id), "group.created", &name).await;
    let group = db::group_by_id(&state.pool, id)
        .await?
        .ok_or_else(group_gone)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "group": GroupDto::from(group) })),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateBody {
    name: Option<String>,
    description: Option<String>,
    member_ids: Option<Vec<i64>>,
    #[serde(default)]
    apps: BTreeMap<String, Choice>,
}

pub(crate) async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<UpdateBody>,
) -> ApiResult<Json<Value>> {
    let actor = super_admin(&state, &headers).await?;
    let group = db::group_by_id(&state.pool, id)
        .await?
        .ok_or_else(group_gone)?;
    let mut change = GroupChange {
        name: body.name.as_deref().map(validate_name).transpose()?,
        description: body
            .description
            .as_deref()
            .map(validate_description)
            .transpose()?,
        apps: rule_changes(body.apps)?,
        ..GroupChange::default()
    };
    if let Some(members) = body.member_ids {
        let known: BTreeSet<i64> = db::users(&state.pool).await?.iter().map(|u| u.id).collect();
        change.members = Some(checked_ids(members, &known, "accounts")?);
    }
    if !db::update_group(&state.pool, id, &change).await? {
        return Err(ApiError::conflict(format!(
            "There is already a group called {}.",
            change.name.unwrap_or_default()
        )));
    }
    let mut what = describe(&change.apps);
    if let Some(name) = &change.name
        && *name != group.name
    {
        what.push(format!("renamed to {name}"));
    }
    if change.description.is_some() {
        what.push("description".to_owned());
    }
    if let Some(members) = &change.members {
        what.push(format!("{} members", members.len()));
    }
    db::audit(
        &state.pool,
        Some(actor.id),
        "group.updated",
        &format!("{}: {}", group.name, what.join(", ")),
    )
    .await;
    let group = db::group_by_id(&state.pool, id)
        .await?
        .ok_or_else(group_gone)?;
    Ok(Json(json!({ "group": GroupDto::from(group) })))
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let actor = super_admin(&state, &headers).await?;
    let group = db::group_by_id(&state.pool, id)
        .await?
        .ok_or_else(group_gone)?;
    db::delete_group(&state.pool, id).await?;
    db::audit(
        &state.pool,
        Some(actor.id),
        "group.deleted",
        &format!("{} ({} members)", group.name, group.member_ids.len()),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

// --- one account's access -------------------------------------------------------------

async fn target(state: &AppState, id: i64) -> ApiResult<User> {
    db::user_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| ApiError::not_found_with("That account no longer exists."))
}

pub(crate) async fn user_access(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    admin(&state, &headers).await?;
    let user = target(&state, id).await?;
    access_body(&state, &user).await.map(Json)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessBody {
    group_ids: Option<Vec<i64>>,
    #[serde(default)]
    apps: BTreeMap<String, Choice>,
}

pub(crate) async fn update_user_access(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<AccessBody>,
) -> ApiResult<Json<Value>> {
    let actor = super_admin(&state, &headers).await?;
    let user = target(&state, id).await?;
    let apps = rule_changes(body.apps)?;
    let groups = match body.group_ids {
        Some(ids) => {
            let known: BTreeSet<i64> = db::groups(&state.pool)
                .await?
                .iter()
                .map(|g| g.id)
                .collect();
            Some(checked_ids(ids, &known, "groups")?)
        }
        None => None,
    };
    db::update_user_access(&state.pool, user.id, groups.as_deref(), &apps).await?;
    let mut what = describe(&apps);
    if let Some(groups) = &groups {
        what.push(format!("{} groups", groups.len()));
    }
    db::audit(
        &state.pool,
        Some(actor.id),
        "user.access_updated",
        &format!("{}: {}", user.username, what.join(", ")),
    )
    .await;
    access_body(&state, &user).await.map(Json)
}
