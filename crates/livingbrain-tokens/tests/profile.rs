//! Names and avatars: `PATCH /v1/me`, `PUT|DELETE /v1/me/avatar`,
//! `PATCH /v1/workspaces/current`, `PUT|DELETE /v1/workspaces/current/avatar`
//! and `GET /v1/avatars/{id}` — what each accepts, who may call it, what is
//! stored, and who may read it back.

mod support;

use cratefield_core::axum::http::{StatusCode, header};
use cratefield_core::{Blob, Kid, Payload, Signer, Statement};
use serde_json::{Value, json};
use support::*;

const ME: &str = "/v1/me";
const MY_AVATAR: &str = "/v1/me/avatar";
const WORKSPACE_PROFILE: &str = "/v1/workspaces/current";
const WORKSPACE_AVATAR: &str = "/v1/workspaces/current/avatar";

// ---------------------------------------------------------------------------
// Fixtures: small images built byte by byte, each carrying metadata a clean
// copy must not.

/// A marker every fixture hides in its metadata.
const SECRET: &[u8] = b"GPS 51.5007N 0.1246W";

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(data.len())
        .expect("small")
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut covered = kind.to_vec();
    covered.extend_from_slice(data);
    out.extend_from_slice(&crc32(&covered).to_be_bytes());
    out
}

/// A `width`×`height` PNG with a text chunk and an `eXIf` chunk.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = width.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    out.extend(png_chunk(b"IHDR", &ihdr));
    let mut text = b"Comment\0".to_vec();
    text.extend_from_slice(SECRET);
    out.extend(png_chunk(b"tEXt", &text));
    out.extend(png_chunk(b"eXIf", SECRET));
    // One stored-deflate block of one RGBA pixel (filter byte + 4).
    let raw = [0u8, 0x4B, 0xE3, 0xA9, 0xFF];
    let mut zlib = vec![0x78, 0x01, 0x01, 0x05, 0x00, 0xFA, 0xFF];
    zlib.extend_from_slice(&raw);
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out.extend(png_chunk(b"IDAT", &zlib));
    out.extend(png_chunk(b"IEND", &[]));
    out
}

/// A JPEG's header segments with an EXIF `APP1` and a comment, then a scan.
fn jpeg() -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8];
    let segment = |out: &mut Vec<u8>, marker: u8, data: &[u8]| {
        out.extend_from_slice(&[0xFF, marker]);
        out.extend_from_slice(&u16::try_from(data.len() + 2).expect("small").to_be_bytes());
        out.extend_from_slice(data);
    };
    segment(&mut out, 0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    let mut exif = b"Exif\0\0".to_vec();
    exif.extend_from_slice(SECRET);
    segment(&mut out, 0xE1, &exif);
    segment(&mut out, 0xFE, SECRET);
    // SOF0: precision, height 2, width 3, one component.
    segment(&mut out, 0xC0, &[8, 0, 2, 0, 3, 1, 1, 0x11, 0]);
    segment(&mut out, 0xDA, &[1, 1, 0, 0, 0x3F, 0]);
    out.extend_from_slice(&[0x12, 0x34, 0xFF, 0x00, 0x56]);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

/// An extended WebP: `VP8X` flagging EXIF, a lossless frame, an `EXIF` chunk.
fn webp() -> Vec<u8> {
    let chunk = |kind: &[u8; 4], data: &[u8]| {
        let mut out = kind.to_vec();
        out.extend_from_slice(&u32::try_from(data.len()).expect("small").to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
        out
    };
    // Flags: EXIF (0x08); canvas 4×5, stored minus one in 24 bits.
    let vp8x = [0x08, 0, 0, 0, 3, 0, 0, 4, 0, 0];
    // VP8L: signature, then (w-1) | (h-1) << 14 in the next 32 bits.
    let bits: u32 = 3 | (4 << 14);
    let mut vp8l = vec![0x2F];
    vp8l.extend_from_slice(&bits.to_le_bytes());
    vp8l.extend_from_slice(&[0, 0, 0]);
    let mut body = b"WEBP".to_vec();
    body.extend(chunk(b"VP8X", &vp8x));
    body.extend(chunk(b"VP8L", &vp8l));
    body.extend(chunk(b"EXIF", SECRET));
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&u32::try_from(body.len()).expect("small").to_le_bytes());
    out.extend(body);
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn avatar_id(url: &Value) -> String {
    url.as_str()
        .and_then(|url| url.strip_prefix("/v1/avatars/"))
        .expect("an avatar url")
        .to_owned()
}

/// A session for `user_id` in another workspace, signed the way the
/// workspaces module seals one.
fn session_in(kit: &Kit, workspace: &str, user_id: &str) -> Caller {
    Caller::browser(
        &kit.harness.signer.sign(&Payload {
            purpose: livingbrain_workspaces::SESSION_PURPOSE.to_owned(),
            subject: json!({
                "team_id": workspace,
                "user_id": user_id,
                "exp": kit.harness.clock.0.unix_timestamp() + 3600,
            })
            .to_string(),
            exp: None,
            kid: Kid::Cur,
        }),
    )
}

/// A second workspace with one member of its own.
async fn second_workspace(kit: &Kit) -> Caller {
    for statement in [
        Statement::with_values(
            "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?)",
            vec![
                text("T1ELSEWHERE"),
                text("Elsewhere"),
                text("U1"),
                text("2026-01-01T00:00:00Z"),
            ],
        ),
        Statement::with_values(
            "INSERT INTO workspace_members (workspace_id, user_id, name, is_admin, updated_at, \
             role) VALUES (?, ?, ?, 0, ?, 'owner')",
            vec![
                text("T1ELSEWHERE"),
                text("U1"),
                text("Mallory"),
                text("2026-01-01T00:00:00Z"),
            ],
        ),
    ] {
        kit.harness.db.execute(&statement).await.expect("seeds");
    }
    session_in(kit, "T1ELSEWHERE", "U1")
}

async fn mint(kit: &Kit, scopes: &[&str]) -> String {
    let made = post_json(
        kit,
        "/v1/tokens",
        &json!({"name": "laptop", "scopes": scopes}),
        &kit.owner(),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.text());
    made.json()["token"].as_str().expect("a token").to_owned()
}

// ---------------------------------------------------------------------------
// /v1/me stays what it was, and grows

#[pollster::test]
async fn me_keeps_every_field_it_had_and_adds_the_new_ones_as_null() {
    let kit = signed_in().await;
    let body = get(&kit, ME, &kit.owner()).await.json();
    // What the Mac app, the CLI and Colonizer already decode.
    assert_eq!(body["workspace"]["id"], WORKSPACE);
    assert_eq!(body["workspace"]["name"], "A workspace");
    assert_eq!(body["principal"]["kind"], "member");
    assert_eq!(body["principal"]["id"], OWNER);
    assert_eq!(body["principal"]["name"], "A member");
    assert_eq!(body["principal"]["role"], "owner");
    assert_eq!(body["principal"]["is_admin"], true);
    assert_eq!(body["credential"]["kind"], "session");
    // The additions, unset.
    assert_eq!(body["principal"]["display_name"], Value::Null);
    assert_eq!(body["principal"]["avatar_url"], Value::Null);
    assert_eq!(body["workspace"]["avatar_url"], Value::Null);
}

// ---------------------------------------------------------------------------
// Names

#[pollster::test]
async fn a_member_chooses_a_name_and_everybody_reads_it() {
    let kit = signed_in().await;
    let changed = patch_json(
        &kit,
        ME,
        &json!({"display_name": "  Ada Lovelace  "}),
        &kit.owner(),
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.text());
    let body = changed.json();
    assert_eq!(body["principal"]["name"], "Ada Lovelace");
    assert_eq!(body["principal"]["display_name"], "Ada Lovelace");

    // The roster, the session read-back and another member's view agree.
    let roster = get(&kit, "/v1/workspaces/members", &kit.other())
        .await
        .json();
    let me = roster
        .as_array()
        .expect("a list")
        .iter()
        .find(|m| m["user_id"] == OWNER)
        .expect("the owner is listed")
        .clone();
    assert_eq!(me["name"], "Ada Lovelace");
    assert_eq!(me["display_name"], "Ada Lovelace");
    let session = get(&kit, "/v1/workspaces/me", &kit.owner()).await.json();
    assert_eq!(session["member"]["name"], "Ada Lovelace");

    // Only the caller's own row changed.
    let other = roster
        .as_array()
        .expect("a list")
        .iter()
        .find(|m| m["user_id"] == OTHER)
        .expect("the other member is listed")
        .clone();
    assert_eq!(other["name"], "A member");
    assert_eq!(other["display_name"], Value::Null);

    // `null` goes back to the name the sign-in method reports.
    let cleared = patch_json(&kit, ME, &json!({"display_name": null}), &kit.owner()).await;
    assert_eq!(cleared.status, StatusCode::OK, "{}", cleared.text());
    assert_eq!(cleared.json()["principal"]["name"], "A member");
    assert_eq!(cleared.json()["principal"]["display_name"], Value::Null);
}

#[pollster::test]
async fn a_chosen_name_survives_the_sign_in_mirror() {
    let kit = signed_in().await;
    patch_json(&kit, ME, &json!({"display_name": "Ada"}), &kit.owner()).await;
    // What a Slack `user_change` does: rewrite the mirrored name.
    kit.harness
        .db
        .execute(&Statement::with_values(
            "UPDATE workspace_members SET name = ? WHERE user_id = ?",
            vec![text("ada.l (Slack)"), text(OWNER)],
        ))
        .await
        .expect("mirrors");
    let body = get(&kit, ME, &kit.owner()).await.json();
    assert_eq!(body["principal"]["name"], "Ada");
    patch_json(&kit, ME, &json!({"display_name": null}), &kit.owner()).await;
    let body = get(&kit, ME, &kit.owner()).await.json();
    assert_eq!(body["principal"]["name"], "ada.l (Slack)");
}

#[pollster::test]
async fn names_are_one_to_sixty_four_visible_characters() {
    let kit = signed_in().await;
    for bad in [
        json!({"display_name": ""}),
        json!({"display_name": "    "}),
        json!({"display_name": "a".repeat(65)}),
        json!({"display_name": "Ada\u{0000}"}),
        json!({"display_name": "Ada\nLovelace"}),
        json!({"display_name": "Ada\u{202E}"}),
        json!({"display_name": 42}),
        json!({}),
    ] {
        let refused = patch_json(&kit, ME, &bad, &kit.owner()).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad}: {}",
            refused.text()
        );
    }
    let ok = patch_json(
        &kit,
        ME,
        &json!({"display_name": "a".repeat(64)}),
        &kit.owner(),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text());
}

#[pollster::test]
async fn a_cookie_write_from_another_site_is_refused() {
    let kit = signed_in().await;
    let refused = patch_json(
        &kit,
        ME,
        &json!({"display_name": "Mallory"}),
        &kit.owner_elsewhere(),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
    let refused = put_bytes(
        &kit,
        MY_AVATAR,
        "image/png",
        png(8, 8),
        &kit.owner_elsewhere(),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
    let refused = delete(&kit, MY_AVATAR, &kit.owner_elsewhere()).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
    let refused = patch_json(
        &kit,
        WORKSPACE_PROFILE,
        &json!({"name": "Pwned"}),
        &kit.owner_elsewhere(),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
    let refused = put_bytes(
        &kit,
        WORKSPACE_AVATAR,
        "image/png",
        png(8, 8),
        &kit.owner_elsewhere(),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
    assert_eq!(
        get(&kit, ME, &kit.owner()).await.json()["principal"]["name"],
        "A member"
    );
}

#[pollster::test]
async fn a_full_token_writes_and_a_narrowed_one_does_not() {
    let kit = signed_in().await;
    let narrowed = mint(&kit, &["shared"]).await;
    let refused = patch_json(
        &kit,
        ME,
        &json!({"display_name": "Bot"}),
        &Caller::Token(narrowed.clone()),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
    let refused = put_bytes(
        &kit,
        MY_AVATAR,
        "image/png",
        png(8, 8),
        &Caller::Token(narrowed),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());

    let full = mint(&kit, &[]).await;
    let ok = patch_json(
        &kit,
        ME,
        &json!({"display_name": "Ada via CLI"}),
        &Caller::Token(full.clone()),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text());
    assert_eq!(ok.json()["principal"]["name"], "Ada via CLI");
    let uploaded = put_bytes(
        &kit,
        MY_AVATAR,
        "image/png",
        png(8, 8),
        &Caller::Token(full),
    )
    .await;
    assert_eq!(uploaded.status, StatusCode::OK, "{}", uploaded.text());
}

#[pollster::test]
async fn nobody_writes_or_reads_anything() {
    let kit = signed_in().await;
    for status in [
        patch_json(&kit, ME, &json!({"display_name": "x"}), &Caller::Anonymous)
            .await
            .status,
        put_bytes(&kit, MY_AVATAR, "image/png", png(8, 8), &Caller::Anonymous)
            .await
            .status,
        get(
            &kit,
            "/v1/avatars/av_01HZZZBBBBBBBBBBBBBBBBBBBB",
            &Caller::Anonymous,
        )
        .await
        .status,
    ] {
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

// ---------------------------------------------------------------------------
// Avatars: what is accepted

#[pollster::test]
async fn an_avatar_is_stored_without_its_metadata_and_served_as_an_image() {
    let kit = signed_in().await;
    let uploaded = put_bytes(&kit, MY_AVATAR, "image/png", png(8, 8), &kit.owner()).await;
    assert_eq!(uploaded.status, StatusCode::OK, "{}", uploaded.text());
    let url = uploaded.json()["principal"]["avatar_url"].clone();
    let id = avatar_id(&url);

    // One physical object, on the shared avatar space, without the secret.
    let stored = kit
        .blob
        .get(&format!("avatars/{WORKSPACE}/{id}"))
        .await
        .expect("reads")
        .expect("stored on the one avatar space");
    assert_eq!(stored.content_type, "image/png");
    assert!(!contains(&stored.bytes, SECRET), "metadata is stripped");
    assert!(!contains(&stored.bytes, b"tEXt"));
    assert!(contains(&stored.bytes, b"IDAT"), "the pixels stay");

    // Served to a member of the workspace, as the image it is.
    let served = get(&kit, url.as_str().expect("a url"), &kit.other()).await;
    assert_eq!(served.status, StatusCode::OK, "{}", served.text());
    let header = |name: header::HeaderName| {
        served
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(header(header::CONTENT_TYPE), "image/png");
    assert_eq!(header(header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert_eq!(
        header(header::CONTENT_DISPOSITION),
        "inline; filename=\"avatar.png\""
    );
    // The harness stamps `no-store` on every `/v1` answer; the venture
    // relaxes it to a private immutable cache for this route alone
    // (`avatar_cache_control` in `livingbrain-venture`).
    assert!(!header(header::CACHE_CONTROL).contains("public"));
    assert_eq!(served.body, stored.bytes);

    // The roster carries the URL too.
    let roster = get(&kit, "/v1/workspaces/members", &kit.other())
        .await
        .json();
    assert!(
        roster
            .as_array()
            .expect("a list")
            .iter()
            .any(|m| m["user_id"] == OWNER && m["avatar_url"] == url)
    );
}

#[pollster::test]
async fn jpeg_and_webp_lose_their_metadata_too() {
    let kit = signed_in().await;
    for (mime, bytes, keep) in [
        ("image/jpeg", jpeg(), &b"JFIF"[..]),
        ("image/webp", webp(), &b"VP8L"[..]),
    ] {
        let uploaded = put_bytes(&kit, MY_AVATAR, mime, bytes, &kit.owner()).await;
        assert_eq!(
            uploaded.status,
            StatusCode::OK,
            "{mime}: {}",
            uploaded.text()
        );
        let url = uploaded.json()["principal"]["avatar_url"].clone();
        let served = get(&kit, url.as_str().expect("a url"), &kit.owner()).await;
        assert_eq!(served.status, StatusCode::OK);
        assert_eq!(
            served.headers.get(header::CONTENT_TYPE).expect("typed"),
            mime
        );
        assert!(!contains(&served.body, SECRET), "{mime} keeps no metadata");
        assert!(!contains(&served.body, b"Exif"), "{mime} keeps no EXIF");
        assert!(contains(&served.body, keep), "{mime} keeps its image data");
    }
}

#[pollster::test]
async fn only_png_jpeg_and_webp_by_their_bytes() {
    let kit = signed_in().await;
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#;
    let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec();
    let cases: [(&str, Vec<u8>, StatusCode); 8] = [
        (
            "image/svg+xml",
            svg.to_vec(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        // An SVG that claims to be a PNG is still not one.
        (
            "image/png",
            svg.to_vec(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        ("image/gif", gif.clone(), StatusCode::UNSUPPORTED_MEDIA_TYPE),
        ("image/png", gif, StatusCode::UNSUPPORTED_MEDIA_TYPE),
        ("text/html", png(8, 8), StatusCode::UNSUPPORTED_MEDIA_TYPE),
        // A PNG sent as a JPEG: the bytes and the label disagree.
        ("image/jpeg", png(8, 8), StatusCode::UNSUPPORTED_MEDIA_TYPE),
        // A PNG that stops halfway.
        (
            "image/png",
            png(8, 8)[..30].to_vec(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        // A small file that declares a canvas no avatar needs.
        ("image/png", png(5000, 8), StatusCode::UNPROCESSABLE_ENTITY),
    ];
    for (mime, bytes, want) in cases {
        for path in [MY_AVATAR, WORKSPACE_AVATAR] {
            let refused = put_bytes(&kit, path, mime, bytes.clone(), &kit.owner()).await;
            assert_eq!(refused.status, want, "{path} {mime}: {}", refused.text());
        }
    }
    assert!(kit.blob.is_empty(), "nothing refused was stored");
}

#[pollster::test]
async fn an_avatar_is_at_most_512_kb() {
    let kit = signed_in().await;
    let mut big = png(8, 8);
    // Pad with an ancillary chunk so the file is still a PNG, just too big.
    let pad = vec![0u8; 512 * 1024];
    let end = big.len() - 12;
    let mut chunk = u32::try_from(pad.len())
        .expect("fits")
        .to_be_bytes()
        .to_vec();
    chunk.extend_from_slice(b"zzZz");
    chunk.extend_from_slice(&pad);
    chunk.extend_from_slice(&[0, 0, 0, 0]);
    big.splice(end..end, chunk);
    let refused = put_bytes(&kit, MY_AVATAR, "image/png", big, &kit.owner()).await;
    assert_eq!(
        refused.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        refused.text()
    );
    assert!(kit.blob.is_empty());
}

// ---------------------------------------------------------------------------
// Avatars: replacing, removing, and who may read one

#[pollster::test]
async fn replacing_an_avatar_deletes_the_old_one_and_delete_goes_back_to_initials() {
    let kit = signed_in().await;
    let first = put_bytes(&kit, MY_AVATAR, "image/png", png(8, 8), &kit.owner()).await;
    let first_url = first.json()["principal"]["avatar_url"].clone();
    let second = put_bytes(&kit, MY_AVATAR, "image/jpeg", jpeg(), &kit.owner()).await;
    let second_url = second.json()["principal"]["avatar_url"].clone();
    assert_ne!(first_url, second_url, "a new upload is a new id");
    assert_eq!(kit.blob.len(), 1, "the old image is gone");
    assert_eq!(
        get(&kit, first_url.as_str().expect("url"), &kit.owner())
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&kit, second_url.as_str().expect("url"), &kit.owner())
            .await
            .status,
        StatusCode::OK
    );

    let removed = delete(&kit, MY_AVATAR, &kit.owner()).await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT, "{}", removed.text());
    assert!(kit.blob.is_empty());
    assert_eq!(
        get(&kit, ME, &kit.owner()).await.json()["principal"]["avatar_url"],
        Value::Null
    );
    assert_eq!(
        get(&kit, second_url.as_str().expect("url"), &kit.owner())
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[pollster::test]
async fn another_workspace_cannot_read_an_avatar_even_with_its_id() {
    let kit = signed_in().await;
    let uploaded = put_bytes(&kit, MY_AVATAR, "image/png", png(8, 8), &kit.owner()).await;
    let url = uploaded.json()["principal"]["avatar_url"].clone();
    let workspace = put_bytes(&kit, WORKSPACE_AVATAR, "image/png", png(8, 8), &kit.owner()).await;
    let workspace_url = workspace.json()["avatar_url"].clone();
    let stranger = second_workspace(&kit).await;
    for url in [url, workspace_url] {
        let refused = get(&kit, url.as_str().expect("url"), &stranger).await;
        assert_eq!(refused.status, StatusCode::NOT_FOUND, "{url}");
    }
    // And an id that is not an id at all is the same answer.
    for path in ["/v1/avatars/av_nope", "/v1/avatars/..%2F..%2Fpages"] {
        assert_eq!(
            get(&kit, path, &kit.owner()).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[pollster::test]
async fn a_removed_members_avatar_is_no_longer_served() {
    let kit = signed_in().await;
    let uploaded = put_bytes(&kit, MY_AVATAR, "image/png", png(8, 8), &kit.other()).await;
    assert_eq!(uploaded.status, StatusCode::OK, "{}", uploaded.text());
    let url = uploaded.json()["principal"]["avatar_url"].clone();
    let removed = delete(
        &kit,
        &format!("/v1/workspaces/members/{OTHER}"),
        &kit.owner(),
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT, "{}", removed.text());
    assert_eq!(
        get(&kit, url.as_str().expect("url"), &kit.owner())
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------------------------
// The workspace's name and avatar

#[pollster::test]
async fn an_owner_names_the_workspace_and_gives_it_an_avatar() {
    let kit = signed_in().await;
    let renamed = patch_json(
        &kit,
        WORKSPACE_PROFILE,
        &json!({"name": " Kestrel Labs "}),
        &kit.owner(),
    )
    .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text());
    assert_eq!(renamed.json()["name"], "Kestrel Labs");

    let uploaded = put_bytes(&kit, WORKSPACE_AVATAR, "image/webp", webp(), &kit.owner()).await;
    assert_eq!(uploaded.status, StatusCode::OK, "{}", uploaded.text());
    let url = uploaded.json()["avatar_url"].clone();
    let id = avatar_id(&url);
    assert!(
        kit.blob
            .get(&format!("avatars/{WORKSPACE}/{id}"))
            .await
            .expect("reads")
            .is_some()
    );

    // Every reader of the workspace sees both.
    let me = get(&kit, ME, &kit.other()).await.json();
    assert_eq!(me["workspace"]["name"], "Kestrel Labs");
    assert_eq!(me["workspace"]["avatar_url"], url);
    let session = get(&kit, "/v1/workspaces/me", &kit.other()).await.json();
    assert_eq!(session["workspace"]["name"], "Kestrel Labs");
    assert_eq!(session["workspace"]["avatar_url"], url);
    let switcher = get(&kit, "/v1/workspaces", &kit.owner()).await.json();
    let current = switcher
        .as_array()
        .expect("a list")
        .iter()
        .find(|w| w["current"] == true)
        .expect("the current workspace")
        .clone();
    assert_eq!(current["name"], "Kestrel Labs");
    assert_eq!(current["avatar_url"], url);
    assert_eq!(
        get(&kit, url.as_str().expect("url"), &kit.other())
            .await
            .status,
        StatusCode::OK
    );

    let removed = delete(&kit, WORKSPACE_AVATAR, &kit.owner()).await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert!(kit.blob.is_empty());
}

#[pollster::test]
async fn a_member_cannot_change_the_workspace() {
    let kit = signed_in().await;
    let refused = patch_json(
        &kit,
        WORKSPACE_PROFILE,
        &json!({"name": "Mine now"}),
        &kit.other(),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
    let refused = put_bytes(&kit, WORKSPACE_AVATAR, "image/png", png(8, 8), &kit.other()).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
    let refused = delete(&kit, WORKSPACE_AVATAR, &kit.other()).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
    assert!(kit.blob.is_empty());
    assert_eq!(
        get(&kit, ME, &kit.other()).await.json()["workspace"]["name"],
        "A workspace"
    );
    // A workspace name follows the same rules as a person's.
    for bad in ["", "   ", &"w".repeat(65), "Acme\u{0007}"] {
        let refused =
            patch_json(&kit, WORKSPACE_PROFILE, &json!({"name": bad}), &kit.owner()).await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}");
    }
}

#[pollster::test]
async fn the_workspace_routes_take_no_bearer() {
    let kit = signed_in().await;
    let full = mint(&kit, &[]).await;
    let refused = patch_json(
        &kit,
        WORKSPACE_PROFILE,
        &json!({"name": "Scripted"}),
        &Caller::Token(full),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        refused.text()
    );
}
