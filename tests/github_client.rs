use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use symphony::config::{
    GithubConfig, GithubProjectOwnerType, GithubRepositoryConfig, TrackerConfig,
};
use symphony::domain::Issue;
use symphony::error::SymphonyError;
use symphony::tracker::github::{
    GitHubGraphqlExecutor, GitHubTrackerClient, MAX_GITHUB_GRAPHQL_RESPONSE_BYTES,
};
use symphony::tracker::{TrackerClient, TrackerWriter};

#[tokio::test]
async fn sends_auth_query_and_project_variables() {
    let server = TestServer::new(vec![ok(project_page(false, None, Vec::new()))]);
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    let issues = client.fetch_candidate_issues().await.unwrap();

    assert!(issues.is_empty());
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("authorization: Bearer test-token"));
    let body: Value = serde_json::from_str(request_body(&requests[0])).unwrap();
    assert!(
        body["query"]
            .as_str()
            .unwrap()
            .contains("SymphonyProjectItems")
    );
    assert_eq!(body["variables"]["repositoryOwner"], "octo-org");
    assert_eq!(body["variables"]["repositoryName"], "octo-repo");
    assert_eq!(body["variables"]["projectOwnerLogin"], "octo-org");
    assert_eq!(body["variables"]["projectNumber"], 7);
    assert!(body["variables"]["after"].is_null());
    assert_eq!(body["variables"]["isOrganization"].as_bool(), Some(true));
    assert_eq!(body["variables"]["isUser"].as_bool(), Some(false));
}

#[tokio::test]
async fn raw_graphql_executor_uses_configured_endpoint_auth_and_preserves_graphql_body() {
    let response_body = json!({
        "data": null,
        "errors": [{"message": "field is unavailable", "path": ["viewer"]}]
    });
    let server = TestServer::new(vec![ok(response_body.clone())]);
    let executor = raw_executor(server.url());
    assert!(!format!("{executor:?}").contains("test-token"));

    let body = executor
        .execute(
            "query Viewer($login: String!) { viewer { login } }",
            json!({"login": "octocat"}),
        )
        .await
        .unwrap();

    assert_eq!(body, response_body);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("authorization: Bearer test-token"));
    let request: Value = serde_json::from_str(request_body(&requests[0])).unwrap();
    assert_eq!(request["variables"], json!({"login": "octocat"}));
    assert!(
        request["query"]
            .as_str()
            .unwrap()
            .contains("query Viewer($login: String!)")
    );
}

#[tokio::test]
async fn graphql_redirects_are_not_followed_or_sent_bearer_credentials() {
    let same_origin = TestServer::new(vec![
        redirect("/graphql"),
        ok(json!({"data": {"unexpected": true}})),
    ]);
    let error = raw_executor(same_origin.url())
        .execute("query { viewer { login } }", json!({}))
        .await
        .unwrap_err();
    assert_tracker_kind(error, "github_status");
    let requests = same_origin.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("authorization: Bearer test-token"));

    let redirect_target = TestServer::new(vec![ok(json!({"data": {"unexpected": true}}))]);
    let redirect_source = TestServer::new(vec![redirect(&redirect_target.url())]);
    let error = raw_executor(redirect_source.url())
        .execute("query { viewer { login } }", json!({}))
        .await
        .unwrap_err();
    assert_tracker_kind(error, "github_status");
    assert_eq!(redirect_source.requests().len(), 1);
    assert!(
        redirect_target.requests().is_empty(),
        "redirect targets must receive no request or credential"
    );
}

#[tokio::test]
async fn raw_graphql_executor_distinguishes_transport_status_and_malformed_responses() {
    let refused = TcpListener::bind("127.0.0.1:0").unwrap();
    let refused_url = format!("http://{}/graphql", refused.local_addr().unwrap());
    drop(refused);
    let err = raw_executor(refused_url)
        .execute("query { viewer { login } }", json!({}))
        .await
        .unwrap_err();
    assert_tracker_kind(err, "github_transport");

    let status_server = TestServer::new(vec![response(502, json!({"message": "upstream"}))]);
    let err = raw_executor(status_server.url())
        .execute("query { viewer { login } }", json!({}))
        .await
        .unwrap_err();
    assert_tracker_kind(err, "github_status");

    let malformed_server = TestServer::new(vec![raw_response(200, "not json")]);
    let err = raw_executor(malformed_server.url())
        .execute("query { viewer { login } }", json!({}))
        .await
        .unwrap_err();
    assert_tracker_kind(err, "github_malformed");

    let err = raw_executor(TestServer::new(Vec::new()).url())
        .execute("query { viewer { login } }", json!(["not", "an", "object"]))
        .await
        .unwrap_err();
    assert_tracker_kind(err, "github_malformed");
}

#[tokio::test]
async fn raw_graphql_executor_accepts_a_response_at_the_decoded_byte_limit() {
    let server = WireServer::new(vec![WireResponse::fixed(200, exact_limit_graphql_body())]);

    let body = raw_executor(server.url())
        .execute("query { viewer { login } }", json!({}))
        .await
        .unwrap();

    assert_eq!(
        body["data"]["padding"].as_str().unwrap().len(),
        MAX_GITHUB_GRAPHQL_RESPONSE_BYTES - graphql_padding_overhead()
    );
}

#[tokio::test]
async fn raw_graphql_executor_rejects_excessive_content_length_before_reading() {
    let server = WireServer::new(vec![
        WireResponse::fixed(200, br#"{}"#.to_vec())
            .with_declared_content_length(MAX_GITHUB_GRAPHQL_RESPONSE_BYTES + 1),
    ]);

    let error = raw_executor(server.url())
        .execute("query { viewer { login } }", json!({}))
        .await
        .unwrap_err();

    assert_tracker_kind(error, "github_response_too_large");
}

#[tokio::test]
async fn raw_graphql_executor_rejects_oversized_chunked_success_and_error_bodies() {
    for status in [200, 502] {
        let server = WireServer::new(vec![WireResponse::chunked(
            status,
            vec![
                br#"{"data":{"padding":""#.to_vec(),
                vec![b'x'; MAX_GITHUB_GRAPHQL_RESPONSE_BYTES],
            ],
        )]);

        let error = raw_executor(server.url())
            .execute("query { viewer { login } }", json!({}))
            .await
            .unwrap_err();

        assert_tracker_kind(error, "github_response_too_large");
    }
}

#[tokio::test]
async fn raw_graphql_executor_rejects_compressed_responses_that_expand_past_the_limit() {
    let server = WireServer::new(vec![
        WireResponse::fixed(
            200,
            gzip_repeated_byte(b'x', MAX_GITHUB_GRAPHQL_RESPONSE_BYTES + 1),
        )
        .with_header("content-encoding", "gzip"),
    ]);

    let error = raw_executor(server.url())
        .execute("query { viewer { login } }", json!({}))
        .await
        .unwrap_err();

    assert_tracker_kind(error, "github_response_too_large");
}

#[tokio::test]
async fn tracker_discards_preceding_pages_when_a_later_response_exceeds_the_limit() {
    let server = WireServer::new(vec![
        WireResponse::fixed(
            200,
            project_page(
                true,
                Some("next"),
                vec![project_item("I_1", 1, "Todo", &[], None, None)],
            )
            .to_string()
            .into_bytes(),
        ),
        WireResponse::chunked(
            200,
            vec![
                br#"{"data":{"padding":""#.to_vec(),
                vec![b'x'; MAX_GITHUB_GRAPHQL_RESPONSE_BYTES],
            ],
        ),
    ]);
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    let error = client.fetch_candidate_issues().await.unwrap_err();

    assert_tracker_kind(error, "github_response_too_large");
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn tracker_writer_does_not_issue_a_mutation_after_a_size_limit_failure() {
    let server = WireServer::new(vec![WireResponse::chunked(
        200,
        vec![
            br#"{"data":{"padding":""#.to_vec(),
            vec![b'x'; MAX_GITHUB_GRAPHQL_RESPONSE_BYTES],
        ],
    )]);
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    let error = client
        .move_issue_to_state(&writer_issue(), "Started")
        .await
        .unwrap_err();

    assert_tracker_kind(error, "github_response_too_large");
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn user_owned_projects_query_only_user_owner() {
    let server = TestServer::new(vec![ok(project_page_for_owner(
        GithubProjectOwnerType::User,
        false,
        None,
        Vec::new(),
    ))]);
    let client = client_for_owner_type(
        server.url(),
        vec!["Todo"],
        BTreeMap::new(),
        GithubProjectOwnerType::User,
    );

    let issues = client.fetch_candidate_issues().await.unwrap();

    assert!(issues.is_empty());
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_str(request_body(&requests[0])).unwrap();
    let query = body["query"].as_str().unwrap();
    assert!(
        query.contains("organization(login: $projectOwnerLogin) @include(if: $isOrganization)")
    );
    assert!(query.contains("user(login: $projectOwnerLogin) @include(if: $isUser)"));
    assert_eq!(body["variables"]["projectOwnerLogin"], "octo-user");
    assert_eq!(body["variables"]["isOrganization"].as_bool(), Some(false));
    assert_eq!(body["variables"]["isUser"].as_bool(), Some(true));
}

#[tokio::test]
async fn pages_project_items_filters_statuses_and_normalizes_issues() {
    let mut priorities = BTreeMap::new();
    priorities.insert("p1".to_string(), 100);
    let todo = project_item(
        "I_todo",
        1,
        "Todo",
        &["Bug", "P1", "blocked-by:octo-org/octo-repo#9"],
        Some("5"),
        None,
    );
    let done = project_item("I_done", 2, "Done", &["Enhancement"], None, None);
    let doing = project_item(
        "I_doing",
        3,
        "in progress",
        &["BUG", "P1"],
        None,
        Some("octo-org/octo-repo#1"),
    );
    let server = TestServer::new(vec![
        ok(project_page(true, Some("cursor-1"), vec![todo, done])),
        ok(project_page(false, None, vec![doing])),
    ]);
    let client = client(server.url(), vec!["todo", "In Progress"], priorities);

    let issues = client.fetch_candidate_issues().await.unwrap();

    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0].id, "I_todo");
    assert_eq!(issues[0].identifier, "octo-org/octo-repo#1");
    assert_eq!(issues[0].description.as_deref(), Some("body 1"));
    assert_eq!(issues[0].priority, Some(5));
    assert_eq!(issues[0].state, "Todo");
    assert_eq!(
        issues[0].labels,
        vec!["bug", "p1", "blocked-by:octo-org/octo-repo#9"]
    );
    assert_eq!(
        issues[0].blocked_by[0].identifier.as_deref(),
        Some("octo-org/octo-repo#9")
    );
    assert!(issues[0].branch_name.is_none());
    assert_eq!(
        issues[0].url.as_deref(),
        Some("https://github.test/octo-org/octo-repo/issues/1")
    );
    assert!(issues[0].created_at.is_some());
    assert_eq!(issues[1].priority, Some(100));
    assert_eq!(
        issues[1].blocked_by[0].identifier.as_deref(),
        Some("octo-org/octo-repo#1")
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let second_body: Value = serde_json::from_str(request_body(&requests[1])).unwrap();
    assert_eq!(second_body["variables"]["after"], "cursor-1");
}

#[tokio::test]
async fn candidate_fetch_skips_project_items_from_other_repositories() {
    let in_repo = project_item("I_in_repo", 1, "Todo", &["Bug"], None, None);
    let mut out_of_repo = project_item("I_out_of_repo", 2, "Todo", &["Bug"], None, None);
    let repository = out_of_repo
        .get_mut("content")
        .unwrap()
        .get_mut("repository")
        .unwrap();
    *repository = json!({
        "nameWithOwner": "other-org/other-repo",
        "name": "other-repo",
        "owner": {"login": "other-org"}
    });
    let server = TestServer::new(vec![ok(project_page(
        false,
        None,
        vec![in_repo, out_of_repo],
    ))]);
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    let issues = client.fetch_candidate_issues().await.unwrap();

    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].id, "I_in_repo");
    assert_eq!(issues[0].identifier, "octo-org/octo-repo#1");
}

#[tokio::test]
async fn candidate_fetch_accepts_multiple_configured_repositories() {
    let in_primary = project_item("I_primary", 1, "Todo", &["Bug"], None, None);
    let mut in_secondary = project_item("I_secondary", 2, "Todo", &["Bug"], None, None);
    *in_secondary
        .get_mut("content")
        .unwrap()
        .get_mut("repository")
        .unwrap() = json!({
        "nameWithOwner": "other-org/other-repo",
        "name": "other-repo",
        "owner": {"login": "other-org"}
    });
    let mut filtered = project_item("I_filtered", 3, "Todo", &["Bug"], None, None);
    *filtered
        .get_mut("content")
        .unwrap()
        .get_mut("repository")
        .unwrap() = json!({
        "nameWithOwner": "unlisted/unlisted",
        "name": "unlisted",
        "owner": {"login": "unlisted"}
    });
    let server = TestServer::new(vec![ok(project_page(
        false,
        None,
        vec![in_primary, in_secondary, filtered],
    ))]);
    let client = client_with_repositories(
        server.url(),
        vec!["Todo"],
        vec![
            GithubRepositoryConfig {
                owner: "octo-org".to_string(),
                name: "octo-repo".to_string(),
            },
            GithubRepositoryConfig {
                owner: "other-org".to_string(),
                name: "other-repo".to_string(),
            },
        ],
    );

    let issues = client.fetch_candidate_issues().await.unwrap();

    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0].identifier, "octo-org/octo-repo#1");
    assert_eq!(issues[1].identifier, "other-org/other-repo#2");
}

#[tokio::test]
async fn fetches_issue_states_by_node_ids_from_project_items() {
    let refreshed = json!({
        "data": {
            "nodes": [issue_node_with_project_item("I_123", 123, "Review")]
        }
    });
    let server = TestServer::new(vec![ok(refreshed)]);
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    let issues = client
        .fetch_issue_states_by_ids(&["I_123".to_string(), "I_missing".to_string()])
        .await
        .unwrap();

    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].id, "I_123");
    assert_eq!(issues[0].state, "Review");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_str(request_body(&requests[0])).unwrap();
    assert!(
        body["query"]
            .as_str()
            .unwrap()
            .contains("SymphonyIssueStates")
    );
    assert!(
        body["query"]
            .as_str()
            .unwrap()
            .contains("project {\n            number")
    );
    assert!(
        body["query"]
            .as_str()
            .unwrap()
            .contains("... on Organization { login }")
    );
    assert!(body["query"].as_str().unwrap().contains("$ids: [ID!]!"));
    assert_eq!(body["variables"]["ids"], json!(["I_123", "I_missing"]));
}

#[tokio::test]
async fn candidate_fetch_pages_nested_labels_and_field_values_before_normalizing() {
    let mut content = issue_node("I_nested", 12, &["Bug"]);
    content.as_object_mut().unwrap().insert(
        "labels".to_string(),
        json!({
            "pageInfo": {"hasNextPage": true, "endCursor": "label-cursor-1"},
            "nodes": [{"name": "Bug"}]
        }),
    );
    let item = json!({
        "id": "ITEM_nested",
        "content": content,
        "fieldValues": {
            "pageInfo": {"hasNextPage": true, "endCursor": "field-cursor-1"},
            "nodes": [text_field("Other", "ignored")]
        }
    });
    let server = TestServer::new(vec![
        ok(project_page(false, None, vec![item])),
        ok(field_values_page(
            false,
            None,
            vec![single_select("Status", "Todo"), text_field("Priority", "8")],
        )),
        ok(labels_page(true, Some("label-cursor-2"), vec!["P1"])),
        ok(labels_page(
            false,
            None,
            vec!["blocked-by:octo-org/octo-repo#8"],
        )),
    ]);
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    let issues = client.fetch_candidate_issues().await.unwrap();

    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].id, "I_nested");
    assert_eq!(issues[0].state, "Todo");
    assert_eq!(issues[0].priority, Some(8));
    assert_eq!(
        issues[0].labels,
        vec!["bug", "p1", "blocked-by:octo-org/octo-repo#8"]
    );
    assert_eq!(
        issues[0].blocked_by[0].identifier.as_deref(),
        Some("octo-org/octo-repo#8")
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    let field_body: Value = serde_json::from_str(request_body(&requests[1])).unwrap();
    assert!(
        field_body["query"]
            .as_str()
            .unwrap()
            .contains("SymphonyProjectItemFieldValues")
    );
    assert_eq!(field_body["variables"]["id"], "ITEM_nested");
    assert_eq!(field_body["variables"]["after"], "field-cursor-1");
    let labels_body: Value = serde_json::from_str(request_body(&requests[2])).unwrap();
    assert!(
        labels_body["query"]
            .as_str()
            .unwrap()
            .contains("SymphonyIssueLabels")
    );
    assert_eq!(labels_body["variables"]["id"], "I_nested");
    assert_eq!(labels_body["variables"]["after"], "label-cursor-1");
    let second_labels_body: Value = serde_json::from_str(request_body(&requests[3])).unwrap();
    assert_eq!(second_labels_body["variables"]["after"], "label-cursor-2");
}

#[tokio::test]
async fn state_refresh_pages_issue_project_items_and_nested_field_values() {
    let mut issue = issue_node("I_refresh", 44, &["Bug"]);
    issue.as_object_mut().unwrap().insert(
        "labels".to_string(),
        json!({
            "pageInfo": {"hasNextPage": true, "endCursor": "label-cursor-1"},
            "nodes": [{"name": "Bug"}]
        }),
    );
    issue.as_object_mut().unwrap().insert(
        "projectItems".to_string(),
        json!({
            "pageInfo": {"hasNextPage": true, "endCursor": "project-item-cursor-1"},
            "nodes": [{
                "id": "ITEM_first",
                "project": configured_project(),
                "fieldValues": {
                    "pageInfo": {"hasNextPage": false, "endCursor": null},
                    "nodes": []
                }
            }]
        }),
    );
    let refreshed = json!({"data": {"nodes": [issue]}});
    let server = TestServer::new(vec![
        ok(refreshed),
        ok(labels_page(
            false,
            None,
            vec!["blocked-by:octo-org/octo-repo#1"],
        )),
        ok(project_items_page(
            false,
            None,
            vec![json!({
                "id": "ITEM_second",
                "project": configured_project(),
                "fieldValues": {
                    "pageInfo": {"hasNextPage": true, "endCursor": "field-cursor-1"},
                    "nodes": []
                }
            })],
        )),
        ok(field_values_page(
            false,
            None,
            vec![
                single_select("Status", "Review"),
                text_field("Priority", "3"),
            ],
        )),
    ]);
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    let issues = client
        .fetch_issue_states_by_ids(&["I_refresh".to_string()])
        .await
        .unwrap();

    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].state, "Review");
    assert_eq!(issues[0].priority, Some(3));
    assert_eq!(
        issues[0].blocked_by[0].identifier.as_deref(),
        Some("octo-org/octo-repo#1")
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    let project_items_body: Value = serde_json::from_str(request_body(&requests[2])).unwrap();
    assert!(
        project_items_body["query"]
            .as_str()
            .unwrap()
            .contains("SymphonyIssueProjectItems")
    );
    assert_eq!(project_items_body["variables"]["id"], "I_refresh");
    assert_eq!(
        project_items_body["variables"]["after"],
        "project-item-cursor-1"
    );
    let field_body: Value = serde_json::from_str(request_body(&requests[3])).unwrap();
    assert_eq!(field_body["variables"]["id"], "ITEM_second");
    assert_eq!(field_body["variables"]["after"], "field-cursor-1");
}

#[tokio::test]
async fn pagination_boundaries_fail_without_returning_partially_normalized_issues() {
    let field_cursor_missing = json!({
        "id": "ITEM_field_cursor",
        "content": issue_node("I_field_cursor", 60, &["Bug"]),
        "fieldValues": {
            "pageInfo": {"hasNextPage": true, "endCursor": null},
            "nodes": [single_select("Status", "Todo")]
        }
    });
    let field_cursor_client = client(
        TestServer::new(vec![ok(project_page(
            false,
            None,
            vec![field_cursor_missing],
        ))])
        .url(),
        vec!["Todo"],
        BTreeMap::new(),
    );
    assert_tracker_kind(
        field_cursor_client
            .fetch_candidate_issues()
            .await
            .unwrap_err(),
        "github_pagination",
    );

    let mut label_content = issue_node("I_label_cursor", 61, &["Bug"]);
    label_content.as_object_mut().unwrap().insert(
        "labels".to_string(),
        json!({
            "pageInfo": {"hasNextPage": true, "endCursor": null},
            "nodes": [{"name": "Bug"}]
        }),
    );
    let label_cursor_missing = json!({
        "id": "ITEM_label_cursor",
        "content": label_content,
        "fieldValues": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [single_select("Status", "Todo")]
        }
    });
    let label_cursor_client = client(
        TestServer::new(vec![ok(project_page(
            false,
            None,
            vec![label_cursor_missing],
        ))])
        .url(),
        vec!["Todo"],
        BTreeMap::new(),
    );
    assert_tracker_kind(
        label_cursor_client
            .fetch_candidate_issues()
            .await
            .unwrap_err(),
        "github_pagination",
    );

    let mut project_item_cursor_missing = issue_node("I_item_cursor", 62, &["Bug"]);
    project_item_cursor_missing.as_object_mut().unwrap().insert(
        "projectItems".to_string(),
        json!({
            "pageInfo": {"hasNextPage": true, "endCursor": null},
            "nodes": []
        }),
    );
    let project_item_cursor_client = client(
        TestServer::new(vec![ok(
            json!({"data": {"nodes": [project_item_cursor_missing]}}),
        )])
        .url(),
        vec!["Todo"],
        BTreeMap::new(),
    );
    assert_tracker_kind(
        project_item_cursor_client
            .fetch_issue_states_by_ids(&["I_item_cursor".to_string()])
            .await
            .unwrap_err(),
        "github_pagination",
    );

    let mut nested_field_cursor_missing = issue_node("I_refresh_field_cursor", 63, &["Bug"]);
    nested_field_cursor_missing.as_object_mut().unwrap().insert(
        "projectItems".to_string(),
        json!({
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [{
                "id": "ITEM_refresh_field_cursor",
                "project": configured_project(),
                "fieldValues": {
                    "pageInfo": {"hasNextPage": true, "endCursor": null},
                    "nodes": [single_select("Status", "Todo")]
                }
            }]
        }),
    );
    let nested_field_cursor_client = client(
        TestServer::new(vec![ok(
            json!({"data": {"nodes": [nested_field_cursor_missing]}}),
        )])
        .url(),
        vec!["Todo"],
        BTreeMap::new(),
    );
    assert_tracker_kind(
        nested_field_cursor_client
            .fetch_issue_states_by_ids(&["I_refresh_field_cursor".to_string()])
            .await
            .unwrap_err(),
        "github_pagination",
    );

    let missing_page_info = json!({
        "id": "ITEM_missing_page_info",
        "content": issue_node("I_missing_page_info", 64, &["Bug"]),
        "fieldValues": {"nodes": [single_select("Status", "Todo")]}
    });
    let malformed_connection_client = client(
        TestServer::new(vec![ok(project_page(false, None, vec![missing_page_info]))]).url(),
        vec!["Todo"],
        BTreeMap::new(),
    );
    assert_tracker_kind(
        malformed_connection_client
            .fetch_candidate_issues()
            .await
            .unwrap_err(),
        "github_malformed",
    );
}

#[tokio::test]
async fn project_item_pagination_has_an_explicit_safety_limit() {
    let responses = (0..1_000)
        .map(|page| {
            ok(project_page(
                true,
                Some(&format!("cursor-{page}")),
                Vec::new(),
            ))
        })
        .collect();
    let server = TestServer::new(responses);
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    let err = client.fetch_candidate_issues().await.unwrap_err();

    assert_tracker_kind(err, "github_pagination");
    assert_eq!(server.requests().len(), 1_000);
}

#[tokio::test]
async fn state_refresh_uses_only_the_configured_project_status() {
    let mut issue = issue_node("I_multiple_projects", 55, &["Bug"]);
    issue.as_object_mut().unwrap().insert(
        "projectItems".to_string(),
        json!({
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [
                issue_project_item(
                    "ITEM_wrong_owner",
                    project("Organization", "other-org", 7),
                    vec![single_select("Status", "Done")],
                ),
                issue_project_item(
                    "ITEM_wrong_owner_type",
                    project("User", "octo-org", 7),
                    vec![single_select("Status", "In Progress")],
                ),
                issue_project_item(
                    "ITEM_wrong_number",
                    project("Organization", "octo-org", 8),
                    vec![single_select("Status", "Review")],
                ),
                issue_project_item(
                    "ITEM_configured",
                    configured_project(),
                    vec![single_select("Status", "Todo")],
                )
            ]
        }),
    );
    let server = TestServer::new(vec![ok(json!({"data": {"nodes": [issue]}}))]);
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    let issues = client
        .fetch_issue_states_by_ids(&["I_multiple_projects".to_string()])
        .await
        .unwrap();

    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].state, "Todo");
}

#[tokio::test]
async fn state_refresh_matches_user_owned_configured_project() {
    let mut issue = issue_node("I_user_project", 56, &["Bug"]);
    issue.as_object_mut().unwrap().insert(
        "projectItems".to_string(),
        json!({
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [
                issue_project_item(
                    "ITEM_org",
                    configured_project(),
                    vec![single_select("Status", "Done")],
                ),
                issue_project_item(
                    "ITEM_user",
                    project("User", "octo-user", 7),
                    vec![single_select("Status", "Review")],
                )
            ]
        }),
    );
    let server = TestServer::new(vec![ok(json!({"data": {"nodes": [issue]}}))]);
    let client = client_for_owner_type(
        server.url(),
        vec!["Todo"],
        BTreeMap::new(),
        GithubProjectOwnerType::User,
    );

    let issues = client
        .fetch_issue_states_by_ids(&["I_user_project".to_string()])
        .await
        .unwrap();

    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].state, "Review");
}

#[tokio::test]
async fn state_refresh_treats_absence_from_configured_project_as_missing_state() {
    let mut not_a_member = issue_node("I_not_a_member", 57, &["Bug"]);
    not_a_member.as_object_mut().unwrap().insert(
        "projectItems".to_string(),
        json!({
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [issue_project_item(
                "ITEM_other",
                project("Organization", "other-org", 7),
                vec![single_select("Status", "Todo")],
            )]
        }),
    );
    let server = TestServer::new(vec![ok(json!({
        "data": {"nodes": [not_a_member]}
    }))]);
    let not_member_client = client(server.url(), vec!["Todo"], BTreeMap::new());

    let issues = not_member_client
        .fetch_issue_states_by_ids(&["I_not_a_member".to_string()])
        .await
        .unwrap();

    assert!(issues.is_empty());
}

#[tokio::test]
async fn state_refresh_rejects_configured_project_item_missing_status() {
    let mut no_status = issue_node("I_missing_status", 58, &["Bug"]);
    no_status.as_object_mut().unwrap().insert(
        "projectItems".to_string(),
        json!({
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [issue_project_item(
                "ITEM_no_status",
                configured_project(),
                vec![text_field("Priority", "1")],
            )]
        }),
    );
    let server = TestServer::new(vec![ok(json!({"data": {"nodes": [no_status]}}))]);
    let missing_status_client = client(server.url(), vec!["Todo"], BTreeMap::new());
    let err = missing_status_client
        .fetch_issue_states_by_ids(&["I_missing_status".to_string()])
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("configured GitHub project item missing Status value")
    );
}

#[tokio::test]
async fn tracker_requests_timeout_after_thirty_seconds() {
    let server = DelayedServer::new(Duration::from_secs(35));
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    let err = tokio::time::timeout(Duration::from_secs(31), client.fetch_candidate_issues())
        .await
        .expect("tracker request must be bounded to 30 seconds")
        .unwrap_err();

    assert_tracker_kind(err, "github_transport");
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn empty_state_and_id_inputs_do_not_call_api() {
    let server = TestServer::new(Vec::new());
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    assert!(client.fetch_issues_by_states(&[]).await.unwrap().is_empty());
    assert!(
        client
            .fetch_issue_states_by_ids(&[])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn maps_transport_status_graphql_malformed_and_pagination_errors() {
    let refused = TcpListener::bind("127.0.0.1:0").unwrap();
    let refused_url = format!("http://{}/graphql", refused.local_addr().unwrap());
    drop(refused);
    let refused_client = client(refused_url, vec!["Todo"], BTreeMap::new());
    let err = refused_client.fetch_candidate_issues().await.unwrap_err();
    assert_tracker_kind(err, "github_transport");

    let cases = vec![
        (
            vec![response(500, json!({"message":"nope"}))],
            "github_status",
        ),
        (
            vec![ok(
                json!({"errors":[{"message":"bad query"}], "data": null}),
            )],
            "github_graphql",
        ),
        (
            vec![ok(json!({"data": {"organization": {"projectV2": {}}}}))],
            "github_malformed",
        ),
        (
            vec![ok(project_page(true, None, Vec::new()))],
            "github_pagination",
        ),
    ];

    for (responses, expected_kind) in cases {
        let server = TestServer::new(responses);
        let client = client(server.url(), vec!["Todo"], BTreeMap::new());
        let err = client.fetch_candidate_issues().await.unwrap_err();
        assert_tracker_kind(err, expected_kind);
    }
}

#[tokio::test]
async fn tracker_writer_moves_configured_project_statuses_through_trait_object() {
    let states = ["Started", "Auto Approved", "High Review"];
    let mut responses = Vec::new();
    for _ in states {
        responses.push(ok(project_status_fields_page()));
        responses.push(ok(issue_project_items_page_for_writer()));
        responses.push(ok(json!({"data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "PVTITEM"}}}})));
    }
    let server = TestServer::new(responses);
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());
    let writer: &dyn TrackerWriter = &client;

    for state in states {
        writer
            .move_issue_to_state(&writer_issue(), state)
            .await
            .unwrap();
    }

    let requests = server.requests();
    assert_eq!(requests.len(), 9);
    for (index, state) in states.into_iter().enumerate() {
        let status_lookup: Value =
            serde_json::from_str(request_body(&requests[index * 3])).unwrap();
        assert!(
            status_lookup["query"]
                .as_str()
                .unwrap()
                .contains("SymphonyProjectStatusFields")
        );
        assert_eq!(status_lookup["variables"]["projectOwnerLogin"], "octo-org");
        assert_eq!(status_lookup["variables"]["projectNumber"], 7);

        let item_lookup: Value =
            serde_json::from_str(request_body(&requests[index * 3 + 1])).unwrap();
        assert!(
            item_lookup["query"]
                .as_str()
                .unwrap()
                .contains("SymphonyIssueProjectItems")
        );
        assert_eq!(item_lookup["variables"]["id"], "ISSUE_1");

        let mutation: Value = serde_json::from_str(request_body(&requests[index * 3 + 2])).unwrap();
        assert!(
            mutation["query"]
                .as_str()
                .unwrap()
                .contains("SymphonyUpdateProjectStatus")
        );
        assert_eq!(mutation["variables"]["projectId"], "PVT_7");
        assert_eq!(mutation["variables"]["itemId"], "PVTITEM_1");
        assert_eq!(mutation["variables"]["fieldId"], "FIELD_STATUS");
        assert_eq!(
            mutation["variables"]["optionId"],
            format!("OPTION_{}", state.replace(' ', "_").to_ascii_uppercase())
        );
    }
}

#[tokio::test]
async fn tracker_writer_uses_tracker_error_surface() {
    let refused = TcpListener::bind("127.0.0.1:0").unwrap();
    let refused_url = format!("http://{}/graphql", refused.local_addr().unwrap());
    drop(refused);
    let refused_client = client(refused_url, vec!["Todo"], BTreeMap::new());
    let err = refused_client
        .move_issue_to_state(&writer_issue(), "Started")
        .await
        .unwrap_err();
    assert_tracker_kind(err, "github_transport");

    let cases = vec![
        (
            vec![response(500, json!({"message":"nope"}))],
            "github_status",
        ),
        (
            vec![ok(
                json!({"errors":[{"message":"bad query"}], "data": null}),
            )],
            "github_graphql",
        ),
        (
            vec![ok(json!({"data": {"organization": {"projectV2": {}}}}))],
            "github_malformed",
        ),
    ];
    for (responses, expected_kind) in cases {
        let server = TestServer::new(responses);
        let client = client(server.url(), vec!["Todo"], BTreeMap::new());
        let err = client
            .move_issue_to_state(&writer_issue(), "Started")
            .await
            .unwrap_err();
        assert_tracker_kind(err, expected_kind);
    }
}

#[tokio::test]
async fn tracker_writer_pages_fields_and_issue_items_before_mutating_configured_project() {
    let first_fields = json!({
        "data": {
            "organization": {
                "projectV2": {
                    "id": "PVT_7",
                    "fields": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "field-cursor" },
                        "nodes": [{
                            "id": "FIELD_PRIORITY",
                            "name": "Priority",
                            "options": []
                        }]
                    }
                }
            }
        }
    });
    let first_items = json!({
        "data": {
            "node": {
                "projectItems": {
                    "pageInfo": { "hasNextPage": true, "endCursor": "item-cursor" },
                    "nodes": [{
                        "id": "PVTITEM_OTHER",
                        "project": {
                            "id": "PVT_OTHER",
                            "number": 7,
                            "owner": { "__typename": "Organization", "login": "octo-org" }
                        },
                        "fieldValues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }]
                }
            }
        }
    });
    let server = TestServer::new(vec![
        ok(first_fields),
        ok(project_status_fields_page()),
        ok(first_items),
        ok(issue_project_items_page_for_writer()),
        ok(
            json!({"data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "PVTITEM_1"}}}}),
        ),
    ]);
    let client = client(server.url(), vec!["Todo"], BTreeMap::new());

    client
        .move_issue_to_state(&writer_issue(), "Started")
        .await
        .unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 5);
    let field_page: Value = serde_json::from_str(request_body(&requests[1])).unwrap();
    assert_eq!(field_page["variables"]["after"], "field-cursor");
    let item_page: Value = serde_json::from_str(request_body(&requests[3])).unwrap();
    assert_eq!(item_page["variables"]["after"], "item-cursor");
    let mutation: Value = serde_json::from_str(request_body(&requests[4])).unwrap();
    assert_eq!(mutation["variables"]["itemId"], "PVTITEM_1");
}

fn writer_issue() -> Issue {
    Issue {
        id: "ISSUE_1".to_string(),
        identifier: "octo-org/octo-repo#1".to_string(),
        title: "Tracker writer test".to_string(),
        description: None,
        priority: None,
        state: "Todo".to_string(),
        branch_name: None,
        url: None,
        labels: Vec::new(),
        blocked_by: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

fn project_status_fields_page() -> Value {
    json!({
        "data": {
            "organization": {
                "projectV2": {
                    "id": "PVT_7",
                    "fields": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [{
                            "id": "FIELD_STATUS",
                            "name": "Status",
                            "options": [
                                { "id": "OPTION_STARTED", "name": "Started" },
                                { "id": "OPTION_AUTO_APPROVED", "name": "Auto Approved" },
                                { "id": "OPTION_HIGH_REVIEW", "name": "High Review" }
                            ]
                        }]
                    }
                }
            }
        }
    })
}

fn issue_project_items_page_for_writer() -> Value {
    json!({
        "data": {
            "node": {
                "projectItems": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": [{
                        "id": "PVTITEM_1",
                        "project": {
                            "id": "PVT_7",
                            "number": 7,
                            "owner": { "__typename": "Organization", "login": "octo-org" }
                        },
                        "fieldValues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }]
                }
            }
        }
    })
}

fn raw_executor(endpoint: String) -> GitHubGraphqlExecutor {
    client(endpoint, vec!["Todo"], BTreeMap::new()).graphql_executor()
}

fn client(
    endpoint: String,
    active_states: Vec<&str>,
    priority_labels: BTreeMap<String, i64>,
) -> GitHubTrackerClient {
    client_for_owner_type(
        endpoint,
        active_states,
        priority_labels,
        GithubProjectOwnerType::Organization,
    )
}

fn client_with_repositories(
    endpoint: String,
    active_states: Vec<&str>,
    repositories: Vec<GithubRepositoryConfig>,
) -> GitHubTrackerClient {
    let primary = repositories.first().unwrap();
    let tracker = TrackerConfig {
        kind: "github".to_string(),
        endpoint,
        allow_insecure_loopback: true,
        api_key: Some("test-token".to_string()),
        active_states: active_states.into_iter().map(str::to_string).collect(),
        terminal_states: vec!["Done".to_string()],
        github: Some(GithubConfig {
            repository_owner: primary.owner.clone(),
            repository_name: primary.name.clone(),
            repositories,
            project_owner_type: GithubProjectOwnerType::Organization,
            project_owner_login: "octo-org".to_string(),
            project_number: 7,
            status_field_name: "Status".to_string(),
            priority_field_name: Some("Priority".to_string()),
            blocker_field_name: Some("Blocked By".to_string()),
            blocker_label_prefix: Some("blocked-by".to_string()),
            priority_labels: BTreeMap::new(),
        }),
    };
    GitHubTrackerClient::from_tracker_config(&tracker).unwrap()
}

fn client_for_owner_type(
    endpoint: String,
    active_states: Vec<&str>,
    priority_labels: BTreeMap<String, i64>,
    project_owner_type: GithubProjectOwnerType,
) -> GitHubTrackerClient {
    let project_owner_login = match project_owner_type {
        GithubProjectOwnerType::Organization => "octo-org",
        GithubProjectOwnerType::User => "octo-user",
    };
    let tracker = TrackerConfig {
        kind: "github".to_string(),
        endpoint,
        allow_insecure_loopback: true,
        api_key: Some("test-token".to_string()),
        active_states: active_states.into_iter().map(str::to_string).collect(),
        terminal_states: vec!["Done".to_string()],
        github: Some(GithubConfig {
            repository_owner: "octo-org".to_string(),
            repository_name: "octo-repo".to_string(),
            repositories: vec![GithubRepositoryConfig {
                owner: "octo-org".to_string(),
                name: "octo-repo".to_string(),
            }],
            project_owner_type,
            project_owner_login: project_owner_login.to_string(),
            project_number: 7,
            status_field_name: "Status".to_string(),
            priority_field_name: Some("Priority".to_string()),
            blocker_field_name: Some("Blocked By".to_string()),
            blocker_label_prefix: Some("blocked-by".to_string()),
            priority_labels,
        }),
    };
    GitHubTrackerClient::from_tracker_config(&tracker).unwrap()
}

fn project_page(has_next_page: bool, end_cursor: Option<&str>, nodes: Vec<Value>) -> Value {
    project_page_for_owner(
        GithubProjectOwnerType::Organization,
        has_next_page,
        end_cursor,
        nodes,
    )
}

fn project_page_for_owner(
    owner_type: GithubProjectOwnerType,
    has_next_page: bool,
    end_cursor: Option<&str>,
    nodes: Vec<Value>,
) -> Value {
    let owner = json!({
        "projectV2": {
            "items": {
                "pageInfo": {"hasNextPage": has_next_page, "endCursor": end_cursor},
                "nodes": nodes
            }
        }
    });
    match owner_type {
        GithubProjectOwnerType::Organization => json!({
            "data": {
                "organization": owner,
                "user": null
            }
        }),
        GithubProjectOwnerType::User => json!({
            "data": {
                "organization": null,
                "user": owner
            }
        }),
    }
}

fn project_item(
    id: &str,
    number: i64,
    status: &str,
    labels: &[&str],
    priority: Option<&str>,
    blocker: Option<&str>,
) -> Value {
    let mut fields = vec![single_select("Status", status)];
    if let Some(priority) = priority {
        fields.push(text_field("Priority", priority));
    }
    if let Some(blocker) = blocker {
        fields.push(text_field("Blocked By", blocker));
    }
    json!({
        "id": format!("ITEM_{id}"),
        "content": issue_node(id, number, labels),
        "fieldValues": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": fields
        }
    })
}

fn issue_node_with_project_item(id: &str, number: i64, status: &str) -> Value {
    let mut node = issue_node(id, number, &["Bug"]);
    node.as_object_mut().unwrap().insert(
        "projectItems".to_string(),
        json!({
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [{
                "id": format!("ITEM_{id}"),
                "project": configured_project(),
                "fieldValues": {
                    "pageInfo": {"hasNextPage": false, "endCursor": null},
                    "nodes": [single_select("Status", status)]
                }
            }]
        }),
    );
    node
}

fn configured_project() -> Value {
    project("Organization", "octo-org", 7)
}

fn project(owner_type: &str, owner_login: &str, number: i64) -> Value {
    json!({
        "number": number,
        "owner": {"__typename": owner_type, "login": owner_login}
    })
}

fn issue_project_item(id: &str, project: Value, fields: Vec<Value>) -> Value {
    json!({
        "id": id,
        "project": project,
        "fieldValues": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": fields
        }
    })
}
fn issue_node(id: &str, number: i64, labels: &[&str]) -> Value {
    json!({
        "__typename": "Issue",
        "id": id,
        "number": number,
        "title": format!("issue {number}"),
        "body": format!("body {number}"),
        "url": format!("https://github.test/octo-org/octo-repo/issues/{number}"),
        "createdAt": "2026-05-13T00:00:00Z",
        "updatedAt": "2026-05-13T01:00:00Z",
        "repository": {
            "nameWithOwner": "octo-org/octo-repo",
            "name": "octo-repo",
            "owner": {"login": "octo-org"}
        },
        "labels": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": labels.iter().map(|name| json!({"name": name})).collect::<Vec<_>>()
        }
    })
}

fn labels_page(has_next_page: bool, end_cursor: Option<&str>, labels: Vec<&str>) -> Value {
    json!({
        "data": {
            "node": {
                "__typename": "Issue",
                "labels": {
                    "pageInfo": {"hasNextPage": has_next_page, "endCursor": end_cursor},
                    "nodes": labels.into_iter().map(|name| json!({"name": name})).collect::<Vec<_>>()
                }
            }
        }
    })
}

fn field_values_page(has_next_page: bool, end_cursor: Option<&str>, nodes: Vec<Value>) -> Value {
    json!({
        "data": {
            "node": {
                "__typename": "ProjectV2Item",
                "fieldValues": {
                    "pageInfo": {"hasNextPage": has_next_page, "endCursor": end_cursor},
                    "nodes": nodes
                }
            }
        }
    })
}

fn project_items_page(has_next_page: bool, end_cursor: Option<&str>, nodes: Vec<Value>) -> Value {
    json!({
        "data": {
            "node": {
                "__typename": "Issue",
                "projectItems": {
                    "pageInfo": {"hasNextPage": has_next_page, "endCursor": end_cursor},
                    "nodes": nodes
                }
            }
        }
    })
}

fn single_select(field: &str, value: &str) -> Value {
    json!({"__typename": "ProjectV2ItemFieldSingleSelectValue", "name": value, "field": {"name": field}})
}

fn text_field(field: &str, value: &str) -> Value {
    json!({"__typename": "ProjectV2ItemFieldTextValue", "text": value, "field": {"name": field}})
}

fn ok(body: Value) -> HttpResponse {
    response(200, body)
}

fn redirect(location: &str) -> HttpResponse {
    HttpResponse {
        status: 302,
        body: String::new(),
        headers: vec![("location".to_string(), location.to_string())],
    }
}

fn response(status: u16, body: Value) -> HttpResponse {
    HttpResponse {
        status,
        body: body.to_string(),
        headers: Vec::new(),
    }
}

fn raw_response(status: u16, body: &str) -> HttpResponse {
    HttpResponse {
        status,
        body: body.to_string(),
        headers: Vec::new(),
    }
}

fn assert_tracker_kind(err: SymphonyError, expected: &'static str) {
    match err {
        SymphonyError::Tracker { kind, .. } => assert_eq!(kind, expected),
        other => panic!("unexpected error: {other:?}"),
    }
}

fn request_body(request: &str) -> &str {
    request.split("\r\n\r\n").nth(1).unwrap()
}

struct HttpResponse {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}

struct TestServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    fn new(responses: Vec<HttpResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/graphql", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                request_log.lock().unwrap().push(request);
                write_http_response(&mut stream, response);
            }
        });
        Self { url, requests }
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

struct DelayedServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl DelayedServer {
    fn new(delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/graphql", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            request_log
                .lock()
                .unwrap()
                .push(read_http_request(&mut stream));
            thread::sleep(delay);
        });
        Self { url, requests }
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).unwrap();
        assert_ne!(read, 0);
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = find_subslice(&buffer, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buffer[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .or_else(|| {
                    headers
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                })
                .unwrap_or("0")
                .parse::<usize>()
                .unwrap();
            let needed = header_end + 4 + content_length;
            while buffer.len() < needed {
                let read = stream.read(&mut chunk).unwrap();
                assert_ne!(read, 0);
                buffer.extend_from_slice(&chunk[..read]);
            }
            buffer.truncate(needed);
            return String::from_utf8(buffer).unwrap();
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_http_response(stream: &mut std::net::TcpStream, response: HttpResponse) {
    let reason = if response.status == 200 {
        "OK"
    } else {
        "Error"
    };
    let headers = response
        .headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    let reply = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{}",
        response.status,
        reason,
        response.body.len(),
        response.body
    );
    stream.write_all(reply.as_bytes()).unwrap();
}

struct WireResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: WireBody,
    declared_content_length: Option<usize>,
}

enum WireBody {
    Fixed(Vec<u8>),
    Chunked(Vec<Vec<u8>>),
}

impl WireResponse {
    fn fixed(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: WireBody::Fixed(body),
            declared_content_length: None,
        }
    }

    fn chunked(status: u16, chunks: Vec<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: WireBody::Chunked(chunks),
            declared_content_length: None,
        }
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    fn with_declared_content_length(mut self, length: usize) -> Self {
        self.declared_content_length = Some(length);
        self
    }
}

struct WireServer {
    url: String,
    requests: Arc<AtomicUsize>,
}

impl WireServer {
    fn new(responses: Vec<WireResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/graphql", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = Arc::clone(&requests);
        thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_http_request(&mut stream);
                request_count.fetch_add(1, Ordering::Relaxed);
                write_wire_response(&mut stream, response);
            }
        });
        Self { url, requests }
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }
}

fn write_wire_response(stream: &mut std::net::TcpStream, response: WireResponse) {
    let reason = if response.status == 200 {
        "OK"
    } else {
        "Error"
    };
    let mut header = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\nconnection: close\r\n",
        response.status, reason
    );
    for (name, value) in response.headers {
        header.push_str(&format!("{name}: {value}\r\n"));
    }
    match response.body {
        WireBody::Fixed(body) => {
            let length = response.declared_content_length.unwrap_or(body.len());
            header.push_str(&format!("content-length: {length}\r\n\r\n"));
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        }
        WireBody::Chunked(chunks) => {
            header.push_str("transfer-encoding: chunked\r\n\r\n");
            let _ = stream.write_all(header.as_bytes());
            for chunk in chunks {
                let _ = stream.write_all(format!("{:X}\r\n", chunk.len()).as_bytes());
                let _ = stream.write_all(&chunk);
                let _ = stream.write_all(b"\r\n");
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    }
}

fn graphql_padding_overhead() -> usize {
    br#"{"data":{"padding":""#.len() + br#""}}"#.len()
}

fn exact_limit_graphql_body() -> Vec<u8> {
    let mut body = br#"{"data":{"padding":""#.to_vec();
    body.extend(std::iter::repeat_n(
        b'x',
        MAX_GITHUB_GRAPHQL_RESPONSE_BYTES - graphql_padding_overhead(),
    ));
    body.extend_from_slice(br#""}}"#);
    body
}

fn gzip_repeated_byte(byte: u8, length: usize) -> Vec<u8> {
    assert!(length > 0);
    let mut bits = DeflateBits::default();
    bits.write(0b011, 3);
    bits.fixed_symbol(u16::from(byte));
    let mut remaining = length - 1;
    while remaining >= 3 {
        let matched = remaining.min(258);
        bits.fixed_length(matched);
        bits.write(0, 5);
        remaining -= matched;
    }
    for _ in 0..remaining {
        bits.fixed_symbol(u16::from(byte));
    }
    bits.fixed_symbol(256);

    let mut gzip = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    gzip.extend(bits.finish());
    gzip.extend(crc32_repeated_byte(byte, length).to_le_bytes());
    gzip.extend((length as u32).to_le_bytes());
    gzip
}

#[derive(Default)]
struct DeflateBits {
    bytes: Vec<u8>,
    pending: u8,
    pending_bits: u8,
}

impl DeflateBits {
    fn write(&mut self, value: u32, width: u8) {
        for bit in 0..width {
            self.pending |= (((value >> bit) & 1) as u8) << self.pending_bits;
            self.pending_bits += 1;
            if self.pending_bits == 8 {
                self.bytes.push(self.pending);
                self.pending = 0;
                self.pending_bits = 0;
            }
        }
    }

    fn fixed_symbol(&mut self, symbol: u16) {
        let (code, width) = match symbol {
            0..=143 => (0x30 + u32::from(symbol), 8),
            144..=255 => (0x190 + u32::from(symbol - 144), 9),
            256..=279 => (u32::from(symbol - 256), 7),
            280..=287 => (0xc0 + u32::from(symbol - 280), 8),
            _ => panic!("invalid fixed-Huffman symbol"),
        };
        self.write(reverse_bits(code, width), width);
    }

    fn fixed_length(&mut self, length: usize) {
        let (minimum, symbol, extra_bits) = match length {
            3..=10 => (3, 257 + (length - 3) as u16, 0),
            11..=12 => (11, 265, 1),
            13..=14 => (13, 266, 1),
            15..=16 => (15, 267, 1),
            17..=18 => (17, 268, 1),
            19..=22 => (19, 269, 2),
            23..=26 => (23, 270, 2),
            27..=30 => (27, 271, 2),
            31..=34 => (31, 272, 2),
            35..=42 => (35, 273, 3),
            43..=50 => (43, 274, 3),
            51..=58 => (51, 275, 3),
            59..=66 => (59, 276, 3),
            67..=82 => (67, 277, 4),
            83..=98 => (83, 278, 4),
            99..=114 => (99, 279, 4),
            115..=130 => (115, 280, 5),
            131..=162 => (131, 281, 5),
            163..=194 => (163, 282, 5),
            195..=226 => (195, 283, 5),
            227..=257 => (227, 284, 5),
            258 => (258, 285, 0),
            _ => panic!("invalid DEFLATE match length"),
        };
        self.fixed_symbol(symbol);
        self.write((length - minimum) as u32, extra_bits);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.pending_bits > 0 {
            self.bytes.push(self.pending);
        }
        self.bytes
    }
}

fn reverse_bits(value: u32, width: u8) -> u32 {
    value.reverse_bits() >> (u32::BITS - u32::from(width))
}

fn crc32_repeated_byte(byte: u8, length: usize) -> u32 {
    let mut crc = !0_u32;
    for _ in 0..length {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0_u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}
