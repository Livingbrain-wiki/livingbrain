//! The two hooks RFC 8628 leaves to a venture — who may approve a device,
//! and what an approved device receives — and the handle that gives them
//! their ports.
//!
//! [`cratefield_module_device_auth::DeviceAuthBuilder`] takes its `Approver`
//! and `Issuer` at **composition** time, before a request exists and so
//! before a Worker port does. [`DevicePorts`] is the seam: the venture builds
//! one handle, gives the grant `handle.approver()` and `handle.issuer()`,
//! and gives this module's `Tokens` the same handle. When the harness builds
//! the module routers it publishes its ports in, so a grant handler can only
//! ever run after they are there.

use std::sync::{Arc, Mutex};

use cratefield_core::{Clock, Database, Ports, Signer};
use cratefield_module_device_auth::{
    Approval, Approver, ApproverError, IssueRequest, Issuer, IssuerError,
};
use serde_json::json;

use crate::store::Store;

/// Relative, so it resolves against the origin the approval page was served
/// from and a request-hostile value cannot choose the redirect target.
pub const DEFAULT_SIGN_IN_URL: &str = "/index.html";

/// The unit separator `livingbrain_mcp::page_scope` joins ids with: it cannot
/// occur in a Slack team id or a ULID, and cannot be typed into a form.
const SUBJECT_SEPARATOR: char = '\u{1f}';

const UNRESOLVED: &str = "the device grant has no ports resolved; the tokens module publishes \
                          them when the harness builds the routers";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Subject {
    workspace_id: String,
    user_id: String,
}

fn decode(subject: &str) -> Option<Subject> {
    let mut halves = subject.split(SUBJECT_SEPARATOR);
    let (Some(workspace_id), Some(user_id), None) = (halves.next(), halves.next(), halves.next())
    else {
        return None;
    };
    if workspace_id.is_empty() || user_id.is_empty() {
        return None;
    }
    Some(Subject {
        workspace_id: workspace_id.to_owned(),
        user_id: user_id.to_owned(),
    })
}

/// The ports a hook needs, resolved once per request build.
#[derive(Clone)]
struct Deps {
    db: Arc<dyn Database>,
    clock: Arc<dyn Clock>,
    signer: Arc<dyn Signer>,
}

/// The handle the composition passes to both the grant and this module.
#[derive(Clone)]
pub struct DevicePorts {
    deps: Arc<Mutex<Option<Deps>>>,
    sign_in_url: String,
}

impl std::fmt::Debug for DevicePorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicePorts")
            .field("sign_in_url", &self.sign_in_url)
            .field(
                "resolved",
                &self.deps.lock().ok().map(|deps| deps.is_some()),
            )
            .finish()
    }
}

impl Default for DevicePorts {
    fn default() -> Self {
        Self::new()
    }
}

impl DevicePorts {
    #[must_use]
    pub fn new() -> Self {
        Self {
            deps: Arc::new(Mutex::new(None)),
            sign_in_url: DEFAULT_SIGN_IN_URL.to_owned(),
        }
    }

    /// Absolute or relative; relative resolves against the approval page's
    /// own origin.
    #[must_use]
    pub fn sign_in_url(mut self, url: impl Into<String>) -> Self {
        self.sign_in_url = url.into();
        self
    }

    #[must_use]
    pub fn approver(&self) -> SessionApprover {
        SessionApprover {
            handle: self.clone(),
        }
    }

    #[must_use]
    pub fn issuer(&self) -> PatIssuer {
        PatIssuer {
            handle: self.clone(),
        }
    }

    /// Publishes what the module context resolved. A partial set clears the
    /// handle rather than leaving a stale one: the hooks refuse, which is the
    /// answer a deployment missing a port deserves.
    pub(crate) fn publish(&self, ports: &Ports) {
        let resolved = match (ports.db.clone(), ports.clock.clone(), ports.signer.clone()) {
            (Some(db), Some(clock), Some(signer)) => Some(Deps { db, clock, signer }),
            _ => None,
        };
        if let Ok(mut slot) = self.deps.lock() {
            *slot = resolved;
        }
    }

    fn deps(&self) -> Option<Deps> {
        self.deps.lock().ok().and_then(|slot| slot.clone())
    }
}

/// A signed-in person is a subject; an anonymous visitor is sent to the app's
/// sign-in page with `return_to` appended, so the code they were entering
/// survives the round trip.
#[derive(Clone, Debug)]
pub struct SessionApprover {
    handle: DevicePorts,
}

#[async_trait::async_trait]
impl Approver for SessionApprover {
    async fn approve(
        &self,
        headers: &cratefield_core::axum::http::HeaderMap,
        return_to: &str,
    ) -> Result<Approval, ApproverError> {
        let Some(deps) = self.handle.deps() else {
            return Err(ApproverError::new(UNRESOLVED));
        };
        match livingbrain_workspaces::caller(&*deps.signer, &*deps.clock, &*deps.db, headers).await
        {
            // Every refusal is the same answer — no cookie, an expired one, a
            // member row that is gone — because the alternative tells a
            // stranger whether a person exists.
            Ok(caller) => Ok(Approval::Subject(format!(
                "{}{}{}",
                caller.workspace_id, SUBJECT_SEPARATOR, caller.user_id
            ))),
            Err(_) => Ok(Approval::SignIn {
                location: sign_in_location(&self.handle.sign_in_url, return_to),
            }),
        }
    }
}

/// `sign_in_url` with `return_to` appended, percent-encoded rather than
/// concatenated — it always carries its own query, so it could otherwise
/// break out of the parameter.
fn sign_in_location(sign_in_url: &str, return_to: &str) -> String {
    let encoded: String = return_to
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect();
    let separator = if sign_in_url.contains('?') { '&' } else { '?' };
    format!("{sign_in_url}{separator}return_to={encoded}")
}

/// An approved device gets a personal access token for the person who
/// pressed Approve, and nothing else.
#[derive(Clone, Debug)]
pub struct PatIssuer {
    handle: DevicePorts,
}

#[async_trait::async_trait]
impl Issuer for PatIssuer {
    async fn issue(&self, request: IssueRequest) -> Result<serde_json::Value, IssuerError> {
        let Some(deps) = self.handle.deps() else {
            return Err(IssuerError::new(UNRESOLVED));
        };
        let subject = decode(&request.subject)
            .ok_or_else(|| IssuerError::new("the approver's subject is not a member pair"))?;
        // The label the client gave the device is what a person will
        // recognise in their settings list; the client id is the fallback.
        let name = request
            .name
            .clone()
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| request.client_id.clone());
        let minted = Store::new(deps.db, deps.clock)
            .mint(
                &subject.workspace_id,
                &subject.user_id,
                &name,
                &request.scopes,
            )
            .await
            .map_err(|err| IssuerError::new(err.to_string()))?;
        // RFC 6749 §5.1. `access_token` is returned exactly once, by the poll
        // that won the consume, and this body is the only place it is written.
        Ok(json!({ "access_token": minted.token, "token_type": "bearer" }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subject_round_trips_and_anything_else_is_refused() {
        assert_eq!(
            decode("T0SPACE\u{1f}U0OWNER").expect("round trips"),
            Subject {
                workspace_id: "T0SPACE".to_owned(),
                user_id: "U0OWNER".to_owned(),
            }
        );
        for refused in [
            "",
            "T0SPACE",
            "\u{1f}U0OWNER",
            "T0SPACE\u{1f}",
            "T0\u{1f}U\u{1f}X",
        ] {
            assert!(decode(refused).is_none(), "{refused:?} parsed");
        }
    }

    #[test]
    fn a_sign_in_location_encodes_the_return_to() {
        let location = sign_in_location("/index.html", "/v1/device-auth?user_code=ABCD-EFGH");
        assert!(
            location.starts_with("/index.html?return_to=")
                && location.contains("%3Fuser_code%3DABCD-EFGH"),
            "{location}"
        );
        assert!(
            sign_in_location("/index.html?a=1", "/x").starts_with("/index.html?a=1&return_to="),
            "a url that already has a query"
        );
    }
}
