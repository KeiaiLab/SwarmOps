//! `Identity::parse`: `<family>/<instance>` (umbrella spec §5), `^[a-z0-9-]+/[A-Za-z0-9._-]+$`,
//! at most 255 bytes.

use swarmops_api::{ErrorCode, Identity};

#[test]
fn parse_accepts_family_slash_instance() {
    for (agent, family, instance) in [
        ("agent/worker-17", "claude-code", "worker-17"),
        ("human/purple", "human", "purple"),
        ("ci-2/Run_3.a-b", "ci-2", "Run_3.a-b"),
    ] {
        let id = Identity::parse(agent).unwrap();
        assert_eq!(
            id,
            Identity {
                agent: agent.to_owned(),
                run_id: None,
            }
        );
        assert_eq!((id.family(), id.instance()), (family, instance));
    }
    let longest = format!("a/{}", "b".repeat(253));
    assert_eq!(Identity::parse(&longest).unwrap().agent.len(), 255);
}

#[test]
fn parse_rejects_names_outside_the_grammar() {
    let too_long = format!("a/{}", "b".repeat(254));
    for agent in [
        "",
        "claude-code",
        "/worker",
        "agent/",
        "Claude/worker",
        "claude_code/worker",
        "a/b/c",
        "a/b c",
        "a/b\n",
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
