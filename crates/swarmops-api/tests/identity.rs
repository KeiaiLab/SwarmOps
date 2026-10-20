//! `Identity`: `<family>/<instance>` (operations spec §5), `^[a-z0-9-]+/[A-Za-z0-9_-][A-Za-z0-9._-]*$`
//! (the instance may not start with `.`), at most 255 bytes, checked by `parse` and on
//! deserialization alike.

use serde_json::json;
use swarmops_api::{ErrorCode, Identity, schema_for_all};

#[test]
fn parse_accepts_family_slash_instance() {
    for (agent, family, instance) in [
        ("agent/worker-17", "agent", "worker-17"),
        ("human/purple", "human", "purple"),
        ("ci-2/Run_3.a-b", "ci-2", "Run_3.a-b"),
        ("a/_x", "a", "_x"),
        ("a/-x", "a", "-x"),
        // Every range's first and last byte: 0-9, a-z, A-Z.
        ("0a/A0a", "0a", "A0a"),
        ("z9-/Z9z_.-", "z9-", "Z9z_.-"),
    ] {
        let id = Identity::parse(agent).unwrap();
        assert_eq!((id.agent(), id.run_id()), (agent, None));
        assert_eq!((id.family(), id.instance()), (family, instance));
    }
    let longest = format!("a/{}", "b".repeat(253));
    assert_eq!(Identity::parse(&longest).unwrap().agent().len(), 255);
}

#[test]
fn parse_rejects_names_outside_the_grammar() {
    let too_long = format!("a/{}", "b".repeat(254));
    for agent in [
        "",
        "agent",
        "/worker",
        "agent/",
        "Agent/worker",
        "agent_code/worker",
        "a/b/c",
        "a/b c",
        "a/b\n",
        "a/.hidden",
        "a/..",
        "a/.",
        // The byte just outside each range: `/0-9:`, `` `a-z{ ``, `@A-Z[`.
        "a:/x",
        "a`/x",
        "a{/x",
        "a/x:",
        "a/x`",
        "a/x{",
        "a/x@",
        "a/x[",
        "a/@x",
        " a/b",
        "é/worker",
        "a/é",
        too_long.as_str(),
    ] {
        let err = Identity::parse(agent).expect_err(agent);
        assert_eq!(err.code, ErrorCode::InvalidArg.as_str(), "{agent:?}");
        assert_eq!(err.exit, 2, "{agent:?}");
    }
}

#[test]
fn deserializing_an_identity_validates_it() {
    let id: Identity =
        serde_json::from_value(json!({"agent": "agent/worker-17", "run_id": "run-3"})).unwrap();
    assert_eq!(
        (id.agent(), id.run_id()),
        ("agent/worker-17", Some("run-3"))
    );
    assert_eq!(
        id,
        Identity::parse("agent/worker-17")
            .unwrap()
            .with_run_id("run-3")
    );
    assert_eq!(
        serde_json::to_value(&id).unwrap(),
        json!({"agent": "agent/worker-17", "run_id": "run-3"})
    );
    for agent in ["Agent/worker", "a/..", "a/b/c", ""] {
        let err = serde_json::from_value::<Identity>(json!({ "agent": agent })).expect_err(agent);
        assert!(
            err.to_string().contains("E_INVALID_ARG"),
            "{agent:?}: {err}"
        );
    }
    assert!(serde_json::from_value::<Identity>(json!({})).is_err());
    // A type error names the type the caller sent, not a helper.
    let err = serde_json::from_value::<Identity>(json!("human/purple")).unwrap_err();
    assert!(
        err.to_string().contains("expected struct Identity"),
        "{err}"
    );
}

#[test]
fn identity_schema_carries_the_agent_grammar() {
    let agent = &schema_for_all()["Identity"]["properties"]["agent"];
    assert_eq!(
        agent["pattern"],
        "^[a-z0-9-]+/[A-Za-z0-9_-][A-Za-z0-9._-]*$"
    );
    assert_eq!(agent["maxLength"], 255);
}
