//! `livingbrain apps` and `livingbrain public`: the owner's side of a
//! connected app (issues #51 and #146; ADR 0005).
//!
//! ```text
//! livingbrain apps issue earthos          # connect it and print its credential, once
//! livingbrain apps list                   # the connected apps and their keys' prefixes
//! livingbrain apps revoke earthos         # revoke every key, now
//! livingbrain apps revoke earthos --key lba_0123…   # one key
//! livingbrain apps export earthos user:42 # everything it keeps for one subject
//! livingbrain apps forget earthos user:42 # erase it
//!
//! livingbrain public publish refund-policy --url https://…
//! livingbrain public unpublish refund-policy
//! livingbrain public list
//! ```
//!
//! Every command is a thin client of `/v1/apps` and `/v1/public`, which
//! answer only to a workspace **owner** signed in with `livingbrain login`
//! (a whole credential, not a narrowed token). The credential `issue` prints
//! is shown here and nowhere else: only its hash is stored.

use clap::Subcommand;
use serde_json::{Value, json};

use crate::api::{Client, encode_segment};
use crate::{CliResult, Out, text};

#[derive(Subcommand)]
pub enum AppsCommand {
    /// Connect an app and print its credential (shown once)
    Issue {
        /// The app's name: lowercase letters, digits and `-` (`earthos`)
        app: String,
        /// Make the credential expire after this many days
        #[arg(long)]
        expires_in_days: Option<u32>,
    },
    /// List the connected apps and their keys (never a secret)
    List,
    /// Revoke an app's credential: every key, or one with `--key`
    Revoke {
        /// The app's name
        app: String,
        /// Revoke only the key with this public prefix (`lba_…`)
        #[arg(long)]
        key: Option<String>,
    },
    /// Print everything an app keeps for one subject (a data-subject export)
    Export {
        /// The app's name
        app: String,
        /// The subject, exactly as the app names it (`user:42`)
        subject: String,
    },
    /// Erase everything an app keeps for one subject
    Forget {
        /// The app's name
        app: String,
        /// The subject, exactly as the app names it (`user:42`)
        subject: String,
    },
}

#[derive(Subcommand)]
pub enum PublicCommand {
    /// Publish a page of the shared space to customers
    Publish {
        /// The page's slug
        slug: String,
        /// Where a citation links to (http or https); the web app's page when omitted
        #[arg(long)]
        url: Option<String>,
    },
    /// Withdraw a published page
    Unpublish {
        /// The page's slug
        slug: String,
    },
    /// List the published pages
    List,
}

pub fn apps(client: &Client, out: &Out, command: &AppsCommand) -> CliResult<()> {
    match command {
        AppsCommand::Issue {
            app,
            expires_in_days,
        } => {
            let mut body = json!({ "app": app });
            if let Some(days) = expires_in_days {
                body["expires_in_days"] = json!(days);
            }
            let value = client.send_json("POST", "/v1/apps", Some(&body))?;
            out.json_or(&value, || {
                println!("Connected {}.", text(&value, "app"));
                println!("Key prefix: {}", text(&value, "prefix"));
                println!("Credential (shown once; store it as a secret):");
                println!("{}", text(&value, "key"));
                println!(
                    "Revoke it with: livingbrain apps revoke {} --key {}",
                    text(&value, "app"),
                    text(&value, "prefix")
                );
            });
        }
        AppsCommand::List => {
            let value = client.send_json("GET", "/v1/apps", None)?;
            out.json_or(&value, || render_apps(&value));
        }
        AppsCommand::Revoke { app, key } => {
            let path = match key {
                Some(prefix) => format!(
                    "/v1/apps/{}/keys/{}",
                    encode_segment(app),
                    encode_segment(prefix)
                ),
                None => format!("/v1/apps/{}", encode_segment(app)),
            };
            let value = client.send_json("DELETE", &path, None)?;
            out.json_or(&value, || match key {
                Some(prefix) => println!("Revoked key {prefix} of {app}."),
                None => println!(
                    "Revoked {app}: {} key(s) now refused.",
                    value["revoked_keys"].as_u64().unwrap_or(0)
                ),
            });
        }
        AppsCommand::Export { app, subject } => {
            let path = format!(
                "/v1/apps/{}/subjects/{}",
                encode_segment(app),
                encode_segment(subject)
            );
            let value = client.send_json("GET", &path, None)?;
            // An export is data: print it whole either way.
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_default()
            );
        }
        AppsCommand::Forget { app, subject } => {
            let path = format!(
                "/v1/apps/{}/subjects/{}",
                encode_segment(app),
                encode_segment(subject)
            );
            let value = client.send_json("DELETE", &path, None)?;
            out.json_or(&value, || {
                println!(
                    "Erased {} memories of {subject} in {app}.",
                    value["forgotten"].as_u64().unwrap_or(0)
                );
            });
        }
    }
    Ok(())
}

pub fn public(client: &Client, out: &Out, command: &PublicCommand) -> CliResult<()> {
    match command {
        PublicCommand::Publish { slug, url } => {
            let mut body = json!({ "slug": slug });
            if let Some(url) = url {
                body["url"] = Value::String(url.clone());
            }
            let value = client.send_json("POST", "/v1/public/pages", Some(&body))?;
            out.json_or(&value, || {
                println!("Published {slug}: {}", text(&value, "url"));
            });
        }
        PublicCommand::Unpublish { slug } => {
            let path = format!("/v1/public/pages/{}", encode_segment(slug));
            let value = client.send_json("DELETE", &path, None)?;
            out.json_or(&value, || println!("Unpublished {slug}."));
        }
        PublicCommand::List => {
            let value = client.send_json("GET", "/v1/public", None)?;
            out.json_or(&value, || {
                let pages = value["pages"].as_array().cloned().unwrap_or_default();
                if pages.is_empty() {
                    println!("Nothing is published.");
                }
                for page in pages {
                    println!(
                        "{}  {}  {}",
                        text(&page, "slug"),
                        text(&page, "title"),
                        text(&page, "url")
                    );
                }
            });
        }
    }
    Ok(())
}

fn render_apps(value: &Value) {
    let apps = value["apps"].as_array().cloned().unwrap_or_default();
    if apps.is_empty() {
        println!("No app is connected.");
    }
    for app in apps {
        println!(
            "{}{}",
            text(&app, "app"),
            if app["disabled"].as_bool().unwrap_or(false) {
                " (revoked)"
            } else {
                ""
            }
        );
        for key in app["keys"].as_array().cloned().unwrap_or_default() {
            let state = if key["revoked_at"].is_string() {
                "revoked"
            } else {
                "live"
            };
            println!(
                "  {}  {state}  last used {}",
                text(&key, "prefix"),
                key["last_used_at"].as_str().unwrap_or("never")
            );
        }
    }
}
