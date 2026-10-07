//! Deleting your own account (hosted, Clerk mode only).
//!
//! `GET /api/account/deletion` says what deleting the signed-in account
//! would do; `DELETE /api/account` does it. The app owns this flow,
//! rather than Clerk's own "Delete account" button (hidden in the
//! frontend), because two things have to be checked or done that Clerk
//! cannot know about:
//!
//! - **The last admin.** Deleting the only admin of an organization that
//!   still has other members would leave nobody able to manage it,
//!   pay for it, or delete it. That is refused, with the organizations
//!   named, until someone else is made an admin.
//! - **Organizations left empty.** Where the account is the only
//!   member, the organization is deleted too, and its data erased.
//!   Nobody could ever open it again otherwise.
//!
//! Then the person's own data is erased everywhere
//! (`gdpr::erase_user_data`) and the account is deleted at Clerk. The
//! `user.deleted` webhook runs the same erasure as a backstop.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::SignedInUser;
use crate::error::ApiError;
use crate::ratelimit::{PerHour, PerMinute};

/// What the person has to type to confirm.
pub const CONFIRM_PHRASE: &str = "delete my account";

#[derive(Debug)]
pub enum ClerkError {
    NotFound,
    Status(reqwest::StatusCode),
    Transport(String),
}

impl std::fmt::Display for ClerkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClerkError::NotFound => write!(f, "not found at Clerk"),
            ClerkError::Status(s) => write!(f, "Clerk answered {s}"),
            ClerkError::Transport(e) => write!(f, "could not reach Clerk: {e}"),
        }
    }
}

impl From<ClerkError> for ApiError {
    fn from(err: ClerkError) -> Self {
        tracing::error!(error = %err, "Clerk call failed during account deletion");
        ApiError::new(
            reqwest::StatusCode::BAD_GATEWAY,
            "Could not reach the sign-in service. Try again shortly; anything already done is not repeated.",
        )
    }
}

/// The few Clerk Backend API calls account deletion needs.
pub struct Clerk<'a> {
    state: &'a AppState,
}

impl<'a> Clerk<'a> {
    pub fn new(state: &'a AppState) -> Self {
        Clerk { state }
    }

    fn url(&self, path: &str) -> Result<reqwest::Url, ClerkError> {
        // The trailing slash is load-bearing: `join` treats the base's
        // last segment as a file and would drop the `/v1`.
        let base = &self.state.config.clerk_api_url;
        let base = if base.ends_with('/') {
            base.clone()
        } else {
            format!("{base}/")
        };
        reqwest::Url::parse(&base)
            .and_then(|b| b.join(path))
            .map_err(|e| ClerkError::Transport(e.to_string()))
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, ClerkError> {
        let response = request
            .bearer_auth(&self.state.config.clerk_secret_key)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| ClerkError::Transport(e.to_string()))?;
        match response.status() {
            s if s.is_success() => Ok(response.json().await.unwrap_or(Value::Null)),
            reqwest::StatusCode::NOT_FOUND => Err(ClerkError::NotFound),
            s => Err(ClerkError::Status(s)),
        }
    }

    async fn get(&self, path: &str) -> Result<Value, ClerkError> {
        let url = self.url(path)?;
        self.send(self.state.http.get(url)).await
    }

    async fn delete(&self, path: &str) -> Result<(), ClerkError> {
        let url = self.url(path)?;
        self.send(self.state.http.delete(url)).await.map(|_| ())
    }

    /// Every address on the account, lower-cased.
    pub async fn user_emails(&self, user_id: &str) -> Result<Vec<String>, ClerkError> {
        let user = self.get(&format!("users/{user_id}")).await?;
        Ok(user["email_addresses"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["email_address"].as_str())
            .map(str::to_lowercase)
            .collect())
    }

    /// `(org id, org name)` for every organization the user belongs to.
    async fn user_organizations(&self, user_id: &str) -> Result<Vec<(String, String)>, ClerkError> {
        let body = self
            .get(&format!(
                "users/{user_id}/organization_memberships?limit=100"
            ))
            .await?;
        Ok(body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let org = &m["organization"];
                Some((
                    org["id"].as_str()?.to_string(),
                    org["name"].as_str().unwrap_or("").to_string(),
                ))
            })
            .collect())
    }

    /// `(user id, is admin)` for every member of an organization.
    async fn members(&self, org_id: &str) -> Result<Vec<(String, bool)>, ClerkError> {
        // Seats top out at 20, well inside one page.
        let body = self
            .get(&format!("organizations/{org_id}/memberships?limit=100"))
            .await?;
        Ok(body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let user_id = m["public_user_data"]["user_id"].as_str()?.to_string();
                let role = m["role"].as_str().unwrap_or("");
                Some((user_id, matches!(role, "org:admin" | "admin")))
            })
            .collect())
    }

    pub async fn organization_member_count(&self, org_id: &str) -> Result<i64, ClerkError> {
        let body = self
            .get(&format!("organizations/{org_id}/memberships?limit=1"))
            .await?;
        Ok(body["total_count"].as_i64().unwrap_or(0))
    }

    pub async fn delete_organization(&self, org_id: &str) -> Result<(), ClerkError> {
        self.delete(&format!("organizations/{org_id}")).await
    }

    async fn delete_user(&self, user_id: &str) -> Result<(), ClerkError> {
        self.delete(&format!("users/{user_id}")).await
    }
}

struct Org {
    id: String,
    name: String,
}

impl Org {
    fn to_json(&self) -> Value {
        json!({ "id": self.id, "name": self.name })
    }
}

/// What deleting this account would do to each of its organizations.
struct Plan {
    /// The account is the only member: deleted with it.
    deleted: Vec<Org>,
    /// Other members stay, and so does an admin: the account just leaves.
    left: Vec<Org>,
    /// The account is the only admin and others remain: refused.
    blocking: Vec<Org>,
}

async fn plan(clerk: &Clerk<'_>, user_id: &str) -> Result<Plan, ClerkError> {
    let mut plan = Plan {
        deleted: Vec::new(),
        left: Vec::new(),
        blocking: Vec::new(),
    };
    for (id, name) in clerk.user_organizations(user_id).await? {
        let members = clerk.members(&id).await?;
        let org = Org { id, name };
        match classify(user_id, &members) {
            Outcome::Deleted => plan.deleted.push(org),
            Outcome::Left => plan.left.push(org),
            Outcome::Blocking => plan.blocking.push(org),
        }
    }
    Ok(plan)
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Deleted,
    Left,
    Blocking,
}

/// One organization's fate, from its `(user id, is admin)` members.
///
/// Refused only when this account is an admin and no other admin would
/// remain. A plain member can always leave: an organization that has
/// already lost its admins is not made worse by one more departure.
fn classify(user_id: &str, members: &[(String, bool)]) -> Outcome {
    let is_admin = members.iter().any(|(u, admin)| u == user_id && *admin);
    let others: Vec<&(String, bool)> = members.iter().filter(|(u, _)| u != user_id).collect();
    if others.is_empty() {
        Outcome::Deleted
    } else if is_admin && !others.iter().any(|(_, admin)| *admin) {
        Outcome::Blocking
    } else {
        Outcome::Left
    }
}

fn plan_json(plan: &Plan) -> Value {
    json!({
        "confirm_phrase": CONFIRM_PHRASE,
        "deletes_organizations": plan.deleted.iter().map(Org::to_json).collect::<Vec<_>>(),
        "leaves_organizations": plan.left.iter().map(Org::to_json).collect::<Vec<_>>(),
        "blocked_by": plan.blocking.iter().map(Org::to_json).collect::<Vec<_>>(),
    })
}

/// `GET /api/account/deletion`: what deleting this account would do.
pub async fn deletion_preview(
    rate: PerMinute<30>,
    State(state): State<AppState>,
    user: SignedInUser,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;
    let clerk = Clerk::new(&state);
    Ok(Json(plan_json(&plan(&clerk, &user.user_id).await?)))
}

/// `DELETE /api/account`: delete the signed-in account.
///
/// Body: `{"confirm": "delete my account"}`.
pub async fn delete_account(
    rate: PerHour<5>,
    State(state): State<AppState>,
    user: SignedInUser,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;
    let confirmed = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("confirm").and_then(Value::as_str).map(str::to_string))
        .is_some_and(|c| c.trim().eq_ignore_ascii_case(CONFIRM_PHRASE));
    if !confirmed {
        return Err(ApiError::bad_request(format!(
            "Type \"{CONFIRM_PHRASE}\" to confirm."
        )));
    }

    let clerk = Clerk::new(&state);
    let plan = plan(&clerk, &user.user_id).await?;
    if !plan.blocking.is_empty() {
        return Err(ApiError::new(
            reqwest::StatusCode::CONFLICT,
            json!({
                "error": "last_admin",
                "message": "You are the only admin of an organization that has other \
                            members. Make someone else an admin first.",
                "organizations": plan.blocking.iter().map(Org::to_json).collect::<Vec<_>>(),
            }),
        ));
    }
    // Before the account goes: afterwards Clerk cannot say what they were.
    let emails = clerk.user_emails(&user.user_id).await?;

    // Organizations only this account belonged to. Erased here rather
    // than left to the `organization.deleted` webhook, so the data is
    // gone when this request returns; the webhook then finds nothing.
    for org in &plan.deleted {
        match clerk.delete_organization(&org.id).await {
            Ok(()) | Err(ClerkError::NotFound) => {}
            Err(err) => return Err(err.into()),
        }
        crate::api::gdpr::erase_org(&state, &org.id).await?;
    }

    match clerk.delete_user(&user.user_id).await {
        Ok(()) | Err(ClerkError::NotFound) => {}
        Err(err) => return Err(err.into()),
    }
    // After the account is gone, so a failure here cannot leave a live
    // account with its history erased. If it fails, the `user.deleted`
    // webhook erases the same rows.
    let counts = crate::api::gdpr::erase_user_data(&state.pool, &user.user_id, &emails).await?;
    tracing::info!(
        user_id = user.user_id,
        organizations_deleted = plan.deleted.len(),
        counts = ?counts,
        "account deleted"
    );
    Ok(Json(json!({
        "deleted": true,
        "organizations_deleted": plan.deleted.len(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org(id: &str) -> Org {
        Org {
            id: id.into(),
            name: format!("{id} name"),
        }
    }

    fn m(list: &[(&str, bool)]) -> Vec<(String, bool)> {
        list.iter().map(|(u, a)| (u.to_string(), *a)).collect()
    }

    #[test]
    fn the_only_member_takes_the_organization_with_them() {
        assert_eq!(classify("me", &m(&[("me", true)])), Outcome::Deleted);
    }

    #[test]
    fn the_last_admin_with_members_left_is_refused() {
        let members = m(&[("me", true), ("you", false)]);
        assert_eq!(classify("me", &members), Outcome::Blocking);
    }

    #[test]
    fn an_admin_with_another_admin_can_leave() {
        let members = m(&[("me", true), ("you", true), ("them", false)]);
        assert_eq!(classify("me", &members), Outcome::Left);
    }

    #[test]
    fn a_plain_member_can_always_leave() {
        assert_eq!(
            classify("me", &m(&[("me", false), ("you", true)])),
            Outcome::Left
        );
        // Even from an organization that already has no admin.
        assert_eq!(
            classify("me", &m(&[("me", false), ("you", false)])),
            Outcome::Left
        );
    }

    #[test]
    fn the_preview_names_every_organization_by_outcome() {
        let plan = Plan {
            deleted: vec![org("a")],
            left: vec![org("b")],
            blocking: vec![org("c")],
        };
        let v = plan_json(&plan);
        assert_eq!(v["confirm_phrase"], CONFIRM_PHRASE);
        assert_eq!(v["deletes_organizations"][0]["id"], "a");
        assert_eq!(v["leaves_organizations"][0]["name"], "b name");
        assert_eq!(v["blocked_by"][0]["id"], "c");
    }
}
