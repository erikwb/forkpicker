# Internals and evidence

Forkpicker keeps raw Git/GitHub evidence separate from model interpretations and maintainer decisions. A failed request means missing coverage, never an empty fork. Source branches, commits, patch identities, and upstream matches remain inspectable after grouping.

## Code map

| Modules | Responsibility |
| --- | --- |
| `main`, `oneshot` | CLI and complete-run orchestration |
| `github`, `git`, `scan` | Paginated collection, shared Git objects, history comparison, evidence |
| `analyze`, `model` | Candidate grouping and portable scan records |
| `metrics`, `integration`, `forks` | Independent measurements, local application, fork aggregation |
| `discovery`, `discovery_window`, `discovery_application` | Model scheduling, release window, assembled-set checks |
| `classify`, `related`, `structured`, `review`, `llm` | Evidence packs, schemas, agent transport, validation |
| `issue_context`, `issue_review`, `inline_issues` | Raw context retrieval and issue matching |
| `priority`, `shortlist`, `triage` | Optional demand and heuristic workflows |
| `render`, `inbox`, `classification_html`, `discovery_html`, `ui/` | Reports, local navigation, escaping |
| `state`, `parallel` | Atomic decisions and bounded worker pools |

## Collection and candidate identity

One shared bare repository per upstream stores complete incremental Git objects from its forks. Known branch tips skip fetching. There are no per-fork checkouts, LFS downloads, or submodule updates. API request budgets do not bound Git transfer size. Fork discovery pagination, upstream indexing, and feature analysis are serial; branch enumeration and Git fetches use separate worker pools. API workers share a request budget and launch pacing, and rate-limit responses stop queued requests.

Upstream branches and tags establish work already known upstream, including release-only changes. The scanned default branch remains the display/integration target. Reachability suppresses inherited history; stable patch IDs identify equivalent patches, and normalized per-file patches recognize some selective backports. These checks do not reliably recognize squashes, partial hunks, equivalent rewrites, or reverted historical matches.

Grouping uses shared issue references, topic overlap, hunk signatures, and related source/test filenames. Each inserted commit must relate to every existing member; groups are capped at 12 commits. A shared frequently edited file alone is insufficient. Earlier inspected commits in the same files remain possible context, not proven prerequisites.

Candidate identity hashes sorted patch IDs, with SHA fallback. Exact sets retain identity across rebases and cherry-picks; overlapping unequal sets remain distinct. Containment groups fold smaller patch sets into an anchor only when every member is a subset of that anchor. Partial overlaps do not create transitive groups. Neither containment nor commit grouping proves semantic independence.

Fork ordering counts confirmed clean containment groups, with name breaking ties. Additional blocked work does not reduce that count. A smaller clean variant does not establish that a larger anchor applies. Clean share includes unknown groups in its denominator; forks without usable checks remain unranked. Counts cover the recorded scan and are not recomputed by browser decisions. Repeated sources are provenance, not independent adoption or authorship evidence.

## Measurements and application checks

| Axis | Default bands |
| --- | --- |
| Text churn, additions plus deletions | Small ≤200; medium ≤1,000; large >1,000 |
| Changed paths | Focused ≤5; spread ≤20; broad >20 |
| Distinct patches | Short ≤3; stacked ≤10; long >10 |
| Author recency at scan time | Recent ≤30 days; aging ≤180; historical >180 |

These are adjustable scope descriptions, not quality or adoption predictions. Churn deduplicates equal patch IDs but counts repeated edits by distinct patches; it is not a net branch diff. Binary changes and missing evidence make affected bands unknown. Ahead/behind counts remain raw branch context, with no merge-difficulty cutoff. Test filenames supply no quality bonus.

`inventory --policy FILE` accepts any subset of `small_lines`, `large_lines_above`, `focused_files`, `broad_files_above`, `short_patches`, `long_patches_above`, `recent_days`, and `historical_days_above`. Lower/middle bands include their upper boundary. Invalid fields or reversed boundaries fail validation.

Application checks select an ancestry-consistent series of distinct patches and replay full binary-capable diffs against the pinned target in a temporary index. Direct failure triggers Git's three-way application. Upstream overlap separately compares paths changed since the common ancestor; approximate renames/deletions provide porting context, not automatic repairs.

| Result | Meaning |
| --- | --- |
| `clean` | All selected nonempty patches applied directly |
| `clean-three-way` | The series applied with at least one three-way step |
| `conflicts` | Git left unmerged entries; paths are recorded |
| `not-applicable` | Application failed without unmerged entries; read its diagnostics |
| `unknown` | Missing objects, unsupported ancestry, operation failure, timeout, or inspection limit |
| `no-file-changes` | Selected commits contain no changed paths |

Checks stop at the first failure, so conflict counts do not cover later unattempted patches. Each Git operation has a 60-second timeout and each patch an 8 MiB inspection cap. Temporary bare repositories read cached objects through alternates; writes remain isolated. Configuration, hooks, external merge drivers, replacement refs, and lazy fetching are disabled. Worktrees are untouched. Clean application does not prove complete dependencies or runtime correctness.

Check reuse depends on method/Git version, exact target, commit/patch identities, sources, and possible prerequisites. Unknown or incomplete-overlap results are retried. Changing workers does not invalidate results; changing the target does.

## Model selection and issue evidence

The default workflow uses release recency, verified PR membership, and local application results before spending model calls. It does not use past model judgments, author reputation, stars, feature-specific bonuses, or a combined impact score. Within the release window, fork bundles rotate by patch recency. Equivalent patches use the earliest author date observed in the raw scan; an eligible patch keeps its whole candidate intact. Unknown dates remain eligible. Author timestamps can still be rewritten or have unseen older copies.

PR exclusion needs verified commit membership, patch-equivalent membership known to the scan, or the exact recorded source-branch head. All candidate patches must be covered; partial/uncertain matches remain. Draft open PRs count, closed unmerged PRs do not. Squashes and unrecorded rewrites can escape this filter.

Mapping sees compact candidate metadata and bounded raw issue excerpts. The model nominates coherent features, supporting members, questions, and context requests. Inspection receives every nominated commit's complete diff plus bounded upstream excerpts. Under the clean policy, the assembled set must apply before inspection; independently clean members do not prove their union applies. Oversized/missing complete diffs are deferred intact. Cross-batch reconciliation and arbitrary searches through the rest of a fork are not part of discovery.

Issue retrieval uses explicit references and text relevance to supply context, not to establish demand. The catalog can omit issue comments and truncate bodies/discussion replies; recorded omissions matter. Separate matching checks open upstream issues against inspected features, then confirms proposed connections using complete saved bodies and diffs. Inline matching uses the same evidence requirements and exact catalog fingerprints. A changed snapshot requires new checks; an absent match does not mean useless work.

The optional issue histogram counts distinct open issue documents per word, with discussions only when requested. Votes, repeated words, and duplicate documents do not add weight. The optional demand shortlist instead orders explicit request links by configured priority labels and observed votes, with textual suggestions separate. Legacy `score`/`recommendations` fields and priority commands remain available for explicit use but do not control default reports, prompts, or `run` selection.

## Persistence and limits

API responses have a five-minute cache and ETag revalidation; `--refresh` revalidates immediately. Git objects, commit evidence, upstream indexes, and application facts persist. Repository scans and decision updates are locked; JSON writes are atomic. A successful model result is saved before scheduling more work. Inspection waves reserve shared budgets before launching, retain valid in-flight results after a failure, and display nomination order rather than completion order.

Portable scan patches are capped at 256 KiB with truncation flags; full patches still supply fingerprints and local integration/discovery checks. Non-merge history limits on individual scans and unexamined merge-resolution changes are recorded. Discovery reads complete nominated diffs, while individual review/explanation context may use excerpts. Full reviews sample at most 80 source snapshots, including up to 40 upstream snapshots; each starts at a 64 KiB cap and shares the total prompt budget. Root convention files are included when available; arbitrary dependencies and nested conventions are not automatically loaded.

Reports embed scripts/styles and escape repository/model text. Avatar requests contact GitHub. Browser choices live in local storage until exported to CLI decisions. Raw reports and agent inputs can contain private source code. Stage timings and byte/call counts support diagnosis; concurrent call durations overlap and must not be summed as wall time. Model citations validate source identity, not the correctness of the interpretation.
