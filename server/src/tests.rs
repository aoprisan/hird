//! The server's own behaviour: who gets in, and as whom.

use crate::roster::Roster;

/// A roster with one worker, for tests that only care about the HTTP edge.
fn one_worker() -> Roster {
    Roster::parse(
        r#"
project = "/srv/acme"
[[worker]]
token = "aaaaaaaaaaaaaaaaaaaaaaaaaaaa"
identity = "ana"
"#,
    )
    .unwrap()
}

/// The roster is the whole authorization story, so the thing worth pinning is
/// that a token resolves to exactly one person and nothing else does.
#[test]
fn only_a_rostered_token_names_anybody() {
    let roster = one_worker();
    assert_eq!(
        roster
            .worker("aaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap()
            .identity,
        "ana"
    );
    assert!(roster.worker("").is_none());
    assert!(roster.worker("aaaaaaaaaaaaaaaaaaaaaaaaaaa").is_none());
    assert!(roster
        .worker("Bearer aaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .is_none());
}
