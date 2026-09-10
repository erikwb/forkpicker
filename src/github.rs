use crate::{
    hash,
    model::{Coverage, EvidenceComment, Issue},
    write_json,
};
use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoName(pub String);

impl std::str::FromStr for RepoName {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        let value = value
            .trim()
            .trim_start_matches("https://github.com/")
            .trim_end_matches('/')
            .trim_end_matches(".git");
        let parts: Vec<_> = value.split('/').collect();
        if parts.len() != 2
            || parts.iter().any(|p| {
                p.is_empty()
                    || *p == "."
                    || *p == ".."
                    || p.starts_with('-')
                    || !p
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            })
        {
            bail!("expected owner/repository or https://github.com/owner/repository");
        }
        Ok(Self(value.into()))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Repository {
    pub id: u64,
    pub full_name: String,
    pub default_branch: String,
    #[serde(default)]
    pub pushed_at: Option<String>,
    #[serde(default)]
    pub forks_count: usize,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Branch {
    pub name: String,
    pub commit: BranchCommit,
}
#[derive(Debug, Clone, Deserialize)]
pub struct BranchCommit {
    pub sha: String,
}

#[derive(Serialize, Deserialize)]
struct Cached {
    fetched: u64,
    etag: Option<String>,
    next: bool,
    value: Value,
}

#[derive(Clone)]
pub struct Github {
    client: Client,
    base: String,
    token: Option<String>,
    cache: PathBuf,
    refresh: bool,
    budget: usize,
    stats: Arc<RequestStats>,
    pacing: Duration,
}

struct RequestStats {
    requests: AtomicUsize,
    hits: AtomicUsize,
    stopped: AtomicBool,
    next_request: Mutex<Instant>,
}

impl Github {
    pub fn new(cache: PathBuf, refresh: bool, budget: usize) -> Result<Self> {
        let token = std::env::var("GH_TOKEN")
            .or_else(|_| std::env::var("GITHUB_TOKEN"))
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                Command::new("gh")
                    .args(["auth", "token", "--hostname", "github.com"])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().into())
            });
        let mut api = Self::with_base(
            cache,
            refresh,
            budget,
            "https://api.github.com".into(),
            token,
        )?;
        // Eight requests/second across all workers, including pagination.
        api.pacing = Duration::from_millis(125);
        Ok(api)
    }

    pub fn with_base(
        cache: PathBuf,
        refresh: bool,
        budget: usize,
        base: String,
        token: Option<String>,
    ) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(40))
                .connect_timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("forkpicker/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base,
            token,
            cache,
            refresh,
            budget,
            stats: Arc::new(RequestStats {
                requests: AtomicUsize::new(0),
                hits: AtomicUsize::new(0),
                stopped: AtomicBool::new(false),
                next_request: Mutex::new(Instant::now()),
            }),
            pacing: Duration::ZERO,
        })
    }

    pub fn revalidating(&self) -> Self {
        let mut client = self.clone();
        client.refresh = true;
        client
    }

    pub fn requests(&self) -> usize {
        self.stats.requests.load(Ordering::SeqCst)
    }

    pub fn hits(&self) -> usize {
        self.stats.hits.load(Ordering::SeqCst)
    }

    pub fn exhausted(&self) -> bool {
        self.stats.stopped.load(Ordering::SeqCst) || self.requests() >= self.budget
    }

    fn reserve_request(&self) -> Result<()> {
        // Serialize launch slots, not HTTP responses. A rate-limit response stops
        // queued workers too; requests already in flight may still complete.
        let mut next = self.stats.next_request.lock().unwrap();
        if let Some(delay) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(delay);
        }
        anyhow::ensure!(!self.exhausted(), "GitHub API budget exhausted or rate limited; increase --api-budget or resume using the cache");
        self.stats.requests.fetch_add(1, Ordering::SeqCst);
        *next = Instant::now() + self.pacing;
        Ok(())
    }

    pub fn get(&mut self, endpoint: &str) -> Result<(Value, bool)> {
        self.request(endpoint, None)
    }

    fn request(&mut self, endpoint: &str, graphql: Option<&str>) -> Result<(Value, bool)> {
        anyhow::ensure!(
            !endpoint.starts_with('/') && !endpoint.contains("://"),
            "invalid API endpoint"
        );
        let url = format!("{}/{}", self.base.trim_end_matches('/'), endpoint);
        let path = self.cache.join("api-v1").join(format!(
            "{}.json",
            hash(format!("{}{}", url, graphql.unwrap_or("")))
        ));
        let cached = std::fs::read(&path)
            .ok()
            .and_then(|s| serde_json::from_slice::<Cached>(&s).ok());
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        if let Some(c) = &cached {
            if !self.refresh && now.saturating_sub(c.fetched) < 300 {
                self.stats.hits.fetch_add(1, Ordering::SeqCst);
                return Ok((c.value.clone(), c.next));
            }
        }
        if self.exhausted() {
            bail!("GitHub API budget exhausted ({} requests); increase --api-budget or resume using the cache", self.budget);
        }
        let request = if let Some(query) = graphql {
            self.client
                .post(&url)
                .json(&serde_json::json!({"query": query}))
        } else {
            self.client.get(&url)
        };
        let mut request = request
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(etag) = cached.as_ref().and_then(|c| c.etag.as_ref()) {
            request = request.header("If-None-Match", etag);
        }
        self.reserve_request()?;
        let response = request
            .send()
            .with_context(|| format!("GitHub request {endpoint}"))?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            let mut cached = cached.context("unexpected 304 without a cache entry")?;
            cached.fetched = now;
            self.stats.hits.fetch_add(1, Ordering::SeqCst);
            write_json(&path, &cached)?;
            return Ok((cached.value, cached.next));
        }
        let status = response.status();
        if !status.is_success() {
            let remaining = response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown")
                .to_owned();
            let reset = response
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown")
                .to_owned();
            let retry = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unspecified")
                .to_owned();
            if status.as_u16() == 403 || status.as_u16() == 429 {
                self.stats.stopped.store(true, Ordering::SeqCst);
                bail!("GitHub {status}: remaining={remaining}, reset Unix time={reset}, retry-after={retry}; stopped issuing requests");
            }
            if let Err(error) = response.error_for_status_ref() {
                return Err(error).with_context(|| format!("GitHub {status} for {endpoint}"));
            }
            bail!("GitHub {status} for {endpoint}");
        }
        let next = response
            .headers()
            .get("link")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.contains("rel=\"next\""));
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let value: Value = response.json()?;
        if graphql.is_some() && value.get("errors").is_some() {
            bail!(
                "GitHub GraphQL could not retrieve discussion context: {}",
                value["errors"]
            );
        }
        write_json(
            &path,
            &Cached {
                fetched: now,
                etag,
                next,
                value: value.clone(),
            },
        )?;
        Ok((value, next))
    }

    pub fn repository(&mut self, name: &RepoName) -> Result<Repository> {
        let repository: Repository =
            serde_json::from_value(self.get(&format!("repos/{}", name.0))?.0)?;
        repository.full_name.parse::<RepoName>()?;
        Ok(repository)
    }

    pub fn forks(&mut self, root: &Repository, coverage: &mut Coverage) -> Vec<Repository> {
        let mut queue = VecDeque::from([root.full_name.clone()]);
        let mut visited = BTreeSet::new();
        let mut all = BTreeMap::new();
        while let Some(name) = queue.pop_front() {
            if !visited.insert(name.clone()) {
                continue;
            }
            let mut page = 1;
            loop {
                let result = self
                    .get(&format!(
                        "repos/{name}/forks?per_page=100&sort=newest&page={page}"
                    ))
                    .and_then(|(v, next)| {
                        Ok((serde_json::from_value::<Vec<Repository>>(v)?, next))
                    });
                match result {
                    Ok((forks, next)) => {
                        eprintln!("[discovery] {name} · page {page} · {} forks", forks.len());
                        for fork in forks {
                            if fork.full_name.parse::<RepoName>().is_err() {
                                coverage
                                    .warnings
                                    .push("Skipped invalid repository name returned by API".into());
                                continue;
                            }
                            if fork.forks_count > 0 && !visited.contains(&fork.full_name) {
                                queue.push_back(fork.full_name.clone());
                            }
                            if fork.id != root.id {
                                all.insert(fork.id, fork);
                            }
                        }
                        if !next {
                            break;
                        }
                        page += 1;
                    }
                    Err(e) => {
                        coverage.warnings.push(format!(
                            "Fork enumeration incomplete at {name}, page {page}: {e}"
                        ));
                        if self.exhausted() {
                            queue.clear();
                        }
                        break;
                    }
                }
            }
        }
        let mut forks: Vec<Repository> = all.into_values().collect();
        // Enumerate the available pages before selecting. Sorting a newest-created
        // page alone would hide old forks with recent work.
        forks.sort_by(|a, b| {
            b.pushed_at
                .cmp(&a.pushed_at)
                .then(a.full_name.cmp(&b.full_name))
        });
        coverage.forks_discovered = forks.len();
        forks
    }

    pub fn branches(&mut self, repo: &Repository) -> Result<Vec<Branch>> {
        let mut all = Vec::new();
        for page in 1.. {
            let (value, next) = self.get(&format!(
                "repos/{}/branches?per_page=100&page={page}",
                repo.full_name
            ))?;
            all.extend(serde_json::from_value::<Vec<Branch>>(value)?);
            if !next {
                break;
            }
        }
        all.sort_by(|a, b| {
            (b.name == repo.default_branch)
                .cmp(&(a.name == repo.default_branch))
                .then(a.name.cmp(&b.name))
        });
        for branch in &all {
            anyhow::ensure!(
                branch.commit.sha.len() == 40
                    && branch.commit.sha.chars().all(|c| c.is_ascii_hexdigit()),
                "invalid branch SHA from API"
            );
        }
        Ok(all)
    }

    pub fn discussions(
        &mut self,
        repository: &str,
        query: Option<&str>,
        coverage: &mut Coverage,
    ) -> Vec<Issue> {
        self.discussion_page(repository, query, coverage, false)
    }

    fn discussion_page(
        &mut self,
        repository: &str,
        query: Option<&str>,
        coverage: &mut Coverage,
        most_voted: bool,
    ) -> Vec<Issue> {
        if self.token.is_none() {
            coverage.warnings.push(
                "GitHub Discussions require authentication; discussion context was not retrieved"
                    .into(),
            );
            return Vec::new();
        }
        let fields = "number title body url isAnswered closed upvoteCount labels(first:20){nodes{name}} comments(first:10){totalCount nodes{author{login} body url replies(first:5){totalCount nodes{author{login} body url}}}}";
        let query = if query.is_some() || most_voted {
            let search = serde_json::to_string(&format!(
                "repo:{repository} {}{}",
                query.unwrap_or(""),
                if most_voted { " sort:top" } else { "" }
            ))
            .expect("string serialization");
            format!("query {{ search(type:DISCUSSION,query:{search},first:20) {{ pageInfo{{hasNextPage}} nodes{{... on Discussion{{{fields}}}}} }} }}")
        } else {
            let Some((owner, name)) = repository.split_once('/') else {
                return Vec::new();
            };
            let owner = serde_json::to_string(owner).expect("string serialization");
            let name = serde_json::to_string(name).expect("string serialization");
            format!("query {{ repository(owner:{owner},name:{name}) {{ discussions(first:20,orderBy:{{field:UPDATED_AT,direction:DESC}}){{pageInfo{{hasNextPage}} nodes{{{fields}}}}} }} }}")
        };
        match self.request("graphql", Some(&query)) {
            Ok((data, _)) => {
                let connection = data
                    .pointer("/data/search")
                    .or_else(|| data.pointer("/data/repository/discussions"));
                if connection
                    .and_then(|v| v.pointer("/pageInfo/hasNextPage"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    coverage
                        .warnings
                        .push("Discussion context limited to the first 20 results".into());
                }
                connection
                    .and_then(|v| v["nodes"].as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(discussion)
                    .collect()
            }
            Err(error) => {
                coverage
                    .warnings
                    .push(format!("Discussion context unavailable: {error}"));
                Vec::new()
            }
        }
    }

    pub fn demand(
        &mut self,
        repository: &str,
        query: Option<&str>,
        priority_labels: Vec<String>,
    ) -> crate::priority::DemandSnapshot {
        let mut coverage = Coverage::default();
        let mut threads = BTreeMap::new();
        for sort in ["reactions-+1", "updated"] {
            let mut url =
                reqwest::Url::parse("https://api.github.com/search/issues").expect("constant URL");
            let search = format!("repo:{repository} is:issue is:open {}", query.unwrap_or(""));
            url.query_pairs_mut()
                .append_pair("q", &search)
                .append_pair("sort", sort)
                .append_pair("order", "desc")
                .append_pair("per_page", "100");
            match self.get(&format!("search/issues?{}", url.query().unwrap_or(""))) {
                Ok((data, next)) => {
                    if next || data["incomplete_results"] == true {
                        coverage.warnings.push(format!("Demand issue sample ({sort}) is incomplete; it is not the full project backlog"));
                    }
                    for value in data["items"].as_array().into_iter().flatten() {
                        if let Some(thread) = issue(value, "demand_sample") {
                            threads.insert(thread.url.clone(), thread);
                        }
                    }
                }
                Err(error) => {
                    coverage
                        .warnings
                        .push(format!("Demand issue sample ({sort}) unavailable: {error}"));
                }
            }
        }
        let recent_query = query.map(|q| format!("{q} sort:updated"));
        for thread in self
            .discussion_page(repository, query, &mut coverage, true)
            .into_iter()
            .chain(self.discussions(repository, recent_query.as_deref(), &mut coverage))
        {
            threads.insert(thread.url.clone(), thread);
        }
        coverage.warnings.sort();
        coverage.warnings.dedup();
        crate::priority::DemandSnapshot {
            fetched_at: chrono::Utc::now().to_rfc3339(),
            query: query.map(str::to_owned),
            priority_labels,
            threads: threads.into_values().collect(),
            warnings: coverage.warnings,
        }
    }

    /// Project-wide demand catalog; no keyword filter or popularity sampling.
    pub fn demand_catalog(
        &mut self,
        repository: &str,
        priority_labels: Vec<String>,
    ) -> Result<crate::priority::DemandSnapshot> {
        let repo: RepoName = repository.parse()?;
        let mut threads = BTreeMap::new();
        let mut warnings = Vec::new();
        for page in 1.. {
            match self.get(&format!(
                "repos/{}/issues?state=open&sort=created&direction=asc&per_page=100&page={page}",
                repo.0
            )) {
                Ok((values, next)) => {
                    let values = values
                        .as_array()
                        .context("invalid issue catalog response")?;
                    for value in values {
                        if let Some(thread) = issue(value, "project_catalog") {
                            if !thread.is_pull_request {
                                threads.insert(thread.url.clone(), thread);
                            }
                        }
                    }
                    if !next {
                        break;
                    }
                }
                Err(e) => {
                    warnings.push(format!("Issue catalog incomplete at page {page}: {e}"));
                    break;
                }
            }
        }
        if self.token.is_none() {
            warnings.push("Discussion catalog requires GitHub authentication".into());
        } else {
            let (owner, name) = repo.0.split_once('/').unwrap();
            let owner = serde_json::to_string(owner)?;
            let name = serde_json::to_string(name)?;
            let mut after: Option<String> = None;
            let mut seen_cursors = BTreeSet::new();
            loop {
                let cursor = serde_json::to_string(&after)?;
                let query = format!("query {{ repository(owner:{owner},name:{name}) {{ discussions(first:100,after:{cursor},orderBy:{{field:CREATED_AT,direction:ASC}}) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ number title body url isAnswered closed upvoteCount category{{slug isAnswerable}} labels(first:20){{nodes{{name}}}} comments(first:10){{ totalCount nodes{{ author{{login}} body url replies(first:5){{totalCount nodes{{author{{login}} body url}}}} }} }} }} }} }} }}");
                match self.request("graphql", Some(&query)) {
                    Ok((value, _)) => {
                        let Some(connection) = value.pointer("/data/repository/discussions") else {
                            warnings.push("Discussion catalog unavailable".into());
                            break;
                        };
                        let Some(nodes) = connection["nodes"].as_array() else {
                            warnings.push("Invalid discussion catalog response".into());
                            break;
                        };
                        for node in nodes {
                            if let Some(thread) = discussion(node) {
                                threads.insert(thread.url.clone(), thread);
                            }
                        }
                        eprintln!(
                            "Demand catalog: {} project threads collected…",
                            threads.len()
                        );
                        if connection["pageInfo"]["hasNextPage"] != true {
                            break;
                        }
                        let Some(cursor) = connection["pageInfo"]["endCursor"].as_str() else {
                            warnings.push("Discussion pagination cursor missing".into());
                            break;
                        };
                        if !seen_cursors.insert(cursor.to_owned()) {
                            warnings.push("Repeated discussion pagination cursor".into());
                            break;
                        }
                        after = Some(cursor.to_owned());
                    }
                    Err(e) => {
                        warnings.push(format!("Discussion catalog incomplete: {e}"));
                        break;
                    }
                }
            }
        }
        warnings.push("Issue comments are not collected. Discussion context includes at most ten comments and five replies per comment; label lists are limited to twenty. Missing links can remain undiscovered.".into());
        Ok(crate::priority::DemandSnapshot {
            fetched_at: chrono::Utc::now().to_rfc3339(),
            query: None,
            priority_labels,
            threads: threads.into_values().collect(),
            warnings,
        })
    }

    pub fn comments(
        &mut self,
        repository: &str,
        issues: &mut [Issue],
        urls: &BTreeSet<String>,
        coverage: &mut Coverage,
    ) {
        for (fetched, issue) in issues
            .iter_mut()
            .filter(|i| urls.contains(&i.url) && i.kind != "discussion" && i.comments_omitted > 0)
            .enumerate()
        {
            if fetched >= 10 {
                coverage.warnings.push("Comment retrieval limited to ten matched issues/PRs; remaining comment counts are recorded".into());
                break;
            }
            match self.get(&format!(
                "repos/{repository}/issues/{}/comments?per_page=20",
                issue.number
            )) {
                Ok((data, next)) => {
                    issue.comments = data
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(comment)
                        .collect();
                    issue.comments_omitted =
                        issue.comments_omitted.saturating_sub(issue.comments.len());
                    if next {
                        coverage.warnings.push(format!(
                            "#{} comment context limited to the first 20 comments",
                            issue.number
                        ));
                    }
                }
                Err(error) => {
                    coverage.warnings.push(format!(
                        "Comments for #{} unavailable: {error}",
                        issue.number
                    ));
                    if self.exhausted() {
                        break;
                    }
                }
            }
        }
    }

    pub fn issues(
        &mut self,
        repository: &str,
        query: Option<&str>,
        coverage: &mut Coverage,
    ) -> Vec<Issue> {
        let endpoint = if let Some(query) = query {
            let mut url =
                reqwest::Url::parse("https://api.github.com/search/issues").expect("constant URL");
            url.query_pairs_mut()
                .append_pair("q", &format!("repo:{repository} {query}"))
                .append_pair("per_page", "100");
            format!("search/issues?{}", url.query().unwrap_or(""))
        } else {
            format!("repos/{repository}/issues?state=all&sort=updated&direction=desc&per_page=100")
        };
        match self.get(&endpoint) {
            Ok((value, next)) => {
                if value.get("incomplete_results").and_then(Value::as_bool) == Some(true) {
                    coverage
                        .warnings
                        .push("GitHub issue search returned incomplete results".into());
                }
                if next {
                    coverage.warnings.push("Issue/PR context limited to the first 100 results; use --query for focused retrieval".into());
                }
                let items = if query.is_some() {
                    value.get("items").and_then(Value::as_array)
                } else {
                    value.as_array()
                };
                items
                    .into_iter()
                    .flatten()
                    .filter_map(|v| {
                        issue(
                            v,
                            if query.is_some() {
                                "search_result"
                            } else {
                                "recently_updated"
                            },
                        )
                    })
                    .collect()
            }
            Err(e) => {
                coverage
                    .warnings
                    .push(format!("Issue/PR context unavailable: {e}"));
                Vec::new()
            }
        }
    }
}

fn comment(value: &Value) -> Option<EvidenceComment> {
    let body = value["body"].as_str()?;
    Some(EvidenceComment {
        author: value
            .pointer("/author/login")
            .or_else(|| value.pointer("/user/login"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .into(),
        url: value
            .get("url")
            .filter(|_| value.get("author").is_some())
            .or_else(|| value.get("html_url"))
            .and_then(Value::as_str)?
            .into(),
        body: body.chars().take(4000).collect(),
        body_truncated: body.chars().count() > 4000,
    })
}

fn discussion(value: &Value) -> Option<Issue> {
    let body = value["body"].as_str()?;
    let mut comments = Vec::new();
    let mut omitted = 0;
    for item in value
        .pointer("/comments/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(c) = comment(item) {
            comments.push(c);
        }
        let replies: Vec<_> = item
            .pointer("/replies/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(comment)
            .collect();
        omitted += (item
            .pointer("/replies/totalCount")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize)
            .saturating_sub(replies.len());
        comments.extend(replies);
    }
    omitted += (value
        .pointer("/comments/totalCount")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize)
        .saturating_sub(
            value
                .pointer("/comments/nodes")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
        );
    Some(Issue {
        number: value["number"].as_u64()?,
        title: value["title"].as_str()?.into(),
        body: body.chars().take(12000).collect(),
        url: value["url"].as_str()?.into(),
        state: if value["closed"].as_bool() == Some(true) {
            "closed"
        } else if value["isAnswered"].as_bool() == Some(true) {
            "answered"
        } else {
            "unanswered"
        }
        .into(),
        is_pull_request: false,
        match_kind: "discussion_context".into(),
        kind: "discussion".into(),
        discussion_category: value
            .pointer("/category/slug")
            .and_then(Value::as_str)
            .map(str::to_owned),
        discussion_answerable: value
            .pointer("/category/isAnswerable")
            .and_then(Value::as_bool),
        body_truncated: body.chars().count() > 12000,
        comments,
        comments_omitted: omitted,
        thumbs_up: None,
        upvotes: value["upvoteCount"].as_u64(),
        labels: value
            .pointer("/labels/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str().map(str::to_owned))
            .collect(),
    })
}

fn issue(value: &Value, match_kind: &str) -> Option<Issue> {
    let body: String = value
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(12_000)
        .collect();
    Some(Issue {
        number: value["number"].as_u64()?,
        title: value["title"].as_str()?.into(),
        body,
        url: value["html_url"].as_str()?.into(),
        state: value["state"].as_str()?.into(),
        is_pull_request: value.get("pull_request").is_some(),
        kind: if value.get("pull_request").is_some() {
            "pull_request"
        } else {
            "issue"
        }
        .into(),
        discussion_category: None,
        discussion_answerable: None,
        body_truncated: value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .count()
            > 12_000,
        comments: Vec::new(),
        comments_omitted: value.get("comments").and_then(Value::as_u64).unwrap_or(0) as usize,
        match_kind: match_kind.into(),
        thumbs_up: value.pointer("/reactions/+1").and_then(Value::as_u64),
        upvotes: None,
        labels: value["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str().or_else(|| l.as_str()).map(str::to_owned))
            .collect(),
    })
}
