use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};
use symphony::config::{GithubConfig, GithubProjectOwnerType, TrackerConfig};
use symphony::error::SymphonyError;
use symphony::tracker::TrackerClient;
use symphony::tracker::github::GitHubTrackerClient;

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
    assert_eq!(body["variables"]["ids"], json!(["I_123", "I_missing"]));
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

fn client(
    endpoint: String,
    active_states: Vec<&str>,
    priority_labels: BTreeMap<String, i64>,
) -> GitHubTrackerClient {
    let tracker = TrackerConfig {
        kind: "github".to_string(),
        endpoint,
        api_key: Some("test-token".to_string()),
        active_states: active_states.into_iter().map(str::to_string).collect(),
        terminal_states: vec!["Done".to_string()],
        github: Some(GithubConfig {
            repository_owner: "octo-org".to_string(),
            repository_name: "octo-repo".to_string(),
            project_owner_type: GithubProjectOwnerType::Organization,
            project_owner_login: "octo-org".to_string(),
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
    json!({
        "data": {
            "organization": {
                "projectV2": {
                    "items": {
                        "pageInfo": {"hasNextPage": has_next_page, "endCursor": end_cursor},
                        "nodes": nodes
                    }
                }
            },
            "user": null
        }
    })
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
        "content": issue_node(id, number, labels),
        "fieldValues": {"nodes": fields}
    })
}

fn issue_node_with_project_item(id: &str, number: i64, status: &str) -> Value {
    let mut node = issue_node(id, number, &["Bug"]);
    node.as_object_mut().unwrap().insert(
        "projectItems".to_string(),
        json!({"nodes": [{"fieldValues": {"nodes": [single_select("Status", status)]}}]}),
    );
    node
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
        "labels": {"nodes": labels.iter().map(|name| json!({"name": name})).collect::<Vec<_>>()}
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

fn response(status: u16, body: Value) -> HttpResponse {
    HttpResponse {
        status,
        body: body.to_string(),
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
    let reply = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        response.status,
        reason,
        response.body.len(),
        response.body
    );
    stream.write_all(reply.as_bytes()).unwrap();
}
