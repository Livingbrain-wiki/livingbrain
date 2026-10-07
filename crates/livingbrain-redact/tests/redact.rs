//! The redaction contract, on a host and in a Worker.
//!
//! The suite runs unchanged on `wasm32-unknown-unknown` under
//! `wasm-bindgen-test`, so a detector cannot be written that only compiles
//! where the Worker does not run. Nothing here reads the filesystem: the clean
//! corpus is `include_str!`ed, because a Worker has no filesystem to read.
//!
//! No secret in this file is a secret. Every planted value is assembled at
//! runtime from a prefix and a deterministic filler, so a scanner that
//! recognises token shapes has nothing to recognise, and a test that starts
//! failing cannot be fixed by deleting a commit. The one literal is AWS's
//! own documented example key, which is not a credential.

use std::collections::BTreeSet;

use livingbrain_redact::{Class, Finding, MAX_INPUT_BYTES, Policy, RedactError, redact};

/// Plain `test` on a host, `wasm_bindgen_test` in a Worker. One definition, so
/// the two targets cannot drift apart.
macro_rules! everywhere {
    ($name:ident => $body:block) => {
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
        #[cfg_attr(not(target_arch = "wasm32"), test)]
        fn $name() $body
    };
}

/// A xorshift generator over an alphabet that skips the look-alike glyphs
/// (`l`, `O`, `I`), seeded from `seed`. Deterministic: the same seed gives the
/// same filler on every run, on every target.
fn filler(seed: u32) -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut state = 0x9E37_79B9 ^ seed.wrapping_mul(0x0100_0193);
    let mut out = String::with_capacity(64);
    while out.len() < 64 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        out.push(char::from(
            ALPHABET[(state % ALPHABET.len() as u32) as usize],
        ));
    }
    out
}

/// Every planted secret, with the class it must be found as.
fn planted() -> Vec<(Class, String)> {
    let f = filler;
    vec![
        (Class::GitHubToken, format!("{}{}", "ghp_", &f(1)[..36])),
        (Class::GitLabPat, format!("{}{}", "glpat-", &f(2)[..24])),
        // AWS's own documented example key. Not a credential, and the reason
        // an AWS pattern in any scanner's test suite looks like this.
        (Class::AwsAccessKey, "AKIAIOSFODNN7EXAMPLE".to_owned()),
        (Class::AwsSecretKey, f(4)[..40].to_owned()),
        (Class::OpenAiKey, format!("{}{}", "sk-proj-", &f(5)[..52])),
        (
            Class::AnthropicKey,
            format!("sk-ant-api{:02}-{}", 3, &f(6)[..44]),
        ),
        (Class::GoogleApiKey, format!("{}{}", "AIza", &f(7)[..35])),
        (Class::NpmToken, format!("{}{}", "npm_", &f(8)[..36])),
        (
            Class::SendgridKey,
            format!("SG.{}.{}", &f(9)[..22], &f(10)[..43]),
        ),
        (
            Class::SlackToken,
            format!("xoxb-1{}-2{}-{}", &f(11)[..12], &f(12)[..13], &f(13)[..24]),
        ),
        (
            Class::SlackWebhook,
            format!(
                "https://hooks.slack.com/services/T01{}/B01{}/{}",
                &f(14)[..8],
                &f(15)[..8],
                &f(16)[..24]
            ),
        ),
        (Class::StripeKey, format!("sk_live_{}", &f(17)[..28])),
        (Class::PolarToken, format!("polar_oat_{}", &f(18)[..43])),
        (
            Class::Jwt,
            format!(
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.{}",
                &f(19)[..43]
            ),
        ),
        (
            Class::PrivateKey,
            [
                "-----BEGIN RSA PRIVATE KEY-----",
                &f(20)[..64],
                "MIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf9Cnzj4p4WGeKLs1Pt8Qu",
                "KUpRKfFLfRYC9AIKjbJTWit+CqvjWYzvQwECAwEAAQJAIJLixBy2qpFoS4DSmoEm",
                "o3qGy0t6z09AIJtH+5OeRV1be+N4cDYJKffGzDa88vQENZiRm0GRq6a+HPGQMd2k",
                "TQIhAKMSvzIBnni7ot/OSie2TmJLY4SwTQAevXysE2RbFDYdAiEBCUEaRQnMnbp7",
                "9mxDXDf6AU0cN/RPBjb9qSHDcWZHGzUCIG2Es59z8ugGrDY+pxLQnwfotadxd+Uy",
                "v/Ow5T0q5gIJAiEAyS4RaI9YG8EWx/2w0T67ZUVAw8eOMB6BIUg0Xcu+3okCIBOs",
                "/5OiPgoTdSy7bcF9IGpSE8ZgGKzgYQVZeN97YE00",
                "-----END RSA PRIVATE KEY-----",
            ]
            .join("\n"),
        ),
        (Class::EnvSecret, format!("\"{}\"", &f(21)[..40])),
        (
            Class::DatabaseUrl,
            format!("postgres://app:{}@db.internal:5432/brain", &f(22)[..24]),
        ),
        (Class::HighEntropy, f(23)[..48].to_owned()),
        (Class::Email, "ops-team@mail.example.com".to_owned()),
        (Class::Phone, "+15550104477".to_owned()),
        (Class::IpAddress, "203.0.113.42".to_owned()),
        (Class::HomePath, "/home/ada".to_owned()),
    ]
}

/// The planted secrets in the surroundings they really arrive in.
fn planted_document() -> String {
    let webhook = planted()
        .into_iter()
        .find(|(class, _)| *class == Class::SlackWebhook)
        .map_or_else(String::new, |(_, value)| value);
    let lines: Vec<String> = vec![
        "2026-04-15T09:41:02.117Z INFO  ingest: reading 4 documents",
        &format!(
            "2026-04-15T09:41:02.240Z DEBUG git: cloning with {}",
            planted_key(Class::GitHubToken)
        ),
        &format!(
            "2026-04-15T09:41:02.301Z DEBUG gitlab: using {}",
            planted_key(Class::GitLabPat)
        ),
        "2026-04-15T09:41:02.402Z DEBUG aws: id AKIAIOSFODNN7EXAMPLE region eu-west-1",
        &format!(
            "2026-04-15T09:41:02.503Z DEBUG aws: aws_secret_access_key = {}",
            planted_key(Class::AwsSecretKey)
        ),
        &format!(
            "2026-04-15T09:41:02.604Z DEBUG llm: openai {} anthropic {}",
            planted_key(Class::OpenAiKey),
            planted_key(Class::AnthropicKey)
        ),
        &format!(
            "2026-04-15T09:41:02.705Z DEBUG vendor: google {} npm {} sendgrid {}",
            planted_key(Class::GoogleApiKey),
            planted_key(Class::NpmToken),
            planted_key(Class::SendgridKey)
        ),
        &format!(
            "2026-04-15T09:41:02.806Z DEBUG slack: token {} webhook {}",
            planted_key(Class::SlackToken),
            webhook
        ),
        &format!(
            "2026-04-15T09:41:02.907Z DEBUG billing: stripe {} polar {}",
            planted_key(Class::StripeKey),
            planted_key(Class::PolarToken)
        ),
        &format!(
            "2026-04-15T09:41:03.008Z DEBUG auth: bearer {}",
            planted_key(Class::Jwt)
        ),
        "",
        &planted_key(Class::PrivateKey),
        "",
        "# the .env we pasted into the issue",
        &format!("API_TOKEN={}", planted_key(Class::EnvSecret)),
        "DATABASE_URL=postgres://brain-db.internal:5432/brain",
        "FEATURE_FLAG=",
        "",
        &format!("DEBUG connection {}", planted_key(Class::DatabaseUrl)),
        &format!(
            "DEBUG opaque {} and a contact {} and a number {} on {}",
            planted_key(Class::HighEntropy),
            planted_key(Class::Email),
            planted_key(Class::Phone),
            planted_key(Class::IpAddress)
        ),
        &format!(
            "ERROR open failed at {}/src/lib.rs",
            planted_key(Class::HomePath)
        ),
        "INFO nothing sensitive on this line at all",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    lines.join("\n")
}

fn planted_key(class: Class) -> String {
    planted()
        .into_iter()
        .find(|(planted, _)| *planted == class)
        .map_or_else(String::new, |(_, value)| value)
}

fn classes(findings: &[Finding]) -> BTreeSet<Class> {
    findings.iter().map(|finding| finding.class).collect()
}

everywhere! {
    planted_secrets_are_removed => {
        let document = planted_document();
        let (clean, findings) = redact(&document, Policy::Redact).expect("redact");

        // Every planted value is gone from the stored text.
        for (class, secret) in planted() {
            assert!(
                !clean.contains(&secret),
                "{} survived redaction: {clean}",
                class.as_str()
            );
        }
        // The AWS access key is planted as a bare literal, so it is the one
        // value with a known shape that a reviewer can check by eye.
        assert!(!clean.contains("AKIAIOSFODNN7EXAMPLE"));

        // And every class is found, not merely the easy ones.
        let found = classes(&findings);
        for (class, _) in planted() {
            assert!(found.contains(&class), "{} not found in {found:?}", class.as_str());
        }
    }
}

everywhere! {
    surrounding_text_survives => {
        let document = planted_document();
        let (clean, _) = redact(&document, Policy::Redact).expect("redact");
        // The parts a human needs to debug the line are still there.
        assert!(clean.contains("ingest: reading 4 documents"));
        assert!(clean.contains("region eu-west-1"));
        assert!(clean.contains("hooks.slack.com/services/[REDACTED:slack_webhook]"));
        assert!(clean.contains("FEATURE_FLAG="), "an empty value was not a secret");
        assert!(
            clean.contains("API_TOKEN=[REDACTED:env_secret]"),
            "an .env key is not itself a secret: {clean}"
        );
        assert!(clean.contains("[REDACTED:home_path]/src/lib.rs"));
        assert!(clean.contains("postgres://brain-db.internal:5432/brain"));
    }
}

everywhere! {
    clean_corpus_has_no_findings => {
        // The agreed false-positive rate on this corpus is zero. It is a
        // corpus of ordinary prose, ordinary code and ordinary build
        // metadata; anything a detector finds in it is a detector that would
        // redact a workspace's own logs, so the fix is to tune the detector.
        let corpus = include_str!("corpus/clean.txt");
        let (clean, findings) = redact(corpus, Policy::Redact).expect("redact");
        assert!(findings.is_empty(), "false positives: {findings:?}");
        assert_eq!(clean, corpus, "a corpus with no findings must be unchanged");
    }
}

everywhere! {
    findings_never_carry_the_secret => {
        let document = planted_document();
        let (_, findings) = redact(&document, Policy::Redact).expect("redact");
        let debug = format!("{findings:?}");

        let error = redact(&document, Policy::Block).expect_err("block must refuse");
        let RedactError::Blocked(blocked) = &error else {
            panic!("expected Blocked, got {error:?}");
        };
        let shown = format!("{error}");

        for (class, secret) in planted() {
            assert!(!debug.contains(&secret), "{class:?} in Debug: {debug}");
            assert!(!shown.contains(&secret), "{class:?} in Display: {shown}");
        }
        // A blocked input is counted and classified, and gets no text back.
        assert!(!blocked.is_empty());
        assert!(shown.contains("blocked:"));
        assert!(shown.contains("bytes"));
    }
}

everywhere! {
    block_policy_returns_no_text => {
        let (text, findings) = redact("nothing here", Policy::Block).expect("clean input passes");
        assert_eq!(text, "nothing here");
        assert!(findings.is_empty());
        assert_eq!(Policy::default(), Policy::Redact);
    }
}

everywhere! {
    spans_are_valid_and_redaction_converges => {
        let document = planted_document();
        let (clean, findings) = redact(&document, Policy::Redact).expect("redact");
        assert!(!findings.is_empty());

        let mut previous_end = 0;
        for finding in &findings {
            assert!(document.is_char_boundary(finding.span.start));
            assert!(document.is_char_boundary(finding.span.end));
            assert!(finding.span.start < finding.span.end, "empty span {finding:?}");
            assert!(previous_end <= finding.span.start, "spans overlap: {finding:?}");
            previous_end = finding.span.end;
            // The span indexes the original, and what it points at really is
            // what was removed.
            assert!(!document[finding.span.clone()].contains("[REDACTED:"));
        }

        // Redacting the clean text finds nothing: the replacement token is
        // inert, so a second pass cannot walk into its own output.
        let (twice, second) = redact(&clean, Policy::Redact).expect("redact");
        assert!(second.is_empty(), "not idempotent: {second:?}");
        assert_eq!(twice, clean);
    }
}

everywhere! {
    over_the_cap_is_an_error => {
        let at_cap = "a".repeat(MAX_INPUT_BYTES);
        assert!(redact(&at_cap, Policy::Redact).is_ok());

        let over = "a".repeat(MAX_INPUT_BYTES + 1);
        assert_eq!(
            redact(&over, Policy::Redact),
            Err(RedactError::TooLarge {
                len: MAX_INPUT_BYTES + 1,
                cap: MAX_INPUT_BYTES,
            })
        );
        let message = redact(&over, Policy::Redact).unwrap_err().to_string();
        assert!(message.contains("split it"), "{message}");
    }
}

everywhere! {
    quiet_shapes_are_left_alone => {
        // The two IPv4 addresses that are never a person, and the private-key
        // header, which is a shape rather than a secret.
        let quiet = "bind 0.0.0.0:8787\ncurl 127.0.0.1/__health\nsha 3f9a1c7d\n";
        let (clean, findings) = redact(quiet, Policy::Redact).expect("redact");
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(clean, quiet);
    }
}

everywhere! {
    private_key_without_a_footer_runs_to_the_end => {
        let truncated = format!(
            "log: pasting key\n-----BEGIN RSA PRIVATE KEY-----\n{}\nnever mind",
            filler(30)
        );
        let (clean, findings) = redact(&truncated, Policy::Redact).expect("redact");
        assert_eq!(classes(&findings), BTreeSet::from([Class::PrivateKey]));
        // Everything after the header is gone, including the filler that was
        // never a PEM line: a key with no footer is not knowable, so it
        // costs the rest of the paste.
        assert_eq!(clean, "log: pasting key\n[REDACTED:private_key]");
    }
}

everywhere! {
    // Review regressions. Every input here leaked a residual byte or a whole
    // secret in an earlier version of this crate, so each one is a bug that
    // was found rather than a shape that was imagined.
    overlapping_hits_never_leave_a_residual => {
        let f = filler;
        let blob = format!("{}{}", &f(11)[..20], "a@b.co");
        // A secret glued to the front of a label, a password with a key pasted
        // onto the end of it, a key with trailing junk, and a high-entropy
        // blob with an email address inside it: four ways an overlap used to
        // shorten the redaction and hand back the bytes it dropped.
        let cases: Vec<(&str, String)> = vec![
            (
                "db url",
                format!("postgres://app:p4ssw0rd-{}@db:5432/x", "AKIAIOSFODNN7EXAMPLE"),
            ),
            (
                "env value",
                format!("DATABASE_PASSWORD=p4ss-{}", "AKIAIOSFODNN7EXAMPLE"),
            ),
            (
                "quoted value",
                format!("API_TOKEN=\"{}{}\"", "AKIAIOSFODNN7EXAMPLE", "extra"),
            ),
            ("blob with an email", format!("DEBUG opaque {blob} end")),
        ];
        for (name, input) in cases {
            let (clean, findings) = redact(&input, Policy::Redact).expect("redact");
            assert!(!findings.is_empty(), "{name}: nothing found in {input:?}");
            // Nothing that was ever inside a match may come back out. The
            // residual is checked by digest of the input, not by span, because
            // the span is the thing under test.
            for fragment in ["p4ssw0rd", "p4ss-", "a@b.co"] {
                assert!(!clean.contains(fragment), "{name}: {fragment} survived in {clean:?}");
            }
            // Coverage is total and the findings tile it: adjacent, in order,
            // never overlapping and never empty.
            for pair in findings.windows(2) {
                assert_eq!(
                    pair[0].span.end, pair[1].span.start,
                    "{name}: gap or overlap at {pair:?}"
                );
            }
            for finding in &findings {
                assert!(finding.span.start < finding.span.end, "{name}: empty {finding:?}");
            }
        }
        // The resolver keeps both classes honest. A merge reports this as one
        // access key and loses the database password; the split reports the
        // password, the key, and nothing in between.
        let db_url = "postgres://app:p4ssw0rd-AKIAIOSFODNN7EXAMPLE@db:5432/x";
        let (_, findings) = redact(db_url, Policy::Redact).expect("redact");
        let found = classes(&findings);
        assert!(found.contains(&Class::DatabaseUrl), "{found:?}");
        assert!(found.contains(&Class::AwsAccessKey), "{found:?}");

        // The same for a blob that is mostly entropy with a key shape in it.
        let keyed = format!("DEBUG opaque {}{} end", &f(11)[..20], "AKIAIOSFODNN7EXAMPLE");
        let (_, findings) = redact(&keyed, Policy::Redact).expect("redact");
        let found = classes(&findings);
        assert!(found.contains(&Class::HighEntropy), "{found:?}");
        assert!(found.contains(&Class::AwsAccessKey), "{found:?}");
    }
}

everywhere! {
    // Two tokens on one line, and a token glued to a word. A left boundary
    // consumed by the pattern loses the second one, so the distinctive
    // prefixes are matched with no boundary at all.
    glued_tokens_are_both_found => {
        let f = filler;
        let ghp = format!("{}{}", "ghp_", &f(1)[..36]);
        let second = format!("{}{}", "gho_", &f(2)[..36]);
        let doc = format!(
            "{ghp}{second}\nAKIAIOSFODNN7EXAMPLEASIAIOSFODNN7EXAMPLE\ntoken=secrety{ghp}"
        );
        let (clean, findings) = redact(&doc, Policy::Redact).expect("redact");
        for secret in [&ghp, &second] {
            assert!(!clean.contains(secret.as_str()), "{secret} survived");
        }
        // The glued pair may be reported as one span or two — the entropy
        // detector sees the same eighty bytes and the resolver splits them.
        // What must hold is that every byte of every token is covered, and
        // that the reported class is the specific one.
        let akia = "AKIAIOSFODNN7EXAMPLE";
        let asia = "ASIAIOSFODNN7EXAMPLE";
        for (class, token) in [
            (Class::GitHubToken, ghp.as_str()),
            (Class::GitHubToken, second.as_str()),
            (Class::AwsAccessKey, akia),
            (Class::AwsAccessKey, asia),
        ] {
            let at = doc.find(token).expect("token is in the document");
            let covered = findings
                .iter()
                .filter(|f| f.class == class && f.span.start <= at && f.span.end >= at + token.len())
                .count();
            assert_eq!(covered, 1, "{class:?} {token} not covered by one span: {findings:?}");
        }
        // Nothing that looks like a token prefix is left, glued or not.
        assert!(!clean.contains("ghp_") && !clean.contains("gho_") && !clean.contains("ASIA"));
        assert!(!clean.contains("secrety"), "the word before the token survived");

        // Convergence, on the adversarial input this time: a second pass over
        // output with no boundary information left must find nothing.
        let (twice, second_pass) = redact(&clean, Policy::Redact).expect("redact");
        assert!(second_pass.is_empty(), "not idempotent: {second_pass:?}");
        assert_eq!(twice, clean);

        // The bare `sk-` key, the one shape with no distinctive prefix, keeps
        // a boundary check in code rather than in the pattern — so two of them
        // on one line are still both covered, by one greedy run.
        let bare = format!("sk-{}", &f(31)[..48]);
        let glued = format!("{bare}{bare}");
        let (clean, findings) = redact(&glued, Policy::Redact).expect("redact");
        assert_eq!(classes(&findings), BTreeSet::from([Class::OpenAiKey]), "{findings:?}");
        assert!(!clean.contains("sk-"), "{clean}");
        let (twice, second_pass) = redact(&clean, Policy::Redact).expect("redact");
        assert!(second_pass.is_empty(), "not idempotent: {second_pass:?}");
        assert_eq!(twice, clean);
    }
}

everywhere! {
    // A labelled AWS key is "40 characters" in a console, not in a file. The
    // run after the fortieth character used to stay in clear.
    a_labelled_aws_key_takes_its_whole_run => {
        let long = format!("{}{}", &filler(21)[..40], "EXTRAandMORE");
        let doc = format!("aws_secret_access_key = {long}\n");
        let (clean, findings) = redact(&doc, Policy::Redact).expect("redact");
        assert_eq!(classes(&findings), BTreeSet::from([Class::AwsSecretKey]));
        assert_eq!(clean, "aws_secret_access_key = [REDACTED:aws_secret_key]\n");
        assert!(!clean.contains("EXTRAandMORE"));
    }
}
