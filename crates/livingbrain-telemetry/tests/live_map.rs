//! The opt-in guarantee for the live map, checked from outside the crate.
//!
//! One invariant, stated once and checked three ways: **nothing is sent until
//! the map has been switched on, and switching it off sends exactly one
//! `online: false` beat.** A disabled [`LiveMap`] has no id, answers `None`
//! to [`LiveMap::heartbeat`] and `None` to [`LiveMap::disable`]; after
//! enabling and then disabling it has forgotten the id and answers `None` to
//! [`LiveMap::heartbeat`] again.
//!
//! The other thing this file holds is the shape of a beat: the keys are the
//! five declared ones, and a coordinate is not among them. The 25 km cell is
//! derived server-side from the address the request came from, so a beat that
//! grew a latitude would be the bug that this file is here to make impossible
//! to merge.

use livingbrain_telemetry::{
    AlreadyEnabled, Heartbeat, InstallId, LiveMap, PagesBucket, Platform, Version,
};
use serde_json::Value;

/// A random install id of the shape `IdGen` produces: sixteen lowercase
/// characters. Deterministic, so a failing test can be read.
fn install_id() -> InstallId {
    InstallId::new("k3m9q7w2x5b8n4c1").unwrap()
}

fn version() -> Version {
    Version::new("0.1.0").unwrap()
}

/// Off is off: no id, no beat, and nothing to turn off.
#[test]
fn a_disabled_map_sends_nothing() {
    let mut map = LiveMap::disabled();
    assert!(!map.is_enabled(), "off by default");
    assert!(
        map.install_id().is_none(),
        "no id exists before it is switched on"
    );
    assert!(
        map.heartbeat(version(), Platform::Linux, 42).is_none(),
        "the periodic beat is the only send path, and it says no"
    );
    assert!(
        map.disable().is_none(),
        "switching off something that was never on sends nothing"
    );
    // And again, so a caller that retries does not start sending.
    assert!(map.heartbeat(version(), Platform::Linux, 42).is_none());
    assert!(!map.is_enabled());
}

/// On, then off: one beat on, exactly one retraction, and the id is gone.
#[test]
fn enabling_then_disabling_retracts_once_and_forgets_the_id() {
    let mut map = LiveMap::disabled();
    let first = map
        .enable(install_id(), version(), Platform::Linux, 3)
        .expect("the map was off");
    assert!(
        first.online,
        "the first beat says the install is on the map"
    );
    assert_eq!(first.install_id, install_id());
    assert_eq!(first.pages, PagesBucket::of(3));
    assert!(map.is_enabled());

    // Beating while on works and keeps the same id.
    let beat = map
        .heartbeat(version(), Platform::Linux, 250)
        .expect("the map is on");
    assert!(beat.online);
    assert_eq!(beat.install_id, install_id());
    assert_eq!(
        beat.pages,
        PagesBucket::of(250),
        "250 pages is the top bucket"
    );

    // Re-enabling would strand the first pin, so it is refused.
    assert_eq!(
        map.enable(install_id(), version(), Platform::Linux, 3),
        Err(AlreadyEnabled),
        "a second enable cannot retire the first id"
    );

    // Exactly one retraction, then silence.
    let last = map.disable().expect("there was something to retract");
    assert!(!last.online, "the retraction says offline");
    assert_eq!(
        last.install_id,
        install_id(),
        "so the server knows which pin"
    );
    assert!(!map.is_enabled());
    assert!(map.install_id().is_none(), "the id is dropped, not kept");
    assert!(map.heartbeat(version(), Platform::Linux, 250).is_none());
    assert!(map.disable().is_none(), "and only one retraction is sent");
}

/// A beat is the five declared fields. No coordinate, and no `cell` either:
/// the server derives the 25 km cell, so naming one on the wire would imply
/// the client computed it.
#[test]
fn a_beat_has_no_coordinate() {
    let mut map = LiveMap::disabled();
    let beat: Heartbeat = map
        .enable(install_id(), version(), Platform::Macos, 12)
        .unwrap();
    let value: Value = serde_json::from_str(&serde_json::to_string(&beat).unwrap()).unwrap();
    let object = value.as_object().unwrap();

    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["install_id", "online", "pages", "platform", "version"],
        "a beat is exactly these five fields"
    );
    assert_eq!(object["platform"], "macos");
    assert_eq!(object["pages"], 1, "12 pages is the middle bucket");
    assert_eq!(object["online"], true);
    for forbidden in [
        "lat",
        "lon",
        "latitude",
        "longitude",
        "geo",
        "cell",
        "ip",
        "address",
        "host",
        "hostname",
    ] {
        assert!(
            !object.contains_key(forbidden),
            "{forbidden} must never be sent by the client"
        );
    }
}

/// A page count is reduced before it is sent; the count itself is never on
/// the wire, in any of the three ranges.
#[test]
fn a_page_count_is_always_a_bucket() {
    let mut map = LiveMap::disabled();
    map.enable(install_id(), version(), Platform::Linux, 0)
        .unwrap();
    for pages in [0_u64, 9, 10, 99, 100, 499, 500, 100_000] {
        let beat = map.heartbeat(version(), Platform::Linux, pages).unwrap();
        let json = serde_json::to_string(&beat).unwrap();
        let want = PagesBucket::of(pages);
        assert_eq!(beat.pages, want, "{pages} pages");
        assert!(
            json.contains(&format!("\"pages\":{}", want.index())),
            "{pages} pages should be bucket {}",
            want.index()
        );
        assert!(beat.pages.index() <= 2, "three buckets, no more");
        // Below ten pages the bucket index happens to be the count too, so the
        // check that the count never reaches the wire only means anything
        // from the second range up.
        if pages >= 10 {
            assert!(
                !json.contains(&format!("\"pages\":{pages}")),
                "{pages} pages reached the wire as a count"
            );
        }
    }
}
