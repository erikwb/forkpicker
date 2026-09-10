"use strict";
const D = JSON.parse(document.getElementById("data").textContent),
  app = document.getElementById("app");
const esc = (v) =>
  String(v ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
const safeRepo = (r) => /^[\w.-]+\/[\w.-]+$/.test(r);
const github = (r) => (safeRepo(r) ? `https://github.com/${r}` : null);
const external = (url, text) =>
  /^https:\/\/github\.com\//.test(url ?? "")
    ? `<a href="${esc(url)}">${esc(text)}</a>`
    : esc(text);
const delta = (e) =>
  `<span class="delta"><span class="plus">+${e.additions}</span> / <span class="minus">−${e.deletions}</span></span>`;
const repoLink = (r) =>
  github(r)
    ? `<a class="repo" href="${esc(github(r))}"><img class="avatar" src="https://github.com/${esc(r.split("/")[0])}.png?size=48" alt="" loading="lazy" referrerpolicy="no-referrer">${esc(r)}</a>`
    : esc(r);
const forkRoutes = new Map(D.forks.map((fork, i) => [fork, `#fork/${i}`]));
const forkRoute = (r) => forkRoutes.get(r);
const members = (g) => g.members.map((m) => D.entries[m.index]).filter(Boolean);
const sources = (g) => [...new Set(members(g).flatMap((e) => e.forks))];
const stageLabel = (s) =>
  ({
    inspected: "Code inspected",
    mapped: "Metadata hypothesis",
    uncertain: "Needs more context",
    screened: "Screened · not inspected",
    unseen: "Not screened",
  })[s] || s;
const stageBadge = (s) =>
  s === "inspected" ? "" : `<span class="tag">${esc(stageLabel(s))}</span>`;
const inWindow = (e) => !D.window || e.activity?.scope !== "outside";
const applyStatus = (e) =>
  e.integration?.target_sha === D.base ? e.integration.status : "not-checked";
const clean = (e) => ["clean", "clean-three-way"].includes(applyStatus(e));
const work = (e) => ["conflicts", "not-applicable"].includes(applyStatus(e));
const applicationMatch = (e, filter) =>
  filter === "all" ||
  (filter === "clean"
    ? clean(e)
    : filter === "work"
      ? work(e)
      : !clean(e) && !work(e));
const inPool = (e) =>
  inWindow(e) && (D.application_pool?.policy !== "clean" || clean(e));
const forkCounts = new Map();
for (const entry of D.entries) {
  if (!inPool(entry)) continue;
  for (const fork of new Set(entry.forks)) {
    forkCounts.set(fork, (forkCounts.get(fork) || 0) + 1);
  }
}
function applicationSummary() {
  const p = D.application_pool;
  if (!p) return "";
  const c = (p.statuses.clean || 0) + (p.statuses["clean-three-way"] || 0),
    w = (p.statuses.conflicts || 0) + (p.statuses["not-applicable"] || 0),
    u = p.before_application - c - w;
  return `<p class="muted"><a href="#catalog?application=clean">${c} apply cleanly</a> · <a href="#catalog?application=work">${w} require integration work</a> · <a href="#catalog?application=unknown">${u} application unknown</a></p>`;
}
function assembled(g) {
  const a = g.assembled;
  if (!a) return "";
  const c = a.check;
  return `<div class="section"><h2>Assembled patch application</h2><p>${esc(c ? { clean: "Applies cleanly", "clean-three-way": "Applies with a clean three-way merge", conflicts: "Conflicts", "not-applicable": "Did not apply", unknown: "Could not establish application" }[c.status] || c.status : a.error)}</p>${c?.conflicting_files?.length ? `<p>${c.conflicting_files.map(esc).join(", ")}</p>` : ""}<p class="muted">Upstream: ${esc(D.base.slice(0, 10))}</p>${c?.status === "unknown" ? `<p>${esc(c.notes.join(" "))}</p>` : ""}</div>`;
}
const shortDate = (s) => String(s || "").slice(0, 10);
const patchDate = (e) =>
  e.activity
    ? `<span title="Earliest observed author date per identical patch">${e.activity.scope === "unknown" ? "Patch date incomplete" : `Latest patch ${esc(shortDate(e.activity.latest_patch_date))}`}</span>`
    : "";
const relativeTime = new Intl.RelativeTimeFormat("en", { numeric: "always" });
function featureDate(g) {
  const dates = members(g)
    .flatMap((e) => e.commits)
    .map((c) => Date.parse(c.date))
    .filter(Number.isFinite);
  if (!dates.length)
    return '<span class="feature-date">Date unavailable</span>';
  const latest = Math.max(...dates),
    seconds = (latest - Date.now()) / 1000;
  const units = [
    ["year", 31536000],
    ["month", 2592000],
    ["day", 86400],
    ["hour", 3600],
    ["minute", 60],
  ];
  const unit = units.find(([, size]) => Math.abs(seconds) >= size);
  const label = unit
    ? relativeTime.format(Math.trunc(seconds / unit[1]), unit[0])
    : "just now";
  return `<time class="feature-date" datetime="${new Date(latest).toISOString()}">${esc(label)}</time>`;
}
function windowSummary() {
  const w = D.window;
  return w
    ? `<p class="muted">${external(w.release_url, w.tag)} published ${esc(shortDate(w.published_at))} · Work since ${esc(shortDate(w.since))} through ${esc(shortDate(w.as_of))} (${esc(w.overlap)} overlap)</p><p class="muted"><a href="#catalog?application=all">${w.recent + w.unknown} recent changes</a>${w.unknown ? ` (${w.unknown} with incomplete dates)` : ""} · <a href="#catalog?scope=outside">${w.outside} older changes outside this run</a></p>`
    : "";
}
const evKeys = Object.keys(D.evidence);
const evidenceRoutes = new Map(evKeys.map((id, i) => [id, `#evidence/${i}`]));
const evRoute = (id) => evidenceRoutes.get(id);
function citations(ids) {
  return `<div class="evidence">${[...new Set(ids || [])]
    .map((id) => {
      const e = D.evidence[id];
      if (e)
        return `<a href="${evRoute(id)}">${esc(e.kind === "upstream" ? "Upstream: " + e.title : e.kind === "issue" ? e.title : "Diff: " + e.title)}</a>`;
      if (id.startsWith("commit:")) {
        const sha = id.slice(7),
          entry = D.entries.find((e) => e.commits.some((c) => c.sha === sha));
        if (entry)
          return external(
            `${github(entry.forks[0])}/commit/${sha}`,
            entry.commits.find((c) => c.sha === sha).title,
          );
      }
      return "";
    })
    .join("")}</div>`;
}
function commits(e) {
  return e.commits
    .map(
      (c) =>
        `<div class="row"><div class="commit">${external(`${github(e.forks[0])}/commit/${c.sha}`, c.title)}${delta(c)}</div>${D.evidence[c.evidence] ? `<a class="muted" href="${evRoute(c.evidence)}">Read inspected diff</a>` : ""}</div>`,
    )
    .join("");
}
const issueRelation = (r) =>
  ({
    likely_addresses: "May address",
    partially_addresses: "May partly address",
    related: "Related to",
  })[r] || "Related to";
const issueLink = (m) =>
  `${issueRelation(m.relation)} ${external(m.issue_url, "#" + m.issue_url.split("/").pop() + " " + m.title)}`;
function issueCard(g, i) {
  const matches = g.issue_matches || [];
  return matches.length
    ? `<div class="issue-links">${matches
        .slice(0, 2)
        .map((m) => `<p>${issueLink(m)}</p>`)
        .join(
          "",
        )}${matches.length > 2 ? `<a href="#feature/${i}">${matches.length - 2} more issue connections</a>` : ""}</div>`
    : "";
}
function issueDetails(g) {
  return g.issue_matches?.length
    ? `<div class="section"><h2>Open issues</h2>${g.issue_matches.map((m) => `<div class="row"><p>${issueLink(m)}</p><p>${esc(m.reason.text)}</p>${citations(m.reason.evidence)}</div>`).join("")}</div>`
    : "";
}
function groupCard(g, i) {
  const forks = sources(g),
    count = members(g).length;
  return `<article class="card"><div class="meta card-sources">${forks.slice(0, 3).map(repoLink).join(" ")}${forks.length > 3 ? `<span>+${forks.length - 3} forks</span>` : ""}<span aria-hidden="true">·</span>${featureDate(g)}</div><h2><a href="#feature/${i}">${esc(g.title)}</a></h2><div class="meta"><span>${count} change${count === 1 ? "" : "s"}</span>${stageBadge(g.stage)}${g.excluded ? '<span class="tag">Excluded after inspection</span>' : ""}</div><p>${esc(g.summary)}</p>${issueCard(g, i)}</article>`;
}

const pageSize = 40;
function pager(total, p, params, path) {
  const link = (n, t) => {
    const q = new URLSearchParams(params);
    q.set("page", n);
    return `<a href="#${path}?${q}">${t}</a>`;
  };
  return total > pageSize
    ? `<div class="pagination">${p ? link(p - 1, "← Previous") : ""}<span>${p * pageSize + 1}–${Math.min(total, (p + 1) * pageSize)} of ${total}</span>${(p + 1) * pageSize < total ? link(p + 1, "Next →") : ""}</div>`
    : "";
}
function toolbar(q, options, stage, excluded = false) {
  return `<div class="toolbar"><input aria-label="Search" type="search" id="search" placeholder="Search features, forks, files…" value="${esc(q)}"><select id="stage" aria-label="Filter by inspection status">${options.map(([v, t]) => `<option value="${v}" ${stage === v ? "selected" : ""}>${t}</option>`).join("")}</select>${excluded ? '<label><input id="excluded" type="checkbox"> Show excluded</label>' : ""}</div>`;
}
function route() {
  const [path = "features", raw = ""] = location.hash.slice(1).split("?"),
    [type, id] = path.split("/"),
    params = new URLSearchParams(raw),
    q = (params.get("q") || "").toLowerCase(),
    stage = params.get("stage") || "all";
  let page = Math.max(0, parseInt(params.get("page") || "0", 10) || 0),
    html = "";
  const match = (e) => JSON.stringify(e).toLowerCase().includes(q);
  document
    .querySelectorAll("nav a")
    .forEach((a) =>
      a.classList.toggle(
        "active",
        a.hash ===
          `#${["feature", "evidence", "change"].includes(type) ? "features" : type === "fork" ? "forks" : type}`,
      ),
    );
  if (type === "features" || !type) {
    let list = D.groups
      .map((g, i) => ({ g, i }))
      .filter(
        ({ g }) =>
          (!g.excluded || params.get("excluded") === "1") &&
          (stage === "all" || g.stage === stage) &&
          match({
            ...g,
            forks: sources(g),
            paths: members(g).flatMap((e) => e.paths),
          }),
      );
    page = Math.min(page, Math.max(0, Math.ceil(list.length / pageSize) - 1));
    html = `<h1>${esc(D.repository)} · Fork features</h1><p class="muted">${D.screened} of ${D.application_pool ? D.application_pool.eligible : D.window ? D.window.recent + D.window.unknown : D.entries.length} changes screened · ${D.inspected} inspected</p>${windowSummary()}${applicationSummary()}${toolbar(
      q,
      [
        ["all", "All features"],
        ["inspected", "Code inspected"],
        ["mapped", "Awaiting inspection"],
        ["uncertain", "Needs more context"],
      ],
      stage,
      D.groups.some((g) => g.excluded),
    )}<p class="muted">${list.length} features</p><div class="cards">${
      list
        .slice(page * pageSize, (page + 1) * pageSize)
        .map(({ g, i }) => groupCard(g, i))
        .join("") ||
      "<p>No features match. The full change catalog remains available.</p>"
    }</div>${pager(list.length, page, params, path)}`;
  } else if (type === "feature" && D.groups[id]) {
    const g = D.groups[id],
      fs = sources(g);
    html = `<a class="back" href="#features">← Features</a><section class="source-forks"><h2>Source forks</h2>${fs.map((f) => `<div class="row source-fork-row">${repoLink(f)}<a href="${forkRoute(f)}">Browse fork changes</a></div>`).join("")}</section><div class="meta">${stageBadge(g.stage)}</div><h1>${esc(g.title)}</h1><p>${esc(g.summary)}</p>${issueDetails(g)}${assembled(g)}${g.question ? `<div class="section"><h2>Question for inspection</h2><p>${esc(g.question)}</p></div>` : ""}<div class="section"><h2>Patch set</h2><div class="panel">${g.members
      .map((m) => {
        const e = D.entries[m.index];
        return `<div class="row"><h3><a href="#change/${m.index}">${esc(e.title)}</a></h3><div class="meta"><span>${esc(m.role)}</span>${delta(e)}<span>${e.paths.length} files</span>${patchDate(e)}</div>${commits(e)}</div>`;
      })
      .join(
        "",
      )}</div></div>${g.claims?.length ? `<div class="section"><h2>Evidence and assessment</h2>${g.claims.map((c) => `<div class="row"><p>${esc(c.text)}</p>${citations(c.evidence)}</div>`).join("")}</div>` : ""}${g.relations?.length ? `<div class="section"><h2>How the changes relate</h2>${g.relations.map((r) => `<p>${esc(r.text)}</p>${citations(r.evidence)}`).join("")}</div>` : ""}`;
  } else if (type === "forks") {
    const fs = D.forks
      .map((f, i) => ({
        f,
        i,
        count: forkCounts.get(f) || 0,
      }))
      .filter((x) => x.count > 0 && x.f.toLowerCase().includes(q));
    page = Math.min(page, Math.max(0, Math.ceil(fs.length / pageSize) - 1));
    html = `<h1>Forks</h1>${toolbar(q, [["all", "All forks"]], stage)}<p class="muted">${fs.length} forks with eligible changes</p><div class="fork-grid">${fs
      .slice(page * pageSize, (page + 1) * pageSize)
      .map(
        ({ f, i, count }) =>
          `<article class="card"><h2>${repoLink(f)}</h2><a href="#fork/${i}">${count} changes · Browse fork →</a></article>`,
      )
      .join("")}</div>${pager(fs.length, page, params, path)}`;
  } else if (type === "catalog" || (type === "fork" && D.forks[id])) {
    const f = type === "fork" ? D.forks[id] : null;
    const scope = params.get("scope") || "recent";
    const application =
      params.get("application") ||
      (["outside", "all"].includes(scope)
        ? "all"
        : D.application_pool?.policy === "clean"
          ? "clean"
          : "all");
    const list = D.entries
      .map((e, i) => ({ e, i }))
      .filter(
        ({ e }) =>
          applicationMatch(e, application) &&
          (!D.window ||
            scope === "all" ||
            (scope === "outside" ? !inWindow(e) : inWindow(e))) &&
          (!f || e.forks.includes(f)) &&
          (stage === "all" || e.stage === stage) &&
          (!e.excluded || params.get("excluded") === "1") &&
          match(e),
      );
    if (D.window)
      list.sort(
        (a, b) =>
          (Date.parse(b.e.activity?.latest_patch_date) || 0) -
          (Date.parse(a.e.activity?.latest_patch_date) || 0),
      );
    page = Math.min(page, Math.max(0, Math.ceil(list.length / pageSize) - 1));
    const gs =
      f && (!D.window || scope !== "outside")
        ? D.groups
            .map((g, i) => ({ g, i }))
            .filter(({ g }) => !g.excluded && sources(g).includes(f))
        : [];
    html = `${f ? '<a class="back" href="#forks">← Forks</a>' : ""}<h1>${f ? repoLink(f) : "All changes"}</h1>${f ? "" : windowSummary()}${
      !f && D.window
        ? `<label>Window <select id="window-scope" aria-label="Filter by date window">${[
            ["recent", "Release window"],
            ["outside", "Older changes"],
            ["all", "All history"],
          ]
            .map(
              ([v, t]) =>
                `<option value="${v}" ${scope === v ? "selected" : ""}>${t}</option>`,
            )
            .join("")}</select></label>`
        : ""
    }${
      !f && D.application_pool
        ? `<label>Application <select id="application-filter" aria-label="Filter by patch application">${[
            ["clean", "Applies cleanly"],
            ["work", "Requires integration work"],
            ["unknown", "Unknown application"],
            ["all", "All results"],
          ]
            .map(
              ([v, t]) =>
                `<option value="${v}" ${application === v ? "selected" : ""}>${t}</option>`,
            )
            .join("")}</select></label>`
        : ""
    }${gs.length ? `<div class="cards">${gs.map(({ g, i }) => groupCard(g, i)).join("")}</div>` : ""}${
      f
        ? ""
        : toolbar(
            q,
            [
              ["all", "All changes"],
              ["unseen", "Not screened"],
              ["screened", "Screened · not inspected"],
              ["inspected", "Code inspected"],
            ],
            stage,
            D.entries.some((e) => e.excluded),
          )
    }<p class="muted">${list.length} changes</p><div class="cards">${
      list
        .slice(page * pageSize, (page + 1) * pageSize)
        .map(
          ({ e, i }) =>
            `<article class="card">${f ? "" : `<div class="meta card-sources">${e.forks.slice(0, 2).map(repoLink).join(" ")}</div>`}<h2><a href="#change/${i}">${esc(e.title)}</a></h2><div class="meta">${stageBadge(e.stage)}${delta(e)}<span>${e.paths.length} files</span>${patchDate(e)}</div></article>`,
        )
        .join("") || "<p>No changes match.</p>"
    }</div>${pager(list.length, page, params, path)}`;
  } else if (type === "change" && D.entries[id]) {
    const e = D.entries[id],
      gs = D.groups
        .map((g, i) => ({ g, i }))
        .filter(({ g }) => g.members.some((m) => String(m.index) === id));
    html = `<a class="back" href="#catalog">← All changes</a><h1>${esc(e.title)}</h1><div class="meta">${stageBadge(e.stage)}${delta(e)}<span>${e.paths.length} files</span>${patchDate(e)}</div>${gs.map(({ g, i }) => `<p>Feature: <a href="#feature/${i}">${esc(g.title)}</a></p>`).join("")}<div class="section"><h2>Commits</h2><div class="panel">${commits(e)}</div></div>${e.issues.length ? `<div class="section"><h2>Linked issues and discussions</h2>${e.issues.map((url) => `<p>${external(url, "#" + url.split("/").pop())}</p>`).join("")}</div>` : ""}${e.integration ? `<div class="section"><h2>Application to saved upstream</h2><p>${esc({ clean: "Patch applies cleanly", "clean-three-way": "Patch applies with a clean three-way merge", conflicts: "Patch has conflicts", "not-applicable": "Patch did not apply" }[e.integration.status] || e.integration.status)} · ${esc(D.base.slice(0, 10))}</p></div>` : ""}<div class="section"><h2>Changed files</h2>${e.paths.map((p) => `<div><code>${esc(p)}</code></div>`).join("")}</div><div class="section"><h2>Available in</h2>${e.forks.map((f) => `<div class="row source-fork-row">${repoLink(f)}<a href="${forkRoute(f)}">Browse fork</a></div>`).join("")}</div>`;
  } else if (type === "evidence" && D.evidence[evKeys[id]]) {
    const e = D.evidence[evKeys[id]];
    html = `<button class="back" id="back">← Back</button><div class="meta"><span class="tag">${esc(e.kind)}</span></div><h1>${esc(e.title)}</h1>${e.url ? external(e.url, "View on GitHub") : ""}${e.truncated ? '<p class="notice">This is a partial excerpt.</p>' : ""}<pre>${esc(e.content)}</pre>${e.comments?.length ? `<h2>Saved replies</h2>${e.comments.map((c) => `<div class="section"><p>${external(c.url, c.author)}</p><pre>${esc(c.body)}</pre>${c.body_truncated ? "<small>Partial excerpt</small>" : ""}</div>`).join("")}` : ""}${e.comments_omitted ? `<p class="muted">${e.comments_omitted} replies not included in this context.</p>` : ""}`;
  } else if (type === "activity") {
    html = `<a class="back" href="#features">← Features</a><h1>Run activity</h1><div class="panel"><p>${D.calls} new calls · ${D.reused} cached results reused</p><p>${D.bytes.toLocaleString()} serialized input bytes · ${D.output_bytes.toLocaleString()} response bytes · ${D.seconds.toFixed(1)} seconds</p><p>${esc(D.stop)}</p><p class="muted">Bytes are measured request/response sizes, not provider tokens or subscription charges.</p>${D.errors.map((e) => `<p>${esc(e)}</p>`).join("")}</div><p>${D.screened} / ${D.entries.length} candidates screened. ${D.inspected} inspected. Deferred candidates remain eligible for investigation.</p>`;
  } else
    html = '<h1>Page not found</h1><a href="#features">Return to features</a>';
  app.innerHTML = html;
  const search = document.getElementById("search");
  const update = (name, val) => {
    const next = new URLSearchParams(params);
    if (val) next.set(name, val);
    else next.delete(name);
    next.delete("page");
    history.replaceState(null, "", `#${path}?${next}`);
    const pos = search?.selectionStart;
    route();
    if (name === "q") {
      const input = document.getElementById("search");
      input?.focus();
      if (input?.value) input.setSelectionRange(pos, pos);
    }
  };
  search?.addEventListener("input", () => update("q", search.value));
  document
    .getElementById("application-filter")
    ?.addEventListener("change", (e) => update("application", e.target.value));
  document
    .getElementById("window-scope")
    ?.addEventListener("change", (e) => update("scope", e.target.value));
  document
    .getElementById("stage")
    ?.addEventListener("change", (e) => update("stage", e.target.value));
  const excluded = document.getElementById("excluded");
  if (excluded) {
    excluded.checked = params.get("excluded") === "1";
    excluded.addEventListener("change", () =>
      update("excluded", excluded.checked ? "1" : ""),
    );
  }
  document.getElementById("back")?.addEventListener("click", () => {
    if (history.length > 1) history.back();
    else location.hash = "features";
  });
  document.title = `${app.querySelector("h1")?.textContent || "Features"} · Forkpicker`;
}
window.addEventListener("hashchange", () => {
  route();
  window.scrollTo(0, 0);
});
route();
