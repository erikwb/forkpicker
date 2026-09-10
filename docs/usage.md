# Usage

For installation, start with the [README](../README.md). Use `forkpicker COMMAND --help` for the full flag reference; [agent configuration](agents.md) covers models and custom wrappers.

## Run the full workflow

```sh
forkpicker run bluez/bluez --open
forkpicker run bluez/bluez --no-llm --open
forkpicker run bluez/bluez --agent claude --model sonnet --limit 10 --open
```

`run` collects forks, branches, issues/discussions, and open PR membership; checks patch application locally; then screens and inspects feature sets and matches open issues. `--no-llm` or `--limit 0` stops after factual integration reporting and requires no agent configuration. Model runs use a configured agent or the first installed CLI in Codex, Claude, Grok order.

Raw collection includes all discovered forks and branches with unlimited history analysis, subject to access and API limits. Model selection excludes dismissed/adopted work and verified open-PR coverage, starts one calendar month before the latest stable GitHub release, and requires direct or clean three-way application against upstream HEAD as scanned. Nominated sets are checked together before inspection. `--all-history` removes the date window. A missing GitHub release (404) also uses all history; other release API errors stop the stage. Arbitrary Git tags do not supply release publication dates.

| Setting | Default for `run` |
| --- | ---: |
| API budget, per collection stage | 5,000 requests |
| API workers / Git fetch workers | 4 / 4 |
| Local application workers | Available CPUs, capped at 32 |
| Total model calls, including failures | 30 |
| Total serialized model input | 4,000,000 bytes |
| Input per call | 256,000 bytes |
| Mapping calls | Up to 5 |
| Descriptions per mapping batch | Up to 200, reduced to fit bytes |
| Concurrent inspections | 2 |
| Timeout per model call | 180 seconds |
| Discovery runtime | 3,600 seconds |

Use `--jobs`, `--fetch-jobs`, `--review-jobs`, `--api-budget`, `--limit`, `--map-calls`, `--total-bytes`, and `--max-bytes` to adjust scope. Increasing a call ceiling does not raise input or time ceilings. These limits bound work, not subscription charges; the agent CLI controls billing.

Discovery reserves `min(limit / 6, 5)` calls and one quarter of the input budget for issue matching. Default discovery therefore gets 25 calls and 3 MB. Matching uses the remaining shared allowance, up to five calls. Complete open-issue catalogs of at most eight issues and 12,000 bytes can fit inside inspections, avoiding separate calls. The added context must fit the request and consume at most one eighth of its input allowance. Larger or truncated catalogs use separate matching; `--separate-issues` always uses that path.

## Reports and saved evidence

Each run creates `reports/OWNER-REPO/TIMESTAMP/`. `--output DIR` chooses a new or empty directory. Open its `index.html` directly, or use `--open` to launch the default browser. Search, feature/fork pages, and Back/Forward navigation work without a server; avatars load from GitHub. Opening a page never invokes a model.

| File | Contents |
| --- | --- |
| `network.json` | Fork/branch coverage, commits, candidates, upstream evidence |
| `issues.json`, `prs.json`, `release.json` | Raw context snapshots, when available |
| `inventory.json` | Measurements and patch-application results |
| `shortlist.json` | Context collection output; its ranking does not select `run` inputs |
| `discovery.json` | Model nominations, inspections, and selection coverage |
| `issue-review.json` | Separate issue-matching results, when that stage runs |
| `run.json` | Stage timings, failures, model calls/input bytes, and output location |

Saved model requests and responses accompany the results. A new `run` reuses raw Git/API evidence and application facts, never previous model judgments. To reuse a particular collection, supply its files:

```sh
forkpicker run bluez/bluez \
  --from-scan reports/bluez/network.json \
  --issue-cache reports/bluez/issues.json --pr-snapshot reports/bluez/prs.json \
  --release-snapshot reports/bluez/release.json --output reports/bluez-followup
```

The corresponding Git object cache is still needed for local checks and full diffs. Missing context snapshots are collected normally. Use `--all-history` when no release snapshot is wanted. Global `--cache-dir`, `--state-dir`, and `--config` override the default XDG locations listed in the README.

## Scan and inspect without a model

```sh
forkpicker scan bluez/bluez --fork Yiin/bluez \
  --max-commits 0 --upstream-history 0 \
  --output reports/bluez.json --html reports/bluez.html

forkpicker inventory reports/bluez.json --check-apply \
  --output reports/inventory.json --html reports/inventory.html

forkpicker report reports/bluez.json --query "notification descriptors" --format markdown
forkpicker show reports/bluez.json FEATURE_ID --patch
```

Omit `--fork` to enumerate the full available fork network; repeat it to select known forks directly. `scan` defaults to all discovered forks/branches but limits analysis to 500 non-merge commits per branch and 2,000 upstream commits. The two zero-valued flags above remove those history limits. Its default API budget is 500; `--api-budget` raises it. Optional `--max-forks` and `--max-branches` limit selection. `--strict` writes the report and exits 2 for incomplete coverage.

Scanning discovers work from code and commit history. `--query` changes textual ordering, and `--with-context` attaches bounded conversation context after discovery. Neither creates candidates. `inventory` is offline; `--check-apply` reads cached Git objects. Use `--repo PATH` for another object store, `--target REF_OR_SHA` for another integration target, `--jobs N` for check concurrency, and `--policy FILE` for measurement thresholds. See [measurements and application checks](internals.md#measurements-and-application-checks).

For refs you already have locally, including repositories hosted outside GitHub:

```sh
forkpicker scan-local /path/to/repository --base main \
  --upstream-ref release/1.x --ref remotes/alice/gatt --ref remotes/bob/hid \
  --output reports/local.json
```

This does not fetch or modify the worktree. Additional upstream refs identify work already present on release branches.

## Decisions and returning to a project

```sh
forkpicker decide reports/bluez.json FEATURE_ID --status saved --reason "Inspect next"
forkpicker inventory reports/bluez.json --baseline reports/previous.json \
  --output reports/inventory.json --html reports/inventory.html
```

Replace `FEATURE_ID` with the identifier from the CLI/JSON report. Decisions are `new`, `saved`, `dismissed`, `needs-adopter`, or `adopted`. Dismissed/adopted work is hidden from normal views; `report --all` includes it. Exact patch sets retain decisions across rebases and cherry-picks. Overlapping changed sets return for reconsideration rather than silently inheriting a dismissal.

A baseline distinguishes previously seen, newly observed, changed, and regrouped patch sets. These describe observation history, not necessarily recent authorship: better coverage can reveal old work. Browser Save/Dismiss choices stay in local storage; export and run their generated `decide` commands to persist them in CLI state. Copying a review command or opening a card does not execute it.

## Discovery from a saved scan

```sh
forkpicker classify reports/bluez.json --discover --fresh --since-release \
  --inventory reports/inventory.json --agent codex \
  --pr-snapshot reports/bluez/prs.json --issue-cache reports/bluez/issues.json \
  --output reports/discovery.json --html reports/discovery.html
```

This exposes the model stages independently. Its defaults differ from `run`: five new calls, at most two mapping calls, and 240,000 total input bytes. `--max-bytes` defaults to 256,000. At most one third of the total allowance goes to mapping, with each mapping request fitting at most half that allocation. Use `--dry-run --plan-dir DIR` to inspect exact requests and schemas without invoking a model. A live release/PR lookup can still make GitHub requests during a dry run.

`--since-release` or `--release-snapshot FILE` enables the release window and defaults to `--application clean`; `--application all` also admits conflicts and unknowns. Without a release window, the default is all application results. `--release-overlap-days N` replaces the calendar-month overlap. Missing application facts are checked locally before selection; unavailable Git objects cause an error rather than an assumption that work is clean.

Mapping rotates compact fork bundles so one large fork cannot occupy every early slot. Release-window bundles use patch recency; raw issue excerpts inform nominations within each batch. A fork can yield several features. Inspection sends complete nominated diffs and messages plus bounded upstream context. Missing full diffs or oversized sets are deferred intact. Inspection cannot roam arbitrarily through the fork or guarantee complete dependencies. Older, deferred, and blocked work remains browsable; model-excluded features are hidden by default and can be revealed.

Use `--inline-issues` to enable the small-catalog matching used automatically by `run`. For separate matching and offline rendering:

```sh
forkpicker match-issues reports/bluez.json --classification reports/discovery.json \
  --issue-cache reports/bluez/issues.json --agent codex \
  --output reports/issue-review.json --html reports/index.html

forkpicker report reports/bluez.json --classification reports/discovery.json \
  --format html --output reports/index.html
```

Separate matching defaults to five calls and 1 MB total input. It screens open project issues, then checks proposed links against complete diffs and saved full issue bodies. Connections must cite the exact issue and that feature's code. Closed issues, discussions, PRs, and foreign repositories do not enter this pass. Missing matches are not evidence that work is useless.

## Failures and continuation

`run` records collection/local-check failures and exits 1. Model failures preserve completed work, stop subsequent waves/stages, and exit 2; in-flight inspections finish and save valid results. Their call/input allowances were reserved before launch. The latest saved dashboard remains available after a later failure. No automatic paid retry or provider fallback occurs.

`run` itself starts a fresh run when repeated. To continue its model work, use the individual commands with the saved inputs and output paths:

- `classify --discover --fresh --continue-run`: retain that run's valid responses and spending. Inputs/settings must match. Add `--extend-budget` with larger `--limit` and/or `--total-bytes` to extend total ceilings, not purchase an additional allowance. Raise `--max-seconds` if needed. `--reinspect` explicitly replaces prior inspections while retaining their original responses and spend.
- `match-issues --continue-run`: retain completed checks and spending against unchanged inputs/model; higher ceilings are allowed.
- Discovery without `--fresh`: repeated commands reuse exact matching requests and advance through unseen work. `--map-calls 0` inspects existing nominations only. `--resume-discovery FILE` carries saved results into a new output when changing sampling scope and cannot combine with `--fresh`.

A budget ceiling can leave work incomplete even without a model error. Review the saved funnel and deferred reasons before increasing spending.

## Other entry points

| Command | Use |
| --- | --- |
| `review REPORT FEATURE_ID` | Inspect one candidate for quality, style, security, and correctness; add `--dry-run` to preview |
| `review REPORT --all` | Review visible candidates in author-recency order, up to five new calls by default; `--limit 0` is unlimited |
| `classify REPORT` without `--discover` | Group work across forks; defaults to 50 forks, ten candidates per batch, ten calls, two workers, and 60 KB per call; `--limit 0` is cache-only |
| `classify --explore-related` | Retrieve bounded supporting work from the same fork, optionally searching `--issue-cache` |
| `shortlist` | Collect raw issues/PR membership and optionally rank explicit request links; `--refresh-demand --with-prs --write-demand-snapshot FILE --write-pr-snapshot FILE` saves reusable context without a model |
| `triage` | Optional model matching of a saved demand shortlist; defaults to 50 candidates, 20% unknown-demand exploration, five candidates per batch, and ten new calls |
| `context REPORT FEATURE_ID --review` | Export the exact review context and response example for another tool |
| `context`, `annotate`, `enrich` | Export, attach, or run an explanation through a custom stdin/stdout JSON command |
| `demand`, `--min-priority` | Opt into the older heuristic policy; it does not drive default reports or `run` selection |

Individual review defaults to 200,000 prompt bytes and a 300-second timeout. It may use excerpts to fit its budget; discovery inspection instead requires complete diffs. Review stops on failure, preserving successes for a repeated command. See [agent configuration](agents.md) for cache identity, overrides, and the model contracts.
