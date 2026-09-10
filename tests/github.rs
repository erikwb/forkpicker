use forkpicker::{
    github::{Github, RepoName, Repository},
    model::Coverage,
};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    thread,
};

fn server(responses: Vec<(String, String)>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (headers, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                request.push_str(&line);
            }
            let length = request
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if length > 0 {
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                request.push_str(&String::from_utf8_lossy(&body));
            }
            requests.push(request);
            write!(stream,"HTTP/1.1 {headers}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        requests
    });
    (base, handle)
}

#[test]
fn http_status_survives_error_context() {
    let (base, handle) = server(vec![
        ("404 Not Found".into(), "{}".into()),
        ("500 Internal Server Error".into(), "{}".into()),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(cache.path().into(), true, 10, base, None).unwrap();
    for status in [
        reqwest::StatusCode::NOT_FOUND,
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let error = api
            .get("repos/owner/repo/releases/latest")
            .unwrap_err()
            .context("read release");
        assert_eq!(
            error.downcast_ref::<reqwest::Error>().unwrap().status(),
            Some(status)
        );
    }
    assert_eq!(handle.join().unwrap().len(), 2);
}

#[test]
fn discussions_include_attributed_comments_and_explicit_omissions() {
    let data = json!({"data":{"search":{"pageInfo":{"hasNextPage":true},"nodes":[{
        "number":14999,"title":"HDR reference white","body":"A proposal","url":"https://github.com/upstream/example/discussions/14999","isAnswered":false,"closed":false,"upvoteCount":17,"labels":{"nodes":[{"name":"feature-request"}]},
        "comments":{"totalCount":2,"nodes":[{"author":{"login":"contributor"},"body":"Working patch: https://github.com/fork/example/commit/1234567","url":"https://github.com/upstream/example/discussions/14999#discussioncomment-1","replies":{"totalCount":1,"nodes":[]}}]}
    }]}}});
    let (base, handle) = server(vec![("200 OK".into(), data.to_string())]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(
        cache.path().into(),
        false,
        10,
        base,
        Some("test-token".into()),
    )
    .unwrap();
    let mut coverage = Coverage::default();
    let discussions = api.discussions("upstream/example", Some("HDR"), &mut coverage);
    assert_eq!(discussions.len(), 1);
    assert_eq!(discussions[0].comments[0].author, "contributor");
    assert_eq!(discussions[0].comments_omitted, 2);
    assert_eq!(discussions[0].kind, "discussion");
    assert_eq!(discussions[0].upvotes, Some(17));
    assert_eq!(discussions[0].labels, vec!["feature-request"]);
    assert!(!coverage.warnings.is_empty());
    let requests = handle.join().unwrap();
    assert!(requests[0].starts_with("POST /graphql"));
    assert!(requests[0].contains("type:DISCUSSION"));
    assert!(!requests[0].contains("mutation"));
}

#[test]
fn demand_samples_popular_and_recent_issues_and_records_unknown_discussion_coverage() {
    let item = json!({"number":42,"title":"HDR reference white","body":"Request","html_url":"https://github.com/upstream/example/issues/42","state":"open","reactions":{"+1":23,"-1":50},"labels":[{"name":"priority: high"}],"comments":900});
    let data = json!({"items":[item],"incomplete_results":false});
    let (base, handle) = server(vec![
        ("200 OK".into(), data.to_string()),
        ("200 OK".into(), data.to_string()),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(cache.path().into(), false, 10, base, None).unwrap();
    let snapshot = api.demand(
        "upstream/example",
        Some("HDR"),
        vec!["priority: high".into()],
    );
    assert_eq!(snapshot.threads.len(), 1, "deduplicate across samples");
    assert_eq!(snapshot.threads[0].thumbs_up, Some(23));
    assert_eq!(snapshot.threads[0].labels, vec!["priority: high"]);
    assert!(snapshot
        .warnings
        .iter()
        .any(|w| w.contains("authentication")));
    let requests = handle.join().unwrap();
    assert!(requests[0].contains("sort=reactions-%2B1"));
    assert!(requests[1].contains("sort=updated"));
    assert!(requests[0].contains("is%3Aopen"));
}

#[test]
fn demand_discussions_use_top_search_and_exclude_closed_state() {
    let issue_data = json!({"items":[],"incomplete_results":false});
    let discussions = json!({"data":{"search":{"pageInfo":{"hasNextPage":false},"nodes":[{
        "number":1,"title":"Old requested feature","body":"Request","url":"https://github.com/upstream/example/discussions/1",
        "closed":true,"isAnswered":false,"upvoteCount":100,"comments":{"nodes":[],"totalCount":0}
    }]}}});
    let (base, handle) = server(vec![
        ("200 OK".into(), issue_data.to_string()),
        ("200 OK".into(), issue_data.to_string()),
        ("200 OK".into(), discussions.to_string()),
        ("200 OK".into(), discussions.to_string()),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let mut api =
        Github::with_base(cache.path().into(), false, 10, base, Some("fixture".into())).unwrap();
    let snapshot = api.demand("upstream/example", Some("feature"), Vec::new());
    assert_eq!(snapshot.threads.len(), 1);
    assert_eq!(snapshot.threads[0].state, "closed");
    assert_eq!(snapshot.threads[0].upvotes, Some(100));
    let requests = handle.join().unwrap();
    assert!(requests[2].contains("sort:top"));
    assert!(requests[2].contains("upvoteCount"));
    assert!(!requests[2].contains("field:UPVOTES"));
    assert!(requests[3].contains("sort:updated"));
}

#[test]
fn discussion_authentication_absence_is_visible_without_request() {
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(
        cache.path().into(),
        false,
        10,
        "http://127.0.0.1:1".into(),
        None,
    )
    .unwrap();
    let mut coverage = Coverage::default();
    assert!(api
        .discussions("upstream/example", None, &mut coverage)
        .is_empty());
    assert_eq!(api.requests(), 0);
    assert!(coverage.warnings[0].contains("authentication"));
}

#[test]
fn graphql_errors_are_not_cached_as_empty_success() {
    let (base, handle) = server(vec![(
        "200 OK".into(),
        json!({"errors":[{"message":"Not accessible"}]}).to_string(),
    )]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(
        cache.path().into(),
        false,
        1,
        base,
        Some("test-token".into()),
    )
    .unwrap();
    let mut coverage = Coverage::default();
    assert!(api
        .discussions("upstream/example", None, &mut coverage)
        .is_empty());
    assert!(coverage.warnings[0].contains("Not accessible"));
    handle.join().unwrap();
}
fn repo(id: u64, name: &str, push: &str) -> serde_json::Value {
    json!({"id":id,"full_name":name,"default_branch":"main","pushed_at":push,"forks_count":0,"archived":false,"disabled":false})
}
fn root() -> Repository {
    serde_json::from_value(repo(1, "upstream/example", "2026-01-01")).unwrap()
}

#[test]
fn enumerates_all_pages_before_sorting_recent_activity() {
    let (base, handle) = server(vec![
        (
            "200 OK\r\nLink: <http://example.invalid/page2>; rel=\"next\"".into(),
            json!([repo(2, "new/example", "2026-01-01")]).to_string(),
        ),
        (
            "200 OK".into(),
            json!([repo(3, "old/example", "2026-09-09")]).to_string(),
        ),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(cache.path().into(), false, 10, base, None).unwrap();
    let mut coverage = Coverage::default();
    let forks = api.forks(&root(), &mut coverage);
    assert_eq!(forks[0].full_name, "old/example");
    assert_eq!(coverage.forks_discovered, 2);
    let requests = handle.join().unwrap();
    assert!(requests[1].contains("page=2"));
}

#[test]
fn recursive_discovery_deduplicates_shared_network_entries() {
    let mut parent = repo(2, "parent/example", "2026-09-01");
    parent["forks_count"] = json!(1);
    let child = repo(3, "child/example", "2026-09-02");
    let (base, handle) = server(vec![
        ("200 OK".into(), json!([parent, child.clone()]).to_string()),
        ("200 OK".into(), json!([child]).to_string()),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(cache.path().into(), false, 10, base, None).unwrap();
    let forks = api.forks(&root(), &mut Coverage::default());
    assert_eq!(forks.len(), 2);
    assert!(handle.join().unwrap()[1].contains("repos/parent/example/forks"));
}

#[test]
fn api_budget_returns_partial_coverage_instead_of_claiming_complete() {
    let (base, handle) = server(vec![(
        "200 OK\r\nLink: <http://example.invalid/next>; rel=\"next\"".into(),
        json!([repo(2, "one/example", "2026-09-01")]).to_string(),
    )]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(cache.path().into(), false, 1, base, None).unwrap();
    let mut coverage = Coverage::default();
    assert_eq!(api.forks(&root(), &mut coverage).len(), 1);
    assert!(coverage.warnings[0].contains("incomplete"));
    assert!(api.exhausted());
    handle.join().unwrap();
}

#[test]
fn cache_and_etag_revalidation_preserve_pagination() {
    let (base, handle) = server(vec![
        (
            "200 OK\r\nETag: \"version-1\"\r\nLink: <http://example.invalid/next>; rel=\"next\""
                .into(),
            "[]".into(),
        ),
        ("304 Not Modified".into(), "".into()),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(cache.path().into(), false, 1, base.clone(), None).unwrap();
    assert!(api.get("repos/test/example").unwrap().1);
    assert!(api.get("repos/test/example").unwrap().1);
    assert_eq!(api.requests(), 1);
    assert_eq!(api.hits(), 1);
    let mut refresh = Github::with_base(cache.path().into(), true, 1, base, None).unwrap();
    assert!(refresh.get("repos/test/example").unwrap().1);
    assert!(handle.join().unwrap()[1]
        .to_lowercase()
        .contains("if-none-match: \"version-1\""));
}

#[test]
fn rate_limit_stops_further_requests() {
    let (base,handle)=server(vec![("429 Too Many Requests\r\nRetry-After: 60\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 123456".into(),"{}".into())]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(cache.path().into(), false, 100, base, None).unwrap();
    let error = api.get("repos/test/example").unwrap_err().to_string();
    assert!(error.contains("retry-after=60"));
    assert!(api.exhausted());
    assert!(api.clone().exhausted());
    assert!(api
        .get("repos/test/other")
        .unwrap_err()
        .to_string()
        .contains("budget"));
    handle.join().unwrap();
}

#[test]
fn parallel_clients_share_one_exact_request_budget() {
    let (base, handle) = server(vec![("200 OK".into(), "[]".into()); 3]);
    let cache = tempfile::tempdir().unwrap();
    let api = Github::with_base(cache.path().into(), false, 3, base, None).unwrap();
    let results = forkpicker::parallel::map(&(0..20).collect::<Vec<_>>(), 8, |i, _| {
        api.clone().get(&format!("repos/test/repo{i}"))
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
    assert_eq!(api.requests(), 3);
    assert!(api.exhausted());
    assert_eq!(handle.join().unwrap().len(), 3);
}

#[test]
fn branches_are_paginated_and_default_is_selected_first() {
    let sha = "a".repeat(40);
    let (base, handle) = server(vec![
        (
            "200 OK\r\nLink: <http://example.invalid/next>; rel=\"next\"".into(),
            json!([{"name":"feature/hdr","commit":{"sha":sha}}]).to_string(),
        ),
        (
            "200 OK".into(),
            json!([{"name":"main","commit":{"sha":sha}}]).to_string(),
        ),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(cache.path().into(), false, 10, base, None).unwrap();
    let branches = api.branches(&root()).unwrap();
    assert_eq!(branches.len(), 2);
    assert_eq!(branches[0].name, "main");
    handle.join().unwrap();
}

#[test]
fn repository_arguments_cannot_escape_github_or_become_git_options() {
    for invalid in [
        "../repo",
        "owner/..",
        "https://evil.example/owner/repo",
        "--upload-pack=x/repo",
        "owner/repo/tree/main",
        "owner/repo?token=secret",
    ] {
        assert!(invalid.parse::<RepoName>().is_err(), "{invalid}");
    }
    assert_eq!(
        "https://github.com/owner/repo.git"
            .parse::<RepoName>()
            .unwrap()
            .0,
        "owner/repo"
    );
}

#[test]
fn shortlist_pr_membership_paginates_and_preserves_budget_omissions() {
    use forkpicker::shortlist;
    let pull = json!({"number":17,"title":"Change","draft":false,"state":"open","head":{"sha":"a".repeat(40)},"base":{"ref":"main","repo":{"id":1,"full_name":"upstream/example","default_branch":"main"}}});
    let (base, handle) = server(vec![
        ("200 OK".into(), json!([pull]).to_string()),
        (
            "200 OK\r\nLink: <http://example.invalid/next>; rel=\"next\"".into(),
            json!([{"sha":"a".repeat(40)}]).to_string(),
        ),
        ("200 OK".into(), json!([{"sha":"b".repeat(40)}]).to_string()),
        ("200 OK".into(), pull.to_string()),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(cache.path().into(), false, 4, base, None).unwrap();
    let snapshot = shortlist::collect_pulls(&mut api, "upstream/example").unwrap();
    assert!(snapshot.listing_complete);
    assert!(snapshot.pulls[0].commits_complete);
    assert_eq!(snapshot.pulls[0].commits.len(), 2);
    assert_eq!(snapshot.api_requests, 4);
    assert!(handle.join().unwrap()[2].contains("page=2"));
    let (base, handle) = server(vec![("200 OK".into(), json!([pull]).to_string())]);
    let mut api = Github::with_base(cache.path().into(), false, 1, base, None).unwrap();
    let partial = shortlist::collect_pulls(&mut api, "upstream/example").unwrap();
    assert!(!partial.pulls[0].commits_complete);
    assert!(!partial.warnings.is_empty());
    handle.join().unwrap();
}

#[test]
fn demand_catalog_paginates_without_topic_or_popularity_filters() {
    let discussion = |n, next, cursor| json!({"data":{"repository":{"discussions":{"pageInfo":{"hasNextPage":next,"endCursor":cursor},"nodes":[{"number":n,"title":"Project request","body":"Request","url":format!("https://github.com/upstream/example/discussions/{n}"),"isAnswered":false,"closed":false,"upvoteCount":7,"labels":{"nodes":[]},"comments":{"totalCount":0,"nodes":[]}}]}}}});
    let (base,handle)=server(vec![
        ("200 OK\r\nLink: <http://example.invalid/next>; rel=\"next\"".into(),json!([{"number":1,"title":"Open issue","body":"Request","html_url":"https://github.com/upstream/example/issues/1","state":"open","reactions":{"+1":2}}]).to_string()),
        ("200 OK".into(),json!([{"number":2,"title":"PR","body":"Change","html_url":"https://github.com/upstream/example/pull/2","state":"open","pull_request":{}}]).to_string()),
        ("200 OK".into(),discussion(3,true,"cursor-1").to_string()),
        ("200 OK".into(),discussion(4,false,"cursor-2").to_string()),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let mut api =
        Github::with_base(cache.path().into(), false, 10, base, Some("fixture".into())).unwrap();
    let catalog = api.demand_catalog("upstream/example", vec![]).unwrap();
    assert!(catalog.query.is_none());
    assert_eq!(catalog.threads.len(), 3);
    assert!(!catalog.threads.iter().any(|t| t.is_pull_request));
    let calls = handle.join().unwrap();
    assert!(calls[0].contains("state=open"));
    assert!(calls[1].contains("page=2"));
    assert!(calls[3].contains("cursor-1"));
    assert!(!calls
        .iter()
        .any(|s| s.contains("sort:top") || s.contains("HDR")));
}

#[test]
fn changing_pr_head_does_not_supply_membership_evidence() {
    use forkpicker::shortlist;
    let pull = json!({"number":17,"title":"Change","draft":false,"state":"open","head":{"sha":"a".repeat(40)},"base":{"ref":"main","repo":{"id":1,"full_name":"upstream/example","default_branch":"main"}}});
    let (base, handle) = server(vec![
        ("200 OK".into(), json!([pull]).to_string()),
        ("200 OK".into(), json!([{"sha":"a".repeat(40)}]).to_string()),
        (
            "200 OK".into(),
            json!({"state":"open","head":{"sha":"b".repeat(40)}}).to_string(),
        ),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let mut api = Github::with_base(cache.path().into(), false, 3, base, None).unwrap();
    let snapshot = shortlist::collect_pulls(&mut api, "upstream/example").unwrap();
    assert!(!snapshot.pulls[0].membership_verified);
    assert!(snapshot.pulls[0].commits.is_empty());
    assert!(snapshot.warnings[0].contains("changed"));
    handle.join().unwrap();
}
