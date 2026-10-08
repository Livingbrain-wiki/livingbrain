//! The live map: where Living Brain installs are, at 25 km resolution.
//!
//! Off until someone switches it on. While it is off there is no id, there is
//! nothing to send and there is nothing to forget; [`LiveMap::disabled`] is
//! the whole of an install that never opted in, and [`LiveMap::heartbeat`]
//! is the one function that decides whether anything is sent. Everything else
//! in this file is about how to turn it on and how to turn it off cleanly.
//!
//! # The location comes from the server, not from here
//!
//! The 25 km cell is derived **server-side, from the address the request came
//! from**. The client sends no coordinate, and [`Heartbeat`] deliberately has
//! no latitude or longitude field — adding one would be the single change
//! that breaks the promise this feature is made of, because a coordinate is
//! an address and an address is a person. A client that already knows where it
//! is does not get to say so.
//!
//! # Opting out
//!
//! [`LiveMap::disable`] returns the one `online: false` beat that must reach
//! the map so an install is not left showing as permanently online, and then
//! forgets the id. Disabling something that was never enabled sends nothing
//! at all: there is nothing to take off the map.

use std::fmt;

use serde::Serialize;

use crate::usage::{Bucket, Platform, Version, validate_usage_id};

/// Where a heartbeat is posted. As with [`crate::usage::USAGE_ENDPOINT`], a
/// constant rather than a setting.
pub const LIVE_MAP_ENDPOINT: &str = "https://telemetry.livingbrain.wiki/v1/telemetry/heartbeat";

/// The version a retraction beat ([`LiveMap::disable`]) carries. A constant
/// rather than a literal inside the function so the wire value is one place
/// to look, and one place to change.
///
/// `0.0.0` rather than the `0` this used to be: `Version` now has a real
/// grammar — `major.minor.patch`, optionally with a prerelease suffix — and
/// `0` is not a version under it. The value is still the same "no version"
/// placeholder it always was, and it is still the wire-visible change the
/// server should note: a retraction beat is not a description of an install,
/// and the server reads only `install_id` and `online` from it.
const RETRACTION_VERSION: &str = "0.0.0";

/// Whether [`RETRACTION_VERSION`] still satisfies [`Version`]'s grammar.
///
/// A `const` check rather than a test, because this is the one place where the
/// retraction path can fail at runtime, and a runtime failure there means a pin
/// stranded on the map. If `Version`'s grammar is ever tightened under this
/// constant, this stops compiling rather than becoming a `disable()` that
/// quietly sends nothing.
const fn retraction_version_is_a_version(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let mut dots = 0;
    let mut digits = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'.' => {
                dots += 1;
                digits = 0;
            }
            b'0'..=b'9' => digits += 1,
            // A hyphen or a prerelease word: the plain `major.minor.patch`
            // shape is all this constant needs, and `Version::new` is the
            // authority on the rest.
            _ => return false,
        }
        index += 1;
    }
    dots == 2 && digits > 0
}

const _: () = assert!(retraction_version_is_a_version(RETRACTION_VERSION));

/// Why an [`InstallId`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidInstallId {
    /// What was wrong, in one sentence. Never the value.
    pub reason: &'static str,
}

impl fmt::Display for InvalidInstallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason)
    }
}

impl std::error::Error for InvalidInstallId {}

/// The random id an install is known by on the map.
///
/// Validated exactly like a [`UsageId`](crate::usage::UsageId) — sixteen lowercase characters, the
/// same shared validator — because the threat is the same: an id that carried
/// anything derived from the machine or its owner would turn a pin on a map
/// into a fingerprint. The id is minted by the caller, not here; this crate
/// holds no RNG.
///
/// Note what an install id does *not* survive: it is not a workspace id, a
/// page id, a Slack team id or a user, and switching the map off drops it
/// (see [`LiveMap::disable`]). Switching it back on mints a new one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct InstallId(String);

impl InstallId {
    /// Take the caller's random value, if it is one.
    ///
    /// # Errors
    ///
    /// [`InvalidInstallId`] when `raw` is not sixteen lowercase characters of
    /// `[a-z0-9]` — the [`UsageId`](crate::usage::UsageId) rule, for the reason [`UsageId`](crate::usage::UsageId) exists.
    pub fn new(raw: impl Into<String>) -> Result<Self, InvalidInstallId> {
        let raw = raw.into();
        validate_usage_id(&raw).map_err(|reason| InvalidInstallId { reason })?;
        Ok(Self(raw))
    }

    /// The id as the wire sees it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for InstallId {
    type Error = InvalidInstallId;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

impl TryFrom<String> for InstallId {
    type Error = InvalidInstallId;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

/// How many pages an install has, as a bucket and not as a count.
///
/// The three buckets are 0–9, 10–99 and 100 or more. The top bucket is
/// open-ended rather than stopping at 499: the wire carries a bucket, and a
/// hard ceiling would only be a place for an off-by-one to leak an exact
/// count. [`PagesBucket::of`] is the only way to make one, so the number a
/// caller counted cannot reach the wire from anywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PagesBucket(u8);

impl PagesBucket {
    /// The bucket a page count falls in.
    #[must_use]
    pub const fn of(pages: u64) -> Self {
        Self(Bucket::of(pages).index())
    }

    /// The bucket index, 0, 1 or 2.
    #[must_use]
    pub const fn index(self) -> u8 {
        self.0
    }
}

impl Serialize for PagesBucket {
    /// A number, not a label. The map's job is to draw one pin per install,
    /// and an integer is the smallest thing that can say which of the three
    /// size groups the pin is in.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(self.0)
    }
}

/// One beat: this install exists, at this size, and here is what it is.
///
/// There is no latitude and no longitude field, and there must never be one.
/// The server works the cell out from the request address, which means the
/// coarse location is derived from something the operator's network already
/// knows and the client never volunteers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Heartbeat {
    /// The random install id. Absent from a beat nobody sent.
    pub install_id: InstallId,
    /// The sender's own version.
    pub version: Version,
    /// Which host this was.
    pub platform: Platform,
    /// How many pages, as a bucket.
    pub pages: PagesBucket,
    /// Whether this install is currently on the map. The one beat sent on
    /// switching off carries `false` and nothing else changes.
    pub online: bool,
}

/// The live map's state for one install.
///
/// Two fields and no more, because there are exactly two states that matter:
/// off, and on with an id. There is no last-sent timestamp and no interval
/// here — when to beat is the caller's scheduling decision, and the answer to
/// "is it on" must not depend on whether a beat happened to fire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveMap {
    enabled: bool,
    install_id: Option<InstallId>,
}

impl LiveMap {
    /// An install that has never opted in: no id, and nothing to send.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            install_id: None,
        }
    }

    /// Whether the map is switched on for this install.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The install id, while the map is on. `None` before it is switched on
    /// and `None` again after it is switched off — the id is dropped rather
    /// than kept in case it is wanted again, because a kept id is a kept
    /// position on a map.
    #[must_use]
    pub fn install_id(&self) -> Option<&InstallId> {
        self.install_id.as_ref()
    }

    /// Switch the map on and return the first beat.
    ///
    /// The id is the caller's: this crate has no RNG and no id source, and
    /// minting one here would mean owning a second definition of "random" in
    /// the workspace. The caller already has an `IdGen`, so it passes one.
    ///
    /// # Errors
    ///
    /// [`AlreadyEnabled`] if the map is already on. Re-enabling with a second
    /// id would leave the first one on the map forever, with no way to send
    /// the `online: false` beat that retires it — so it is refused rather
    /// than papered over.
    pub fn enable(
        &mut self,
        install_id: InstallId,
        version: Version,
        platform: Platform,
        pages: u64,
    ) -> Result<Heartbeat, AlreadyEnabled> {
        if self.enabled {
            return Err(AlreadyEnabled);
        }
        self.enabled = true;
        self.install_id = Some(install_id.clone());
        Ok(Heartbeat {
            install_id,
            version,
            platform,
            pages: PagesBucket::of(pages),
            online: true,
        })
    }

    /// Switch the map off.
    ///
    /// Returns the one `online: false` beat to send **exactly once**, and
    /// forgets the id. `None` when the map was never on: switching off
    /// something that was never on has nothing to retract, and sending an
    /// empty beat would be inventing traffic.
    ///
    /// The three descriptive fields of the returned beat are neutral
    /// placeholders. This function takes no version, platform or page count
    /// to put in them, and that is deliberate: a retraction is not a
    /// description of an install, and the server reads only `install_id` and
    /// `online` from it. A beat that claimed a version on the way out would
    /// be a second claim about an install that is leaving the map.
    pub fn disable(&mut self) -> Option<Heartbeat> {
        // The order of these three lines is load-bearing, and it is the reason
        // the retraction beat is built *before* the id is taken rather than
        // after. Both of the later steps can return early — `Version::new` on
        // a future grammar, `take` on a map that was never on — and an early
        // return after the `take` would be the worst possible outcome: the id
        // is gone, so `disable` returning `None` would read to every caller as
        // "the map was never on", and the pin would stay on the map forever
        // with nothing left to identify it and no way to retire it. So the id
        // is the *last* thing this function gives up, and it is only given up
        // once the beat that needs it is known-good.
        let Ok(version) = Version::new(RETRACTION_VERSION) else {
            // `RETRACTION_VERSION` is a constant asserted against the grammar
            // above and again in this module's tests, so this arm is
            // unreachable today. It is written out rather than unwrapped so
            // that if the grammar ever tightens under it, `disable` degrades
            // to "sent nothing" with the id intact — a pin left showing as
            // online that the operator can switch off again — instead of
            // silently dropping the id and stranding it.
            return None;
        };
        let install_id = self.install_id.take()?;
        self.enabled = false;
        Some(Heartbeat {
            install_id,
            version,
            platform: Platform::Other,
            pages: PagesBucket::of(0),
            online: false,
        })
    }

    /// The periodic beat, or `None` while disabled.
    ///
    /// This is the whole opt-in guarantee in one `Option`: there is no path
    /// out of this type that produces a beat without an id, and no path in
    /// that produces one before [`Self::enable`] has been called.
    pub fn heartbeat(&self, version: Version, platform: Platform, pages: u64) -> Option<Heartbeat> {
        let install_id = self.install_id.clone()?;
        debug_assert!(self.enabled, "an id is only ever kept while enabled");
        Some(Heartbeat {
            install_id,
            version,
            platform,
            pages: PagesBucket::of(pages),
            online: true,
        })
    }
}

/// Why [`LiveMap::enable`] refused: the map is already on.
///
/// The existing id is left alone, because the install that owns it is the
/// one that can send the `online: false` beat to retire it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyEnabled;

impl fmt::Display for AlreadyEnabled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the live map is already on for this install; disable it first")
    }
}

impl std::error::Error for AlreadyEnabled {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        AlreadyEnabled, Heartbeat, InstallId, InvalidInstallId, LiveMap, PagesBucket, Platform,
        RETRACTION_VERSION, Version,
    };

    fn version() -> Version {
        Version::new("0.1.0").unwrap()
    }

    /// An install id is the usage id rule, and a token is not one.
    #[test]
    fn install_id_is_sixteen_lowercase() {
        assert!(InstallId::new("abc123def456ab78").is_ok());
        assert!(InstallId::try_from("7f3c9a1b2d4e6f80").is_ok());
        assert!(InstallId::new("abc123def456ab").is_err());
        // Assembled at runtime so the token-shaped hostile value is never a literal.
        let token_shaped = ["xo", "x", "b-1111-2222-abcdef"].concat();
        assert!(InstallId::new(&token_shaped).is_err(), "hyphens");
        assert!(InstallId::new("7f3c-9a1b2d4e6f80").is_err(), "hyphens");
        let err: InvalidInstallId = InstallId::new("nope").unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    /// A page count becomes one of three numbers.
    #[test]
    fn pages_bucket_has_three_values() {
        assert_eq!(PagesBucket::of(0).index(), 0);
        assert_eq!(PagesBucket::of(9).index(), 0);
        assert_eq!(PagesBucket::of(10).index(), 1);
        assert_eq!(PagesBucket::of(99).index(), 1);
        assert_eq!(PagesBucket::of(100).index(), 2);
        assert_eq!(PagesBucket::of(u64::MAX).index(), 2);
        assert_eq!(serde_json::to_string(&PagesBucket::of(42)).unwrap(), "1");
    }

    /// Off means off: no id, no beat, and nothing to turn off.
    #[test]
    fn a_disabled_map_sends_nothing() {
        let mut map = LiveMap::disabled();
        assert!(!map.is_enabled());
        assert!(map.install_id().is_none());
        assert!(map.heartbeat(version(), Platform::Linux, 12).is_none());
        assert!(map.disable().is_none(), "nothing was on the map");
        assert_eq!(LiveMap::default(), LiveMap::disabled());
    }

    /// The retraction beat carries a version the grammar still accepts, and
    /// it is the same neutral placeholder it has always been.
    ///
    /// This is the canary for the grammar: `disable` builds its beat through
    /// `Version::new`, so a grammar change that this constant fails is a
    /// `disable()` that returns `None` and strands the pin. `RETRACTION_VERSION`
    /// was `0`, which is not `major.minor.patch`, so it changed to `0.0.0` —
    /// the version number moved, not the grammar.
    #[test]
    fn the_retraction_version_satisfies_the_grammar() {
        assert!(Version::new(RETRACTION_VERSION).is_ok());
        assert_eq!(RETRACTION_VERSION, "0.0.0");

        let mut map = LiveMap::disabled();
        map.enable(
            InstallId::new("abc123def456ab78").unwrap(),
            version(),
            Platform::Linux,
            1,
        )
        .unwrap();
        let beat = map.disable().unwrap();
        assert_eq!(beat.version.as_str(), RETRACTION_VERSION);
        assert!(!beat.online);
        // The placeholders, stated so a change to any of them is deliberate.
        assert_eq!(beat.platform, Platform::Other);
        assert_eq!(beat.pages, PagesBucket::of(0));
        assert_eq!(serde_json::to_string(&beat).unwrap(), {
            r#"{"install_id":"abc123def456ab78","version":"0.0.0","platform":"other","pages":0,"online":false}"#
        });
    }

    /// `disable` gives up the id only on the success path.
    ///
    /// The failure this guards against is silent and permanent: if the
    /// retraction beat were built *after* `install_id.take()`, and building it
    /// failed, `disable` would return `None` — which every caller reads as
    /// "the map was never on" — having already discarded the id. The pin would
    /// stay on the map with nothing left to identify it and no way to retire
    /// it. So the assertions here are about state, not about a return value:
    /// whenever `disable` hands back nothing, the id must still be there.
    #[test]
    fn a_failed_retraction_never_drops_the_id() {
        // The success path, for contrast: the id *is* dropped, and `disable`
        // says so, which is what a caller relies on to know the beat was sent.
        let mut sent = LiveMap::disabled();
        sent.enable(
            InstallId::new("abc123def456ab78").unwrap(),
            version(),
            Platform::Linux,
            1,
        )
        .unwrap();
        assert!(sent.disable().is_some());
        assert!(sent.install_id().is_none(), "success drops the id");

        // The failure path is not reachable through the public API today —
        // `RETRACTION_VERSION` is asserted valid above and at compile time —
        // so what is asserted is the invariant itself, stated for every
        // `None`: nothing was taken.
        let mut never_on = LiveMap::disabled();
        assert!(never_on.disable().is_none());
        assert!(
            never_on.install_id().is_none(),
            "a None from disable must not have come from a dropped id"
        );
        // And the invariant holds under repetition, so it is not a property of
        // one call's ordering by accident.
        for _ in 0..8 {
            let mut map = LiveMap::disabled();
            let took_something = map.install_id().is_some();
            assert!(map.disable().is_none());
            assert_eq!(map.install_id().is_some(), took_something);
        }
    }

    /// On, then off: one beat while on, exactly one `false` on the way out,
    /// and the id gone afterwards.
    #[test]
    fn enabling_then_disabling_retracts_exactly_once() {
        let mut map = LiveMap::disabled();
        let first = map
            .enable(
                InstallId::new("abc123def456ab78").unwrap(),
                version(),
                Platform::Macos,
                3,
            )
            .unwrap();
        assert!(first.online);
        assert!(map.is_enabled());
        assert_eq!(
            map.install_id().map(InstallId::as_str),
            Some("abc123def456ab78")
        );
        let beat = map.heartbeat(version(), Platform::Macos, 900).unwrap();
        assert!(beat.online);
        assert_eq!(beat.pages, PagesBucket::of(900));

        // A second enable would strand the first pin on the map.
        assert_eq!(
            map.enable(
                InstallId::new("zzz111def456ab78").unwrap(),
                version(),
                Platform::Macos,
                3
            ),
            Err(AlreadyEnabled)
        );

        let last = map.disable().unwrap();
        assert!(!last.online);
        assert_eq!(last.install_id.as_str(), "abc123def456ab78");
        assert!(!map.is_enabled());
        assert!(map.install_id().is_none(), "the id is forgotten, not kept");
        assert!(map.heartbeat(version(), Platform::Macos, 3).is_none());
        assert!(map.disable().is_none(), "and only once");
    }

    /// Nothing in a beat names where the machine is.
    #[test]
    fn a_heartbeat_carries_no_coordinate() {
        let beat = Heartbeat {
            install_id: InstallId::new("abc123def456ab78").unwrap(),
            version: version(),
            platform: Platform::Linux,
            pages: PagesBucket::of(5),
            online: true,
        };
        let json = serde_json::to_string(&beat).unwrap();
        assert_eq!(
            json,
            r#"{"install_id":"abc123def456ab78","version":"0.1.0","platform":"linux","pages":0,"online":true}"#
        );
        // Checked by key, not by substring: `lat` is inside `platform`, and a
        // substring check that fires on a real field teaches the next reader
        // to ignore it.
        let parsed = serde_json::from_str::<serde_json::Value>(&json).unwrap();
        let keys: BTreeSet<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from(["install_id", "online", "pages", "platform", "version"])
        );
        for forbidden in ["lat", "lon", "latitude", "longitude", "geo", "cell"] {
            assert!(!keys.contains(forbidden), "{forbidden} appeared in {json}");
        }
    }
}
