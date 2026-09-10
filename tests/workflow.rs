use forkpicker::{
    analyze,
    git::{parse_files, Git},
    llm,
    model::*,
    priority, render, review,
    scan::{inspect, ScanOptions},
    state,
};
use std::process::Command;
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    cache: TempDir,
}

#[test]
fn saved_classification_html_links_scan_evidence_and_protects_inputs() {
    use serde_json::json;
    let f = Fixture::new();
    let sha = f.hdr();
    f.write("src/parser.c", "int parse = 7;\n");
    f.commit("parser: handle errors");
    let report = f.scan(&["hdr"], &["main"]);
    let feature = report
        .features
        .iter()
        .find(|c| c.commits.contains(&sha))
        .unwrap();
    let result = json!({"schema_version":1,"repository":report.repository,"base_sha":report.base_sha,
        "source_fingerprint":forkpicker::shortlist::fingerprint(&report),"generated_at":"now","agent":"fixture",
        "requested_model":null,"requested_effort":null,"planned_batch_calls":1,"attempted_calls":1,"reused_calls":0,
        "forks_with_candidates":1,"errors":[],"limitations":[],"forks":[{"repository":"fork/example",
        "candidate_ids":[feature.id],"batches":[],"reconciliation":null,"status":"complete","notes":[],
        "classification":{"schema_version":1,"scope":"selection","headline":"Change <color> handling",
        "summary":{"text":"Observed changes","evidence":[]},"groups":[{"name":"Color handling",
        "summary":{"text":format!("Changes {}",feature.id),"evidence":["request:https://github.com/upstream/example/issues/42"]},
        "members":[{"candidate_id":feature.id,"role":"implementation"}]}],"relationships":[],"unclassified":[],"limitations":[]}}]});
    let rp = f.cache.path().join("report.json");
    let cp = f.cache.path().join("classification.json");
    let hp = f.cache.path().join("classification.html");
    forkpicker::write_json(&rp, &report).unwrap();
    forkpicker::write_json(&cp, &result).unwrap();
    let run = |output: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "report",
                rp.to_str().unwrap(),
                "--classification",
                cp.to_str().unwrap(),
                "--format",
                "html",
                "--output",
                output.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    };
    let output = run(&hp);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let html = std::fs::read_to_string(&hp).unwrap();
    assert!(html.contains(&format!("https://github.com/fork/example/commit/{sha}")));
    assert!(html.contains("1 lines added, 1 lines removed"));
    assert!(html.contains("href=\"#catalog-1\">1 other changes"));
    assert!(html.contains("parser: handle errors"));
    assert!(html.contains("href=\"https://github.com/upstream/example/issues/42\""));
    assert!(html.contains("Change &lt;color&gt; handling"));
    assert!(!html.contains(&feature.id));
    assert!(!html.contains("<details"));
    let original = std::fs::read(&rp).unwrap();
    assert!(!run(&rp).status.success());
    assert_eq!(std::fs::read(&rp).unwrap(), original);
    assert!(!run(&cp).status.success());
    let mut stale = result;
    stale["base_sha"] = json!("stale");
    forkpicker::write_json(&cp, &stale).unwrap();
    assert!(!run(&hp).status.success());
    assert_eq!(std::fs::read_to_string(&hp).unwrap(), html);
}

#[test]
fn classification_native_schema_budget_resume_reconciliation_and_invalid_output() {
    use forkpicker::classify;
    use serde_json::json;
    let fixture = Fixture::new();
    fixture.hdr();
    fixture.git(&["checkout", "main"]);
    fixture.git(&["checkout", "-qb", "parser"]);
    fixture.write("src/parser.c", "int parse = 1;\n");
    fixture.commit("parser: reject malformed expressions");
    let mut report = fixture.scan(&["hdr", "parser"], &["main"]);
    assert_eq!(report.features.len(), 2);
    // Two forks containing identical candidates should share both batch and reconciliation calls.
    for feature in &mut report.features {
        let mut alias = feature.sources[0].clone();
        alias.repository = "other/example".into();
        feature.sources.push(alias);
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let rp = root.join("report.json");
    let output = root.join("classification.json");
    let cp = root.join("config.json");
    let marker = root.join("calls");
    let script = root.join("model.py");
    let failure = root.join("fail");
    forkpicker::write_json(&rp, &report).unwrap();
    std::fs::write(&script,r#"import sys,json,pathlib
pack=json.load(sys.stdin)
args=sys.argv
schema=json.loads(pathlib.Path(args[args.index('--output-schema')+1]).read_text())
assert schema==pack['response_schema']
assert schema['additionalProperties'] is False
assert set(schema['$defs']['candidate']['enum'])=={c['candidate_id'] for c in pack['candidates']}
pathlib.Path(args[1]).open('a').write(pack['scope']+'\n')
answer={'schema_version':1,'scope':pack['scope'],'headline':'Describe observed changes','summary':{'text':'Observed work','evidence':pack['candidates'][0]['evidence_ids']},'groups':[],'relationships':[],'unclassified':[],'limitations':[]}
answer['usefulness']=[{'candidate_id':c['candidate_id'],'verdict':'uncertain','reason':{'text':'Needs context','evidence':c['evidence_ids']}} for c in pack['candidates']]
for c in pack['candidates']:
 answer['groups'].append({'name':c['candidate_id'],'summary':{'text':'Observed change','evidence':c['evidence_ids']},'members':[{'candidate_id':c['candidate_id'],'role':'implementation'}]})
if pathlib.Path(args[2]).exists(): answer['groups']=[]
print(json.dumps(answer))
"#).unwrap();
    forkpicker::write_json(
        &cp,
        &json!({"agents":{"codex":{"command":["python3",script,marker,failure]}}}),
    )
    .unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--config",
                cp.to_str().unwrap(),
                "--cache-dir",
                root.join("cache").to_str().unwrap(),
                "classify",
                rp.to_str().unwrap(),
                "--agent",
                "codex",
                "--batch-size",
                "1",
                "--jobs",
                "1",
                "--output",
                output.to_str().unwrap(),
            ])
            .args(extra)
            .output()
            .unwrap()
    };
    let read =
        || serde_json::from_slice::<classify::Run>(&std::fs::read(&output).unwrap()).unwrap();
    assert!(run(&[
        "--dry-run",
        "--plan-dir",
        root.join("plans").to_str().unwrap()
    ])
    .status
    .success());
    assert!(!marker.exists());
    assert_eq!(read().planned_batch_calls, 2);
    assert_eq!(std::fs::read_dir(root.join("plans")).unwrap().count(), 4);
    assert_eq!(run(&["--limit", "0"]).status.code(), Some(2));
    assert!(!marker.exists());
    let r = run(&["--limit", "1"]);
    assert_eq!(
        r.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(read().attempted_calls, 1);
    assert!(read()
        .forks
        .iter()
        .all(|f| f.classification.is_none() && f.status == "partial"));
    assert_eq!(run(&["--limit", "1"]).status.code(), Some(2));
    assert!(read()
        .forks
        .iter()
        .all(|f| f.batches.iter().all(Option::is_some) && f.classification.is_none()));
    let r = run(&["--limit", "1"]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(read().attempted_calls, 1);
    assert!(read()
        .forks
        .iter()
        .all(|f| f.status == "complete" && f.reconciliation.is_some()));
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 3);
    assert!(run(&["--limit", "0"]).status.success());
    assert_eq!(read().attempted_calls, 0);
    // Changed model invalidates all requests; malformed answers spend once, never cache or retry.
    std::fs::write(&failure, "fail").unwrap();
    assert_eq!(
        run(&["--model", "different", "--limit", "10"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(read().attempted_calls, 1);
    assert!(read()
        .forks
        .iter()
        .all(|f| f.batches.iter().all(Option::is_none)));
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 4);
    let original = std::fs::read(&rp).unwrap();
    assert!(!run(&["--output", rp.to_str().unwrap()]).status.success());
    assert_eq!(std::fs::read(&rp).unwrap(), original);
    assert!(!run(&["--agent", "muse"]).status.success());
    assert!(!run(&["--agent", "opencode"]).status.success());
    let mut inv = forkpicker::metrics::measure(&report, &Default::default());
    inv.base_sha = "different".into();
    let ip = root.join("inventory.json");
    forkpicker::write_json(&ip, &inv).unwrap();
    assert!(!run(&["--inventory", ip.to_str().unwrap()]).status.success());
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 4);
}
impl Fixture {
    fn new() -> Self {
        let f = Self {
            dir: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
        };
        f.git(&["init", "-q", "-b", "main"]);
        f.git(&["config", "user.name", "Test Maintainer"]);
        f.git(&["config", "user.email", "maintainer@example.invalid"]);
        f.write("src/render.c", "int white = 80;\nint peak = 1000;\n");
        f.write("src/parser.c", "int parse = 0;\n");
        f.write("CMakeLists.txt", "version 7\n");
        f.write("deps.lock", "old\n");
        f.commit("Initial upstream");
        f
    }
    fn git(&self, args: &[&str]) -> String {
        let o = Command::new("git")
            .arg("-C")
            .arg(self.dir.path())
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap().trim().into()
    }
    fn write(&self, name: &str, data: &str) {
        let p = self.dir.path().join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
    fn commit(&self, message: &str) -> String {
        self.git(&["add", "."]);
        self.git(&["commit", "-qm", message]);
        self.git(&["rev-parse", "HEAD"])
    }
    fn scan(&self, refs: &[&str], upstream: &[&str]) -> Report {
        self.scan_options(
            refs,
            upstream,
            ScanOptions {
                max_commits: 0,
                upstream_history: 0,
            },
        )
    }
    fn scan_options(&self, refs: &[&str], upstream: &[&str], options: ScanOptions) -> Report {
        let git = Git::new(self.dir.path());
        let known = upstream
            .iter()
            .map(|r| (r.to_string(), git.resolve(r).unwrap()))
            .collect();
        let sources = refs
            .iter()
            .map(|r| Source {
                repository: "fork/example".into(),
                branch: r.to_string(),
                tip: git.resolve(r).unwrap(),
                url: Some("https://github.com/fork/example".into()),
            })
            .collect();
        inspect(
            &git,
            "upstream/example",
            "main",
            known,
            sources,
            self.cache.path(),
            &options,
        )
        .unwrap()
    }
    fn hdr(&self) -> String {
        self.git(&["checkout", "-qb", "hdr"]);
        self.write("src/render.c", "int white = 203;\nint peak = 1000;\n");
        self.commit("render: normalize HDR reference white")
    }
}

#[test]
fn discrete_features_include_singletons_and_keep_related_tests() {
    let f = Fixture::new();
    f.hdr();
    f.write("src/render.c", "int white = 308;\nint peak = 1000;\n");
    f.commit("render: refresh HDR reference white");
    f.write("tests/render.c", "assert(white == 308);\n");
    f.commit("test: verify HDR reference white");
    f.write("src/parser.c", "int parse = 1;\n");
    f.commit("parser: reject malformed expressions");
    let report = f.scan(&["hdr"], &["main"]);
    assert_eq!(report.features.len(), 2);
    let hdr = report
        .features
        .iter()
        .find(|x| x.title.contains("HDR"))
        .unwrap();
    assert_eq!(hdr.commits.len(), 3);
    assert_eq!(hdr.test_files, vec!["tests/render.c"]);
    assert!(
        hdr.score
            > report
                .features
                .iter()
                .find(|x| x.title.contains("parser"))
                .unwrap()
                .score
    );
    assert!(hdr.grouping_evidence.iter().any(|s| s.contains("topic")));
}

#[test]
fn shared_file_alone_does_not_merge_unrelated_work() {
    let f = Fixture::new();
    f.hdr();
    f.write("src/render.c", "int white = 203;\nint peak = 500;\n");
    f.commit("cursor: eliminate tearing during resize");
    assert_eq!(f.scan(&["hdr"], &["main"]).features.len(), 2);
}

#[test]
fn changed_sha_with_same_patch_is_suppressed_as_upstream() {
    let f = Fixture::new();
    let base = f.git(&["rev-parse", "HEAD"]);
    f.write("src/parser.c", "int parse = 1;\n");
    let upstream = f.commit("parser: reject invalid input");
    f.git(&["checkout", "-qb", "fork", &base]);
    f.git(&["cherry-pick", "--no-commit", &upstream]);
    let backport = f.commit("Apply parser fix on release");
    assert_ne!(upstream, backport);
    let report = f.scan(&["fork"], &["main"]);
    assert!(report.features.is_empty());
    assert_eq!(
        report.branches[0].equivalent_upstream[0].kind,
        "stable_patch_id"
    );
    assert_eq!(
        report.branches[0].equivalent_upstream[0].upstream_commits,
        vec![upstream]
    );
}

#[test]
fn selective_file_backport_is_recognized() {
    let f = Fixture::new();
    let base = f.git(&["rev-parse", "HEAD"]);
    f.write("CMakeLists.txt", "version any\n");
    f.write("deps.lock", "new\n");
    let upstream = f.commit("Accept packaged dependencies and update lockfile");
    f.git(&["checkout", "-qb", "fork", &base]);
    f.write("CMakeLists.txt", "version any\n");
    f.commit("Build compatibility backport");
    let report = f.scan(&["fork"], &["main"]);
    assert!(report.features.is_empty());
    assert_eq!(
        report.branches[0].equivalent_upstream[0].kind,
        "all_file_patches"
    );
    assert_eq!(
        report.branches[0].equivalent_upstream[0].upstream_commits,
        vec![upstream]
    );
}

#[test]
fn inherited_release_work_is_not_a_new_feature() {
    let f = Fixture::new();
    f.git(&["checkout", "-qb", "release"]);
    f.write("src/parser.c", "int parse = 3;\n");
    f.commit("Release fix");
    f.hdr();
    let report = f.scan(&["hdr"], &["main", "release"]);
    assert_eq!(report.features.len(), 1);
    assert_eq!(report.branches[0].ahead, 2);
    assert_eq!(report.branches[0].novel_commits, 1);
}

#[test]
fn branch_aliases_are_preserved_without_duplicate_features() {
    let f = Fixture::new();
    f.hdr();
    f.git(&["branch", "fork-main"]);
    let report = f.scan(&["hdr", "fork-main"], &["main"]);
    assert_eq!(report.features.len(), 1);
    assert_eq!(report.features[0].sources.len(), 2);
    assert!(report.branches[1].alias_of.is_some());
}

#[test]
fn rebased_patch_identity_preserves_decision_and_attribution() {
    let f = Fixture::new();
    let sha = f.hdr();
    let original = f.scan(&["hdr"], &["main"]);
    state::decide(
        &original,
        &original.features[0].id,
        "dismissed",
        "Different color policy",
        f.cache.path(),
    )
    .unwrap();
    f.git(&["checkout", "-qb", "other", "main"]);
    f.git(&["cherry-pick", "--no-commit", &sha]);
    f.commit("Improve HDR white rendering");
    let mut rebased = f.scan(&["other"], &["main"]);
    state::apply(&mut rebased, f.cache.path()).unwrap();
    assert_eq!(rebased.features[0].id, original.features[0].id);
    assert_eq!(rebased.features[0].status, "dismissed");
    let merged = f.scan(&["hdr", "other"], &["main"]);
    assert_eq!(merged.features.len(), 1);
    assert_eq!(merged.features[0].commits.len(), 2);
    assert_eq!(merged.features[0].patch_ids.len(), 1);
}

#[test]
fn materially_changed_patch_set_resurfaces() {
    let f = Fixture::new();
    f.hdr();
    let original = f.scan(&["hdr"], &["main"]);
    state::decide(
        &original,
        &original.features[0].id,
        "dismissed",
        "Need tests",
        f.cache.path(),
    )
    .unwrap();
    f.write("tests/render.c", "assert(white == 203);\n");
    f.commit("Test HDR reference white");
    let mut updated = f.scan(&["hdr"], &["main"]);
    state::apply(&mut updated, f.cache.path()).unwrap();
    assert_eq!(updated.features[0].status, "updated");
    assert!(updated.features[0]
        .decision_reason
        .as_ref()
        .unwrap()
        .contains("Need tests"));
}

#[test]
fn omissions_and_unrelated_history_are_explicit() {
    let f = Fixture::new();
    f.hdr();
    f.write("src/parser.c", "int parse = 1;\n");
    f.commit("Parser change");
    let report = f.scan_options(
        &["hdr"],
        &["main"],
        ScanOptions {
            max_commits: 1,
            upstream_history: 1,
        },
    );
    assert_eq!(report.branches[0].commits_omitted, 1);
    assert!(!report.coverage.warnings.is_empty());
    f.git(&["checkout", "--orphan", "unrelated"]);
    f.git(&["rm", "-rf", "."]);
    f.write("different.txt", "unrelated\n");
    f.commit("Unrelated repository");
    let report = f.scan(&["unrelated"], &["main"]);
    assert!(report.branches[0]
        .error
        .as_ref()
        .unwrap()
        .contains("common ancestor"));
    assert!(report.features.is_empty());
}

#[test]
fn local_scan_does_not_modify_dirty_worktree() {
    let f = Fixture::new();
    f.hdr();
    f.write("src/render.c", "uncommitted user work\n");
    let before = f.git(&["status", "--porcelain"]);
    f.scan(&["hdr"], &["main"]);
    assert_eq!(before, f.git(&["status", "--porcelain"]));
    assert_eq!(
        std::fs::read_to_string(f.dir.path().join("src/render.c")).unwrap(),
        "uncommitted user work\n"
    );
}

fn assessment(report: &Report) -> Assessment {
    let f = &report.features[0];
    Assessment {
        feature_id: f.id.clone(),
        base_sha: report.base_sha.clone(),
        title: "HDR reference white".into(),
        summary: vec![Claim {
            text: "Changes reference white".into(),
            evidence: vec![format!("commit:{}", f.commits[0])],
        }],
        review_questions: vec!["Validate on hardware".into()],
        suggested_next_step: "Review tests".into(),
    }
}

#[test]
fn llm_assessments_require_current_identity_and_real_evidence() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let mut a = assessment(&report);
    llm::validate(&report, &a).unwrap();
    a.summary[0].evidence = vec!["commit:invented".into()];
    assert!(llm::validate(&report, &a).is_err());
    a = assessment(&report);
    a.base_sha = "outdated".into();
    assert!(llm::validate(&report, &a).is_err());
    a = assessment(&report);
    a.summary[0].evidence.clear();
    assert!(llm::validate(&report, &a).is_err());
    let mut value = serde_json::to_value(assessment(&report)).unwrap();
    value["tests_passed"] = serde_json::json!(true);
    assert!(serde_json::from_value::<Assessment>(value).is_err());
}

#[test]
fn context_respects_byte_budget_and_marks_truncation() {
    let f = Fixture::new();
    f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    let sha = report.features[0].commits[0].clone();
    report.commits.get_mut(&sha).unwrap().patch = "日\n".repeat(50_000);
    let pack = llm::context(&report, &report.features[0].id, 10_000).unwrap();
    assert!(serde_json::to_vec(&pack).unwrap().len() <= 10_000);
    assert_eq!(pack["commits"][0]["patch_truncated"], true);
    assert!(llm::context(&report, &report.features[0].id, 100).is_err());
}

#[test]
fn html_and_terminal_neutralize_untrusted_text() {
    let f = Fixture::new();
    f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    report.features[0].title = "<script>alert(1)</script>\u{1b}[2J".into();
    let html = render::html(&report, None, true);
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(html.contains("&lt;script&gt;"));
    assert!(!render::terminal(&report, None, true).contains('\u{1b}'));
    report.features[0].issue_links =
        vec!["https://github.com/upstream/example/issues/42)<script>alert(1)</script>".into()];
    let markdown = render::markdown(&report, None, true);
    assert!(!markdown.contains("<script>"));
    assert!(markdown.contains("[Issue/PR context](<https://github.com/"));
    assert!(markdown.contains("%3Cscript%3E"));
}

#[test]
fn demand_pages_reject_executable_and_foreign_urls_in_saved_inputs() {
    use forkpicker::{shortlist, triage};
    use serde_json::json;
    let fixture = Fixture::new();
    fixture.hdr();
    let mut report = fixture.scan(&["hdr"], &["main"]);
    report.repository = "upstream/example".into();
    let sha = report.features[0].commits[0].clone();
    report.commits.get_mut(&sha).unwrap().message = "Implement #42".into();
    report.demand = Some(demand_snapshot(vec![demand_thread(
        "Issue <title>",
        "open",
        5,
    )]));
    let mut shortlist = shortlist::build(&report, None, None, false, 10).unwrap();
    assert_eq!(shortlist.selected_feature_ids.len(), 1);
    for url in [
        "javascript:alert(document.domain)",
        "java\nscript:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "https://github.com.attacker.invalid/",
        "https://github.com@attacker.invalid/",
        "https://github.com/upstream/example/issues/42",
    ] {
        // Imported snapshots must be safe to render even if their URLs are forged.
        shortlist.requests[0].url = url.into();
        shortlist.candidates[0].matched_requests = vec![url.into()];
        let experiment: triage::Experiment = serde_json::from_value(json!({
            "schema_version":1, "repository":report.repository, "base_sha":report.base_sha,
            "source_fingerprint":"fixture", "generated_at":"now", "agent":"fixture",
            "requested_model":null, "policy":"fixture", "planned_batches":1,
            "attempted_calls":1, "reused_batches":0, "errors":[], "suggested_review_ids":[],
            "cards":[{"feature_id":"candidate", "title":"Candidate", "sampling":"fixture",
                "patches":[], "commits":[], "requests":[{"url":url, "title":"Issue <title>"}],
                "limitations":[]}],
            "batches":[{"key":"fixture", "feature_ids":["candidate"], "input_bytes":0,
                "duration_ms":0, "provider_usage":null, "response":{"assessments":[{
                    "feature_id":"candidate", "summary":{"text":"Summary", "evidence":[]},
                    "matches":[{"request_url":url, "relation":"related", "rationale":"Reason", "evidence":[]}],
                    "limitations":[]}]}}]
        })).unwrap();
        for html in [shortlist::html(&shortlist), triage::html(&experiment)] {
            assert!(html.contains("Issue &lt;title&gt;"));
            let href = format!("href=\"{}\"", render::html_escape(url));
            assert_eq!(html.contains(&href), url.ends_with("/issues/42"), "{url}");
        }
    }
}

#[test]
fn parser_does_not_split_diff_strings_inside_code() {
    let patch="diff --git a/file.rs b/file.rs\n--- a/file.rs\n+++ b/file.rs\n@@ -1 +1 @@\n-old\n+let text = \"diff --git a/x b/x\";\n";
    let files = parse_files(patch);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "file.rs");
    assert_eq!(files[0].additions, 1);
}

#[test]
fn binary_changes_are_never_suppressed_by_empty_fingerprints() {
    let f = Fixture::new();
    f.git(&["checkout", "-qb", "binary"]);
    std::fs::write(f.dir.path().join("image.bin"), [0, 255, 1, 2]).unwrap();
    f.commit("Add image asset");
    let report = f.scan(&["binary"], &["main"]);
    assert_eq!(report.features.len(), 1);
    assert!(report
        .commits
        .values()
        .any(|c| c.files.iter().any(|f| f.binary)));
}

#[test]
fn source_file_without_new_tests_does_not_claim_no_coverage() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    assert!(report.features[0]
        .review_notes
        .iter()
        .any(|n| n.contains("existing tests")));
}

#[test]
fn cli_scan_report_context_and_decision_round_trip() {
    let f = Fixture::new();
    f.hdr();
    let output = f.cache.path().join("report.json");
    let html = f.cache.path().join("report.html");
    let command = |args: Vec<String>| {
        let out = Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .arg("--cache-dir")
            .arg(f.cache.path().join("cache"))
            .arg("--state-dir")
            .arg(f.cache.path().join("state"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    command(vec![
        "scan-local".into(),
        f.dir.path().display().to_string(),
        "--ref".into(),
        "hdr".into(),
        "--output".into(),
        output.display().to_string(),
        "--html".into(),
        html.display().to_string(),
    ]);
    let report: Report = serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
    let id = report.features[0].id.clone();
    command(vec![
        "decide".into(),
        output.display().to_string(),
        id.clone(),
        "--status".into(),
        "needs-adopter".into(),
        "--reason".into(),
        "Author has limited time".into(),
    ]);
    let rendered = command(vec![
        "report".into(),
        output.display().to_string(),
        "--format".into(),
        "markdown".into(),
    ]);
    assert!(String::from_utf8_lossy(&rendered.stdout).contains("needs_adopter"));
    let pack = command(vec!["context".into(), output.display().to_string(), id]);
    let value: serde_json::Value = serde_json::from_slice(&pack.stdout).unwrap();
    assert!(value["evidence_ids"].as_array().unwrap().len() > 1);
    assert!(html.exists());
}

#[test]
fn optional_context_does_not_change_candidates_priority_or_search() {
    let f = Fixture::new();
    f.hdr();
    f.write("src/parser.c", "int parse = 1;\n");
    f.commit("parser: reject malformed expressions");
    let mut report = f.scan(&["hdr"], &["main"]);
    let candidates = |report: &Report| {
        report
            .features
            .iter()
            .map(|feature| (feature.id.clone(), feature.score, feature.commits.clone()))
            .collect::<Vec<_>>()
    };
    let before = candidates(&report);
    report.issues = vec![Issue {
        number: 42,
        title: "HDR reference white is too dim".into(),
        body: "database migration".into(),
        url: "https://github.com/upstream/example/issues/42".into(),
        state: "open".into(),
        is_pull_request: false,
        match_kind: "search_result".into(),
        kind: "issue".into(),
        discussion_category: None,
        discussion_answerable: None,
        body_truncated: false,
        comments: Vec::new(),
        comments_omitted: 0,
        thumbs_up: None,
        upvotes: None,
        labels: Vec::new(),
    }];
    analyze::link_issues(&mut report.features, &report.issues, &report.commits);
    assert_eq!(candidates(&report), before);
    assert_eq!(
        report
            .features
            .iter()
            .map(|f| f.issue_links.len())
            .sum::<usize>(),
        1
    );
    assert_eq!(render::selected(&report, Some("HDR white"), false).len(), 1);
    assert!(render::selected(&report, Some("database migration"), false).is_empty());
}

#[test]
fn issue_reference_does_not_raise_priority() {
    let f = Fixture::new();
    f.hdr();
    let before = f.scan(&["hdr"], &["main"]);
    f.git(&[
        "commit",
        "--amend",
        "-qm",
        "render: normalize HDR reference white (#42)",
    ]);
    let after = f.scan(&["hdr"], &["main"]);
    assert_eq!(before.features[0].id, after.features[0].id);
    assert_eq!(before.features[0].score, after.features[0].score);
}

#[test]
fn corrupted_decisions_are_not_overwritten() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let path = f.cache.path().join(format!(
        "{}.json",
        forkpicker::hash(report.repository.to_lowercase())
    ));
    std::fs::write(&path, "broken").unwrap();
    assert!(state::decide(
        &report,
        &report.features[0].id,
        "saved",
        "Review later",
        f.cache.path()
    )
    .is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "broken");
}

#[test]
#[cfg(unix)]
fn llm_command_round_trip_and_timeout() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let output = f.cache.path().join("assessment.json");
    std::fs::write(&output, serde_json::to_vec(&assessment(&report)).unwrap()).unwrap();
    let result = llm::run_command(
        &["cat".into(), output.display().to_string()],
        b"context",
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    llm::validate(&report, &result).unwrap();
    let started = std::time::Instant::now();
    assert!(llm::run_command(
        &["sleep".into(), "5".into()],
        b"context",
        std::time::Duration::from_millis(60)
    )
    .unwrap_err()
    .to_string()
    .contains("exceeded"));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert!(llm::run_command(
        &["false".into()],
        b"context",
        std::time::Duration::from_secs(2)
    )
    .is_err());
    let diagnostic = llm::run_command(
        &[
            "python3".into(),
            "-c".into(),
            "import sys; print('unsupported schema flag', file=sys.stderr); sys.exit(2)".into(),
        ],
        b"context",
        std::time::Duration::from_secs(2),
    )
    .unwrap_err()
    .to_string();
    assert!(diagnostic.contains("unsupported schema flag"));
}

#[test]
fn merge_resolution_omissions_are_counted() {
    let f = Fixture::new();
    f.hdr();
    f.git(&["checkout", "-qb", "side", "main"]);
    f.write("src/parser.c", "int parse = 9;\n");
    f.commit("Reject invalid syntax");
    f.git(&["checkout", "hdr"]);
    f.git(&["merge", "--no-ff", "-m", "Merge side", "side"]);
    let report = f.scan(&["hdr"], &["main"]);
    assert_eq!(report.branches[0].merges_omitted, 1);
    assert_eq!(report.features.len(), 2);
    assert!(report.coverage.warnings.iter().any(|w| w.contains("merge")));
}

#[test]
fn comment_references_are_validated_and_context_bodies_bounded() {
    let f = Fixture::new();
    f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    let url = "https://github.com/upstream/example/discussions/42";
    report.issues.push(Issue {
        number: 42,
        title: "HDR white".into(),
        body: "Reference white".into(),
        url: url.into(),
        state: "unanswered".into(),
        is_pull_request: false,
        match_kind: "discussion_context".into(),
        kind: "discussion".into(),
        discussion_category: Some("bugs".into()),
        discussion_answerable: Some(true),
        body_truncated: false,
        comments: vec![EvidenceComment {
            author: "alice".into(),
            url: format!("{url}#comment-1"),
            body: "界".repeat(50_000),
            body_truncated: false,
        }],
        comments_omitted: 0,
        thumbs_up: None,
        upvotes: None,
        labels: Vec::new(),
    });
    report.features[0].issue_links.push(url.into());
    let pack = llm::context(&report, &report.features[0].id, 10_000).unwrap();
    assert_eq!(pack["issues"][0]["comments"][0]["body_truncated"], true);
    assert!(serde_json::to_vec(&pack).unwrap().len() <= 10_000);
    let mut a = assessment(&report);
    a.summary[0].evidence = vec![format!("comment:{url}#comment-1")];
    llm::validate(&report, &a).unwrap();
}

#[test]
fn review_sources_are_pinned_to_git_objects_and_budgeted() {
    let f = Fixture::new();
    let sha = f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    f.write("src/render.c", "UNCOMMITTED WORK MUST NOT BE READ\n");
    let git = Git::new(f.dir.path());
    let pack = review::context(&report, &report.features[0].id, 30_000, Some(&git)).unwrap();
    let sources = pack["source_files"].as_array().unwrap();
    assert_eq!(sources.len(), 2);
    let after = sources.iter().find(|s| s["commit"] == sha).unwrap();
    assert_eq!(after["content"], "int white = 203;\nint peak = 1000;\n");
    assert!(sources
        .iter()
        .any(|s| s["content"].as_str().unwrap().contains("white = 80")));
    assert!(!pack.to_string().contains("UNCOMMITTED WORK"));
    let before_state = report.features[0].status.clone();
    let mut changed = report.clone();
    changed.features[0].status = "saved".into();
    changed.features[0].decision_reason = Some("Interested".into());
    let unchanged = review::context(&changed, &report.features[0].id, 30_000, Some(&git)).unwrap();
    assert_eq!(pack, unchanged, "triage should not invalidate review input");
    assert_eq!(report.features[0].status, before_state);
    let mut large = pack;
    large["source_files"][0]["content"] = serde_json::json!("🚀 large source\n".repeat(5000));
    llm::bound_context(&mut large, 10_000).unwrap();
    assert!(serde_json::to_vec(&large).unwrap().len() <= 10_000);
    assert_eq!(large["source_files"][0]["truncated"], true);
    let patch_only = review::context(&report, &report.features[0].id, 20_000, None).unwrap();
    assert_eq!(patch_only["source_coverage"]["git_available"], false);
    assert!(patch_only["source_files"].as_array().unwrap().is_empty());
}

#[test]
fn review_style_compares_scanned_upstream_not_fork_parent_or_worktree() {
    let f = Fixture::new();
    let fork_sha = f.hdr();
    f.git(&["checkout", "main"]);
    f.write("src/render.c", "int white = 80;\nint peak = 1500;\n");
    f.write(".clang-format", "IndentWidth: 4\n");
    let base = f.commit("Update upstream conventions and defaults");
    f.write(".clang-format", "UNCOMMITTED CONVENTIONS\n");
    let report = f.scan(&["hdr"], &["main"]);
    let git = Git::new(f.dir.path());
    let pack = review::context(&report, &report.features[0].id, 30_000, Some(&git)).unwrap();
    let sources = pack["source_files"].as_array().unwrap();
    assert!(sources.iter().any(|s| s["role"] == "upstream"
        && s["commit"] == base
        && s["path"] == "src/render.c"
        && s["content"].as_str().unwrap().contains("1500")));
    assert!(sources.iter().any(|s| s["role"] == "before"
        && s["commit"] != base
        && s["content"].as_str().unwrap().contains("1000")));
    assert!(sources
        .iter()
        .any(|s| s["role"] == "after" && s["commit"] == fork_sha));
    assert!(sources.iter().any(|s| s["role"] == "upstream"
        && s["path"] == ".clang-format"
        && s["content"] == "IndentWidth: 4\n"));
    assert!(!pack.to_string().contains("UNCOMMITTED CONVENTIONS"));
    assert_eq!(
        pack["source_coverage"]["upstream_comparison_available"],
        true
    );
}

#[test]
fn review_cleanliness_and_style_require_supported_separate_judgments() {
    let f = Fixture::new();
    let sha = f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let git = Git::new(f.dir.path());
    let mut pack = review::context(&report, &report.features[0].id, 30_000, Some(&git)).unwrap();
    let mut response: review::CodeReview =
        serde_json::from_value(pack["response_example"].clone()).unwrap();
    let upstream_id = format!("file:{}:src/render.c", report.base_sha);
    response.dimensions[1].status = review::ReviewStatus::Reviewed;
    response.dimensions[1].verdict = Some(review::Verdict::Fits);
    response.dimensions[1].evidence = vec![format!("commit:{sha}"), upstream_id.clone()];
    review::validate(&report, &response).unwrap();
    review::validate_context(&response, &pack).unwrap();
    let mut bad = response.clone();
    bad.dimensions[0].verdict = Some(review::Verdict::Fits);
    assert!(
        review::validate(&report, &bad).is_err(),
        "style verdict is not cleanliness"
    );
    let mut bad = response.clone();
    bad.dimensions[1].evidence = vec![format!("commit:{sha}")];
    assert!(
        review::validate_context(&bad, &pack).is_err(),
        "fork code alone cannot establish upstream style"
    );
    let mut bad = response.clone();
    bad.dimensions[1].evidence = vec![upstream_id];
    assert!(
        review::validate_context(&bad, &pack).is_err(),
        "upstream alone cannot assess fork changes"
    );
    for source in pack["source_files"].as_array_mut().unwrap() {
        if source["role"] == "upstream" {
            source["content"] = serde_json::json!("");
        }
    }
    assert!(
        review::validate_context(&response, &pack).is_err(),
        "budget-trimmed upstream cannot support a verdict"
    );
    response.dimensions[1].status = review::ReviewStatus::InsufficientContext;
    response.dimensions[1].verdict = Some(review::Verdict::Unknown);
    response.dimensions[1].evidence.clear();
    review::validate_context(&response, &pack).unwrap();
    let mut legacy = serde_json::to_value(&response).unwrap();
    for dimension in legacy["dimensions"].as_array_mut().unwrap() {
        dimension.as_object_mut().unwrap().remove("verdict");
        dimension.as_object_mut().unwrap().remove("evidence");
    }
    let legacy: review::CodeReview = serde_json::from_value(legacy).unwrap();
    review::validate(&report, &legacy).unwrap();
    assert!(
        review::validate_context(&legacy, &pack).is_err(),
        "new requests require explicit assessments"
    );
    pack["prompt_version"] = serde_json::json!(1);
    review::validate_context(&legacy, &pack).unwrap();
}

#[test]
fn review_requires_all_dimensions_and_real_changed_file_evidence() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let pack = review::context(&report, &report.features[0].id, 30_000, None).unwrap();
    let valid: review::CodeReview =
        serde_json::from_value(pack["response_example"].clone()).unwrap();
    review::validate(&report, &valid).unwrap();
    let mut versioned = valid.clone();
    versioned.schema_version = Some(1);
    review::validate(&report, &versioned).unwrap();
    versioned.schema_version = Some(99);
    assert!(review::validate(&report, &versioned).is_err());
    let mut invalid = valid.clone();
    invalid.dimensions.pop();
    assert!(review::validate(&report, &invalid).is_err());
    let mut invalid = valid.clone();
    invalid.dimensions[0].dimension = review::Dimension::Style;
    assert!(review::validate(&report, &invalid).is_err());
    let mut invalid = valid.clone();
    invalid.findings[0].path = "unrelated/secrets.c".into();
    assert!(review::validate(&report, &invalid).is_err());
    let mut invalid = valid.clone();
    invalid.findings[0].evidence.clear();
    assert!(review::validate(&report, &invalid).is_err());
    let mut invalid = valid.clone();
    invalid.base_sha = "0".repeat(40);
    assert!(review::validate(&report, &invalid).is_err());
    let mut no_findings = valid;
    no_findings.findings.clear();
    review::validate(&report, &no_findings).unwrap();
    let mut invented_fact = pack["response_example"].clone();
    invented_fact["tests_passed"] = serde_json::json!(true);
    assert!(serde_json::from_value::<review::CodeReview>(invented_fact).is_err());
}

#[test]
fn review_adapters_decode_native_cli_envelopes_and_reject_errors() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let pack = review::context(&report, &report.features[0].id, 30_000, None).unwrap();
    let result = serde_json::to_string(&pack["response_example"]).unwrap();
    let claude = serde_json::json!({"type":"result","is_error":false,"result":result});
    assert!(review::decode(&claude.to_string(), &review::OutputFormat::ClaudeJson).is_ok());
    let error = serde_json::json!({"type":"result","is_error":true,"result":result});
    assert!(review::decode(&error.to_string(), &review::OutputFormat::ClaudeJson).is_err());
    let events = format!(
        "{}\n{}\n",
        serde_json::json!({"type":"step_start"}),
        serde_json::json!({"type":"text","part":{"text":result}})
    );
    assert!(review::decode(&events, &review::OutputFormat::OpencodeJson).is_ok());
    let events = format!(
        "{}\n{}",
        events,
        serde_json::json!({"type":"error","error":"quota exceeded"})
    );
    assert!(review::decode(&events, &review::OutputFormat::OpencodeJson).is_err());
    assert!(review::decode(
        &format!("```json\n{result}\n```"),
        &review::OutputFormat::Json
    )
    .is_ok());
}

#[cfg(unix)]
#[test]
fn cli_reviews_save_resume_and_preserve_agent_model_preferences() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let id = &report.features[0].id;
    let pack = review::context(&report, id, 30_000, None).unwrap();
    let response = f.cache.path().join("response.json");
    forkpicker::write_json(&response, &pack["response_example"]).unwrap();
    let output = f.cache.path().join("report.json");
    forkpicker::write_json(&output, &report).unwrap();
    let log = f.cache.path().join("calls");
    let cwd_log = f.cache.path().join("cwd");
    let input_log = f.cache.path().join("input.json");
    let config_path = f.cache.path().join("config.json");
    let config = serde_json::json!({"default_agent":"fixture","agents":{"fixture":{
        "command":["sh","-c","printf x >> \"$CALL_LOG\"; pwd > \"$CWD_LOG\"; cat > \"$INPUT_LOG\"; cat \"$RESPONSE\""],
        "environment":{"CALL_LOG":log,"CWD_LOG":cwd_log,"INPUT_LOG":input_log,"RESPONSE":response}
    }}});
    forkpicker::write_json(&config_path, &config).unwrap();
    let html = f.cache.path().join("review.html");
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--config",
                config_path.to_str().unwrap(),
                "--cache-dir",
                f.cache.path().to_str().unwrap(),
                "--state-dir",
                f.cache.path().join("state").to_str().unwrap(),
                "review",
                output.to_str().unwrap(),
                "--all",
                "--html",
                html.to_str().unwrap(),
            ])
            .args(["--min-priority", "low"])
            .args(extra)
            .output()
            .unwrap()
    };
    let dry = run(&["--dry-run"]);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(!log.exists());
    let result = run(&[]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "x");
    let saved = forkpicker::scan::load_report(&output).unwrap();
    assert_eq!(saved.reviews.len(), 1);
    assert!(saved.reviews[0].requested_model.is_none());
    assert_eq!(saved.features[0].score, report.features[0].score);
    assert_eq!(saved.features[0].status, report.features[0].status);
    assert_ne!(
        std::fs::read_to_string(cwd_log).unwrap().trim(),
        f.dir.path().to_str().unwrap()
    );
    let input: serde_json::Value =
        serde_json::from_slice(&std::fs::read(input_log).unwrap()).unwrap();
    assert_eq!(input["feature"]["id"], *id);
    assert!(std::fs::read_to_string(&html)
        .unwrap()
        .contains("Code review · unverified"));
    let rendered = std::fs::read_to_string(&html).unwrap();
    assert!(rendered.contains("Code cleanliness · mixed"));
    assert!(rendered.contains("Fit with upstream style · unknown"));
    assert!(run(&[]).status.success());
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "x",
        "cached review should not call the agent"
    );
    // Reuse the separate review cache after regenerating a report.
    forkpicker::write_json(&output, &report).unwrap();
    assert!(run(&[]).status.success());
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "x");
    assert_eq!(
        forkpicker::scan::load_report(&output)
            .unwrap()
            .reviews
            .len(),
        1
    );
    assert!(run(&["--force"]).status.success());
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "xx");
    // An adapter/schema failure preserves prior completed work and returns nonzero.
    std::fs::write(&response, "invalid json").unwrap();
    assert_eq!(run(&["--force"]).status.code(), Some(2));
    assert_eq!(
        forkpicker::scan::load_report(&output)
            .unwrap()
            .reviews
            .len(),
        1
    );
}

#[cfg(unix)]
#[test]
fn agent_argv_placeholders_are_literal_and_output_files_supported() {
    let config = review::Config::default();
    assert_eq!(config.agents.len(), 6);
    assert!(config
        .agents
        .values()
        .all(|p| p.model.is_none() && p.effort.is_none()));
    let mut profile = config.agents["codex"].clone();
    profile.command = vec![
        "sh".into(),
        "-c".into(),
        "cat > \"$1\"".into(),
        "fixture".into(),
        "{output}".into(),
    ];
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("input with spaces.json");
    let output = temp.path().join("output with spaces.json");
    let command = review::invocation(&profile, &input, &output).unwrap();
    let value = llm::run_process(
        &command,
        b"{\"ok\":true}",
        std::time::Duration::from_secs(2),
        Some(temp.path()),
        &Default::default(),
        Some(&output),
    )
    .unwrap();
    assert_eq!(value, "{\"ok\":true}");
    profile.model = Some("literal$(touch injected)".into());
    let command = review::invocation(&profile, &input, &output).unwrap();
    assert_eq!(command.last().unwrap(), "literal$(touch injected)");
    assert!(!temp.path().join("injected").exists());
}

#[cfg(unix)]
#[test]
fn review_call_limit_resumes_without_repeating_completed_features() {
    let f = Fixture::new();
    f.hdr();
    f.write("src/parser.c", "int parse = 7;\n");
    f.commit("parser: reject malformed expressions");
    let report = f.scan(&["hdr"], &["main"]);
    assert_eq!(report.features.len(), 2);
    let output = f.cache.path().join("report.json");
    let response = f.cache.path().join("response.json");
    let config_path = f.cache.path().join("config.json");
    let log = f.cache.path().join("calls");
    forkpicker::write_json(&output, &report).unwrap();
    let config = serde_json::json!({"default_agent":"fixture","agents":{"fixture":{
        "command":["sh","-c","cat > /dev/null; printf x >> \"$CALL_LOG\"; cat \"$RESPONSE\""],
        "environment":{"CALL_LOG":log,"RESPONSE":response}
    }}});
    forkpicker::write_json(&config_path, &config).unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--config",
                config_path.to_str().unwrap(),
                "--cache-dir",
                f.cache.path().to_str().unwrap(),
                "--state-dir",
                f.cache.path().join("state").to_str().unwrap(),
                "review",
                output.to_str().unwrap(),
                "--all",
                "--limit",
                "1",
            ])
            .args(["--min-priority", "low"])
            .output()
            .unwrap()
    };
    for (i, feature) in report.features.iter().enumerate() {
        let pack = review::context(&report, &feature.id, 30_000, None).unwrap();
        forkpicker::write_json(&response, &pack["response_example"]).unwrap();
        let result = run();
        assert_eq!(
            result.status.code(),
            Some(if i == 0 { 2 } else { 0 }),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            forkpicker::scan::load_report(&output)
                .unwrap()
                .reviews
                .len(),
            i + 1
        );
    }
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "xx");
    assert!(run().status.success());
    assert_eq!(std::fs::read_to_string(log).unwrap(), "xx");
}

#[test]
fn review_cache_identity_tracks_model_effort_and_evidence() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let pack = review::context(&report, &report.features[0].id, 30_000, None).unwrap();
    let mut profile = review::Config::default().agents["codex"].clone();
    let original = review::Request {
        agent: "codex",
        profile: &profile,
        pack: &pack,
    }
    .key()
    .unwrap();
    profile.model = Some("explicit-model".into());
    let model_key = review::Request {
        agent: "codex",
        profile: &profile,
        pack: &pack,
    }
    .key()
    .unwrap();
    assert_ne!(original, model_key);
    profile.effort = Some("high".into());
    assert_ne!(
        model_key,
        review::Request {
            agent: "codex",
            profile: &profile,
            pack: &pack
        }
        .key()
        .unwrap()
    );
    let mut changed = pack.clone();
    changed["commits"][0]["patch"] = serde_json::json!("different patch");
    assert_ne!(
        review::Request {
            agent: "codex",
            profile: &profile,
            pack: &pack
        }
        .key()
        .unwrap(),
        review::Request {
            agent: "codex",
            profile: &profile,
            pack: &changed
        }
        .key()
        .unwrap()
    );
}

fn demand_thread(title: &str, state: &str, votes: u64) -> Issue {
    serde_json::from_value(serde_json::json!({
        "number":42,"title":title,"body":"Feature request","url":"https://github.com/upstream/example/issues/42",
        "state":state,"is_pull_request":false,"kind":"issue","match_kind":"demand_sample","thumbs_up":votes,
        "labels":[]
    })).unwrap()
}

fn demand_snapshot(threads: Vec<Issue>) -> priority::DemandSnapshot {
    priority::DemandSnapshot {
        fetched_at: "2026-09-09T00:00:00Z".into(),
        query: None,
        priority_labels: priority::default_priority_labels(),
        threads,
        warnings: Vec::new(),
    }
}

#[test]
fn promising_code_needs_no_announcement_and_routine_changes_are_low_priority() {
    let f = Fixture::new();
    f.hdr();
    f.write("tests/render.c", "assert(white == 203);\n");
    f.commit("test: verify HDR reference white");
    f.write("deps.lock", "new dependency lock\n");
    f.commit("chore: refresh lockfile");
    let report = f.scan(&["hdr"], &["main"]);
    let hdr = report
        .features
        .iter()
        .find(|f| f.title.contains("HDR"))
        .unwrap();
    let lock = report
        .features
        .iter()
        .find(|f| f.title.contains("lockfile"))
        .unwrap();
    let ranked = priority::recommendation(&report, hdr);
    assert_eq!(ranked.tier, priority::Tier::High);
    assert_eq!(ranked.demand_status, "not_collected");
    assert_eq!(
        priority::recommendation(&report, lock).tier,
        priority::Tier::Low
    );
    assert_eq!(priority::selected(&report, priority::Tier::High).len(), 1);
}

#[test]
fn relevant_demand_can_raise_priority_without_mutating_code_candidates() {
    let f = Fixture::new();
    f.hdr();
    f.git(&[
        "commit",
        "--amend",
        "-qm",
        "render: normalize HDR reference white (#42)",
    ]);
    let mut report = f.scan(&["hdr"], &["main"]);
    let candidates = serde_json::to_value(&report.features).unwrap();
    let feature = report.features[0].clone();
    assert_eq!(
        priority::recommendation(&report, &feature).tier,
        priority::Tier::Medium
    );
    let mut thread = demand_thread("HDR reference white", "open", 100);
    thread.labels = vec!["priority: high".into()];
    report.demand = Some(demand_snapshot(vec![thread.clone(), thread]));
    priority::refresh(&mut report);
    let ranked = priority::get(&report, &feature);
    assert_eq!(ranked.tier, priority::Tier::High);
    assert_eq!(ranked.demand_points, 20);
    assert_eq!(ranked.demand_matches.len(), 1);
    assert_eq!(serde_json::to_value(&report.features).unwrap(), candidates);
    assert!(ranked.demand_matches[0]
        .signals
        .iter()
        .any(|s| s.contains("priority: high")));
    // Externally collected demand does not invalidate unchanged code review inputs.
    let with_demand = review::context(&report, &feature.id, 30_000, None).unwrap();
    report.demand = None;
    assert_eq!(
        with_demand,
        review::context(&report, &feature.id, 30_000, None).unwrap()
    );
}

#[test]
fn popularity_requires_relevance_and_unmet_demand() {
    let f = Fixture::new();
    f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    let feature = report.features[0].clone();
    for (title, state) in [
        ("Database migration support", "open"),
        ("HDR reference white", "closed"),
        ("HDR reference white", "answered"),
    ] {
        report.demand = Some(demand_snapshot(vec![demand_thread(title, state, 10_000)]));
        assert_eq!(priority::recommendation(&report, &feature).demand_points, 0);
    }
    let mut other_project = demand_thread("HDR reference white", "open", 10_000);
    other_project.url = "https://github.com/other/project/issues/42".into();
    report.demand = Some(demand_snapshot(vec![other_project]));
    assert_eq!(priority::recommendation(&report, &feature).demand_points, 0);
    let mut pull = demand_thread("HDR reference white", "open", 10_000);
    pull.is_pull_request = true;
    report.demand = Some(demand_snapshot(vec![pull]));
    assert_eq!(priority::recommendation(&report, &feature).demand_points, 0);
    let thread = demand_thread("HDR reference white", "open", 10_000);
    report.demand = Some(demand_snapshot(vec![thread]));
    let ranked = priority::recommendation(&report, &feature);
    assert_eq!(
        ranked.demand_points, 12,
        "lexical matches need a conservative cap"
    );
    assert_eq!(ranked.tier, priority::Tier::Medium);
    report.demand.as_mut().unwrap().threads[0].comments_omitted = 1_000_000;
    assert_eq!(
        priority::recommendation(&report, &feature).score,
        ranked.score,
        "raw activity is not demand"
    );
}

#[test]
fn smaller_patch_variants_do_not_trigger_redundant_model_reviews() {
    let f = Fixture::new();
    f.hdr();
    f.git(&["branch", "older"]);
    f.write("tests/render.c", "assert(white == 203);\n");
    f.commit("test: verify HDR reference white");
    let report = f.scan(&["hdr", "older"], &["main"]);
    assert_eq!(report.features.len(), 2);
    let older = report
        .features
        .iter()
        .find(|f| f.patch_ids.len() == 1)
        .unwrap();
    let ranked = priority::recommendation(&report, older);
    assert_eq!(ranked.tier, priority::Tier::Low);
    assert!(ranked.superseded_by.is_some());
    assert_eq!(priority::selected(&report, priority::Tier::High).len(), 1);
}

#[test]
fn explicit_legacy_filter_does_not_invoke_an_agent_for_lower_priority_work() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let path = f.cache.path().join("report.json");
    forkpicker::write_json(&path, &report).unwrap();
    let config = f.cache.path().join("config.json");
    forkpicker::write_json(&config,&serde_json::json!({"agents":{"fixture":{"command":["this-executable-does-not-exist-forkpicker"]}}})).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_forkpicker"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--state-dir",
            f.cache.path().to_str().unwrap(),
            "review",
            path.to_str().unwrap(),
            "--all",
            "--min-priority",
            "high",
            "--agent",
            "fixture",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("1 lower-priority candidates skipped"));
    assert!(forkpicker::scan::load_report(&path)
        .unwrap()
        .reviews
        .is_empty());
}

#[test]
fn indexed_relationships_and_priority_match_exhaustive_comparisons() {
    let fixture = Fixture::new();
    fixture.hdr();
    let mut report = fixture.scan(&["hdr"], &["main"]);
    let template = report.features[0].clone();
    report.features = (0..128)
        .map(|i| {
            let mut feature = template.clone();
            feature.id = format!("candidate-{i:03}");
            feature.title = [
                "HDR reference white",
                "HDR capture highlights",
                "parser invalid expressions",
                "renderer cursor scaling",
            ][i % 4]
                .into();
            feature.files = if i % 11 == 0 {
                vec![]
            } else {
                vec![format!("src/file-{}.cpp", i % 7)]
            };
            feature.patch_ids = if i % 13 == 0 {
                vec![]
            } else {
                vec![format!("patch-{}", i % 19)]
            };
            if i % 3 == 0 {
                feature.patch_ids.push(format!("patch-{}", i % 23));
            }
            feature.related_features.clear();
            feature
        })
        .collect();
    let expected: std::collections::BTreeMap<_, _> = report
        .features
        .iter()
        .map(|feature| {
            let related: std::collections::BTreeSet<_> = report
                .features
                .iter()
                .filter(|other| {
                    feature.id != other.id
                        && (feature
                            .patch_ids
                            .iter()
                            .any(|p| other.patch_ids.contains(p))
                            || (feature.files.iter().any(|p| other.files.contains(p))
                                && analyze::tokens(&feature.title)
                                    .intersection(&analyze::tokens(&other.title))
                                    .count()
                                    >= 2))
                })
                .map(|f| f.id.clone())
                .collect();
            (feature.id.clone(), related)
        })
        .collect();
    let consolidated = analyze::consolidate(report.features.clone());
    for feature in consolidated {
        assert_eq!(
            feature
                .related_features
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            expected[&feature.id]
        );
    }
    let priorities: std::collections::BTreeMap<_, _> = report
        .features
        .iter()
        .map(|f| {
            (
                f.id.clone(),
                serde_json::to_value(priority::recommendation(&report, f)).unwrap(),
            )
        })
        .collect();
    priority::refresh(&mut report);
    for (id, actual) in &report.recommendations {
        assert_eq!(serde_json::to_value(actual).unwrap(), priorities[id]);
    }
}

#[test]
fn demand_shortlist_uses_requests_not_test_counts_or_topic_preferences() {
    use forkpicker::shortlist;
    let fixture = Fixture::new();
    fixture.hdr();
    let mut report = fixture.scan(&["hdr"], &["main"]);
    report.repository = "upstream/example".into();
    let sha = report.features[0].commits[0].clone();
    report.commits.get_mut(&sha).unwrap().message = "Implement requested behavior #42".into();
    report.demand = Some(demand_snapshot(vec![demand_thread(
        "Requested behavior",
        "open",
        5,
    )]));
    let original = shortlist::build(&report, None, None, false, 10).unwrap();
    assert_eq!(original.counts["eligible"], 1);
    report.features[0].title = "Entirely different subsystem".into();
    report.features[0].test_files = vec!["tests/new.c".into(); 100];
    report.features[0].score = 100;
    let changed = shortlist::build(&report, None, None, false, 10).unwrap();
    assert_eq!(original.selected_feature_ids, changed.selected_feature_ids);
    assert_eq!(
        original.requests[0].positive_votes,
        changed.requests[0].positive_votes
    );
    report.commits.get_mut(&sha).unwrap().message = "No explicit request".into();
    report.features[0].title = "Requested behavior".into();
    let unknown = shortlist::build(&report, None, None, false, 10).unwrap();
    assert!(unknown.selected_feature_ids.is_empty());
    assert_eq!(unknown.candidates.len(), 1);
    report.demand.as_mut().unwrap().query = Some("HDR".into());
    assert!(shortlist::build(&report, None, None, false, 10).is_err());
}

#[test]
fn classification_filters_prs_before_fork_limits_seeds_and_related_catalogs() {
    use forkpicker::{classify, shortlist};
    use serde_json::json;
    let f = Fixture::new();
    let covered_sha = f.hdr();
    f.write("src/parser.c", "int parse = 7;\n");
    let free_sha = f.commit("parser: handle errors");
    let mut report = f.scan(&["hdr"], &["main"]);
    for feature in &mut report.features {
        feature.sources[0].repository = "z/free".into();
        if feature.commits.contains(&covered_sha) {
            let mut alias = feature.sources[0].clone();
            alias.repository = "a/already-submitted".into();
            feature.sources.push(alias);
        }
    }
    let covered = report
        .features
        .iter()
        .find(|f| f.commits.contains(&covered_sha))
        .unwrap()
        .id
        .clone();
    let free = report
        .features
        .iter()
        .find(|f| f.commits.contains(&free_sha))
        .unwrap()
        .id
        .clone();
    let mut snapshot = shortlist::PullSnapshot {
        repository: report.repository.clone(),
        fetched_at: report.generated_at.clone(),
        listing_complete: false,
        warnings: vec!["Fixture partial listing".into()],
        api_requests: 0,
        pulls: vec![shortlist::Pull {
            number: 42,
            title: "An unrelated title".into(),
            draft: true,
            head_sha: "a".repeat(40),
            base_branch: "main".into(),
            commits: vec![covered_sha],
            commits_complete: false,
            membership_verified: true,
        }],
    };
    let rp = f.cache.path().join("report.json");
    let pp = f.cache.path().join("prs.json");
    let output = f.cache.path().join("classification.json");
    let config = f.cache.path().join("config.json");
    let plans = f.cache.path().join("plans");
    forkpicker::write_json(&rp, &report).unwrap();
    forkpicker::write_json(&pp, &snapshot).unwrap();
    forkpicker::write_json(&config,&json!({"agents":{"codex":{"command":["python3","-c","raise Exception('must not invoke model')"]}}})).unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--config",
                config.to_str().unwrap(),
                "classify",
                rp.to_str().unwrap(),
                "--pr-snapshot",
                pp.to_str().unwrap(),
                "--agent",
                "codex",
                "--max-forks",
                "1",
                "--dry-run",
                "--output",
                output.to_str().unwrap(),
            ])
            .args(extra)
            .output()
            .unwrap()
    };
    let r = run(&[]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let read =
        || serde_json::from_slice::<classify::Run>(&std::fs::read(&output).unwrap()).unwrap();
    let result = read();
    assert_eq!(result.attempted_calls, 0);
    assert_eq!(result.forks.len(), 1);
    assert_eq!(result.forks[0].repository, "z/free");
    assert_eq!(result.forks[0].candidate_ids, vec![free.clone()]);
    assert_eq!(
        result.pr_filter.unwrap().excluded_candidates[&covered],
        vec![42]
    );
    let r = run(&[
        "--explore-related",
        "--candidate",
        &covered,
        "--candidate",
        &free,
        "--seed-limit",
        "1",
        "--plan-dir",
        plans.to_str().unwrap(),
    ]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let e: forkpicker::related::Experiment =
        serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
    assert_eq!(e.exploration[0].seed_ids, vec![free.clone()]);
    assert_eq!(e.exploration[0].catalog_candidates, 0);
    for path in std::fs::read_dir(&plans).unwrap() {
        assert!(
            !std::fs::read_to_string(path.unwrap().path())
                .unwrap()
                .contains(&covered),
            "covered patch leaked into model input"
        );
    }
    snapshot.pulls[0].commits.push(free_sha);
    forkpicker::write_json(&pp, &snapshot).unwrap();
    assert!(run(&[
        "--explore-related",
        "--candidate",
        &covered,
        "--candidate",
        &free
    ])
    .status
    .success());
    assert!(read().forks.is_empty());
    assert_eq!(read().attempted_calls, 0);
    snapshot.repository = "other/project".into();
    forkpicker::write_json(&pp, &snapshot).unwrap();
    assert!(!run(&[]).status.success());
}

#[test]
fn pr_coverage_requires_verified_evidence_and_recognizes_patch_equivalence() {
    use forkpicker::shortlist::{self, Pull, PullSnapshot};
    let f = Fixture::new();
    let first = f.hdr();
    f.write("src/render.c", "int white = 204;\nint peak = 1000;\n");
    let second = f.commit("render: normalize HDR reference white");
    let mut report = f.scan(&["hdr"], &["main"]);
    assert_eq!(report.features.len(), 1);
    let id = report.features[0].id.clone();
    let alias = "d".repeat(40);
    let mut rebased = report.commits[&first].clone();
    rebased.sha = alias.clone();
    report.commits.insert(alias.clone(), rebased);
    let mut snapshot = PullSnapshot {
        repository: report.repository.clone(),
        fetched_at: report.generated_at.clone(),
        listing_complete: false,
        warnings: vec![],
        api_requests: 0,
        pulls: vec![Pull {
            number: 1,
            title: report.features[0].title.clone(),
            draft: false,
            head_sha: "a".repeat(40),
            base_branch: "main".into(),
            commits: vec![alias],
            commits_complete: false,
            membership_verified: true,
        }],
    };
    let partial = shortlist::pull_coverage(&report, &snapshot).unwrap();
    assert!(partial[&id].open_prs.is_empty());
    assert_eq!(partial[&id].partial_prs, vec![1]);
    let mut other = snapshot.pulls[0].clone();
    other.number = 2;
    other.commits = vec![second];
    snapshot.pulls.push(other);
    assert_eq!(
        shortlist::pull_coverage(&report, &snapshot).unwrap()[&id].open_prs,
        vec![1, 2]
    );
    for pr in &mut snapshot.pulls {
        pr.membership_verified = false;
    }
    assert!(
        shortlist::pull_coverage(&report, &snapshot)
            .unwrap()
            .is_empty(),
        "titles and unverified membership cannot hide work"
    );
    snapshot.repository = "foreign/example".into();
    assert!(shortlist::pull_coverage(&report, &snapshot).is_err());
}

#[test]
fn shortlist_distinguishes_new_observations_and_requires_full_pr_patch_coverage() {
    use forkpicker::shortlist::{self, Pull, PullSnapshot};
    let fixture = Fixture::new();
    fixture.hdr();
    let baseline = fixture.scan(&["hdr"], &["main"]);
    fixture.write("src/render.c", "int white = 204;\nint peak = 1000;\n");
    fixture.commit("render: normalize HDR reference white #42");
    let mut report = fixture.scan(&["hdr"], &["main"]);
    let mut baseline = baseline;
    report.repository = "upstream/example".into();
    baseline.repository = report.repository.clone();
    report.demand = Some(demand_snapshot(vec![demand_thread(
        "HDR reference white",
        "open",
        12,
    )]));
    let mut prs = PullSnapshot {
        repository: report.repository.clone(),
        fetched_at: report.generated_at.clone(),
        pulls: vec![Pull {
            number: 1,
            title: "Partial patch".into(),
            draft: false,
            head_sha: "a".repeat(40),
            base_branch: "main".into(),
            commits: vec![report.features[0].commits[0].clone()],
            commits_complete: true,
            membership_verified: true,
        }],
        listing_complete: true,
        warnings: vec![],
        api_requests: 0,
    };
    let partial = shortlist::build(&report, Some(&baseline), Some(&prs), true, 10).unwrap();
    assert_eq!(partial.candidates[0].delta, "newly_observed_patches");
    assert!(partial.candidates[0].open_prs.is_empty());
    assert_eq!(partial.candidates[0].partial_prs, vec![1]);
    assert_eq!(partial.selected_feature_ids.len(), 1);
    prs.pulls[0].commits = report.features[0].commits.clone();
    let covered = shortlist::build(&report, Some(&baseline), Some(&prs), true, 10).unwrap();
    assert!(covered.selected_feature_ids.is_empty());
    assert_eq!(covered.candidates[0].open_prs, vec![1]);
    assert!(shortlist::build(&report, None, None, true, 10).is_err());
    let unchanged = shortlist::build(&report, Some(&report), None, true, 10).unwrap();
    assert_eq!(unchanged.candidates[0].delta, "unchanged_patch_set");
    assert!(unchanged.selected_feature_ids.is_empty());
    assert!(shortlist::validate_selection(&partial, &report).is_ok());
    report.base_sha = "b".repeat(40);
    assert!(shortlist::validate_selection(&partial, &report).is_err());
}

#[test]
fn shortlist_demand_order_and_unknown_votes_are_explicit() {
    use forkpicker::shortlist;
    let fixture = Fixture::new();
    fixture.hdr();
    let mut report = fixture.scan(&["hdr"], &["main"]);
    report.repository = "upstream/example".into();
    let mut voted = demand_thread("Popular request", "open", 100);
    let mut urgent = voted.clone();
    urgent.number = 43;
    urgent.url = "https://github.com/upstream/example/issues/43".into();
    urgent.thumbs_up = Some(1);
    urgent.labels = vec!["priority: high".into()];
    let mut unknown = voted.clone();
    unknown.number = 44;
    unknown.url = "https://github.com/upstream/example/issues/44".into();
    unknown.thumbs_up = None;
    voted.comments = vec![EvidenceComment {
        author: "author".into(),
        url: "https://github.com/upstream/example/issues/42#comment".into(),
        body: "Noise".repeat(1000),
        body_truncated: false,
    }];
    report.demand = Some(demand_snapshot(vec![unknown, urgent, voted]));
    let result = shortlist::build(&report, None, None, false, 10).unwrap();
    assert!(result.requests[0].url.ends_with("/43"));
    assert!(result.requests[1].url.ends_with("/42"));
    assert_eq!(result.requests[2].positive_votes, None);
    assert!(result.selected_feature_ids.is_empty());
    assert_eq!(result.counts["no_observed_demand_link"], 1);
}

#[test]
fn shortlist_indexes_preserve_reference_scope_and_patch_equivalent_links() {
    use forkpicker::shortlist;
    let f = Fixture::new();
    f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    report.repository = "upstream/example".into();
    report.features[0].sources[0].repository = "author/fork".into();
    let sha = report.features[0].commits[0].clone();
    let mut older = report.commits[&sha].clone();
    older.sha = "d".repeat(40);
    report.commits.insert(older.sha.clone(), older);
    let mut thread = demand_thread("Unrelated title", "open", 3);
    thread.kind = "discussion".into();
    thread.discussion_category = Some("bugs".into());
    thread.url = "https://github.com/upstream/example/discussions/42".into();
    report.commits.get_mut(&sha).unwrap().message =
        "Mentions #42, which is an issue, not this discussion".into();
    report.demand = Some(demand_snapshot(vec![thread.clone()]));
    assert!(shortlist::build(&report, None, None, false, 10)
        .unwrap()
        .selected_feature_ids
        .is_empty());
    thread.body = format!(
        "Implementation: https://github.com/author/fork/commit/{}",
        "d".repeat(12)
    );
    report.demand = Some(demand_snapshot(vec![thread.clone()]));
    assert_eq!(
        shortlist::build(&report, None, None, false, 10)
            .unwrap()
            .selected_feature_ids
            .len(),
        1
    );
    thread.body = thread.body.replace("author/fork", "unrelated/repository");
    report.demand = Some(demand_snapshot(vec![thread]));
    assert!(shortlist::build(&report, None, None, false, 10)
        .unwrap()
        .selected_feature_ids
        .is_empty());
}

#[test]
fn shortlist_cli_round_trip_and_review_rejects_changed_evidence() {
    let f = Fixture::new();
    f.hdr();
    f.write("src/render.c", "int white = 204;\nint peak = 1000;\n");
    f.commit("render: normalize HDR reference white #42");
    let mut report = f.scan(&["hdr"], &["main"]);
    report.repository = "upstream/example".into();
    report.demand = Some(demand_snapshot(vec![demand_thread(
        "HDR reference white",
        "open",
        4,
    )]));
    let dir = tempfile::tempdir().unwrap();
    let report_path = dir.path().join("report.json");
    let selection = dir.path().join("shortlist.json");
    forkpicker::write_json(&report_path, &report).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--state-dir",
                dir.path().to_str().unwrap(),
                "--cache-dir",
                f.cache.path().to_str().unwrap(),
            ])
            .args(args)
            .output()
            .unwrap()
    };
    let result = run(&[
        "shortlist",
        report_path.to_str().unwrap(),
        "--output",
        selection.to_str().unwrap(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let picked: forkpicker::shortlist::Shortlist =
        serde_json::from_slice(&std::fs::read(&selection).unwrap()).unwrap();
    assert_eq!(picked.selected_feature_ids.len(), 1);
    let review_args = [
        "review",
        report_path.to_str().unwrap(),
        "--shortlist",
        selection.to_str().unwrap(),
        "--agent",
        "codex",
        "--dry-run",
        "--repo",
        f.dir.path().to_str().unwrap(),
    ];
    let result = run(&review_args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("1 planned calls"));
    let protected = std::fs::read(&report_path).unwrap();
    assert!(!run(&[
        "shortlist",
        report_path.to_str().unwrap(),
        "--output",
        report_path.to_str().unwrap()
    ])
    .status
    .success());
    assert_eq!(std::fs::read(&report_path).unwrap(), protected);
    report
        .commits
        .values_mut()
        .next()
        .unwrap()
        .patch
        .push_str("\nchanged evidence\n");
    forkpicker::write_json(&report_path, &report).unwrap();
    let result = run(&review_args);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("pinned evidence"));
}

#[test]
fn community_votes_do_not_become_feature_demand_and_category_areas_are_equal() {
    use forkpicker::shortlist;
    let f = Fixture::new();
    f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    report.repository = "upstream/example".into();
    let sha = report.features[0].commits[0].clone();
    let mut thread = demand_thread("Project announcement", "unanswered", 10000);
    thread.kind = "discussion".into();
    thread.discussion_category = Some("announcements".into());
    thread.discussion_answerable = Some(false);
    thread.url = "https://github.com/upstream/example/discussions/42".into();
    report.commits.get_mut(&sha).unwrap().message = thread.url.clone();
    report.demand = Some(demand_snapshot(vec![thread.clone()]));
    let community = shortlist::build(&report, None, None, false, 10).unwrap();
    assert!(community.selected_feature_ids.is_empty());
    assert_eq!(community.requests[0].demand_class, "community_activity");
    assert_eq!(community.candidates[0].other_thread_links.len(), 1);
    thread.discussion_category = Some("feature-requests-renderer".into());
    report.demand = Some(demand_snapshot(vec![thread.clone()]));
    let renderer = shortlist::build(&report, None, None, false, 10).unwrap();
    assert_eq!(renderer.selected_feature_ids.len(), 1);
    thread.discussion_category = Some("feature-requests-config".into());
    report.demand = Some(demand_snapshot(vec![thread.clone()]));
    let config = shortlist::build(&report, None, None, false, 10).unwrap();
    assert_eq!(renderer.selected_feature_ids, config.selected_feature_ids);
    thread.discussion_category = Some("general".into());
    report.demand = Some(demand_snapshot(vec![thread]));
    let unknown = shortlist::build(&report, None, None, false, 10).unwrap();
    assert_eq!(unknown.requests[0].demand_class, "unclassified");
    assert!(unknown.selected_feature_ids.is_empty());
    assert_eq!(unknown.candidates[0].other_thread_links.len(), 1);
}

#[test]
fn triage_retrieves_symbol_evidence_without_explicit_links_and_preserves_votes() {
    use forkpicker::{shortlist, triage};
    let fixture = Fixture::new();
    fixture.hdr();
    let mut report = fixture.scan(&["hdr"], &["main"]);
    report.repository = "upstream/example".into();
    let sha = report.features[0].commits[0].clone();
    report.features[0].title = "Improve behavior".into();
    report.commits.get_mut(&sha).unwrap().message = "Adjust implementation".into();
    report.commits.get_mut(&sha).unwrap().files[0].symbols =
        vec!["normalizeCaptureReferenceWhite".into()];
    report.demand = Some(demand_snapshot(vec![demand_thread(
        "Capture reference white normalization",
        "open",
        7,
    )]));
    let s = shortlist::build(&report, None, None, false, 10).unwrap();
    assert!(s.selected_feature_ids.is_empty());
    let cards = triage::plan(&report, &s, report.demand.as_ref().unwrap(), 1, 0, "seed").unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].sampling, "retrieved_request");
    assert_eq!(cards[0].requests[0]["positive_votes"], 7);
    let encoded = serde_json::to_vec(&triage::context(
        &cards,
        &report.repository,
        &report.base_sha,
    ))
    .unwrap();
    report.features[0].score = 999;
    report.features[0].test_files = vec!["tests/test.c".into(); 100];
    let other = shortlist::build(&report, None, None, false, 10).unwrap();
    let again = triage::plan(
        &report,
        &other,
        report.demand.as_ref().unwrap(),
        1,
        0,
        "seed",
    )
    .unwrap();
    assert_eq!(
        encoded,
        serde_json::to_vec(&triage::context(
            &again,
            &report.repository,
            &report.base_sha
        ))
        .unwrap()
    );
    report.demand.as_mut().unwrap().threads[0].thumbs_up = Some(99);
    assert!(triage::plan(&report, &s, report.demand.as_ref().unwrap(), 1, 0, "seed").is_err());
}

#[test]
fn triage_rejects_invented_or_cross_candidate_evidence_and_duplicate_results() {
    use forkpicker::triage::*;
    use serde_json::json;
    let card = Card {
        feature_id: "f1".into(),
        title: "Example".into(),
        sampling: "unknown_exploration".into(),
        patches: vec!["p1".into()],
        commits: vec![json!({"evidence_id":"commit:c1"})],
        requests: vec![json!({"url":"https://github.com/o/r/issues/1"})],
        limitations: vec![],
    };
    let assessment = Assessment {
        feature_id: "f1".into(),
        summary: Claim {
            text: "Observation".into(),
            evidence: vec!["commit:c1".into()],
        },
        matches: vec![Match {
            request_url: "https://github.com/o/r/issues/1".into(),
            relation: "plausible_implementation".into(),
            rationale: "Behavioral explanation".into(),
            evidence: vec![
                "request:https://github.com/o/r/issues/1".into(),
                "commit:c1".into(),
            ],
        }],
        limitations: vec!["Static excerpt only".into()],
    };
    let valid = Response {
        schema_version: None,
        assessments: vec![assessment.clone()],
    };
    assert!(validate(&valid, std::slice::from_ref(&card)).is_ok());
    let mut bad = valid.clone();
    bad.assessments[0].matches[0].request_url = "https://github.com/o/r/issues/999".into();
    assert!(validate(&bad, std::slice::from_ref(&card)).is_err());
    let mut bad = valid.clone();
    bad.assessments[0].matches[0].evidence[1] = "commit:other-candidate".into();
    assert!(validate(&bad, std::slice::from_ref(&card)).is_err());
    let mut bad = valid.clone();
    bad.assessments.push(assessment);
    assert!(validate(&bad, std::slice::from_ref(&card)).is_err());
    let mut value = serde_json::to_value(&valid).unwrap();
    value["assessments"][0]["impact_score"] = json!(100);
    assert!(serde_json::from_value::<Response>(value).is_err());
}

#[test]
fn triage_cli_spending_cache_and_changed_context_are_bounded() {
    use forkpicker::{shortlist, triage};
    use serde_json::json;
    let fixture = Fixture::new();
    fixture.hdr();
    let mut report = fixture.scan(&["hdr"], &["main"]);
    report.repository = "upstream/example".into();
    report.demand = Some(demand_snapshot(vec![demand_thread(
        "Capture white reference",
        "open",
        7,
    )]));
    let s = shortlist::build(&report, None, None, false, 10).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let rp = root.join("report.json");
    let sp = root.join("shortlist.json");
    let dp = root.join("demand.json");
    let output = root.join("triage.json");
    let cp = root.join("config.json");
    let response = root.join("response.json");
    let marker = root.join("calls");
    let script = root.join("model.py");
    forkpicker::write_json(&rp, &report).unwrap();
    forkpicker::write_json(&sp, &s).unwrap();
    forkpicker::write_json(&dp, report.demand.as_ref().unwrap()).unwrap();
    let card = triage::plan(
        &report,
        &s,
        report.demand.as_ref().unwrap(),
        1,
        20,
        "forkpicker-v1",
    )
    .unwrap()
    .remove(0);
    forkpicker::write_json(&response,&json!({"assessments":[{"feature_id":card.feature_id,"summary":{"text":"Observed change","evidence":[card.commits[0]["evidence_id"]]},"matches":[],"limitations":["Static only"]}]})).unwrap();
    std::fs::write(&script,"import sys,pathlib\npathlib.Path(sys.argv[1]).open('a').write('call\\n')\nprint(pathlib.Path(sys.argv[2]).read_text())\n").unwrap();
    forkpicker::write_json(
        &cp,
        &json!({"agents":{"fake":{"command":["python3",script,marker,response]}}}),
    )
    .unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--config",
                cp.to_str().unwrap(),
                "--cache-dir",
                root.join("cache").to_str().unwrap(),
                "--state-dir",
                root.join("state").to_str().unwrap(),
                "triage",
                rp.to_str().unwrap(),
                "--shortlist",
                sp.to_str().unwrap(),
                "--demand-snapshot",
                dp.to_str().unwrap(),
                "--agent",
                "fake",
                "--candidates",
                "1",
                "--output",
                output.to_str().unwrap(),
            ])
            .args(extra)
            .output()
            .unwrap()
    };
    assert!(run(&["--dry-run"]).status.success());
    assert!(!marker.exists());
    assert_eq!(run(&["--limit", "0"]).status.code(), Some(2));
    assert!(!marker.exists());
    assert!(run(&[]).status.success());
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    assert!(run(&[]).status.success());
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    let result: triage::Experiment =
        serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
    assert_eq!(result.reused_batches, 1);
    assert!(result.suggested_review_ids.is_empty());
    // Recover a saved provider response after loss of its parsed cache entry, without a call.
    std::fs::remove_file(
        root.join("cache/triage")
            .join(format!("{}.json", result.batches[0].key)),
    )
    .unwrap();
    assert!(run(&[]).status.success());
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    let sha = report.features[0].commits[0].clone();
    report
        .commits
        .get_mut(&sha)
        .unwrap()
        .patch
        .push_str("\n+new evidence");
    forkpicker::write_json(&rp, &report).unwrap();
    assert!(!run(&[]).status.success());
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    let fresh = shortlist::build(&report, None, None, false, 10).unwrap();
    forkpicker::write_json(&sp, &fresh).unwrap();
    assert!(run(&[]).status.success());
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 2);
    assert!(!run(&["--output", rp.to_str().unwrap()]).status.success());
}

#[test]
fn triage_preserves_baseline_filter_and_rejects_unsupported_review_selections() {
    use forkpicker::{shortlist, triage};
    let fixture = Fixture::new();
    fixture.hdr();
    let mut report = fixture.scan(&["hdr"], &["main"]);
    report.repository = "upstream/example".into();
    report.demand = Some(demand_snapshot(vec![demand_thread(
        "Capture reference white",
        "open",
        7,
    )]));
    let s = shortlist::build(&report, Some(&report), None, true, 10).unwrap();
    assert!(
        triage::plan(&report, &s, report.demand.as_ref().unwrap(), 50, 20, "seed")
            .unwrap()
            .is_empty()
    );
    let mut experiment:triage::Experiment=serde_json::from_value(serde_json::json!({"schema_version":1,"repository":report.repository,"base_sha":report.base_sha,"source_fingerprint":shortlist::fingerprint(&report),"generated_at":"now","agent":"fake","requested_model":null,"policy":"test","cards":[],"batches":[],"planned_batches":0,"attempted_calls":0,"reused_batches":0,"errors":[],"suggested_review_ids":[]})).unwrap();
    assert!(triage::validate_selection(&experiment, &report)
        .unwrap()
        .is_empty());
    experiment
        .suggested_review_ids
        .push(report.features[0].id.clone());
    assert!(triage::validate_selection(&experiment, &report).is_err());
}

#[test]
fn triage_accepts_known_response_version_and_rejects_unsupported_versions() {
    use forkpicker::triage;
    use serde_json::json;
    let card = triage::Card {
        feature_id: "f".into(),
        title: "change".into(),
        sampling: "unknown_exploration".into(),
        patches: vec![],
        commits: vec![json!({"evidence_id":"commit:c"})],
        requests: vec![],
        limitations: vec![],
    };
    let mut raw = json!({"schema_version":1,"assessments":[{"feature_id":"f","summary":{"text":"Observation","evidence":["commit:c"]},"matches":[],"limitations":["Excerpt only"]}]});
    let response: triage::Response = serde_json::from_value(raw.clone()).unwrap();
    assert!(triage::validate(&response, std::slice::from_ref(&card)).is_ok());
    raw["schema_version"] = json!(2);
    let response: triage::Response = serde_json::from_value(raw).unwrap();
    assert!(triage::validate(&response, &[card]).is_err());
}

#[test]
fn code_review_retains_raw_output_before_validation_failure() {
    use std::time::Duration;
    let fixture = Fixture::new();
    fixture.hdr();
    let report = fixture.scan(&["hdr"], &["main"]);
    let pack = review::context(&report, &report.features[0].id, 30_000, None).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("response.json");
    let mut invalid = pack["response_example"].clone();
    invalid["schema_version"] = serde_json::json!(99);
    forkpicker::write_json(&response, &invalid).unwrap();
    let mut profile = review::Config::default().agents["codex"].clone();
    profile.command = vec!["cat".into(), response.to_string_lossy().into_owned()];
    let request = review::Request {
        agent: "fake",
        profile: &profile,
        pack: &pack,
    };
    assert!(request
        .run_saved(&report, Duration::from_secs(5), dir.path())
        .is_err());
    let raw: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            dir.path()
                .join("review-responses")
                .join(format!("{}.json", request.key().unwrap())),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(raw["response"], invalid);
    assert!(raw["input_bytes"].as_u64().unwrap() > 0);
    let mut recoverable = pack["response_example"].clone();
    recoverable["schema_version"] = serde_json::json!(1);
    recoverable["source_coverage"] = pack["source_coverage"].clone();
    recoverable["summary"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"text":"Uncited interpretation"}));
    let recovered = request
        .record(&report, recoverable.clone(), None, 1)
        .unwrap();
    assert_eq!(
        recovered.review.summary.len(),
        pack["response_example"]["summary"]
            .as_array()
            .unwrap()
            .len()
    );
    assert!(recovered
        .review
        .limitations
        .iter()
        .any(|l| l.contains("Uncited model note (not verified)")));
    recoverable["source_coverage"]["files_omitted"] = serde_json::json!(999);
    assert!(request.record(&report, recoverable, None, 1).is_err());
}

#[test]
fn inventory_counts_each_patch_once_and_keeps_missing_or_binary_size_unknown() {
    use forkpicker::metrics::{measure, Policy};
    let f = Fixture::new();
    let sha = f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    let original = measure(&report, &Policy::default()).candidates.remove(0);
    assert_eq!(
        (original.additions, original.deletions, original.files),
        (1, 1, 1)
    );
    assert_eq!(original.change_size, "small"); // One-line fixes have no minimum-size penalty.
    let mut duplicate = report.commits[&sha].clone();
    duplicate.sha = "a".repeat(40);
    report.features[0].commits.push(duplicate.sha.clone());
    report.commits.insert(duplicate.sha.clone(), duplicate);
    let measured = measure(&report, &Policy::default()).candidates.remove(0);
    assert_eq!(measured.changed_lines, original.changed_lines);
    assert_eq!(measured.unique_patches, 1);
    report.features[0].commits.push("missing".into());
    let missing = measure(&report, &Policy::default()).candidates.remove(0);
    assert_eq!(missing.missing_commit_records, 1);
    assert_eq!(
        (
            missing.change_size.as_str(),
            missing.file_spread.as_str(),
            missing.patch_series.as_str()
        ),
        ("unknown", "unknown", "unknown")
    );
    report.features[0].commits = vec![sha.clone()];
    report.commits.get_mut(&sha).unwrap().files[0].binary = true;
    let binary = measure(&report, &Policy::default()).candidates.remove(0);
    assert_eq!(
        (binary.binary_files, binary.change_size.as_str()),
        (1, "unknown")
    );
    report.commits.get_mut(&sha).unwrap().files.clear();
    let empty = measure(&report, &Policy::default()).candidates.remove(0);
    assert_eq!(empty.change_size, "no-file-changes");
    assert_eq!(empty.file_spread, "no-file-changes");
}

#[test]
fn inventory_thresholds_are_inclusive_configurable_and_do_not_reward_test_presence() {
    use forkpicker::metrics::{measure, Policy};
    let f = Fixture::new();
    let sha = f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    for (lines, expected) in [
        (0, "small"),
        (200, "small"),
        (201, "medium"),
        (1000, "medium"),
        (1001, "large"),
    ] {
        let file = &mut report.commits.get_mut(&sha).unwrap().files[0];
        file.additions = lines;
        file.deletions = 0;
        file.is_test = true;
        file.is_documentation = true;
        let facts = measure(&report, &Policy::default()).candidates.remove(0);
        assert_eq!(facts.change_size, expected);
        assert_eq!((facts.test_files, facts.documentation_files), (1, 1));
    }
    let policy: Policy =
        serde_json::from_str(r#"{"small_lines": 1, "large_lines_above": 10}"#).unwrap();
    policy.validate().unwrap();
    assert_eq!(policy.focused_files, 5);
    assert!(Policy {
        small_lines: 2000,
        ..Policy::default()
    }
    .validate()
    .is_err());
    assert!(serde_json::from_str::<Policy>(r#"{"small_line":1}"#).is_err());
    for (files, expected) in [(5, "focused"), (6, "spread"), (20, "spread"), (21, "broad")] {
        let prototype = report.commits[&sha].files[0].clone();
        report.commits.get_mut(&sha).unwrap().files = (0..files)
            .map(|i| FileChange {
                path: format!("dir{i}/f.c"),
                ..prototype.clone()
            })
            .collect();
        let facts = measure(&report, &Policy::default()).candidates.remove(0);
        assert_eq!(facts.file_spread, expected);
        assert_eq!(facts.directories, files);
    }
}

#[test]
fn inventory_preserves_source_drift_ranges_and_unknown_coverage() {
    use forkpicker::metrics::{measure, Policy};
    let f = Fixture::new();
    f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    report.branches[0].behind = 201;
    let mut other = report.branches[0].clone();
    other.source.tip = "b".repeat(40);
    other.source.branch = "other".into();
    other.behind = 0;
    report.features[0].sources.push(other.source.clone());
    report.branches.push(other);
    let facts = measure(&report, &Policy::default()).candidates.remove(0);
    assert_eq!(
        (facts.source_behind_min, facts.source_behind_max),
        (Some(0), Some(201))
    );
    report.branches[1].error = Some("not measured".into());
    let facts = measure(&report, &Policy::default()).candidates.remove(0);
    assert_eq!(facts.source_branches_measured, 1);
    assert_eq!(facts.source_behind_min, Some(201));
}

#[test]
fn factual_order_uses_snapshot_author_dates_and_ignores_legacy_priority() {
    use forkpicker::metrics::{measure, Policy};
    let f = Fixture::new();
    let sha = f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    report.generated_at = "2026-09-09T12:00:00Z".into();
    report.commits.get_mut(&sha).unwrap().date = "2026-09-08T12:00:00Z".into();
    let mut older = report.features[0].clone();
    older.id = "older".into();
    older.score = 1000;
    let mut commit = report.commits[&sha].clone();
    commit.sha = "b".repeat(40);
    commit.date = "2025-01-01T00:00:00Z".into();
    older.commits = vec![commit.sha.clone()];
    report.commits.insert(commit.sha.clone(), commit);
    report.features.push(older);
    assert_eq!(
        render::selected(&report, None, false)[0].id,
        report.features[0].id
    );
    let facts = measure(&report, &Policy::default());
    assert_eq!(facts.candidates[0].author_age_days, Some(1));
    assert_eq!(facts.candidates[1].recency, "historical");
    report.commits.get_mut(&sha).unwrap().date = "2027-01-01T00:00:00Z".into();
    let facts = measure(&report, &Policy::default());
    assert_eq!(facts.candidates[0].feature_id, "older");
    assert_eq!(facts.candidates[1].recency, "future-dated");
    assert_eq!(facts.candidates[1].author_age_days, None);
    let html = render::html(&report, None, false);
    assert!(html.contains("data-size="));
    assert!(!html.contains("data-priority="));
    let pack = llm::context(&report, &report.features[0].id, 100_000).unwrap();
    assert!(pack["feature"].get("score").is_none());
    assert!(pack["feature"].get("reasons").is_none());
    assert!(
        !render::feature_markdown(&report, &report.features[0]).contains("Review recommendation")
    );
}

#[test]
fn histogram_counts_documents_without_votes_repetition_or_unrelated_threads() {
    use forkpicker::metrics::{add_histogram, measure, Policy};
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let mut issue = demand_thread("HDR HDR reference", "open", 0);
    issue.body = "HDR HDR".into();
    let mut second = issue.clone();
    second.url = second.url.replace("42", "43");
    let mut closed = issue.clone();
    closed.state = "closed".into();
    closed.url.push('4');
    let mut pr = issue.clone();
    pr.is_pull_request = true;
    pr.url.push('5');
    let mut other = issue.clone();
    other.url = "https://github.com/another/project/issues/42".into();
    let mut discussion = issue.clone();
    discussion.kind = "discussion".into();
    discussion.url = "https://github.com/upstream/example/discussions/44".into();
    let mut snapshot = demand_snapshot(vec![
        issue.clone(),
        issue,
        second,
        closed,
        pr,
        other,
        discussion,
    ]);
    let mut inv = measure(&report, &Policy::default());
    add_histogram(&mut inv, &report, &snapshot, false);
    assert_eq!(inv.issue_documents, Some(2));
    assert_eq!(inv.issue_histogram["hdr"], 2);
    let before = inv.candidates[0].topic_overlap_score;
    snapshot.threads[0].thumbs_up = Some(100000);
    snapshot.threads[0].body.push_str(" HDR HDR HDR");
    add_histogram(&mut inv, &report, &snapshot, false);
    assert_eq!(inv.candidates[0].topic_overlap_score, before);
    add_histogram(&mut inv, &report, &snapshot, true);
    assert_eq!(inv.issue_documents, Some(3));
}

#[test]
fn inventory_cli_exports_custom_bands_without_overwriting_inputs() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let input = f.cache.path().join("report.json");
    let output = f.cache.path().join("inventory.json");
    let html = f.cache.path().join("inventory.html");
    let policy = f.cache.path().join("policy.json");
    forkpicker::write_json(&input, &report).unwrap();
    std::fs::write(&policy, r#"{"small_lines":0,"large_lines_above":1}"#).unwrap();
    let run = |dest: &std::path::Path, web: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--state-dir",
                f.cache.path().to_str().unwrap(),
                "inventory",
                input.to_str().unwrap(),
                "--policy",
                policy.to_str().unwrap(),
                "--output",
                dest.to_str().unwrap(),
                "--html",
                web.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    };
    let result = run(&output, &html);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let inv: forkpicker::metrics::Inventory =
        serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
    assert_eq!(inv.candidates[0].change_size, "large");
    assert!(std::fs::read_to_string(&html)
        .unwrap()
        .contains("data-size=\"large\""));
    let original = std::fs::read(&input).unwrap();
    assert!(!run(&input, &html).status.success());
    assert!(!run(&output, &output).status.success());
    let alias = f.cache.path().join(".").join("same.json");
    assert!(!run(&f.cache.path().join("same.json"), &alias)
        .status
        .success());
    assert_eq!(std::fs::read(&input).unwrap(), original);
}

#[test]
fn default_review_batch_previews_visible_candidates_without_legacy_scoring() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let input = f.cache.path().join("report.json");
    let config = f.cache.path().join("config.json");
    forkpicker::write_json(&input, &report).unwrap();
    forkpicker::write_json(&config,&serde_json::json!({"agents":{"fixture":{"command":["this-executable-does-not-exist-forkpicker"]}}})).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_forkpicker"))
        .args([
            "--state-dir",
            f.cache.path().to_str().unwrap(),
            "--cache-dir",
            f.cache.path().to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "review",
            input.to_str().unwrap(),
            "--all",
            "--agent",
            "fixture",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("would review"));
    assert!(stderr.contains("author recency"));
    assert!(!stderr.contains("lower-priority"));
    assert!(forkpicker::scan::load_report(&input)
        .unwrap()
        .reviews
        .is_empty());
}

fn integration_fixture(
    f: &Fixture,
    report: &Report,
    target: Option<&str>,
) -> forkpicker::metrics::Inventory {
    let mut inventory =
        forkpicker::metrics::measure(report, &forkpicker::metrics::Policy::default());
    forkpicker::integration::run(
        report,
        &mut inventory,
        &Git::new(f.dir.path()),
        f.cache.path(),
        target,
        2,
    )
    .unwrap();
    inventory
}

#[test]
fn integration_ignores_unrelated_upstream_changes_and_reuses_exact_target_cache() {
    let f = Fixture::new();
    f.hdr();
    f.git(&["checkout", "main"]);
    f.write("unrelated.txt", "upstream activity\n");
    f.commit("Unrelated change");
    let mut report = f.scan(&["hdr"], &["main"]);
    report.branches[0].behind = 2000; // History volume cannot change actual applicability.
    let first = integration_fixture(&f, &report, None);
    let check = first.candidates[0].integration.as_ref().unwrap();
    assert_eq!(check.status, "clean");
    assert_eq!(check.overlap[0].changed_upstream.len(), 0);
    assert!(!check.cached);
    let again = integration_fixture(&f, &report, None);
    assert_eq!(again.integration.unwrap().cache_hits, 1);
    f.write("src/render.c", "int white = 999;\nint peak = 1000;\n");
    f.commit("Upstream changes same line");
    let changed = integration_fixture(&f, &report, Some("main"));
    let check = changed.candidates[0].integration.as_ref().unwrap();
    assert!(!check.cached);
    assert_eq!(check.status, "conflicts");
    assert_eq!(check.conflicting_files, vec!["src/render.c"]);
    assert_eq!(check.overlap[0].changed_upstream, vec!["src/render.c"]);
    assert_ne!(check.target_sha, report.base_sha);
}

#[test]
fn integration_three_way_recovers_changed_context_without_claiming_a_conflict() {
    let f = Fixture::new();
    let lines: Vec<_> = (0..30).map(|i| format!("line {i}\n")).collect();
    f.write("src/render.c", &lines.concat());
    f.commit("Long base");
    f.git(&["checkout", "-qb", "feature"]);
    let mut fork = lines.clone();
    fork[10] = "fork change\n".into();
    f.write("src/render.c", &fork.concat());
    f.commit("Feature");
    f.git(&["checkout", "main"]);
    let mut upstream = lines;
    upstream[7] = "upstream change\n".into();
    f.write("src/render.c", &upstream.concat());
    f.commit("Neighboring upstream edit");
    let report = f.scan(&["feature"], &["main"]);
    let inventory = integration_fixture(&f, &report, None);
    let check = inventory.candidates[0].integration.as_ref().unwrap();
    assert_eq!(check.status, "clean-three-way", "{:?}", check.notes);
    assert_eq!(check.overlap_max(), Some(1));
    assert!(check.conflicting_files.is_empty());
}

#[test]
fn integration_reports_deletions_and_approximate_renames() {
    let f = Fixture::new();
    f.hdr();
    f.git(&["checkout", "main"]);
    f.git(&["mv", "src/render.c", "src/display.c"]);
    f.commit("Rename upstream");
    let report = f.scan(&["hdr"], &["main"]);
    let inventory = integration_fixture(&f, &report, None);
    let check = inventory.candidates[0].integration.as_ref().unwrap();
    assert_eq!(
        check.overlap[0].renamed_upstream,
        vec![("src/render.c".into(), "src/display.c".into())]
    );
    assert_eq!(check.overlap[0].changed_upstream, vec!["src/render.c"]);
    assert_ne!(check.status, "clean");
    f.git(&["rm", "src/display.c"]);
    f.commit("Delete upstream");
    let inventory = integration_fixture(&f, &report, Some("main"));
    assert_eq!(
        inventory.candidates[0]
            .integration
            .as_ref()
            .unwrap()
            .overlap[0]
            .deleted_upstream,
        vec!["src/render.c"]
    );
}

#[test]
fn integration_preserves_worktree_and_never_uses_configured_merge_drivers() {
    let f = Fixture::new();
    f.write(".gitattributes", "*.c merge=evil\n");
    f.commit("Project merge attributes");
    let marker = f.cache.path().join("driver-ran");
    f.git(&[
        "config",
        "merge.evil.driver",
        &format!("touch {}", marker.display()),
    ]);
    f.git(&["config", "core.hooksPath", f.cache.path().to_str().unwrap()]);
    f.hdr();
    f.git(&["checkout", "main"]);
    f.write("src/render.c", "upstream conflict\n");
    f.commit("Conflicting upstream");
    let report = f.scan(&["hdr"], &["main"]);
    f.write("src/render.c", "uncommitted user edits\n");
    let before = f.git(&["status", "--porcelain"]);
    let index = std::fs::read(f.dir.path().join(".git/index")).unwrap();
    let inventory = integration_fixture(&f, &report, None);
    assert_eq!(
        inventory.candidates[0].integration.as_ref().unwrap().status,
        "conflicts"
    );
    assert!(!marker.exists());
    assert_eq!(
        std::fs::read_to_string(f.dir.path().join("src/render.c")).unwrap(),
        "uncommitted user edits\n"
    );
    assert_eq!(
        std::fs::read(f.dir.path().join(".git/index")).unwrap(),
        index
    );
    assert_eq!(f.git(&["status", "--porcelain"]), before);
}

#[test]
fn integration_uses_full_objects_and_keeps_missing_evidence_unknown() {
    let f = Fixture::new();
    let sha = f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    report.commits.get_mut(&sha).unwrap().patch = "truncated nonsense".into();
    report.commits.get_mut(&sha).unwrap().patch_truncated = true;
    let inventory = integration_fixture(&f, &report, None);
    assert_eq!(
        inventory.candidates[0].integration.as_ref().unwrap().status,
        "clean"
    );
    let missing = "0".repeat(40);
    report.features[0].commits = vec![missing.clone()];
    let mut c = report.commits[&sha].clone();
    c.sha = missing.clone();
    report.commits.insert(missing, c);
    let inventory = integration_fixture(&f, &report, None);
    let check = inventory.candidates[0].integration.as_ref().unwrap();
    assert_eq!(check.status, "unknown");
    assert!(check
        .notes
        .iter()
        .any(|n| n.contains("missing candidate Git objects")));
    assert!(!check.cached);
}

#[test]
fn integration_replays_series_in_ancestry_order_and_does_not_add_prerequisites() {
    let f = Fixture::new();
    f.git(&["checkout", "-qb", "feature"]);
    f.write("src/new.c", "initial\n");
    let first = f.commit("New subsystem");
    f.write("src/new.c", "feature behavior\n");
    let second = f.commit("Implement behavior");
    let mut report = f.scan(&["feature"], &["main"]);
    let mut candidate = report.features[0].clone();
    candidate.commits = vec![second.clone()];
    candidate.patch_ids = vec![report.commits[&second].patch_id.clone().unwrap()];
    candidate.context_commits = vec![first.clone()];
    report.features = vec![candidate];
    let missing = integration_fixture(&f, &report, None);
    let check = missing.candidates[0].integration.as_ref().unwrap();
    assert_eq!(check.status, "not-applicable");
    assert_eq!(check.possible_prerequisites, vec![first.clone()]);
    assert!(check
        .notes
        .iter()
        .any(|n| n.contains("omitted prerequisites")));
    report.features[0].commits = vec![second.clone(), first.clone()];
    let complete = integration_fixture(&f, &report, None);
    let check = complete.candidates[0].integration.as_ref().unwrap();
    assert_eq!(check.status, "clean");
    assert_eq!(check.ordered_commits, vec![first, second]);
}

#[test]
fn integration_cli_emits_evidence_and_replaces_drift_cutoffs() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let input = f.cache.path().join("report.json");
    let output = f.cache.path().join("inventory.json");
    let html = f.cache.path().join("inventory.html");
    forkpicker::write_json(&input, &report).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_forkpicker"))
        .args([
            "--cache-dir",
            f.cache.path().to_str().unwrap(),
            "--state-dir",
            f.cache.path().to_str().unwrap(),
            "inventory",
            input.to_str().unwrap(),
            "--check-apply",
            "--repo",
            f.dir.path().to_str().unwrap(),
            "--jobs",
            "2",
            "--output",
            output.to_str().unwrap(),
            "--html",
            html.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let inv: forkpicker::metrics::Inventory =
        serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(
        inv.candidates[0].integration.as_ref().unwrap().status,
        "clean"
    );
    let html = std::fs::read_to_string(html).unwrap();
    assert!(html.contains("data-application=\"clean\""));
    assert!(!html.contains("data-drift="));
    assert!(!html.contains("near_behind"));
}

#[test]
fn integration_deduplicates_rebased_copies_and_rejects_incompatible_series() {
    let f = Fixture::new();
    let original = f.hdr();
    f.git(&["checkout", "main"]);
    f.write("unrelated.txt", "upstream\n");
    f.commit("Advance upstream");
    f.git(&["checkout", "-qb", "copy"]);
    f.git(&["cherry-pick", &original]);
    let report = f.scan(&["hdr", "copy"], &["main"]);
    assert_eq!(report.features.len(), 1);
    assert_eq!(report.features[0].commits.len(), 2);
    let inventory = integration_fixture(&f, &report, None);
    let check = inventory.candidates[0].integration.as_ref().unwrap();
    assert_eq!(check.status, "clean");
    assert_eq!(check.ordered_commits.len(), 1);
    f.git(&["checkout", "main"]);
    f.git(&["checkout", "-qb", "other"]);
    f.write("src/parser.c", "int parse = 42;\n");
    f.commit("Separate history");
    let mut report = f.scan(&["hdr", "other"], &["main"]);
    assert_eq!(report.features.len(), 2);
    let other = report.features.pop().unwrap();
    report.features[0].commits.extend(other.commits);
    report.features[0].patch_ids.extend(other.patch_ids);
    report.features[0].sources.extend(other.sources);
    let inventory = integration_fixture(&f, &report, None);
    let check = inventory.candidates[0].integration.as_ref().unwrap();
    assert_eq!(check.status, "unknown");
    assert!(check
        .notes
        .iter()
        .any(|n| n.contains("incompatible histories")));
}

#[test]
fn inbox_groups_contained_patch_sets_without_transitive_topic_clusters() {
    let f = Fixture::new();
    f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    let prototype = report.features[0].clone();
    report.features = [
        ("a", vec!["p", "q"]),
        ("b", vec!["q", "r"]),
        ("c", vec!["p"]),
        ("d", vec!["z"]),
    ]
    .into_iter()
    .map(|(id, patches)| Feature {
        id: id.into(),
        patch_ids: patches.into_iter().map(str::to_owned).collect(),
        ..prototype.clone()
    })
    .collect();
    let inv = forkpicker::metrics::measure(&report, &forkpicker::metrics::Policy::default());
    let family = |id: &str| {
        inv.candidates
            .iter()
            .find(|c| c.feature_id == id)
            .unwrap()
            .inbox
            .family
            .clone()
    };
    assert_eq!(family("a"), family("c"));
    assert_ne!(family("a"), family("b"));
    assert_ne!(family("a"), family("d"));
}

#[test]
fn inbox_baseline_distinguishes_observation_from_creation_and_rejects_wrong_scope() {
    let f = Fixture::new();
    f.hdr();
    let mut prior = f.scan(&["hdr"], &["main"]);
    prior.generated_at = "2026-09-08T00:00:00Z".into();
    prior.features[0].patch_ids = vec!["p".into(), "q".into()];
    let mut current = prior.clone();
    current.generated_at = "2026-09-09T00:00:00Z".into();
    let prototype = current.features[0].clone();
    current.features = [
        ("same", vec!["p", "q"]),
        ("extended", vec!["p", "q", "r"]),
        ("regrouped", vec!["p"]),
        ("new", vec!["z"]),
    ]
    .into_iter()
    .map(|(id, p)| Feature {
        id: id.into(),
        patch_ids: p.into_iter().map(str::to_owned).collect(),
        ..prototype.clone()
    })
    .collect();
    let mut inv = forkpicker::metrics::measure(&current, &forkpicker::metrics::Policy::default());
    forkpicker::inbox::compare(&current, &mut inv, &prior).unwrap();
    for (id, expected) in [
        ("same", "unchanged"),
        ("extended", "extended"),
        ("regrouped", "regrouped"),
        ("new", "new"),
    ] {
        assert_eq!(
            inv.candidates
                .iter()
                .find(|f| f.feature_id == id)
                .unwrap()
                .inbox
                .observation,
            expected
        );
    }
    assert!(inv
        .baseline
        .as_ref()
        .unwrap()
        .note
        .contains("coverage gaps"));
    prior.repository = "wrong/repository".into();
    assert!(forkpicker::inbox::compare(&current, &mut inv, &prior).is_err());
    prior.repository = current.repository.clone();
    prior.generated_at = "2026-09-10T00:00:00Z".into();
    assert!(forkpicker::inbox::compare(&current, &mut inv, &prior).is_err());
}

#[test]
fn inbox_explains_reference_scope_and_hides_retrieval_scores_from_html() {
    let f = Fixture::new();
    let sha = f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    report
        .commits
        .get_mut(&sha)
        .unwrap()
        .message
        .push_str("\nSee #42");
    let issue = demand_thread("HDR reference white", "open", 1);
    report.issues = vec![issue.clone()];
    let no_catalog = forkpicker::metrics::measure(&report, &forkpicker::metrics::Policy::default());
    assert_eq!(no_catalog.candidates[0].inbox.context[0].kind, "explicit");
    let mut discussion = issue.clone();
    discussion.kind = "discussion".into();
    discussion.url = "https://github.com/upstream/example/discussions/42".into();
    let mut other = issue.clone();
    other.url = "https://github.com/elsewhere/project/issues/42".into();
    let catalog = demand_snapshot(vec![issue, discussion, other]);
    let mut inv = forkpicker::metrics::measure(&report, &forkpicker::metrics::Policy::default());
    forkpicker::metrics::add_histogram(&mut inv, &report, &catalog, false);
    let links = &inv.candidates[0].inbox.context;
    assert_eq!(links.iter().filter(|l| l.kind == "explicit").count(), 1);
    assert!(links
        .iter()
        .any(|l| l.url.contains("/discussions/") && l.kind == "text"));
    assert!(!links.iter().any(|l| l.url.contains("elsewhere")));
    inv.report_path = Some("/tmp/</script><script>alert(9)</script>.json".into());
    let html = render::html_with_inventory(&report, None, false, &inv);
    assert!(!html.contains("Vocabulary overlap"));
    assert!(!html.contains("topic_overlap_score"));
    assert!(html.contains("Shared title words"));
    assert!(!html.contains("<script>alert(9)</script>"));
    assert!(html.find("<article ").unwrap() < html.find("class=\"diagnostics\"").unwrap());
    assert!(html.contains("--dry-run"));
}

#[test]
fn inventory_baseline_cli_preserves_inputs_and_embeds_command_source_path() {
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let input = f.cache.path().join("report.json");
    let output = f.cache.path().join("inventory.json");
    forkpicker::write_json(&input, &report).unwrap();
    let before = std::fs::read(&input).unwrap();
    let run = |dest: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--state-dir",
                f.cache.path().to_str().unwrap(),
                "inventory",
                input.to_str().unwrap(),
                "--baseline",
                input.to_str().unwrap(),
                "--output",
                dest.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    };
    let result = run(&output);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let inv: forkpicker::metrics::Inventory =
        serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(inv.candidates[0].inbox.observation, "unchanged");
    assert_eq!(
        inv.report_path.unwrap(),
        input.canonicalize().unwrap().to_string_lossy()
    );
    assert!(!run(&input).status.success());
    assert_eq!(std::fs::read(input).unwrap(), before);
}

#[test]
fn fork_ranking_deduplicates_branches_and_contained_variants_within_each_fork() {
    let f = Fixture::new();
    f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    let checked = integration_fixture(&f, &report, None);
    let check = checked.candidates[0].integration.clone().unwrap();
    let prototype = report.features[0].clone();
    report.branches.clear();
    report.features = [
        ("a", vec!["p", "q"], "owner/one", "conflicts"),
        ("b", vec!["p"], "owner/one", "clean"),
        ("c", vec!["q", "r"], "owner/one", "not-applicable"),
        ("d", vec!["s"], "owner/one", "unknown"),
        ("e", vec!["p"], "owner/two", "clean-three-way"),
        ("f", vec!["q"], "owner/two", "conflicts"),
        ("g", vec![], "owner/empty", "no-file-changes"),
        ("h", vec!["h"], "owner/unchecked", "unknown"),
    ]
    .into_iter()
    .map(|(id, patches, repo, _)| {
        let mut source = prototype.sources[0].clone();
        source.repository = repo.into();
        let mut alias = source.clone();
        alias.branch = "another-branch".into();
        Feature {
            id: id.into(),
            patch_ids: patches.into_iter().map(str::to_owned).collect(),
            sources: vec![source, alias],
            ..prototype.clone()
        }
    })
    .collect();
    let mut inv = forkpicker::metrics::measure(&report, &Default::default());
    for fact in &mut inv.candidates {
        let status = match fact.feature_id.as_str() {
            "a" | "f" => "conflicts",
            "b" => "clean",
            "c" => "not-applicable",
            "e" => "clean-three-way",
            "g" => "no-file-changes",
            _ => "unknown",
        };
        fact.integration = Some(forkpicker::integration::Check {
            status: status.into(),
            ..check.clone()
        });
    }
    let forks = forkpicker::forks::summarize(&report, &inv);
    let one = forks.iter().find(|f| f.repository == "owner/one").unwrap();
    assert_eq!(
        (
            one.groups.len(),
            one.clean_groups,
            one.blocked_groups,
            one.unknown_groups
        ),
        (3, 1, 1, 1)
    );
    assert_eq!(one.unique_patches, 4);
    assert_eq!(one.candidate_ids.len(), 4);
    assert_eq!(one.confirmed_clean_fraction, Some(1.0 / 3.0));
    let two = forks.iter().find(|f| f.repository == "owner/two").unwrap();
    // A larger anchor in another fork cannot join disjoint subsets in this fork.
    assert_eq!(
        (two.groups.len(), two.clean_groups, two.blocked_groups),
        (2, 1, 1)
    );
    assert!(two.inspection_priority > one.inspection_priority);
    for name in ["owner/empty", "owner/unchecked"] {
        assert_eq!(
            forks
                .iter()
                .find(|f| f.repository == name)
                .unwrap()
                .inspection_priority,
            None
        );
    }
    // A failure plus an unknown variant is not an entirely blocked group.
    inv.candidates
        .iter_mut()
        .find(|f| f.feature_id == "b")
        .unwrap()
        .integration
        .as_mut()
        .unwrap()
        .status = "unknown".into();
    let forks = forkpicker::forks::summarize(&report, &inv);
    let one = forks.iter().find(|f| f.repository == "owner/one").unwrap();
    assert_eq!(
        (one.clean_groups, one.blocked_groups, one.unknown_groups),
        (0, 1, 2)
    );
    // Results for another target never contribute to the score.
    for fact in &mut inv.candidates {
        fact.integration.as_mut().unwrap().target_sha = "different-target".into();
    }
    assert!(forkpicker::forks::summarize(&report, &inv)
        .iter()
        .all(|f| f.inspection_priority.is_none()));
}

#[test]
fn related_exploration_retrieves_only_same_fork_evidence_and_resumes_each_stage() {
    use serde_json::json;
    let f = Fixture::new();
    f.hdr();
    f.git(&["checkout", "main"]);
    f.git(&["checkout", "-qb", "parser"]);
    f.write("src/parser.c", "int parse = 1;\n");
    f.commit("parser: reject malformed expressions");
    let mut report = f.scan(&["hdr", "parser"], &["main"]);
    let seed = report.features[0].id.clone();
    let mut foreign = report.features[1].clone();
    foreign.id = "foreign-candidate".into();
    for s in &mut foreign.sources {
        s.repository = "foreign/example".into();
    }
    report.features.push(foreign);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let rp = root.join("report.json");
    let output = root.join("result.json");
    let cp = root.join("config.json");
    let script = root.join("agent.py");
    let marker = root.join("calls");
    forkpicker::write_json(&rp, &report).unwrap();
    std::fs::write(&script,r#"import sys,json,pathlib
+p=json.load(sys.stdin)
+schema=json.loads(pathlib.Path(sys.argv[sys.argv.index('--output-schema')+1]).read_text())
+assert schema==p['response_schema']
+pathlib.Path(sys.argv[1]).open('a').write('call\n')
+if 'catalog' in p:
+ assert len(p['catalog'])==1 and p['catalog'][0]['candidate_id']!='foreign-candidate'
+ answer={'inspect':[{'candidate_id':p['catalog'][0]['candidate_id'],'seed_id':p['seeds'][0]['candidate_id'],'reason':'Check supporting behavior'}],'limitations':[]}
+ if 'issue_context' in p: answer['issue_queries']=['chromaticity baseline']
+else:
+ assert p['scope']=='selection' and len(p['candidates'])==2
+ assert all(c['commits'] for c in p['candidates'])
+ seed=next(c for c in p['candidates'] if c['candidate_id'] in p['seed_ids'])
+ other=next(c for c in p['candidates'] if c!=seed)
+ claim={'text':'Observed seed','evidence':seed['evidence_ids']}
+ answer={'schema_version':1,'scope':'selection','headline':'Describe seed behavior','summary':claim,'groups':[{'name':'Seed feature','summary':claim,'members':[{'candidate_id':seed['candidate_id'],'role':'implementation'}]}],'relationships':[],'unclassified':[{'candidate_id':other['candidate_id'],'reason':'Actual patch is unrelated'}],'limitations':[]}
+ answer['usefulness']=[{'candidate_id':c['candidate_id'],'verdict':'uncertain','reason':{'text':'Needs context','evidence':c['evidence_ids']}} for c in p['candidates']]
+if 'issue_context' in p and 'catalog' not in p:
+ assert p['issue_context']['threads'][0]['title']=='Chromaticity baseline'
+ assert 'issue_queries' not in p['response_schema']['properties']
+ answer['summary']['evidence'].append(p['issue_context']['threads'][0]['evidence_id'])
+print(json.dumps(answer))
+"#.replace("\n+","\n")).unwrap();
    forkpicker::write_json(
        &cp,
        &json!({"agents":{"codex":{"command":["python3",script,marker]}}}),
    )
    .unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--config",
                cp.to_str().unwrap(),
                "--cache-dir",
                root.join("cache").to_str().unwrap(),
                "classify",
                rp.to_str().unwrap(),
                "--agent",
                "codex",
                "--explore-related",
                "--candidate",
                &seed,
                "--related-limit",
                "1",
                "--output",
                output.to_str().unwrap(),
            ])
            .args(extra)
            .output()
            .unwrap()
    };
    assert!(run(&["--dry-run"]).status.success());
    assert!(!marker.exists());
    let first = run(&["--limit", "1"]);
    assert_eq!(
        first.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let read = || {
        serde_json::from_slice::<forkpicker::related::Experiment>(&std::fs::read(&output).unwrap())
            .unwrap()
    };
    assert_eq!(read().exploration[0].requested.len(), 1);
    assert!(read().exploration[0].classification.is_none());
    let second = run(&["--limit", "1"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let result = read();
    assert_eq!(result.run.attempted_calls, 1);
    assert_eq!(result.run.reused_calls, 1);
    assert_eq!(
        result.run.forks[0]
            .classification
            .as_ref()
            .unwrap()
            .unclassified
            .len(),
        1
    );
    assert!(run(&["--limit", "0"]).status.success());
    assert_eq!(read().run.attempted_calls, 0);
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 2);
    let issue_path = root.join("issues.json");
    forkpicker::write_json(&issue_path,&json!({"fetched_at":"now","query":null,"priority_labels":[],"warnings":[],"threads":[{"number":7,"title":"Chromaticity baseline","body":"Unexpected chromaticity baseline under particular conditions","url":"https://github.com/upstream/example/issues/7","state":"open","is_pull_request":false,"kind":"issue","match_kind":""}]})).unwrap();
    let issue_run = run(&[
        "--issue-cache",
        issue_path.to_str().unwrap(),
        "--limit",
        "2",
    ]);
    assert!(
        issue_run.status.success(),
        "{}",
        String::from_utf8_lossy(&issue_run.stderr)
    );
    let issue_result = read();
    assert_eq!(
        issue_result.exploration[0].issue_queries,
        vec!["chromaticity baseline"]
    );
    assert_eq!(
        issue_result.exploration[0].issue_context.as_ref().unwrap()["threads"][0]["title"],
        "Chromaticity baseline"
    );
    assert!(issue_result.run.forks[0]
        .classification
        .as_ref()
        .unwrap()
        .summary
        .evidence
        .iter()
        .any(|id| id.starts_with("request:")));
    assert!(run(&[
        "--issue-cache",
        issue_path.to_str().unwrap(),
        "--limit",
        "0"
    ])
    .status
    .success());
    assert_eq!(read().run.attempted_calls, 0);
    // A rejected native response remains inspectable, but is never a valid cache hit.
    let source = std::fs::read_to_string(&script).unwrap();
    std::fs::write(
        &script,
        source.replace(
            "print(json.dumps(answer))",
            "if 'catalog' not in p: answer['unclassified']=[]\nprint(json.dumps(answer))",
        ),
    )
    .unwrap();
    assert_eq!(
        run(&["--model", "rejection-test", "--limit", "2"])
            .status
            .code(),
        Some(2)
    );
    assert!(read().exploration[0].classification.is_none());
    assert!(read().run.errors[0].contains("response saved to"));
    let files: Vec<_> = std::fs::read_dir(root.join("cache/related-rejected"))
        .unwrap()
        .collect();
    assert_eq!(files.len(), 1);
    let rejected: forkpicker::related::Record =
        serde_json::from_slice(&std::fs::read(files[0].as_ref().unwrap().path()).unwrap()).unwrap();
    assert!(rejected.value["unclassified"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(!root
        .join("cache/related")
        .join(format!("{}.json", rejected.key))
        .exists());
}

#[test]
fn discovery_advances_unseen_work_with_budgeted_cached_inspection_and_offline_html() {
    use forkpicker::discovery::Discovery;
    use serde_json::json;
    let f = Fixture::new();
    f.hdr();
    f.git(&["checkout", "main"]);
    f.git(&["checkout", "-qb", "parser"]);
    f.write("src/parser.c", "int parse = 1;\n");
    f.commit("parser: reject malformed expressions");
    let mut report = f.scan(&["hdr", "parser"], &["main"]);
    let mut duplicate = report.features[0].clone();
    duplicate.id = "equivalent-copy".into();
    report.features.push(duplicate);
    let root = f.cache.path();
    let rp = root.join("report.json");
    let op = root.join("discovery.json");
    let hp = root.join("discovery.html");
    let cp = root.join("config.json");
    let script = root.join("model.py");
    let marker = root.join("calls");
    let failure = root.join("fail");
    forkpicker::write_json(&rp, &report).unwrap();
    std::fs::write(&script,r#"import sys,json,pathlib
p=json.load(sys.stdin)
assert json.loads(pathlib.Path(sys.argv[sys.argv.index('--output-schema')+1]).read_text())==p['response_schema']
pathlib.Path(sys.argv[1]).open('a').write(p.get('stage','inspect')+'\n')
if p.get('stage')=='map':
 c=p['candidates'][0]
 answer={'groups':[{'headline':'Improve behavior','candidate_ids':[c['candidate_id']],'basis':'plausible_feature','benefit':'A potential capability','question':'Does the code implement it?','files_to_inspect':c['paths'][:1],'issue_queries':[]}],'deferred_ids':[x['candidate_id'] for x in p['candidates'][1:]]}
 if pathlib.Path(sys.argv[2]).exists(): answer['groups'][0]['candidate_ids']=['foreign-id']
else:
 answer={'schema_version':1,'scope':p['scope'],'headline':'Improve behavior','summary':{'text':'Observed behavior','evidence':p['candidates'][0]['evidence_ids']},'groups':[],'relationships':[],'unclassified':[],'usefulness':[],'limitations':['No tests executed']}
 for c in p['candidates']:
  claim={'text':'Observed code change','evidence':c['evidence_ids']}
  answer['groups'].append({'name':'Change '+c['candidate_id'],'summary':claim,'members':[{'candidate_id':c['candidate_id'],'role':'implementation'}]})
  answer['usefulness'].append({'candidate_id':c['candidate_id'],'verdict':'useful','reason':claim})
print(json.dumps(answer))
"#).unwrap();
    forkpicker::write_json(
        &cp,
        &json!({"agents":{"codex":{"command":["python3",script,marker,failure]}}}),
    )
    .unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--config",
                cp.to_str().unwrap(),
                "--cache-dir",
                root.join("model-cache").to_str().unwrap(),
                "classify",
                rp.to_str().unwrap(),
                "--agent",
                "codex",
                "--discover",
                "--map-calls",
                "1",
                "--html",
                hp.to_str().unwrap(),
            ])
            .args(if extra.contains(&"--screen-size") {
                &[][..]
            } else {
                &["--screen-size", "1"][..]
            })
            .args(if extra.contains(&"--output") {
                vec![]
            } else {
                vec!["--output", op.to_str().unwrap()]
            })
            .args(extra)
            .output()
            .unwrap()
    };
    let read = || serde_json::from_slice::<Discovery>(&std::fs::read(&op).unwrap()).unwrap();
    let ok = |r: std::process::Output| {
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr))
    };
    ok(run(&["--dry-run"]));
    assert!(!marker.exists());
    assert_eq!(read().call_limit, 5);
    assert_eq!(read().entries.len(), 2);
    ok(run(&["--limit", "2"]));
    let a = read();
    assert_eq!(a.run.attempted_calls, 2);
    assert_eq!(a.screened().len(), 1);
    assert_eq!(a.inspected().len(), 1);
    assert!(a.new_input_bytes <= a.input_limit);
    ok(run(&["--limit", "2"]));
    let b = read();
    assert_eq!(b.run.attempted_calls, 2);
    assert_eq!(b.screened().len(), 2);
    assert_eq!(b.inspected().len(), 2);
    assert_eq!(b.run.reused_calls, 2);
    ok(run(&["--limit", "0"]));
    assert_eq!(read().run.attempted_calls, 0);
    assert_eq!(read().inspected().len(), 2);
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 4);
    let html = std::fs::read_to_string(&hp).unwrap();
    assert!(html.contains("hashchange"));
    assert!(html.contains("Read inspected diff"));
    assert!(html.contains("Metadata hypothesis"));
    let rendered = root.join("rerender.html");
    let result = Command::new(env!("CARGO_BIN_EXE_forkpicker"))
        .args([
            "report",
            rp.to_str().unwrap(),
            "--classification",
            op.to_str().unwrap(),
            "--format",
            "html",
            "--output",
            rendered.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    ok(result);
    assert_eq!(
        std::fs::read(&hp).unwrap(),
        std::fs::read(&rendered).unwrap()
    );
    // Carry forward a saved inbox even when the new batch window changes.
    let prior = root.join("prior-discovery.json");
    std::fs::copy(&op, &prior).unwrap();
    ok(run(&[
        "--resume-discovery",
        prior.to_str().unwrap(),
        "--screen-size",
        "200",
        "--limit",
        "0",
    ]));
    assert_eq!(read().screened().len(), 2);
    assert_eq!(read().inspected().len(), 2);
    assert_eq!(read().run.reused_calls, 4);
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 4);
    assert!(!run(&[
        "--resume-discovery",
        prior.to_str().unwrap(),
        "--output",
        prior.to_str().unwrap()
    ])
    .status
    .success());
    let mut foreign: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&prior).unwrap()).unwrap();
    foreign["base_sha"] = json!("foreign");
    forkpicker::write_json(&prior, &foreign).unwrap();
    assert!(!run(&["--resume-discovery", prior.to_str().unwrap()])
        .status
        .success());
    // Input cap prevents spending; cached results remain independent of that cap.
    ok(run(&["--model", "fresh", "--total-bytes", "8000"]));
    assert_eq!(read().run.attempted_calls, 0);
    std::fs::write(&failure, "fail").unwrap();
    let bad = run(&["--model", "fresh"]);
    assert_eq!(bad.status.code(), Some(2));
    assert_eq!(read().run.attempted_calls, 1);
    assert!(read().mappings.is_empty());
    // Independent experiments ignore existing model records and don't mutate them.
    std::fs::remove_file(&failure).unwrap();
    let global_cache = root.join("model-cache/discovery");
    let before: std::collections::BTreeMap<_, _> = std::fs::read_dir(&global_cache)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.path(), e.metadata().unwrap().modified().unwrap())
        })
        .collect();
    let calls_before = std::fs::read_to_string(&marker).unwrap().lines().count();
    for _ in 0..2 {
        ok(run(&["--fresh", "--limit", "2"]));
        assert!(read().fresh);
        assert_eq!(read().run.reused_calls, 0);
        assert_eq!(read().run.attempted_calls, 2);
        assert_eq!(read().mappings.len(), 1);
    }
    let after: std::collections::BTreeMap<_, _> = std::fs::read_dir(&global_cache)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.path(), e.metadata().unwrap().modified().unwrap())
        })
        .collect();
    assert_eq!(before, after);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().lines().count(),
        calls_before + 4
    );
    assert!(op.with_extension("responses").exists());
    let calls_before_continuation = std::fs::read_to_string(&marker).unwrap().lines().count();
    ok(run(&["--fresh", "--continue-run", "--limit", "2"]));
    assert_eq!(
        read().run.attempted_calls,
        2,
        "continuation preserves spent budget"
    );
    assert_eq!(
        read().run.reused_calls,
        0,
        "no global model cache during a fresh experiment"
    );
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().lines().count(),
        calls_before_continuation
    );
    assert!(
        !run(&["--fresh", "--continue-run", "--limit", "3"])
            .status
            .success(),
        "cannot silently reset the original budget"
    );
    // Extend a map-only experiment, then inspect its existing nomination exactly once.
    ok(run(&["--fresh", "--limit", "1"]));
    let before_extension = read();
    let calls_before_extension = std::fs::read_to_string(&marker).unwrap().lines().count();
    ok(run(&[
        "--fresh",
        "--continue-run",
        "--extend-budget",
        "--limit",
        "2",
    ]));
    let extended = read();
    assert_eq!(extended.run.generated_at, before_extension.run.generated_at);
    assert_eq!(extended.run.attempted_calls, 2);
    assert_eq!(extended.run.reused_calls, 0);
    assert_eq!(extended.mappings[0].key, before_extension.mappings[0].key);
    assert_eq!(extended.inspections.len(), 1);
    assert_eq!(extended.budget_extensions.len(), 1);
    assert_eq!(extended.budget_extensions[0].calls_spent, 1);
    assert_eq!(extended.budget_extensions[0].previous_call_limit, 1);
    assert_eq!(extended.budget_extensions[0].call_limit, 2);
    assert_eq!(
        extended.new_input_bytes,
        before_extension.new_input_bytes + extended.inspections[0].input_bytes
    );
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().lines().count(),
        calls_before_extension + 1
    );
    ok(run(&["--fresh", "--continue-run", "--limit", "2"]));
    assert_eq!(read().budget_extensions.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().lines().count(),
        calls_before_extension + 1
    );
    assert!(!run(&[
        "--fresh",
        "--continue-run",
        "--extend-budget",
        "--limit",
        "1"
    ])
    .status
    .success());
    let before_reinspection = read();
    ok(run(&[
        "--fresh",
        "--continue-run",
        "--reinspect",
        "--limit",
        "2",
    ]));
    let reinspected = read();
    assert_eq!(reinspected.superseded_inspections.len(), 1);
    assert_eq!(
        reinspected.superseded_inspections[0].key,
        before_reinspection.inspections[0].key
    );
    assert_eq!(
        reinspected.mappings[0].key,
        before_reinspection.mappings[0].key
    );
    assert_eq!(
        reinspected.run.attempted_calls,
        before_reinspection.run.attempted_calls
    );
    assert_eq!(
        reinspected.new_input_bytes,
        before_reinspection.new_input_bytes
    );
    assert_eq!(
        reinspected.inspections.len(),
        1,
        "identical full-diff input can retain this experiment's response without another call"
    );
    ok(run(&["--fresh", "--continue-run", "--limit", "2"]));
    assert_eq!(read().superseded_inspections.len(), 1);
    assert!(
        !run(&["--fresh", "--resume-discovery", prior.to_str().unwrap()])
            .status
            .success()
    );
}

#[test]
fn release_window_uses_original_patch_dates_and_preserves_whole_candidates() {
    use forkpicker::{
        discovery::Entry,
        discovery_window::{self, Scope},
    };
    use serde_json::json;
    let f = Fixture::new();
    f.hdr();
    let mut report = f.scan(&["hdr"], &["main"]);
    report.generated_at = "2026-09-09T12:00:00Z".into();
    let template = report.features[0].clone();
    let commit = report.commits.values().next().unwrap().clone();
    report.features.clear();
    report.commits.clear();
    for (sha, patch, date) in [
        ("old", "p-old", "2026-01-01T00:00:00Z"),
        ("copy", "p-old", "2026-09-08T00:00:00Z"),
        ("new", "p-new", "2026-09-07T00:00:00Z"),
        ("boundary", "p-boundary", "2026-07-05T10:14:01-04:00"),
        ("future", "p-future", "2027-01-01T00:00:00Z"),
        ("unknown", "p-unknown", "invalid"),
        ("merge", "p-merge", "2026-09-09T00:00:00Z"),
    ] {
        let mut c = commit.clone();
        c.sha = forkpicker::hash(sha);
        c.patch_id = Some(patch.into());
        c.date = date.into();
        c.parents = if sha == "merge" {
            vec!["a".into(), "b".into()]
        } else {
            vec!["a".into()]
        };
        report.commits.insert(c.sha.clone(), c);
    }
    let mut entries = Vec::new();
    for (id, patches) in [
        ("rebased", vec!["p-old"]),
        ("mixed", vec!["p-old", "p-new"]),
        ("boundary", vec!["p-boundary"]),
        ("future", vec!["p-future"]),
        ("unknown", vec!["p-old", "p-unknown"]),
        ("merge-only", vec!["p-merge"]),
    ] {
        let mut candidate = template.clone();
        candidate.id = id.into();
        candidate.context_commits.clear();
        candidate.patch_ids = patches.into_iter().map(String::from).collect();
        candidate.commits = report
            .commits
            .values()
            .filter(|c| candidate.patch_ids.contains(c.patch_id.as_ref().unwrap()))
            .map(|c| c.sha.clone())
            .collect();
        report.features.push(candidate);
        entries.push(Entry {
            candidate_id: id.into(),
            aliases: vec![],
            forks: vec![format!("{id}/repo")],
            metadata: json!({}),
            integration: None,
            activity: None,
        });
    }
    let release = json!({"tag_name":"v1", "html_url":format!("https://github.com/{}/releases/tag/v1",report.repository), "published_at":"2026-08-05T14:14:01Z", "draft":false, "prerelease":false});
    let w = discovery_window::apply(&report, &mut entries, release.clone(), None).unwrap();
    assert_eq!(w.since, "2026-07-05T14:14:01+00:00");
    assert_eq!((w.recent, w.unknown, w.outside), (2, 3, 1));
    assert_eq!(
        entries[0].activity.as_ref().unwrap().scope,
        Scope::Outside,
        "a later identical copy must not refresh old work"
    );
    assert_eq!(entries[1].activity.as_ref().unwrap().scope, Scope::Recent);
    assert_eq!(
        report.features[1].patch_ids.len(),
        2,
        "older supporting commits must survive"
    );
    assert_eq!(
        entries[2].activity.as_ref().unwrap().scope,
        Scope::Recent,
        "inclusive cutoff compares instants, not strings"
    );
    let order = discovery_window::ordered(&entries);
    assert_eq!(order[0].candidate_id, "mixed");
    assert_eq!(order.len(), 5, "unknown dates remain eligible");
    assert!(!order.iter().any(|e| e.candidate_id == "rebased"));
    let days = discovery_window::apply(&report, &mut entries, release.clone(), Some(30)).unwrap();
    assert_eq!(days.since, "2026-07-06T14:14:01+00:00");
    // Exercise the public offline CLI path with no model calls.
    let root = f.cache.path();
    let report_path = root.join("window-report.json");
    let release_path = root.join("release.json");
    let output = root.join("window-discovery.json");
    let html = root.join("window.html");
    forkpicker::write_json(&report_path, &report).unwrap();
    forkpicker::write_json(&release_path, &release).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_forkpicker"))
        .args([
            "classify",
            report_path.to_str().unwrap(),
            "--discover",
            "--fresh",
            "--agent",
            "codex",
            "--release-snapshot",
            release_path.to_str().unwrap(),
            "--application",
            "all",
            "--dry-run",
            "--output",
            output.to_str().unwrap(),
            "--html",
            html.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result: forkpicker::discovery::Discovery =
        serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(result.window.unwrap().eligible(), 5);
    assert_eq!(
        result.entries.len(),
        6,
        "historical catalog survives filtering"
    );
    assert_eq!(result.run.attempted_calls, 0);
    let rendered = std::fs::read_to_string(html).unwrap();
    assert!(rendered.contains("window-scope") && rendered.contains("Older changes"));
    for invalid in [
        json!({"prerelease":true}),
        json!({"draft":true}),
        json!({"published_at":"2026-10-01T00:00:00Z"}),
        json!({"html_url":"https://github.com/another/repo/releases/tag/v1"}),
    ] {
        let mut bad = release.clone();
        for (key, value) in invalid.as_object().unwrap() {
            bad[key] = value.clone();
        }
        assert!(discovery_window::apply(&report, &mut entries, bad, None).is_err());
    }
}

#[test]
fn release_recency_orders_forks_without_letting_one_fork_take_every_slot() {
    use forkpicker::{
        discovery::Entry,
        discovery_window::{self, Activity, Scope},
    };
    let make = |id: String, fork: &str, date: &str| Entry {
        candidate_id: id,
        aliases: vec![],
        forks: vec![fork.into()],
        metadata: serde_json::json!({}),
        integration: None,
        activity: Some(Activity {
            latest_patch_date: Some(date.into()),
            age_days: Some(1),
            missing_patch_dates: 0,
            scope: Scope::Recent,
        }),
    };
    let mut entries: Vec<_> = (0..30)
        .map(|i| {
            make(
                format!("large-{i:02}"),
                "large/repo",
                "2026-09-08T01:00:00+00:00",
            )
        })
        .collect();
    entries.push(make("small".into(), "small/repo", "2026-09-07T00:00:00Z"));
    entries.push(make(
        "newest".into(),
        "newest/repo",
        "2026-09-07T23:00:00-04:00",
    ));
    let order = discovery_window::ordered(&entries);
    assert_eq!(
        order[0].candidate_id, "newest",
        "recency compares UTC instants"
    );
    assert_eq!(
        order[13].candidate_id, "small",
        "small forks precede the second large-fork bundle"
    );
    assert_eq!(order.len(), 32);
}

#[test]
fn assembled_application_checks_the_union_and_refuses_unrelated_history() {
    let f = Fixture::new();
    f.git(&["checkout", "-qb", "feature"]);
    f.write("src/new.c", "initial\n");
    let first = f.commit("New subsystem");
    f.write("src/new.c", "feature behavior\n");
    let second = f.commit("Implement behavior");
    f.git(&["checkout", "main"]);
    f.git(&["checkout", "-qb", "other"]);
    f.write("src/other.c", "another feature\n");
    let other = f.commit("Other subsystem");
    let mut report = f.scan(&["feature", "other"], &["main"]);
    let template = report.features[0].clone();
    report.features = [&first, &second, &other]
        .into_iter()
        .map(|sha| {
            let mut feature = template.clone();
            feature.id = sha.clone();
            feature.commits = vec![sha.clone()];
            feature.patch_ids = vec![report.commits[sha].patch_id.clone().unwrap()];
            feature.context_commits.clear();
            feature.files = report.commits[sha]
                .files
                .iter()
                .map(|f| f.path.clone())
                .collect();
            feature
        })
        .collect();
    let git = Git::new(f.dir.path());
    let alone =
        forkpicker::integration::check_set(&report, std::slice::from_ref(&second), &git).unwrap();
    assert_eq!(alone.status, "not-applicable");
    let together =
        forkpicker::integration::check_set(&report, &[second.clone(), first.clone()], &git)
            .unwrap();
    assert_eq!(together.status, "clean");
    assert_eq!(together.ordered_commits, vec![first.clone(), second]);
    let unrelated = forkpicker::integration::check_set(&report, &[first, other], &git).unwrap();
    assert_eq!(unrelated.status, "unknown");
    assert!(unrelated
        .notes
        .iter()
        .any(|s| s.contains("incompatible histories")));
}

#[test]
fn discovery_application_filter_is_applied_before_any_model_request() {
    let f = Fixture::new();
    f.hdr();
    f.git(&["checkout", "main"]);
    f.write("src/render.c", "upstream incompatible rendering\n");
    f.commit("Replace rendering");
    let report = f.scan(&["hdr"], &["main"]);
    let inventory = integration_fixture(&f, &report, None);
    let expected = inventory
        .candidates
        .iter()
        .filter(|c| {
            c.integration
                .as_ref()
                .is_some_and(|i| forkpicker::discovery_application::clean(&i.status))
        })
        .count();
    let root = f.cache.path();
    let rp = root.join("report.json");
    let ip = root.join("inventory.json");
    let op = root.join("discovery.json");
    forkpicker::write_json(&rp, &report).unwrap();
    forkpicker::write_json(&ip, &inventory).unwrap();
    for policy in ["clean", "all"] {
        let output = Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "classify",
                rp.to_str().unwrap(),
                "--discover",
                "--fresh",
                "--application",
                policy,
                "--inventory",
                ip.to_str().unwrap(),
                "--agent",
                "codex",
                "--dry-run",
                "--output",
                op.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let d: forkpicker::discovery::Discovery =
            serde_json::from_slice(&std::fs::read(&op).unwrap()).unwrap();
        assert_eq!(
            d.application_pool.unwrap().eligible,
            if policy == "clean" {
                expected
            } else {
                d.entries.len()
            }
        );
        assert_eq!(d.entries.len(), report.features.len());
        assert_eq!(d.run.attempted_calls, 0);
    }
}

#[test]
fn clean_discovery_spends_no_inspection_call_on_an_unverifiable_union() {
    use serde_json::json;
    let f = Fixture::new();
    f.hdr();
    f.git(&["checkout", "main"]);
    f.git(&["checkout", "-qb", "parser"]);
    f.write("src/parser.c", "int parser=1;\n");
    f.commit("Implement parser");
    let mut report = f.scan(&["hdr", "parser"], &["main"]);
    report.repository = format!("local:{}", f.dir.path().display());
    let inventory = integration_fixture(&f, &report, None);
    assert!(inventory
        .candidates
        .iter()
        .all(|c| forkpicker::discovery_application::clean(
            &c.integration.as_ref().unwrap().status
        )));
    let root = f.cache.path();
    let rp = root.join("report.json");
    let ip = root.join("inventory.json");
    let op = root.join("discovery.json");
    let cp = root.join("config.json");
    let script = root.join("model.py");
    let calls = root.join("calls");
    forkpicker::write_json(&rp, &report).unwrap();
    forkpicker::write_json(&ip, &inventory).unwrap();
    std::fs::write(&script,r#"import json,sys,pathlib
p=json.load(sys.stdin)
pathlib.Path(sys.argv[1]).open('a').write(p.get('stage','inspect')+'\n')
assert p.get('stage')=='map', 'An unverifiable union must not get a paid inspection'
print(json.dumps({'groups':[{'headline':'Combine behaviors','candidate_ids':[c['candidate_id'] for c in p['candidates']],'basis':'plausible_feature','benefit':'Potential related work','question':'Do the implementations fit together?','files_to_inspect':[],'issue_queries':[]}]}))
"#).unwrap();
    forkpicker::write_json(
        &cp,
        &json!({"agents":{"codex":{"command":["python3",script,calls]}}}),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forkpicker"))
        .args([
            "--config",
            cp.to_str().unwrap(),
            "classify",
            rp.to_str().unwrap(),
            "--discover",
            "--fresh",
            "--application",
            "clean",
            "--inventory",
            ip.to_str().unwrap(),
            "--agent",
            "codex",
            "--map-calls",
            "1",
            "--limit",
            "2",
            "--output",
            op.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let d: forkpicker::discovery::Discovery =
        serde_json::from_slice(&std::fs::read(op).unwrap()).unwrap();
    assert_eq!(d.run.attempted_calls, 1);
    assert_eq!(d.screened().len(), 2);
    assert!(d.inspections.is_empty());
    assert_eq!(d.assembled_checks.len(), 1);
    assert_eq!(
        d.assembled_checks[0].check.as_ref().unwrap().status,
        "unknown"
    );
    assert_eq!(std::fs::read_to_string(calls).unwrap(), "map\n");
}

#[test]
fn discovery_inspects_whole_diffs_and_all_commits_or_defers_without_spending() {
    use serde_json::json;
    let f = Fixture::new();
    f.git(&["checkout", "-qb", "series"]);
    let mut shas = Vec::new();
    for i in 0..6 {
        let body = (0..2500)
            .map(|line| format!("int value_{i}_{line} = {line};\n"))
            .collect::<String>();
        f.write(&format!("src/change{i}.c"), &body);
        f.write(
            &format!("tests/change{i}.c"),
            "// UNREQUESTED FILE MUST REACH MODEL\n",
        );
        shas.push(f.commit(&format!("series: implement behavior {i}")));
    }
    let mut report = f.scan(&["series"], &["main"]);
    report.repository = format!("local:{}", f.dir.path().display());
    let mut feature = report.features[0].clone();
    feature.commits = shas.clone();
    feature.files = shas
        .iter()
        .flat_map(|s| report.commits[s].files.iter().map(|f| f.path.clone()))
        .collect();
    report.features = vec![feature];
    let git = Git::new(f.dir.path());
    let expected: std::collections::BTreeMap<_, _> = shas
        .iter()
        .map(|s| (s.clone(), git.full_patch(s).unwrap()))
        .collect();
    // Simulate a truncated portable report: recover this diff from exact local Git objects.
    let c = report.commits.get_mut(&shas[0]).unwrap();
    c.patch.truncate(100);
    c.patch_truncated = true;
    let root = f.cache.path();
    let rp = root.join("report.json");
    let op = root.join("discovery.json");
    let cp = root.join("config.json");
    let ep = root.join("expected.json");
    let script = root.join("model.py");
    let marker = root.join("calls");
    forkpicker::write_json(&rp, &report).unwrap();
    forkpicker::write_json(&ep, &expected).unwrap();
    std::fs::write(&script, r#"import sys,json,pathlib
p=json.load(sys.stdin)
pathlib.Path(sys.argv[1]).open('a').write(p.get('stage','inspect')+'\n')
if p.get('stage')=='map':
 c=p['candidates'][0]
 answer={'groups':[{'headline':'Implement behavior','candidate_ids':[c['candidate_id']],'basis':'plausible_feature','benefit':'Potential behavior','question':'Is the series coherent?','files_to_inspect':['src/change0.c'],'issue_queries':[]}]}
else:
 expected=json.loads(pathlib.Path(sys.argv[2]).read_text())
 assert p['diff_scope']=='complete-nominated-commits-v1'
 assert p['response_schema']['properties']['groups']['items']['properties']['members']['maxItems']==1
 c=p['candidates'][0]
 assert c['omitted_commits']==0 and len(c['commits'])==6
 assert {m['sha']:m['patch'] for m in c['commits']}==expected
 assert all(not m['patch_truncated'] and not m['message_truncated'] and not m['files_omitted'] for m in c['commits'])
 assert all('UNREQUESTED FILE MUST REACH MODEL' in m['patch'] for m in c['commits'])
 claim={'text':'Complete series is supplied','evidence':c['evidence_ids']}
 answer={'schema_version':1,'scope':p['scope'],'headline':'Implement behavior','summary':claim,'groups':[{'name':'Implement behavior','summary':claim,'members':[{'candidate_id':c['candidate_id'],'role':'implementation'}]}],'relationships':[],'unclassified':[],'usefulness':[{'candidate_id':c['candidate_id'],'verdict':'useful','reason':claim}],'limitations':['No execution']}
print(json.dumps(answer))
"#).unwrap();
    forkpicker::write_json(
        &cp,
        &json!({"agents":{"codex":{"command":["python3",script,marker,ep]}}}),
    )
    .unwrap();
    let run = |max_bytes: &str| {
        let result = Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--config",
                cp.to_str().unwrap(),
                "--cache-dir",
                root.to_str().unwrap(),
                "classify",
                rp.to_str().unwrap(),
                "--discover",
                "--fresh",
                "--agent",
                "codex",
                "--map-calls",
                "1",
                "--limit",
                "2",
                "--max-bytes",
                max_bytes,
                "--total-bytes",
                "2000000",
                "--output",
                op.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice::<forkpicker::discovery::Discovery>(&std::fs::read(&op).unwrap())
            .unwrap()
    };
    let complete = run("1000000");
    assert_eq!(complete.inspections.len(), 1);
    assert!(complete.inspection_deferrals.is_empty());
    assert_eq!(complete.run.attempted_calls, 2);
    let oversized = run("8000");
    assert!(oversized.inspections.is_empty());
    assert_eq!(oversized.run.attempted_calls, 1);
    assert_eq!(oversized.inspection_deferrals.len(), 1);
    assert!(oversized.inspection_deferrals[0]["reason"]
        .as_str()
        .unwrap()
        .contains("complete nominated diffs"));
    report.repository = "unavailable/repository".into();
    forkpicker::write_json(&rp, &report).unwrap();
    let unavailable = run("1000000");
    assert!(unavailable.inspections.is_empty());
    assert_eq!(unavailable.run.attempted_calls, 1);
    assert!(unavailable.inspection_deferrals[0]["reason"]
        .as_str()
        .unwrap()
        .contains("needs local Git objects"));
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        "map\ninspect\nmap\nmap\n"
    );
}

#[test]
fn open_issue_review_verifies_matches_preserves_classification_and_continues_spend() {
    use serde_json::{json, Value};
    let f = Fixture::new();
    f.hdr();
    let report = f.scan(&["hdr"], &["main"]);
    let root = f.cache.path();
    let rp = root.join("report.json");
    let cp = root.join("config.json");
    let dp = root.join("discovery.json");
    let ip = root.join("issues.json");
    let op = root.join("issue-review.json");
    let hp = root.join("index.html");
    let script = root.join("model.py");
    let marker = root.join("calls");
    forkpicker::write_json(&rp, &report).unwrap();
    let open = Issue {
        number: 42,
        title: "Dim reference whites".into(),
        body: format!("{}FINAL ISSUE BODY", "context ".repeat(400)),
        url: "https://github.com/upstream/example/issues/42".into(),
        state: "open".into(),
        is_pull_request: false,
        match_kind: "project_catalog".into(),
        kind: "issue".into(),
        discussion_category: None,
        discussion_answerable: None,
        body_truncated: false,
        comments: vec![],
        comments_omitted: 0,
        thumbs_up: None,
        upvotes: None,
        labels: vec![],
    };
    let mut closed = open.clone();
    closed.state = "closed".into();
    closed.number = 43;
    closed.url = closed.url.replace("42", "43");
    let mut foreign = open.clone();
    foreign.url = "https://github.com/foreign/repo/issues/42".into();
    let mut discussion = open.clone();
    discussion.kind = "discussion".into();
    discussion.url = discussion.url.replace("issues", "discussions");
    forkpicker::write_json(
        &ip,
        &forkpicker::priority::DemandSnapshot {
            fetched_at: "2026-09-10T00:00:00Z".into(),
            query: None,
            priority_labels: vec![],
            threads: vec![open, closed, foreign, discussion],
            warnings: vec![],
        },
    )
    .unwrap();
    std::fs::write(&script,r#"import json,sys,pathlib
p=json.load(sys.stdin);stage=p.get('stage','inspect');pathlib.Path(sys.argv[1]).open('a').write(stage+'\n')
if stage=='map':
 c=p['candidates'][0];answer={'groups':[{'headline':'Improve rendering','candidate_ids':[c['candidate_id']],'basis':'plausible_feature','benefit':'Render white','question':'Does white change?','files_to_inspect':[],'issue_queries':[]}]}
elif stage=='inspect':
 c=p['candidates'][0];claim={'text':'Reference white changes','evidence':c['evidence_ids']};answer={'schema_version':1,'scope':p['scope'],'headline':'Improve rendering','summary':claim,'groups':[{'name':'Improve rendering','summary':claim,'members':[{'candidate_id':c['candidate_id'],'role':'implementation'}]}],'relationships':[],'unclassified':[],'usefulness':[{'candidate_id':c['candidate_id'],'verdict':'useful','reason':claim}],'limitations':[]}
elif stage=='screen-issues':
 assert len(p['issues'])==1 and p['issues'][0]['url'].endswith('/issues/42')
 assert p['issues'][0]['body_truncated'] and 'FINAL ISSUE BODY' not in p['issues'][0]['body']
 answer={'assessments':[{'feature_id':f['feature_id'],'issue_urls':[p['issues'][0]['url']]} for f in p['features']]}
else:
 assert stage=='confirm-issues' and p['issues'][0]['body'].endswith('FINAL ISSUE BODY')
 f=p['features'][0];c=f['candidates'][0]
 assert not c['commits'][0]['patch_truncated'] and '203' in c['commits'][0]['patch']
 answer={'assessments':[{'feature_id':f['feature_id'],'matches':[{'issue_url':p['issues'][0]['url'],'relation':'likely_addresses','reason':{'text':'Reference white increase may address the reported dimness','evidence':[p['issues'][0]['evidence_id'],c['evidence_ids'][0]]}}]}]}
print(json.dumps(answer))
"#).unwrap();
    forkpicker::write_json(
        &cp,
        &json!({"agents":{"codex":{"command":["python3",script,marker]}}}),
    )
    .unwrap();
    let classified = Command::new(env!("CARGO_BIN_EXE_forkpicker"))
        .args([
            "--config",
            cp.to_str().unwrap(),
            "classify",
            rp.to_str().unwrap(),
            "--discover",
            "--fresh",
            "--agent",
            "codex",
            "--limit",
            "2",
            "--output",
            dp.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        classified.status.success(),
        "{}",
        String::from_utf8_lossy(&classified.stderr)
    );
    let before: Value = serde_json::from_slice(&std::fs::read(&dp).unwrap()).unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args([
                "--config",
                cp.to_str().unwrap(),
                "match-issues",
                rp.to_str().unwrap(),
                "--classification",
                dp.to_str().unwrap(),
                "--issue-cache",
                ip.to_str().unwrap(),
                "--agent",
                "codex",
                "--output",
                op.to_str().unwrap(),
                "--html",
                hp.to_str().unwrap(),
            ])
            .args(extra)
            .output()
            .unwrap()
    };
    assert!(
        !run(&["--limit", "1"]).status.success(),
        "screening alone must remain incomplete"
    );
    let result = run(&["--continue-run", "--limit", "2"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let matched: forkpicker::issue_review::Review =
        serde_json::from_slice(&std::fs::read(&op).unwrap()).unwrap();
    assert!(matched.complete);
    assert_eq!(matched.attempted_calls, 2);
    assert_eq!(matched.available_open_issues, 1);
    assert_eq!(matched.findings[0].matches.len(), 1);
    assert_eq!(
        matched.input_bytes,
        matched.calls.iter().map(|c| c.input_bytes).sum::<usize>()
    );
    let after: Value = serde_json::from_slice(&std::fs::read(&dp).unwrap()).unwrap();
    assert_eq!(before["inspections"], after["inspections"]);
    assert_eq!(before["mappings"], after["mappings"]);
    let html = std::fs::read_to_string(&hp).unwrap();
    assert!(html.contains("issue_matches"));
    assert!(html.contains("https://github.com/upstream/example/issues/42"));
    assert!(html.contains("FINAL ISSUE BODY"));
    assert!(run(&["--continue-run", "--limit", "2"]).status.success());
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "map\ninspect\nscreen-issues\nconfirm-issues\n"
    );
    assert!(!run(&["--continue-run", "--limit", "1"]).status.success());
}

#[test]
fn oneshot_runs_full_pipeline_with_shared_budget_and_preserves_failed_work() {
    use serde_json::{json, Value};
    let f = Fixture::new();
    let hdr = f.hdr();
    f.git(&["checkout", "main"]);
    f.git(&["checkout", "-qb", "parser"]);
    f.write("src/parser.c", "int parse = 2;\n");
    let parser = f.commit("parser: validate input");
    let report = f.scan(&["hdr", "parser"], &["main"]);
    let root = f.cache.path();
    let git_path = root
        .join("repositories")
        .join(forkpicker::hash(&report.repository))
        .join("git");
    std::fs::create_dir_all(git_path.parent().unwrap()).unwrap();
    assert!(Command::new("git")
        .args(["clone", "--bare"])
        .arg(f.dir.path())
        .arg(&git_path)
        .output()
        .unwrap()
        .status
        .success());
    let rp = root.join("raw.json");
    let ip = root.join("issues.json");
    let pp = root.join("prs.json");
    let cp = root.join("config.json");
    let release = root.join("release.json");
    let marker = root.join("calls");
    let failure = root.join("fail");
    let script = root.join("model.py");
    forkpicker::write_json(&rp, &report).unwrap();
    forkpicker::write_json(&release,&json!({"tag_name":"v1","html_url":"https://github.com/upstream/example/releases/tag/v1","published_at":report.generated_at,"draft":false,"prerelease":false})).unwrap();
    forkpicker::write_json(&ip,&json!({"fetched_at":report.generated_at,"query":null,"priority_labels":[],"warnings":[],"threads":[{"number":42,"title":"Reference white is dim","body":"Please increase the reference white level","url":"https://github.com/upstream/example/issues/42","state":"open","is_pull_request":false,"kind":"issue","match_kind":"project_catalog"}]})).unwrap();
    forkpicker::write_json(&pp,&json!({"repository":report.repository,"fetched_at":report.generated_at,"pulls":[{"number":7,"title":"Validate input","draft":false,"head_sha":parser,"base_branch":"main","commits":[parser],"commits_complete":true,"membership_verified":true}],"listing_complete":true,"warnings":[],"api_requests":0})).unwrap();
    std::fs::write(&script,r#"import json,sys,pathlib
p=json.load(sys.stdin);stage=p.get('stage','inspect');pathlib.Path(sys.argv[1]).open('a').write(stage+'\n')
assert '--output-schema' in sys.argv
if stage=='map':
 assert len(p['candidates'])==1, 'PR-covered work must not reach the model'
 c=p['candidates'][0];answer={'groups':[{'headline':'Improve rendering','candidate_ids':[c['candidate_id']],'basis':'plausible_feature','benefit':'Render white','question':'Does white change?','files_to_inspect':[],'issue_queries':[],'demand':{'status':'not_established','reason':{'text':'No confirmed demand at this stage','evidence':[]}}}]}
elif stage=='inspect':
 if pathlib.Path(sys.argv[2]).exists(): print('{}');sys.exit(0)
 c=p['candidates'][0];assert c['omitted_commits']==0 and not c['commits'][0]['patch_truncated']
 claim={'text':'Reference white changes','evidence':c['evidence_ids']};answer={'schema_version':1,'scope':p['scope'],'headline':'Improve rendering','summary':claim,'groups':[{'name':'Improve rendering','summary':claim,'members':[{'candidate_id':c['candidate_id'],'role':'implementation'}]}],'relationships':[],'unclassified':[],'usefulness':[{'candidate_id':c['candidate_id'],'verdict':'useful','reason':claim}],'limitations':[]}
 if p.get('inline_issues'):
  issue=p['issue_context']['threads'][0]
  answer['groups'][0]['issue_matches']=[{'issue_url':issue['url'],'relation':'likely_addresses','reason':{'text':'Reference white increase addresses the reported dimness','evidence':[issue['evidence_id'],c['evidence_ids'][0]]}}]
elif stage=='screen-issues':
 answer={'assessments':[{'feature_id':f['feature_id'],'issue_urls':[p['issues'][0]['url']]} for f in p['features']]}
else:
 assert stage=='confirm-issues'
 answer={'assessments':[{'feature_id':f['feature_id'],'matches':[{'issue_url':p['issues'][0]['url'],'relation':'likely_addresses','reason':{'text':'The reference white increase may address dimness','evidence':[p['issues'][0]['evidence_id'],f['candidates'][0]['evidence_ids'][0]]}}]} for f in p['features']]}
print(json.dumps(answer))
"#).unwrap();
    forkpicker::write_json(&cp,&json!({"default_agent":"codex","agents":{"codex":{"command":["python3",script,marker,failure]}}})).unwrap();
    let run = |name: &str, extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .args(["run", "upstream/example", "--from-scan"])
            .arg(&rp)
            .arg("--issue-cache")
            .arg(&ip)
            .arg("--pr-snapshot")
            .arg(&pp)
            .arg("--release-snapshot")
            .arg(&release)
            .arg("--config")
            .arg(&cp)
            .arg("--cache-dir")
            .arg(root)
            .arg("--state-dir")
            .arg(root.join("state"))
            .arg("--output")
            .arg(root.join(name))
            .args(if name == "inline" {
                vec![]
            } else {
                vec!["--separate-issues"]
            })
            .args(extra)
            .output()
            .unwrap()
    };
    let result = run("full", &["--limit", "6"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "map\ninspect\nscreen-issues\nconfirm-issues\n"
    );
    let load = |name: &str, file: &str| -> Value {
        serde_json::from_slice(&std::fs::read(root.join(name).join(file)).unwrap()).unwrap()
    };
    let d = load("full", "discovery.json");
    assert!(d["fresh"].as_bool().unwrap());
    assert_eq!(d["window"]["recent"], 1);
    assert_eq!(d["inspections"].as_array().unwrap().len(), 1);
    assert!(
        d["inspections"][0]["context"]["candidates"][0]["evidence_ids"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("commit:{hdr}")))
    );
    let manifest = load("full", "run.json");
    let issue = load("full", "issue-review.json");
    assert_eq!(manifest["attempted_calls"], 4);
    assert_eq!(
        manifest["input_bytes"].as_u64().unwrap(),
        d["new_input_bytes"].as_u64().unwrap() + issue["input_bytes"].as_u64().unwrap()
    );
    assert_eq!(manifest["status"], "finished");
    let html = std::fs::read_to_string(root.join("full/index.html")).unwrap();
    assert!(
        html.contains("May address")
            && html.contains("https://github.com/upstream/example/issues/42")
            && html.contains("#feature/")
    );
    // Never overwrite a previous experiment or spend again merely by rerunning its path.
    assert!(!run("full", &[]).status.success());
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 4);
    let limited = run("limited", &["--limit", "2"]);
    assert!(
        limited.status.success(),
        "{}",
        String::from_utf8_lossy(&limited.stderr)
    );
    assert_eq!(load("limited", "run.json")["attempted_calls"], 2);
    assert!(!root.join("limited/issue-review.json").exists());
    // A later experiment intentionally does not reuse model judgments.
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 6);
    std::fs::write(&failure, "fail").unwrap();
    let failed = run("failed", &["--limit", "6"]);
    assert_eq!(failed.status.code(), Some(2));
    assert_eq!(load("failed", "run.json")["status"], "incomplete");
    assert_eq!(load("failed", "run.json")["attempted_calls"], 2);
    assert!(!root.join("failed/issue-review.json").exists());
    assert!(root.join("failed/index.html").exists());
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 8);
    for (name, flag) in [("facts", "--no-llm"), ("zero", "--limit")] {
        let extra = if flag == "--limit" {
            vec![flag, "0"]
        } else {
            vec![flag]
        };
        let result = run(name, &extra);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(load(name, "run.json")["attempted_calls"], 0);
        assert!(!root.join(name).join("discovery.json").exists());
        assert!(root.join(name).join("index.html").exists());
    }
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 8);
    std::fs::remove_file(&failure).unwrap();
    let inline = run("inline", &["--limit", "6"]);
    assert!(
        inline.status.success(),
        "{}",
        String::from_utf8_lossy(&inline.stderr)
    );
    assert_eq!(load("inline", "run.json")["attempted_calls"], 2);
    assert_eq!(
        load("inline", "run.json")["issue_matching"]["mode"],
        "during_inspection"
    );
    assert!(!root.join("inline/issue-review.json").exists());
    let result = load("inline", "discovery.json");
    assert_eq!(
        result["inspections"][0]["value"]["groups"][0]["issue_matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(std::fs::read_to_string(root.join("inline/index.html"))
        .unwrap()
        .contains("Reference white increase addresses the reported dimness"));
    assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 10);
    assert_eq!(
        std::fs::read(&rp).unwrap(),
        serde_json::to_vec_pretty(&report).unwrap()
    );
}

#[test]
fn discovery_parallel_inspections_reserve_budget_resume_and_stop_after_failed_wave() {
    use serde_json::{json, Value};
    let f = Fixture::new();
    f.hdr();
    f.git(&["checkout", "main"]);
    f.git(&["checkout", "-qb", "parser"]);
    f.write("src/parser.c", "int parse = 7;\n");
    f.commit("parser: validate input");
    f.git(&["checkout", "main"]);
    f.git(&["checkout", "-qb", "docs"]);
    f.write("README.md", "Setup instructions\n");
    f.commit("docs: document installation");
    let report = f.scan(&["hdr", "parser", "docs"], &["main"]);
    assert_eq!(report.features.len(), 3);
    let root = f.cache.path();
    let rp = root.join("report.json");
    let cp = root.join("config.json");
    let script = root.join("model.py");
    let mode = root.join("mode.json");
    forkpicker::write_json(&rp, &report).unwrap();
    std::fs::write(&script,r#"import json,sys,pathlib,time,fcntl
p=json.load(sys.stdin);mode=json.loads(pathlib.Path(sys.argv[1]).read_text());out=pathlib.Path(mode['output']);stats=pathlib.Path(mode['stats'])
if p.get('stage')=='map':
 groups=[{'headline':'Inspect '+str(i),'candidate_ids':[c['candidate_id']],'basis':'plausible_feature','benefit':'Handle a concrete behavior','question':'Does it work?','files_to_inspect':[],'issue_queries':[]} for i,c in enumerate(p['candidates'])]
 if mode.get('overlap'):groups=[dict(groups[0]),dict(groups[0]),*groups[1:]]
 print(json.dumps({'groups':groups}));sys.exit(0)
def update(delta):
 with stats.open('a+') as s:
  fcntl.flock(s,fcntl.LOCK_EX);s.seek(0);v=json.loads(s.read() or '{"active":0,"max":0,"calls":0}')
  v['active']+=delta;v['max']=max(v['max'],v['active'])
  if delta>0:v['calls']+=1
  s.seek(0);s.truncate();s.write(json.dumps(v));s.flush();return v
state=update(1)
saved=json.loads(out.read_text());assert saved['attempted_calls']<=saved['call_limit'] and saved['new_input_bytes']<=saved['input_limit']
assert saved['attempted_calls']>=mode['reserved'], 'All calls in the wave must be charged before execution'
start=time.monotonic()
while True:
 with stats.open() as s:
  fcntl.flock(s,fcntl.LOCK_SH);state=json.load(s)
 if state['max']>=mode['barrier']:break
 assert time.monotonic()-start<10, 'Inspections did not run concurrently'
 time.sleep(.01)
time.sleep(.1)
c=p['candidates'][0];claim={'text':'Observed behavior','evidence':c['evidence_ids']}
answer={'schema_version':1,'scope':p['scope'],'headline':'Improve behavior','summary':claim,'groups':[{'name':'Improve behavior','summary':claim,'members':[{'candidate_id':c['candidate_id'],'role':'implementation'}]}],'relationships':[],'unclassified':[],'usefulness':[{'candidate_id':c['candidate_id'],'verdict':'useful','reason':claim}],'limitations':[]}
if mode.get('fail') and p['proposed_features'][0]['headline']=='Inspect 0':answer={}
update(-1);print(json.dumps(answer))
"#).unwrap();
    forkpicker::write_json(
        &cp,
        &json!({"agents":{"codex":{"command":["python3",script,mode]}}}),
    )
    .unwrap();
    let run = |name: &str,
               limit: usize,
               jobs: usize,
               barrier: usize,
               fail: bool,
               overlap: bool,
               extra: &[&str]| {
        let output = root.join(format!("{name}.json"));
        forkpicker::write_json(&mode,&json!({"output":output,"stats":root.join(format!("{name}-stats.json")),"barrier":barrier,"reserved":if limit==2 || overlap || jobs==1 {2}else{3},"fail":fail,"overlap":overlap})).unwrap();
        Command::new(env!("CARGO_BIN_EXE_forkpicker"))
            .arg("classify")
            .arg(&rp)
            .args([
                "--discover",
                "--fresh",
                "--agent",
                "codex",
                "--application",
                "all",
            ])
            .arg("--config")
            .arg(&cp)
            .arg("--cache-dir")
            .arg(root.join("cache"))
            .arg("--state-dir")
            .arg(root.join("state"))
            .arg("--output")
            .arg(&output)
            .arg("--limit")
            .arg(limit.to_string())
            .arg("--jobs")
            .arg(jobs.to_string())
            .args(extra)
            .output()
            .unwrap()
    };
    let load = |name: &str| -> Value {
        serde_json::from_slice(&std::fs::read(root.join(format!("{name}.json"))).unwrap()).unwrap()
    };
    let result = run("parallel", 4, 2, 2, false, false, &[]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(load("parallel-stats")["max"], 2);
    assert_eq!(load("parallel-stats")["calls"], 3);
    let d = load("parallel");
    assert_eq!(d["attempted_calls"], 4);
    assert_eq!(d["inspections"].as_array().unwrap().len(), 3);
    assert_eq!(
        d["new_input_bytes"].as_u64().unwrap(),
        d["mappings"]
            .as_array()
            .unwrap()
            .iter()
            .chain(d["inspections"].as_array().unwrap())
            .map(|r| r["input_bytes"].as_u64().unwrap())
            .sum::<u64>()
    );
    let failed = run("failed", 4, 2, 2, true, false, &[]);
    assert_eq!(failed.status.code(), Some(2));
    assert_eq!(load("failed")["attempted_calls"], 3);
    assert_eq!(load("failed")["inspections"].as_array().unwrap().len(), 1);
    assert_eq!(load("failed-stats")["calls"], 2);
    let limited = run("limited", 2, 2, 1, false, false, &[]);
    assert!(
        limited.status.success(),
        "{}",
        String::from_utf8_lossy(&limited.stderr)
    );
    assert_eq!(load("limited")["attempted_calls"], 2);
    assert_eq!(load("limited-stats")["max"], 1);
    let continued = run(
        "limited",
        4,
        2,
        2,
        false,
        false,
        &["--continue-run", "--extend-budget"],
    );
    assert!(
        continued.status.success(),
        "{}",
        String::from_utf8_lossy(&continued.stderr)
    );
    assert_eq!(load("limited")["attempted_calls"], 4);
    assert_eq!(load("limited-stats")["calls"], 3);
    let overlap = run("overlap", 4, 2, 1, false, true, &[]);
    assert!(
        overlap.status.success(),
        "{}",
        String::from_utf8_lossy(&overlap.stderr)
    );
    assert_eq!(
        load("overlap-stats")["calls"],
        3,
        "overlapping nomination should be covered, not reviewed twice"
    );
    assert_eq!(load("overlap")["attempted_calls"], 4);
}
