//! Store behaviour, run against every dialect the environment provides
//! (SQLite always; Postgres when `FZ_TEST_POSTGRES_URL` is set — CI's
//! parity leg). Unit tests rather than integration tests because the drain
//! half needs a [`Drain`], which nothing outside the crate can make. Fakes
//! only: an in-process key ring, the kit's databases, no network.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use cratefield_core::{Database, Statement};
use cratefield_kms::Kms;
use cratefield_testing::TestHarness;
use livingbrain_workspaces::Workspaces;

use super::*;
use crate::common::{int, text};
use crate::drain::{claim, complete, fail, repos_due, save_backfill_progress};

const W1: &str = "W1SPACE";
const W2: &str = "W2SPACE";
const OWNER: &str = "U1OWNER";
const ADMIN: &str = "U1ADMIN";
const PLAIN: &str = "U1PLAIN";
const OWNER2: &str = "U2OWNER";
const APP1: i64 = 1001;
const APP2: i64 = 2002;
const INST1: i64 = 5001;
const INST2: i64 = 6002;

fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("an instant")
}

fn later(secs: i64) -> time::OffsetDateTime {
    now() + time::Duration::seconds(secs)
}

fn kms(byte: u8) -> Arc<dyn Kms> {
    let key = STANDARD.encode([byte; 32]);
    Arc::new(
        cratefield_kms::WorkerSecretKms::from_lookup(|name| match name {
            "HARNESS_KEK_CURRENT" => Some("1".to_owned()),
            "HARNESS_KEK_V1" => Some(key.clone()),
            _ => None,
        })
        .expect("the key ring is well formed"),
    )
}

/// One kit per available dialect, with the workspaces module (members) and
/// this one mounted.
fn kits() -> Vec<TestHarness> {
    TestHarness::all_dialects(|| vec![Box::new(Workspaces::new()), Box::new(GithubApp::new())])
}

async fn seed_member(db: &dyn Database, workspace: &str, user: &str, role: &str) {
    db.execute(&Statement::with_values(
        "INSERT INTO workspace_members (workspace_id, user_id, name, is_admin, updated_at, role) \
         VALUES (?, ?, ?, ?, ?, ?)",
        vec![
            text(workspace),
            text(user),
            text("someone"),
            int(0),
            text("2026-01-01T00:00:00Z"),
            text(role),
        ],
    ))
    .await
    .expect("member seeds");
}

/// Two workspaces: W1 with an owner, an admin and a plain member; W2 with
/// its own owner.
async fn seed(db: &dyn Database) {
    for (id, owner) in [(W1, OWNER), (W2, OWNER2)] {
        db.execute(&Statement::with_values(
            "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?)",
            vec![
                text(id),
                text("ws"),
                text(owner),
                text("2026-01-01T00:00:00Z"),
            ],
        ))
        .await
        .expect("workspace seeds");
    }
    seed_member(db, W1, OWNER, "owner").await;
    seed_member(db, W1, ADMIN, "admin").await;
    seed_member(db, W1, PLAIN, "member").await;
    seed_member(db, W2, OWNER2, "owner").await;
}

fn new_app(app_id: i64, tag: &str) -> NewApp {
    NewApp {
        app_id,
        slug: format!("living-brain-{tag}"),
        client_id: format!("Iv1.{tag}"),
        private_key_pem: Secret::new(format!(
            "-----BEGIN {k}-----\nPEMBODY{tag}\n-----END {k}-----\n",
            k = "RSA PRIVATE KEY"
        )),
        webhook_secret: Secret::new(format!("whsec-{tag}-0123456789")),
        client_secret: Secret::new(format!("clsec-{tag}-9876543210")),
    }
}

async fn proof(
    db: &dyn Database,
    purpose: StatePurpose,
    workspace: &str,
    member: &str,
) -> StateProof {
    let token = create_state(
        db,
        now(),
        purpose,
        workspace,
        member,
        DEFAULT_STATE_TTL_SECS,
    )
    .await
    .expect("state mints");
    consume_state(db, later(1), purpose, &token, workspace, member)
        .await
        .expect("consume runs")
        .expect("state is good")
}

/// W1 with app APP1 and installation INST1 bound and one repo; W2 with
/// app APP2 and installation INST2 bound and one repo.
async fn connected(kms: &dyn Kms, db: &dyn Database) {
    seed(db).await;
    for (ws, member, app, inst, tag) in [
        (W1, OWNER, APP1, INST1, "one"),
        (W2, OWNER2, APP2, INST2, "two"),
    ] {
        let manifest = proof(db, StatePurpose::Manifest, ws, member).await;
        assert_eq!(
            save_app(kms, db, now(), manifest, &new_app(app, tag))
                .await
                .expect("saves"),
            SaveAppOutcome::Saved
        );
        let install = proof(db, StatePurpose::Install, ws, member).await;
        assert_eq!(
            bind_installation(db, now(), install, inst, &account(tag))
                .await
                .expect("binds"),
            BindOutcome::Bound
        );
        assert_eq!(
            upsert_repos(
                db,
                now(),
                ws,
                inst,
                &[RepoRef {
                    repo_id: inst * 10,
                    full_name: format!("org-{tag}/Repo {tag}"),
                }]
            )
            .await
            .expect("repo upserts"),
            1
        );
    }
}

fn account(tag: &str) -> Account {
    Account {
        login: format!("org-{tag}"),
        kind: "Organization".to_owned(),
        id: None,
    }
}

// ---------------------------------------------------------------------------
// State tokens

#[test]
fn a_state_is_single_use_expiring_and_bound_to_its_workspace_member_and_purpose() {
    pollster::block_on(async {
        for kit in kits() {
            let db = &*kit.db;
            seed(db).await;
            let mint = || create_state(db, now(), StatePurpose::Manifest, W1, OWNER, 600);

            // Single use.
            let token = mint().await.expect("mints");
            let first = consume_state(db, later(1), StatePurpose::Manifest, &token, W1, OWNER)
                .await
                .expect("runs");
            assert_eq!(first.as_ref().map(StateProof::workspace_id), Some(W1));
            assert_eq!(
                consume_state(db, later(2), StatePurpose::Manifest, &token, W1, OWNER)
                    .await
                    .expect("runs"),
                None,
                "a spent state is refused"
            );

            // Expired.
            let token = mint().await.expect("mints");
            assert_eq!(
                consume_state(db, later(601), StatePurpose::Manifest, &token, W1, OWNER)
                    .await
                    .expect("runs"),
                None
            );

            // Wrong workspace, wrong member, wrong purpose: refused, and the
            // token is burnt — the right caller cannot use it afterwards.
            for (purpose, ws, member) in [
                (StatePurpose::Manifest, W2, OWNER),
                (StatePurpose::Manifest, W1, ADMIN),
                (StatePurpose::Install, W1, OWNER),
            ] {
                let token = mint().await.expect("mints");
                assert_eq!(
                    consume_state(db, later(1), purpose, &token, ws, member)
                        .await
                        .expect("runs"),
                    None
                );
                assert_eq!(
                    consume_state(db, later(1), StatePurpose::Manifest, &token, W1, OWNER)
                        .await
                        .expect("runs"),
                    None,
                    "a misused state is burnt"
                );
            }

            // An unknown token is the same answer.
            assert_eq!(
                consume_state(db, later(1), StatePurpose::Manifest, "nope", W1, OWNER)
                    .await
                    .expect("runs"),
                None
            );

            // Only the hash is stored.
            let token = mint().await.expect("mints");
            let rows = db
                .query(&Statement::new("SELECT state_hash FROM github_states"))
                .await
                .expect("reads");
            for row in &rows.rows {
                assert_ne!(row.get::<String>("state_hash").unwrap_or_default(), token);
            }
        }
    });
}

#[test]
fn only_an_owner_or_admin_mints_a_state_and_a_demotion_voids_one() {
    pollster::block_on(async {
        for kit in kits() {
            let db = &*kit.db;
            seed(db).await;
            for member in [PLAIN, "U9STRANGER", OWNER2] {
                assert!(matches!(
                    create_state(db, now(), StatePurpose::Install, W1, member, 600).await,
                    Err(StoreError::Forbidden)
                ));
            }
            create_state(db, now(), StatePurpose::Install, W1, ADMIN, 600)
                .await
                .expect("an admin mints");

            // Minted as an admin, demoted before the callback: refused.
            let token = create_state(db, now(), StatePurpose::Install, W1, ADMIN, 600)
                .await
                .expect("mints");
            db.execute(&Statement::with_values(
                "UPDATE workspace_members SET role = 'member' WHERE user_id = ?",
                vec![text(ADMIN)],
            ))
            .await
            .expect("demotes");
            assert_eq!(
                consume_state(db, later(1), StatePurpose::Install, &token, W1, ADMIN)
                    .await
                    .expect("runs"),
                None
            );
        }
    });
}

// ---------------------------------------------------------------------------
// App credentials

#[test]
fn app_credentials_are_sealed_at_rest_and_open_only_for_their_workspace() {
    pollster::block_on(async {
        let kms = kms(7);
        for kit in kits() {
            let db = &*kit.db;
            seed(db).await;

            // Only a manifest proof saves an app.
            let install = proof(db, StatePurpose::Install, W1, OWNER).await;
            assert!(matches!(
                save_app(&*kms, db, now(), install, &new_app(APP1, "one")).await,
                Err(StoreError::Forbidden)
            ));

            let manifest = proof(db, StatePurpose::Manifest, W1, OWNER).await;
            assert_eq!(
                save_app(&*kms, db, now(), manifest, &new_app(APP1, "one"))
                    .await
                    .expect("saves"),
                SaveAppOutcome::Saved
            );

            // Nothing at rest is a plaintext credential.
            let rows = db
                .query(&Statement::new("SELECT * FROM github_app"))
                .await
                .expect("reads");
            for (column, value) in rows.rows[0].columns() {
                let shown = format!("{value:?}");
                for needle in ["PEMBODYone", "whsec-one", "clsec-one"] {
                    assert!(!shown.contains(needle), "{column} holds {needle}");
                }
            }

            let creds = app_credentials(&*kms, db, W1)
                .await
                .expect("opens")
                .expect("an app");
            assert_eq!(creds.app_id, APP1);
            assert!(creds.private_key_pem.expose().contains("PEMBODYone"));
            assert_eq!(creds.webhook_secret.expose(), "whsec-one-0123456789");
            assert_eq!(creds.client_secret.expose(), "clsec-one-9876543210");
            assert!(!format!("{creds:?}").contains("whsec"), "Debug redacts");

            let (ws, secret) = webhook_secret(&*kms, db, APP1)
                .await
                .expect("runs")
                .expect("known");
            assert_eq!((ws.as_str(), secret.expose()), (W1, "whsec-one-0123456789"));
            assert!(
                webhook_secret(&*kms, db, APP2)
                    .await
                    .expect("runs")
                    .is_none()
            );

            // Another workspace sees no app, and no credentials.
            assert!(app_summary(db, W2).await.expect("runs").is_none());
            assert!(
                app_credentials(&*kms, db, W2)
                    .await
                    .expect("runs")
                    .is_none()
            );

            // A second app for W1, or APP1 claimed by W2: refused, unchanged.
            let again = proof(db, StatePurpose::Manifest, W1, OWNER).await;
            assert_eq!(
                save_app(&*kms, db, now(), again, &new_app(APP2, "x"))
                    .await
                    .expect("runs"),
                SaveAppOutcome::WorkspaceHasApp
            );
            let w2 = proof(db, StatePurpose::Manifest, W2, OWNER2).await;
            assert_eq!(
                save_app(&*kms, db, now(), w2, &new_app(APP1, "x"))
                    .await
                    .expect("runs"),
                SaveAppOutcome::AppIdTaken
            );
            assert!(app_summary(db, W2).await.expect("runs").is_none());

            // A row moved to another workspace does not open: the AAD names
            // the workspace.
            db.execute(&Statement::with_values(
                "UPDATE github_app SET workspace_id = ? WHERE app_id = ?",
                vec![text(W2), int(APP1)],
            ))
            .await
            .expect("moves");
            assert!(matches!(
                app_credentials(&*kms, db, W2).await,
                Err(StoreError::Kms(cratefield_kms::KmsError::Tampered(_)))
            ));
            // And a different master key does not open it either.
            db.execute(&Statement::with_values(
                "UPDATE github_app SET workspace_id = ? WHERE app_id = ?",
                vec![text(W1), int(APP1)],
            ))
            .await
            .expect("moves back");
            assert!(matches!(
                app_credentials(&*self::kms(9), db, W1).await,
                Err(StoreError::Kms(_))
            ));
        }
    });
}

// ---------------------------------------------------------------------------
// Binding

#[test]
fn binding_needs_an_owner_or_admin_and_never_moves_an_installation() {
    pollster::block_on(async {
        let kms = kms(7);
        for kit in kits() {
            let db = &*kit.db;
            seed(db).await;

            // A member cannot even start the install round trip.
            assert!(matches!(
                create_state(db, now(), StatePurpose::Install, W1, PLAIN, 600).await,
                Err(StoreError::Forbidden)
            ));

            // No app yet: nothing to bind to.
            let install = proof(db, StatePurpose::Install, W1, ADMIN).await;
            assert_eq!(
                bind_installation(db, now(), install, INST1, &account("one"))
                    .await
                    .expect("runs"),
                BindOutcome::NoApp
            );

            let manifest = proof(db, StatePurpose::Manifest, W1, OWNER).await;
            save_app(&*kms, db, now(), manifest, &new_app(APP1, "one"))
                .await
                .expect("saves");
            let manifest = proof(db, StatePurpose::Manifest, W2, OWNER2).await;
            save_app(&*kms, db, now(), manifest, &new_app(APP2, "two"))
                .await
                .expect("saves");

            // A manifest proof does not bind.
            let wrong = proof(db, StatePurpose::Manifest, W1, ADMIN).await;
            assert!(matches!(
                bind_installation(db, now(), wrong, INST1, &account("one")).await,
                Err(StoreError::Forbidden)
            ));

            // The admin binds; binding again is a no-op success.
            let install = proof(db, StatePurpose::Install, W1, ADMIN).await;
            assert_eq!(
                bind_installation(db, now(), install, INST1, &account("one"))
                    .await
                    .expect("runs"),
                BindOutcome::Bound
            );
            let install = proof(db, StatePurpose::Install, W1, OWNER).await;
            assert_eq!(
                bind_installation(db, now(), install, INST1, &account("one"))
                    .await
                    .expect("runs"),
                BindOutcome::AlreadyThisWorkspace
            );

            // W2's owner tries to take W1's installation: refused, unchanged.
            let hijack = proof(db, StatePurpose::Install, W2, OWNER2).await;
            assert_eq!(
                bind_installation(db, now(), hijack, INST1, &account("one"))
                    .await
                    .expect("runs"),
                BindOutcome::Refused
            );
            let route = route_installation(db, APP1, INST1)
                .await
                .expect("runs")
                .expect("bound");
            assert_eq!(route.workspace_id, W1);
            let rows = list_installations(db, W1).await.expect("runs");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].bound_by.as_deref(), Some(ADMIN));
            assert!(list_installations(db, W2).await.expect("runs").is_empty());

            // An installation another workspace's app reported unbound is
            // not claimable either: it is held under that app.
            assert!(
                observe_installation(db, now(), APP2, INST2, &account("two"))
                    .await
                    .expect("runs")
            );
            let grab = proof(db, StatePurpose::Install, W1, OWNER).await;
            assert_eq!(
                bind_installation(db, now(), grab, INST2, &account("two"))
                    .await
                    .expect("runs"),
                BindOutcome::Refused
            );
            // ...and one app's webhook cannot re-file another app's
            // installation.
            assert!(
                !observe_installation(db, now(), APP1, INST2, &account("evil"))
                    .await
                    .expect("runs")
            );
            let rows = db
                .query(&Statement::with_values(
                    "SELECT app_id, account_login, workspace_id FROM github_installations \
                     WHERE installation_id = ?",
                    vec![int(INST2)],
                ))
                .await
                .expect("reads");
            assert_eq!(rows.rows[0].get::<i64>("app_id"), Some(APP2));
            assert_eq!(
                rows.rows[0].get::<String>("account_login").as_deref(),
                Some("org-two")
            );

            // A webhook never binds.
            assert!(
                route_installation(db, APP2, INST2)
                    .await
                    .expect("runs")
                    .is_none()
            );

            // A deleted installation is never bound.
            assert!(
                set_installation_status(db, now(), APP2, INST2, InstallationStatus::Deleted)
                    .await
                    .expect("runs")
            );
            let late = proof(db, StatePurpose::Install, W2, OWNER2).await;
            assert_eq!(
                bind_installation(db, now(), late, INST2, &account("two"))
                    .await
                    .expect("runs"),
                BindOutcome::Refused
            );

            // Routing checks the app: W1's installation under APP2 is nothing.
            assert!(
                route_installation(db, APP2, INST1)
                    .await
                    .expect("runs")
                    .is_none()
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Isolation

#[test]
fn no_read_or_write_crosses_workspaces() {
    pollster::block_on(async {
        let kms = kms(7);
        for kit in kits() {
            let db = &*kit.db;
            connected(&*kms, db).await;

            let w1 = list_repos(db, W1).await.expect("runs");
            assert_eq!(w1.len(), 1);
            assert_eq!(w1[0].installation_id, INST1);
            assert_eq!(
                w1[0].project, "Repo-one",
                "the default project is the repo name"
            );
            assert_eq!(w1[0].scope, "shared");
            assert_eq!(w1[0].backfill_state, BackfillPhase::Pending);

            let w1_repo = INST1 * 10;
            // Reads.
            assert!(
                get_repo(db, W2, INST1, w1_repo)
                    .await
                    .expect("runs")
                    .is_none()
            );
            assert!(
                get_repo(db, W1, INST1, w1_repo)
                    .await
                    .expect("runs")
                    .is_some()
            );
            assert!(
                list_repos(db, W2)
                    .await
                    .expect("runs")
                    .iter()
                    .all(|r| r.workspace_id == W2)
            );
            assert!(
                list_installations(db, W2)
                    .await
                    .expect("runs")
                    .iter()
                    .all(|i| i.installation_id == INST2)
            );
            assert_eq!(
                app_summary(db, W2).await.expect("runs").map(|a| a.app_id),
                Some(APP2)
            );

            // Writes keyed on the wrong workspace change nothing.
            assert_eq!(
                upsert_repos(
                    db,
                    now(),
                    W2,
                    INST1,
                    &[RepoRef {
                        repo_id: 77,
                        full_name: "x/y".into()
                    }]
                )
                .await
                .expect("runs"),
                0
            );
            assert!(
                !remove_repo(db, now(), W2, INST1, w1_repo)
                    .await
                    .expect("runs")
            );
            note_webhook(db, later(5), W2, INST1, w1_repo)
                .await
                .expect("runs");
            assert_eq!(
                get_repo(db, W1, INST1, w1_repo)
                    .await
                    .expect("runs")
                    .unwrap()
                    .last_webhook_at,
                None
            );
            let item = WorkItem {
                repo_id: Some(w1_repo),
                kind: "issues".into(),
                subject: "1".into(),
            };
            assert!(
                enqueue(db, now(), W2, INST1, &item)
                    .await
                    .expect("runs")
                    .is_none()
            );
            assert!(
                enqueue(db, now(), W1, INST1, &item)
                    .await
                    .expect("runs")
                    .is_some()
            );
            assert_eq!(queued_count(db, W1).await.expect("runs"), 1);
            assert_eq!(queued_count(db, W2).await.expect("runs"), 0);
            // A repo of another installation cannot ride on this one.
            let foreign = WorkItem {
                repo_id: Some(INST2 * 10),
                kind: "issues".into(),
                subject: "1".into(),
            };
            assert!(
                enqueue(db, now(), W1, INST1, &foreign)
                    .await
                    .expect("runs")
                    .is_none()
            );

            // W2's owner cannot change W1's repo; W1's plain member cannot
            // change it either.
            let settings = RepoSettings {
                included: Some(false),
                ..RepoSettings::default()
            };
            assert!(
                !set_repo_settings(db, now(), W2, OWNER2, INST1, w1_repo, &settings)
                    .await
                    .expect("runs")
            );
            assert!(matches!(
                set_repo_settings(db, now(), W1, PLAIN, INST1, w1_repo, &settings).await,
                Err(StoreError::Forbidden)
            ));
            assert!(matches!(
                set_repo_settings(db, now(), W1, OWNER2, INST1, w1_repo, &settings).await,
                Err(StoreError::Forbidden)
            ));
            assert!(
                get_repo(db, W1, INST1, w1_repo)
                    .await
                    .expect("runs")
                    .unwrap()
                    .included
            );

            // A scope must be shared or a current member's own.
            for scope in ["user:U2OWNER", "channel:C1", "user:", "everyone"] {
                let bad = RepoSettings {
                    scope: Some(scope.into()),
                    ..RepoSettings::default()
                };
                assert!(
                    matches!(
                        set_repo_settings(db, now(), W1, ADMIN, INST1, w1_repo, &bad).await,
                        Err(StoreError::Invalid(_))
                    ),
                    "{scope}"
                );
            }
            let good = RepoSettings {
                scope: Some(format!("user:{PLAIN}")),
                project: Some("Brain Repo!".into()),
                included: Some(false),
            };
            assert!(
                set_repo_settings(db, now(), W1, ADMIN, INST1, w1_repo, &good)
                    .await
                    .expect("runs")
            );
            let repo = get_repo(db, W1, INST1, w1_repo)
                .await
                .expect("runs")
                .unwrap();
            assert_eq!(
                (repo.scope.as_str(), repo.project.as_str()),
                ("user:U1PLAIN", "Brain-Repo-")
            );
            assert!(!repo.included && !repo.active);
        }
    });
}

// ---------------------------------------------------------------------------
// Deliveries

#[test]
fn a_replayed_delivery_is_detected_and_old_ones_are_pruned() {
    pollster::block_on(async {
        for kit in kits() {
            let db = &*kit.db;
            let id = "72d3162e-cc78-11e3-81ab-4c9367dc0958";
            assert!(
                record_delivery(db, now(), APP1, id, "issues")
                    .await
                    .expect("runs")
            );
            assert!(
                !record_delivery(db, later(1), APP1, id, "issues")
                    .await
                    .expect("runs")
            );
            assert!(
                !record_delivery(db, later(1), APP1, &id.to_ascii_uppercase(), "issues")
                    .await
                    .expect("runs"),
                "case does not make a new id"
            );
            assert!(
                record_delivery(db, now(), APP2, id, "issues")
                    .await
                    .expect("runs"),
                "per app"
            );
            for bad in ["", "not a guid", "../../etc", &"a".repeat(65)] {
                assert!(matches!(
                    record_delivery(db, now(), APP1, bad, "issues").await,
                    Err(StoreError::Invalid(_))
                ));
            }
            assert_eq!(prune_deliveries(db, later(10)).await.expect("runs"), 0);
            assert_eq!(
                prune_deliveries(db, later(DELIVERY_RETENTION_SECS + 1))
                    .await
                    .expect("runs"),
                2
            );
            assert!(
                record_delivery(db, later(DELIVERY_RETENTION_SECS + 2), APP1, id, "issues")
                    .await
                    .expect("runs")
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Suspension and deletion

#[test]
fn a_suspended_or_deleted_installation_stops_its_repos_and_work() {
    pollster::block_on(async {
        let kms = kms(7);
        let drain = Drain::for_tests();
        for kit in kits() {
            let db = &*kit.db;
            connected(&*kms, db).await;
            let repo = INST1 * 10;
            let item = WorkItem {
                repo_id: Some(repo),
                kind: "issues".into(),
                subject: "7".into(),
            };
            enqueue(db, now(), W1, INST1, &item)
                .await
                .expect("runs")
                .expect("queued");
            assert_eq!(repos_due(db, &drain, 10).await.expect("runs").len(), 2);

            // Another app's webhook cannot suspend W1's installation.
            assert!(
                !set_installation_status(db, now(), APP2, INST1, InstallationStatus::Suspended)
                    .await
                    .expect("runs")
            );

            assert!(
                set_installation_status(db, now(), APP1, INST1, InstallationStatus::Suspended)
                    .await
                    .expect("runs")
            );
            let r = get_repo(db, W1, INST1, repo).await.expect("runs").unwrap();
            assert!(!r.active && r.included);
            assert!(
                repos_due(db, &drain, 10)
                    .await
                    .expect("runs")
                    .iter()
                    .all(|r| r.workspace_id == W2)
            );
            assert!(
                claim(db, &drain, later(1), 10, 60)
                    .await
                    .expect("runs")
                    .is_empty()
            );
            assert!(
                enqueue(db, now(), W1, INST1, &item)
                    .await
                    .expect("runs")
                    .is_none()
            );
            assert!(
                upsert_repos(
                    db,
                    now(),
                    W1,
                    INST1,
                    &[RepoRef {
                        repo_id: 9,
                        full_name: "a/b".into()
                    }]
                )
                .await
                .expect("runs")
                    == 0
            );
            let progress = BackfillProgress {
                cursor: Some("{}".into()),
                items: 3,
                done: false,
            };
            assert!(
                !save_backfill_progress(db, &drain, now(), W1, INST1, repo, &progress)
                    .await
                    .expect("runs")
            );
            let route = route_installation(db, APP1, INST1)
                .await
                .expect("runs")
                .unwrap();
            assert_eq!(route.status, InstallationStatus::Suspended);

            // Unsuspend: back, and the queued item is claimable again.
            assert!(
                set_installation_status(db, now(), APP1, INST1, InstallationStatus::Active)
                    .await
                    .expect("runs")
            );
            assert!(
                get_repo(db, W1, INST1, repo)
                    .await
                    .expect("runs")
                    .unwrap()
                    .active
            );
            assert_eq!(
                claim(db, &drain, later(1), 10, 60)
                    .await
                    .expect("runs")
                    .len(),
                1
            );

            // Deleted: terminal, queue dropped, cannot be revived.
            assert!(
                set_installation_status(db, now(), APP1, INST1, InstallationStatus::Deleted)
                    .await
                    .expect("runs")
            );
            assert!(
                !get_repo(db, W1, INST1, repo)
                    .await
                    .expect("runs")
                    .unwrap()
                    .active
            );
            assert_eq!(queued_count(db, W1).await.expect("runs"), 0);
            assert!(
                !set_installation_status(db, now(), APP1, INST1, InstallationStatus::Active)
                    .await
                    .expect("runs")
            );
            assert_eq!(
                route_installation(db, APP1, INST1)
                    .await
                    .expect("runs")
                    .unwrap()
                    .status,
                InstallationStatus::Deleted
            );
            // An admin cannot re-include a repo of a deleted installation
            // into activity.
            let include = RepoSettings {
                included: Some(true),
                ..RepoSettings::default()
            };
            assert!(
                set_repo_settings(db, now(), W1, OWNER, INST1, repo, &include)
                    .await
                    .expect("runs")
            );
            assert!(
                !get_repo(db, W1, INST1, repo)
                    .await
                    .expect("runs")
                    .unwrap()
                    .active
            );

            // W2 was untouched throughout.
            assert!(
                get_repo(db, W2, INST2, INST2 * 10)
                    .await
                    .expect("runs")
                    .unwrap()
                    .active
            );
        }
    });
}

#[test]
fn removing_the_app_forgets_its_credentials_and_stops_everything() {
    pollster::block_on(async {
        let kms = kms(7);
        for kit in kits() {
            let db = &*kit.db;
            connected(&*kms, db).await;
            assert!(matches!(
                remove_app(db, now(), W1, PLAIN).await,
                Err(StoreError::Forbidden)
            ));
            assert!(remove_app(db, now(), W1, ADMIN).await.expect("runs"));
            assert!(
                app_credentials(&*kms, db, W1)
                    .await
                    .expect("runs")
                    .is_none()
            );
            assert!(
                webhook_secret(&*kms, db, APP1)
                    .await
                    .expect("runs")
                    .is_none()
            );
            assert_eq!(
                route_installation(db, APP1, INST1)
                    .await
                    .expect("runs")
                    .unwrap()
                    .status,
                InstallationStatus::Deleted
            );
            assert!(
                !get_repo(db, W1, INST1, INST1 * 10)
                    .await
                    .expect("runs")
                    .unwrap()
                    .active
            );
            assert!(
                app_credentials(&*kms, db, W2)
                    .await
                    .expect("runs")
                    .is_some()
            );
            assert!(!remove_app(db, now(), W1, ADMIN).await.expect("runs"));
        }
    });
}

// ---------------------------------------------------------------------------
// The drain

#[test]
fn the_drain_records_progress_and_owns_its_leases() {
    pollster::block_on(async {
        let kms = kms(7);
        let drain = Drain::for_tests();
        for kit in kits() {
            let db = &*kit.db;
            connected(&*kms, db).await;
            let repo = INST1 * 10;

            for bad in [
                "not json",
                &format!("\"{}\"", "x".repeat(drain::MAX_CURSOR_BYTES)),
            ] {
                let progress = BackfillProgress {
                    cursor: Some(bad.to_owned()),
                    items: 1,
                    done: false,
                };
                assert!(matches!(
                    save_backfill_progress(db, &drain, now(), W1, INST1, repo, &progress).await,
                    Err(StoreError::Invalid(_))
                ));
            }
            let step = BackfillProgress {
                cursor: Some(r#"{"page":2}"#.into()),
                items: 50,
                done: false,
            };
            assert!(
                save_backfill_progress(db, &drain, now(), W1, INST1, repo, &step)
                    .await
                    .expect("runs")
            );
            // Keyed on the wrong workspace: lands nowhere.
            assert!(
                !save_backfill_progress(db, &drain, now(), W2, INST1, repo, &step)
                    .await
                    .expect("runs")
            );
            drain::record_repo_error(
                db,
                &drain,
                later(1),
                W1,
                INST1,
                repo,
                "HTTP 502\nfrom upstream",
            )
            .await
            .expect("runs");
            let r = get_repo(db, W1, INST1, repo).await.expect("runs").unwrap();
            assert_eq!(r.backfill_state, BackfillPhase::Running);
            assert_eq!(r.backfill_cursor.as_deref(), Some(r#"{"page":2}"#));
            assert_eq!((r.backfill_items, r.backfill_batches), (50, 1));
            assert_eq!(r.last_error.as_deref(), Some("HTTP 502from upstream"));

            let last = BackfillProgress {
                cursor: None,
                items: 7,
                done: true,
            };
            assert!(
                save_backfill_progress(db, &drain, later(2), W1, INST1, repo, &last)
                    .await
                    .expect("runs")
            );
            let r = get_repo(db, W1, INST1, repo).await.expect("runs").unwrap();
            assert_eq!(r.backfill_state, BackfillPhase::Done);
            assert_eq!((r.backfill_items, r.backfill_batches), (57, 2));
            assert_eq!(r.backfill_started_at, Some(now().unix_timestamp()));
            assert_eq!(r.last_error, None);
            assert!(
                repos_due(db, &drain, 10)
                    .await
                    .expect("runs")
                    .iter()
                    .all(|r| r.workspace_id == W2)
            );

            // Leases.
            let item = WorkItem {
                repo_id: Some(repo),
                kind: "pull_request".into(),
                subject: "3".into(),
            };
            enqueue(db, now(), W1, INST1, &item)
                .await
                .expect("runs")
                .expect("queued");
            let held = claim(db, &drain, later(1), 10, 60).await.expect("runs");
            assert_eq!(held.len(), 1);
            assert_eq!(
                (
                    held[0].kind.as_str(),
                    held[0].subject.as_str(),
                    held[0].attempts
                ),
                ("pull_request", "3", 1)
            );
            assert!(
                claim(db, &drain, later(2), 10, 60)
                    .await
                    .expect("runs")
                    .is_empty(),
                "leased"
            );
            let stolen = Claimed {
                lease_token: "0".repeat(32),
                ..held[0].clone()
            };
            assert!(!complete(db, &drain, &stolen).await.expect("runs"));

            // Fail it until it is dead.
            let mut current = held[0].clone();
            let mut t = 100;
            for attempt in 1..=drain::MAX_ATTEMPTS {
                assert_eq!(current.attempts, attempt);
                assert!(
                    fail(db, &drain, later(t), &current, "boom", 0)
                        .await
                        .expect("runs")
                );
                t += 1;
                let next = claim(db, &drain, later(t), 10, 60).await.expect("runs");
                if attempt == drain::MAX_ATTEMPTS {
                    assert!(next.is_empty(), "dead after {attempt}");
                } else {
                    current = next.into_iter().next().expect("retried");
                }
            }
            assert_eq!(queued_count(db, W1).await.expect("runs"), 0);

            // An expired lease is taken over, and the old holder cannot
            // finish it.
            enqueue(db, now(), W1, INST1, &item)
                .await
                .expect("runs")
                .expect("queued");
            let first = claim(db, &drain, later(500), 10, 60).await.expect("runs");
            let second = claim(db, &drain, later(600), 10, 60).await.expect("runs");
            assert_eq!(second.len(), 1);
            assert!(!complete(db, &drain, &first[0]).await.expect("runs"));
            assert!(complete(db, &drain, &second[0]).await.expect("runs"));
        }
    });
}

/// The user rule, made structural: a [`Drain`] is minted only by the
/// scheduled entry point. Any other source file naming its constructor
/// fails here, so no route, CLI hook or helper can become an import
/// trigger without this test being changed in review.
#[test]
fn only_the_scheduled_drain_mints_a_drain() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&src).expect("src reads") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        if name == "drain.rs" || name == "tests.rs" {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("reads");
        if body.contains("for_scheduled_run") || body.contains("Drain {") {
            offenders.push(name);
        }
    }
    assert!(
        offenders.is_empty(),
        "a Drain is minted outside drain.rs: {offenders:?}"
    );
    let drain = std::fs::read_to_string(src.join("drain.rs")).expect("reads");
    assert_eq!(
        drain.matches("for_scheduled_run()").count(),
        3,
        "drain.rs names the constructor exactly at its definition, in `run`, and in `for_tests`"
    );
    let lib = std::fs::read_to_string(src.join("lib.rs")).expect("reads");
    assert!(
        lib.contains("drain::run(ctx, cron, &self.shared)"),
        "Module::scheduled is the entry point"
    );
}

/// Nothing outside the Worker drives a GitHub import: the CLI does not
/// depend on this crate.
#[test]
fn the_cli_cannot_reach_the_github_store() {
    let cli =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../livingbrain-cli/Cargo.toml");
    let manifest = std::fs::read_to_string(cli).expect("the CLI manifest reads");
    assert!(
        !manifest.contains("livingbrain-github"),
        "the CLI must not depend on the GitHub crates"
    );
}
