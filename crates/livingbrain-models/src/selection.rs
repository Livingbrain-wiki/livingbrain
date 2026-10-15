//! Which connection serves a job.
//!
//! The product rule, plainly: an unset job uses Main. With nothing
//! connected, the managed model answers. One pure function holds the rule,
//! so every caller resolves through the same one and none grows its own.

use crate::store::ModelConnection;

/// The connection that serves a job for `role`: the role's own connection
/// when it has one, otherwise the Main connection, otherwise `None` — with
/// nothing connected, the managed model answers. A role outside
/// [`ROLES`](crate::handlers::ROLES) resolves to `None` too; there is
/// nothing for it to fall back to.
pub(crate) fn resolve<'a>(rows: &'a [ModelConnection], role: &str) -> Option<&'a ModelConnection> {
    if !crate::handlers::ROLES.contains(&role) {
        return None;
    }
    rows.iter()
        .find(|row| row.role == role)
        .or_else(|| rows.iter().find(|row| row.role == "main"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One connected row for `role`, distinct per role so a test can see
    /// which row came back. Nothing the resolver reads but `role` varies.
    fn row(role: &str) -> ModelConnection {
        ModelConnection {
            role: role.to_owned(),
            provider: format!("provider-for-{role}"),
            base_url: format!("https://{role}.models.example.com/v1"),
            auth: "bearer".to_owned(),
            wire: "openai".to_owned(),
            model: format!("model-for-{role}"),
            key_last4: "1234".to_owned(),
            fallback_to_managed: false,
            status: "works".to_owned(),
            missing: "[]".to_owned(),
            context_size: None,
            checked_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    /// A role's own connection wins over Main.
    #[test]
    fn a_role_s_own_connection_wins() {
        let rows = [row("main"), row("triage"), row("research")];
        for role in ["triage", "main", "research"] {
            let serving = resolve(&rows, role).expect("a connection serves the role");
            assert_eq!(serving.role, role);
        }
    }

    /// An unset job uses Main: triage and research with only a Main
    /// connection both resolve to it.
    #[test]
    fn an_unset_job_uses_the_main_connection() {
        let rows = [row("main")];
        for role in ["triage", "research"] {
            let serving = resolve(&rows, role).expect("Main serves the unset role");
            assert_eq!(serving.role, "main");
        }
    }

    /// Main itself resolves to Main, not to a second-level fallback.
    #[test]
    fn main_resolves_to_itself() {
        let rows = [row("main")];
        assert_eq!(resolve(&rows, "main").expect("Main is set").role, "main");
    }

    /// With nothing connected, nothing serves the role: the managed model
    /// answers, and `None` is how the resolver says so.
    #[test]
    fn with_nothing_connected_the_managed_model_answers() {
        let rows = [];
        for role in crate::handlers::ROLES {
            assert!(resolve(&rows, role).is_none(), "{role} resolved a row");
        }
    }

    /// A role that is not one of the three resolves to `None` even when
    /// every real role is connected — there is no fallback for a role the
    /// module cannot connect in the first place.
    #[test]
    fn an_unknown_role_resolves_to_none() {
        let rows = [row("main"), row("triage"), row("research")];
        assert!(resolve(&rows, "editor").is_none());
        assert!(resolve(&rows, "").is_none());
    }
}
