use symphony::domain::{BlockerRef, Issue};
use symphony::error::SymphonyError;
use symphony::prompt::render_prompt;

fn issue() -> Issue {
    Issue {
        id: "gid://github/Issue/1".into(),
        identifier: "OCTO-1".into(),
        title: "Fix renderer".into(),
        description: Some("Details".into()),
        priority: Some(2),
        state: "Todo".into(),
        branch_name: Some("octo-1-fix-renderer".into()),
        url: Some("https://github.com/octo/repo/issues/1".into()),
        labels: vec!["bug".into(), "backend".into()],
        blocked_by: vec![BlockerRef {
            id: Some("blocker-id".into()),
            identifier: Some("OCTO-0".into()),
            state: Some("Done".into()),
        }],
        created_at: None,
        updated_at: None,
    }
}

#[test]
fn renders_issue_fields_attempt_and_nested_collections() {
    let rendered = render_prompt(
        "{{ issue.identifier }} {{ issue.title }} attempt={{ attempt }} labels={% for label in issue.labels %}{{ label }};{% endfor %} blockers={% for blocker in issue.blocked_by %}{{ blocker.identifier }}:{{ blocker.state }};{% endfor %}",
        &issue(),
        Some(4),
    )
    .unwrap();

    assert_eq!(
        rendered,
        "OCTO-1 Fix renderer attempt=4 labels=bug;backend; blockers=OCTO-0:Done;"
    );
}

#[test]
fn unknown_variables_and_indexes_fail_rendering() {
    let top_level = render_prompt("{{ missing }}", &issue(), None).unwrap_err();
    assert!(matches!(top_level, SymphonyError::TemplateRenderError(_)));

    let nested = render_prompt("{{ issue.missing }}", &issue(), None).unwrap_err();
    assert!(matches!(nested, SymphonyError::TemplateRenderError(_)));
}

#[test]
fn unknown_filters_fail_template_parsing() {
    let error = render_prompt("{{ issue.title | not_a_real_filter }}", &issue(), None).unwrap_err();
    assert!(matches!(error, SymphonyError::TemplateParseError(_)));
}

#[test]
fn empty_template_uses_default_prompt_at_render_time() {
    let rendered = render_prompt("   \n\t", &issue(), None).unwrap();
    assert_eq!(rendered, "You are working on an issue from GitHub.");
}
