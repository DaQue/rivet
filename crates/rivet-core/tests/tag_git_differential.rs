//! Differential test against **real git**.
//!
//! Provenance: every fixture in `fixtures/tag/` is a raw tag-object *body* that real `git`
//! produced while the tag spec was being pinned down. The object id git reported for each one
//! was re-derived independently as `sha1("tag <len>\0" + body)` and matched git exactly, so
//! these files are byte-level ground truth, not hand-written approximations.
//! Recorded ids live next to them in `fixtures/tag/expected-object-ids.json`.
//!
//! What "Git-compatible" means for tags, therefore: if real git accepts a body, `Tag::parse`
//! must accept it, `to_bytes()` must return those exact bytes (so the object id is unchanged),
//! and armour detection must agree with `git cat-file -p` / `git tag -n99`.

use bstr::BStr;
use rivet_core::{Error, ObjectFormat, ObjectId, Tag};

const BODY_C: &[u8] = include_bytes!("fixtures/tag/bodyC"); // 2ba1d4220af5… PGP armour at line start
const BODY_C2: &[u8] = include_bytes!("fixtures/tag/bodyC2"); // 21067f425b1a… marker mid-line after "msg"
const BODY_C3: &[u8] = include_bytes!("fixtures/tag/bodyC3"); // ece6300e81dc… blank lines, then marker at line start
const BODY_D: &[u8] = include_bytes!("fixtures/tag/bodyD"); // 4885e09db709… no tagger header (git accepts)
const BODY_E: &[u8] = include_bytes!("fixtures/tag/bodyE"); // 0140edce077c… unknown header "extra value" (git accepts)
const BODY_F: &[u8] = include_bytes!("fixtures/tag/bodyF"); // e1a3591775be… no blank line, no message (git accepts)
const BODY_G: &[u8] = include_bytes!("fixtures/tag/bodyG"); // b4615fee579a… SSH armour at line start
const BODY_H: &[u8] = include_bytes!("fixtures/tag/bodyH"); // 73aa55cc6cb3… armour not at line start
const BODY_I: &[u8] = include_bytes!("fixtures/tag/bodyI"); // 4f36da0ecafa… tag/tagger headers swapped
const BODY_J: &[u8] = include_bytes!("fixtures/tag/bodyJ"); // 46cf7ba51094… object/type/tag only (git accepts)
const BODY_K: &[u8] = include_bytes!("fixtures/tag/bodyK"); // 9404c1ea208d… type wombat (git refuses)
const BODY_N1: &[u8] = include_bytes!("fixtures/tag/bodyN1"); // b68e4d033748… nested PGP SIGNATURE + PGP MESSAGE
const BODY_N2: &[u8] = include_bytes!("fixtures/tag/bodyN2"); // 0f8980b3e1b7… PGP SIGNATURE then SSH SIGNATURE
const BODY_N3: &[u8] = include_bytes!("fixtures/tag/bodyN3"); // 87e1dbebeb19… SSH SIGNATURE then PGP SIGNATURE

#[test]
fn round_trip_is_byte_exact_for_every_body_git_accepts() {
    let cases: &[(&str, &[u8])] = &[
        ("C", BODY_C),
        ("C2", BODY_C2),
        ("C3", BODY_C3),
        ("E", BODY_E),
        ("G", BODY_G),
        ("H", BODY_H),
        ("I", BODY_I),
        ("N1", BODY_N1),
        ("N2", BODY_N2),
        ("N3", BODY_N3),
    ];
    for (case, body) in cases {
        let tag = Tag::parse(ObjectFormat::Sha1, body).unwrap_or_else(|e| {
            panic!("case {case}: git accepts this body, rivet must parse it: {e:?}")
        });
        assert_eq!(
            tag.to_bytes(),
            *body,
            "case {case}: re-encoding must return the identical bytes (object id unchanged)"
        );
    }
}

#[test]
fn armour_detection_matches_real_git() {
    for (case, body) in [("C", BODY_C), ("C3", BODY_C3), ("G", BODY_G), ("N1", BODY_N1), ("N2", BODY_N2), ("N3", BODY_N3)] {
        let tag = Tag::parse(ObjectFormat::Sha1, body).expect(case);
        assert!(tag.is_signed(), "case {case}: git stores a signature here");
        assert!(
            tag.signature().is_some_and(|s| !s.is_empty()),
            "case {case}: the signature accessor must expose the armour"
        );
    }
    // A marker that is not at the start of a line is message text, not a signature.
    for (case, body) in [("C2", BODY_C2), ("E", BODY_E), ("H", BODY_H)] {
        let tag = Tag::parse(ObjectFormat::Sha1, body).expect(case);
        assert!(
            !tag.is_signed(),
            "case {case}: git treats this body as an unsigned tag"
        );
    }
}

#[test]
fn message_is_everything_before_a_line_start_marker() {
    // `git tag -n99` shows "msg" for case C: the newline before the signature line is message
    // text, not part of the signature.
    let c = Tag::parse(ObjectFormat::Sha1, BODY_C).unwrap();
    assert_eq!(c.message(), b"msg\n");

    // Case C3 keeps both blank lines inside the message.
    let c3 = Tag::parse(ObjectFormat::Sha1, BODY_C3).unwrap();
    assert_eq!(c3.message(), b"msg\n\n\n");

    // Case C2: the marker sits mid-line, so nothing here is a signature and the whole tail is
    // the message (including the armour text, verbatim).
    let c2 = Tag::parse(ObjectFormat::Sha1, BODY_C2).unwrap();
    // Every body git produced ends with a newline, so C2's message does too.
    assert_eq!(
        c2.message(),
        b"msg-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----\n"
    );
}

#[test]
fn unknown_headers_are_tolerated_and_header_order_is_free() {
    // Unknown header (git ignores it) …
    let e = Tag::parse(ObjectFormat::Sha1, BODY_E).unwrap();
    assert_eq!(e.name(), "t_unknown");
    assert_eq!(e.message(), b"msg\n");

    // … and tag/tagger order does not matter.
    let i = Tag::parse(ObjectFormat::Sha1, BODY_I).unwrap();
    assert_eq!(i.name(), "t_order");
    assert_eq!(i.message(), b"msg\n");
}

#[test]
fn unknown_type_value_is_a_hard_error() {
    // `git mktag` refuses this body too: unknown tag type 'wombat'.
    assert!(Tag::parse(ObjectFormat::Sha1, BODY_K).is_err());
}

#[test]
fn documented_deviations_from_real_git() {
    // Git accepts all three of these bodies (each was assigned an object id), but rivet's
    // contract pins `tagger` as required and requires a blank line before the message. The
    // deviations are deliberate; this test exists so that changing either side fails loudly
    // instead of drifting silently.
    let d = Tag::parse(ObjectFormat::Sha1, BODY_D);
    assert!(
        matches!(d, Err(Error::MissingHeader { .. })),
        "case D has no tagger header and rivet requires one: {d:?}"
    );

    // J is missing *both* the tagger header and the blank line that ends the header block, so
    // the terminator check fires first. D (blank line present, no tagger) still reports
    // MissingHeader — together the two cases pin both checks.
    let j = Tag::parse(ObjectFormat::Sha1, BODY_J);
    assert!(
        matches!(j, Err(Error::MalformedObject { .. })),
        "case J is object/type/tag only, with no blank line: {j:?}"
    );

    let f = Tag::parse(ObjectFormat::Sha1, BODY_F);
    assert!(
        f.is_err(),
        "case F has no blank line and no message: rivet rejects, git accepts"
    );
}

#[test]
fn recorded_git_object_ids_are_reproduced() {
    const EXPECTED: &str = include_str!("fixtures/tag/expected-object-ids.json");
    // Small dependency-free line scanner: find `"<KEY>"`, then the next
    // `"git_object_id": "..."` value that follows it.
    fn recorded_id(json: &str, key: &str) -> String {
        let needle = format!("\"{key}\"");
        let mut found_key = false;
        for line in json.lines() {
            let line = line.trim_start();
            if found_key {
                if line.starts_with("\"git_object_id\":") {
                    let rest = line.split('"').nth(3).unwrap_or_default();
                    return rest.to_string();
                }
            } else if line.contains(&needle) {
                found_key = true;
            }
        }
        panic!("no recorded id for key {key}");
    }
    // Bodies accepted by rivet (git's ids recorded for each).
    let bodies: &[(&str, &[u8])] = &[
        ("C", BODY_C),
        ("C2", BODY_C2),
        ("C3", BODY_C3),
        ("E", BODY_E),
        ("G", BODY_G),
        ("H", BODY_H),
        ("I", BODY_I),
        ("N1", BODY_N1),
        ("N2", BODY_N2),
        ("N3", BODY_N3),
    ];
    for (case, body) in bodies {
        let tag = Tag::parse(ObjectFormat::Sha1, body).unwrap_or_else(|e| {
            panic!("case {case}: rivet must parse this body: {e:?}")
        });
        let recorded = recorded_id(EXPECTED, case);
        let expected = ObjectId::from_hex(ObjectFormat::Sha1, BStr::new(recorded.as_bytes()))
            .unwrap_or_else(|e| panic!("case {case}: recorded id {recorded} is not a valid hex id: {e:?}"));
        let got = tag.id(ObjectFormat::Sha1).unwrap_or_else(|e| panic!("case {case}: id computation failed: {e:?}"));
        assert_eq!(got, expected, "case {case}: rivet's object id must match git's recorded id");
    }
    // The four bodies rivet rejects by contract: parse must still error, so changing
    // either side fails loudly.
    let rejected = ["D", "F", "J", "K"];
    for case in rejected {
        let body = match case {
            "D" => BODY_D,
            "F" => BODY_F,
            "J" => BODY_J,
            "K" => BODY_K,
            _ => panic!("unlisted rejected case {case}"),
        };
        assert!(
            Tag::parse(ObjectFormat::Sha1, body).is_err(),
            "case {case}: rivet must keep rejecting this body by contract"
        );
    }
}
