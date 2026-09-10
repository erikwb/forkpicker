"use strict";
const config = JSON.parse(document.querySelector("#inbox-config").textContent);
const container = document.querySelector("#candidates"),
  cards = [...container.querySelectorAll("article")];
const byId = new Map(cards.map((c) => [c.id, c]));
const fields = Object.fromEntries(
  [
    "search",
    "application",
    "observation",
    "status",
    "size",
    "sort",
    "group",
    "empty",
  ].map((id) => [id, document.getElementById(id)]),
);
const searchText = new Map(cards.map((c) => [c, c.textContent.toLowerCase()]));
const key = "forkpicker-inbox-v1:" + config.repository.toLowerCase();
let decisions = {},
  lastUndo = null,
  storageAvailable = true,
  noticeTimer;
try {
  decisions = JSON.parse(localStorage.getItem(key) || "{}");
  if (!decisions || Array.isArray(decisions) || typeof decisions !== "object")
    decisions = {};
} catch {
  storageAvailable = false;
  decisions = {};
}
const decisionStatuses = new Set(["saved", "dismissed"]);
function validDecision(d) {
  return (
    d &&
    decisionStatuses.has(d.status) &&
    typeof d.reason === "string" &&
    d.reason.trim()
  );
}
for (const c of cards) {
  c.dataset.originalStatus = c.dataset.status;
  if (validDecision(decisions[c.id])) {
    c.dataset.status = decisions[c.id].status;
    c.querySelector(".decision-label").textContent =
      decisions[c.id].status + " · browser";
  }
}
let selectedFork = null,
  currentView = "forks",
  navigationReady = false,
  restoringNavigation = false,
  lastHandledURL = "",
  navigationVersion = 0;
const forkMap = new Map((config.forks || []).map((f) => [f.repository, f]));
const families = new Map();
function buildFamilies() {
  families.clear();
  const memberFamilies = new Map(
    selectedFork
      ? selectedFork.groups.flatMap((g) =>
          g.candidate_ids.map((id) => [id, g.anchor]),
        )
      : [],
  );
  for (const c of cards) {
    const id = memberFamilies.get(c.id) || c.dataset.family;
    if (!families.has(id)) {
      const el = document.createElement("section");
      el.className = "family";
      const bar = document.createElement("div");
      bar.className = "familybar";
      const label = document.createElement("span"),
        buttons = document.createElement("div"),
        toggle = document.createElement("button"),
        compare = document.createElement("button");
      toggle.type = compare.type = "button";
      compare.textContent = "Compare";
      buttons.append(toggle, compare);
      bar.append(label, buttons);
      el.append(bar);
      families.set(id, {
        id,
        el,
        bar,
        label,
        toggle,
        compare,
        cards: [],
        expanded: false,
        matched: [],
      });
    }
    const family = families.get(id);
    family.cards.push(c);
    family.el.append(c);
  }
  container.replaceChildren();
  for (const family of families.values()) {
    container.append(family.el);
    family.toggle.onclick = () => {
      family.expanded = !family.expanded;
      filter();
    };
    family.compare.onclick = () => compareFamily(family);
  }
}
buildFamilies();
function persist() {
  try {
    localStorage.setItem(key, JSON.stringify(decisions));
  } catch {
    storageAvailable = false;
  }
}
function notice(message, undo = false) {
  clearTimeout(noticeTimer);
  const box = document.querySelector("#toast");
  box.replaceChildren(document.createTextNode(message));
  if (undo) {
    const button = document.createElement("button");
    button.textContent = "Undo";
    button.onclick = () => {
      if (!lastUndo) return;
      const { id, previous } = lastUndo;
      if (previous) decisions[id] = previous;
      else delete decisions[id];
      updateDecision(byId.get(id));
      persist();
      lastUndo = null;
      filter();
      box.hidden = true;
    };
    box.append(button);
  }
  box.hidden = false;
  noticeTimer = setTimeout(() => {
    box.hidden = true;
  }, 15000);
}
function updateDecision(c) {
  const d = decisions[c.id];
  c.dataset.status = validDecision(d) ? d.status : c.dataset.originalStatus;
  c.querySelector(".decision-label").textContent = validDecision(d)
    ? d.status + " · browser"
    : c.dataset.status === "new"
      ? "Undecided"
      : c.dataset.status === "updated"
        ? "Decision needs revisit"
        : c.dataset.status;
}
function compareCards(a, b) {
  const key = fields.sort.value,
    av = a.dataset[key],
    bv = b.dataset[key];
  if (av === "" && bv !== "") return 1;
  if (bv === "" && av !== "") return -1;
  return (
    Number(av) - Number(bv) ||
    Number(a.dataset.original) - Number(b.dataset.original)
  );
}
function matches(c, words) {
  const status = fields.status.value,
    application = fields.application.value,
    observation = fields.observation.value;
  return (
    (!selectedFork || selectedFork.candidate_ids.includes(c.id)) &&
    words.every((w) => searchText.get(c).includes(w)) &&
    (status
      ? c.dataset.status === status
      : config.include_inactive ||
        !["dismissed", "adopted"].includes(c.dataset.status)) &&
    (!fields.size.value || c.dataset.size === fields.size.value) &&
    (fields.empty.checked || c.dataset.size !== "no-file-changes") &&
    (!observation ||
      (observation === "changed"
        ? ["extended", "regrouped"].includes(c.dataset.observation)
        : c.dataset.observation === observation)) &&
    (!application ||
      (application === "applicable"
        ? ["clean", "clean-three-way"].includes(c.dataset.application)
        : application === "blocked"
          ? ["conflicts", "not-applicable"].includes(c.dataset.application)
          : c.dataset.application === application))
  );
}
function filter() {
  const words = fields.search.value.toLowerCase().split(/\s+/).filter(Boolean);
  let matched = 0,
    shown = 0,
    groups = 0;
  const sorted = [];
  for (const family of families.values()) {
    family.matched = family.cards
      .filter((c) => matches(c, words))
      .sort(compareCards);
    for (const c of family.cards) c.hidden = true;
    family.el.hidden = !family.matched.length;
    if (!family.matched.length) continue;
    matched += family.matched.length;
    groups++;
    const expanded = !fields.group.checked || family.expanded;
    family.matched.forEach((c, i) => {
      family.el.append(c);
      c.hidden = !expanded && i > 0;
      if (!c.hidden) shown++;
    });
    family.bar.hidden = family.matched.length < 2;
    family.label.textContent =
      family.matched.length + " patch-set variants · shared patch identities";
    family.toggle.textContent = expanded ? "Collapse" : "Show variants";
    family.toggle.hidden = !fields.group.checked;
    family.toggle.setAttribute("aria-expanded", String(expanded));
    sorted.push(family);
  }
  sorted.sort((a, b) => compareCards(a.matched[0], b.matched[0]));
  const fragment = document.createDocumentFragment();
  sorted.forEach((f) => fragment.append(f.el));
  container.append(fragment);
  document.querySelector("#count").textContent =
    `${matched.toLocaleString()} matching patch sets · ${groups.toLocaleString()} groups · ${(matched - shown).toLocaleString()} variant${matched - shown === 1 ? "" : "s"} folded`;
  document.querySelector("#no-results").hidden = matched > 0;
  const count = Object.entries(decisions).filter(
    ([id, d]) => byId.has(id) && validDecision(d),
  ).length;
  document.querySelector("#local-count").textContent =
    count + " browser decisions";
  document.querySelector("#local-count").hidden = count === 0;
  document.querySelector("#export-decisions").disabled = count === 0;
  rememberNavigation();
}
for (const [id, el] of Object.entries(fields))
  el.addEventListener(id === "search" ? "input" : "change", () => {
    if (id === "application" && el.value === "no-file-changes")
      fields.empty.checked = true;
    filter();
  });
for (const b of document.querySelectorAll("[data-quick]"))
  b.onclick = () => {
    fields.application.value = b.dataset.quick;
    filter();
  };
const dialog = document.querySelector("#action-dialog"),
  body = document.querySelector("#dialog-body");
function openDialog(title) {
  document.querySelector("#dialog-title").textContent = title;
  body.replaceChildren();
  dialog.showModal();
}
document.querySelector("#dialog-close").onclick = () => dialog.close();
function paragraph(value) {
  const p = document.createElement("p");
  p.textContent = value;
  body.append(p);
  return p;
}
function input(label, value = "", tag = "input") {
  const wrapper = document.createElement("label");
  wrapper.textContent = label;
  const input = document.createElement(tag);
  input.value = value;
  wrapper.append(input);
  body.append(wrapper);
  return input;
}
function button(label, handler) {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  b.onclick = handler;
  body.append(b);
  return b;
}
const quote = (s) => "'" + String(s).replace(/'/g, "'\\''") + "'";
async function copy(text, area) {
  try {
    await navigator.clipboard.writeText(text);
    notice("Copied.");
  } catch {
    area.focus();
    area.select();
    notice("Select and copy the command text.");
  }
}
function decisionDialog(c, status) {
  openDialog(
    (status === "saved" ? "Save: " : "Dismiss: ") +
      c.querySelector("h2").textContent,
  );
  paragraph(
    "Stored in this browser. Export decisions to apply them to Forkpicker.",
  );
  const reason = input(
    "Reason",
    validDecision(decisions[c.id])
      ? decisions[c.id].reason
      : status === "saved"
        ? "Saved for follow-up"
        : "",
    "textarea",
  );
  reason.required = true;
  button(status === "saved" ? "Save locally" : "Dismiss locally", () => {
    reason.setCustomValidity(reason.value.trim() ? "" : "Enter a reason.");
    if (!reason.reportValidity()) return;
    lastUndo = { id: c.id, previous: decisions[c.id] };
    decisions[c.id] = {
      status,
      reason: reason.value.trim(),
      recorded_at: new Date().toISOString(),
    };
    updateDecision(c);
    persist();
    dialog.close();
    filter();
    notice(
      storageAvailable
        ? "Decision saved in this browser. Export to update Forkpicker."
        : "Decision kept for this page only; browser storage unavailable. Export before closing.",
      true,
    );
  });
}
function reviewDialog(c) {
  openDialog("Prepare a review");
  paragraph(c.querySelector("h2").textContent);
  paragraph(
    "One candidate, one-call cap. This command previews the review without calling a model. Actual review uses your chosen CLI’s authentication and billing.",
  );
  const path = input("Scan JSON", config.report_path || "");
  path.required = true;
  const agent = input("Agent CLI adapter", "codex");
  agent.setAttribute("list", "agents");
  agent.required = true;
  const area = input("Dry-run command", "", "textarea");
  area.className = "commands";
  area.readOnly = true;
  const update = () => {
    area.value =
      "forkpicker review " +
      quote(path.value) +
      " " +
      quote(c.id) +
      " --agent " +
      quote(agent.value) +
      " --limit 1 --dry-run";
  };
  path.oninput = agent.oninput = update;
  update();
  paragraph("Inspect the preview before removing --dry-run to run a review.");
  button("Copy preview command", () => {
    if (path.reportValidity() && agent.reportValidity()) copy(area.value, area);
  });
}
function compareFamily(family) {
  openDialog("Compare patch-set variants");
  paragraph(
    "These sets are contained in a common larger patch set. Shared patches do not prove identical behavior. Choose a row to inspect its evidence.",
  );
  const wrap = document.createElement("div");
  wrap.className = "tablewrap";
  const table = document.createElement("table");
  const head = document.createElement("tr");
  for (const title of [
    "Candidate",
    "Lines",
    "Files",
    "Patches",
    "Application",
  ]) {
    const th = document.createElement("th");
    th.textContent = title;
    head.append(th);
  }
  table.append(head);
  for (const c of family.matched) {
    const row = document.createElement("tr"),
      first = document.createElement("td"),
      link = document.createElement("button");
    link.textContent = c.querySelector("h2").textContent;
    link.onclick = () => {
      family.expanded = true;
      dialog.close();
      filter();
      c.scrollIntoView({ block: "start" });
    };
    first.append(link);
    row.append(first);
    for (const value of [
      c.dataset.lines || "Unknown",
      c.dataset.files || "Unknown",
      c.dataset.patches,
      c.dataset.applicationLabel,
    ]) {
      const td = document.createElement("td");
      td.textContent = value;
      row.append(td);
    }
    table.append(row);
  }
  wrap.append(table);
  body.append(wrap);
}
function exportDecisions() {
  openDialog("Export browser decisions");
  paragraph(
    "These commands persist the decisions in Forkpicker. Review them before running. Only choices for patch sets in this dashboard are included.",
  );
  const path = input("Scan JSON", config.report_path || "");
  path.required = true;
  const area = input("Commands", "", "textarea");
  area.className = "commands";
  area.readOnly = true;
  const update = () => {
    area.value = Object.entries(decisions)
      .filter(([id, d]) => byId.has(id) && validDecision(d))
      .map(
        ([id, d]) =>
          "forkpicker " +
          (config.decision_state_dir
            ? "--state-dir " + quote(config.decision_state_dir) + " "
            : "") +
          "decide " +
          quote(path.value) +
          " " +
          quote(id) +
          " --status " +
          quote(d.status) +
          " --reason " +
          quote(d.reason),
      )
      .join("\n");
  };
  path.oninput = update;
  update();
  button("Copy commands", () => {
    if (path.reportValidity()) copy(area.value, area);
  });
  button("Download commands", () => {
    if (!path.reportValidity()) return;
    const url = URL.createObjectURL(
        new Blob(["#!/bin/sh\nset -e\n" + area.value + "\n"], {
          type: "text/plain",
        }),
      ),
      a = document.createElement("a");
    a.href = url;
    a.download = "forkpicker-decisions.sh";
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  });
}
document.querySelector("#export-decisions").onclick = exportDecisions;
container.addEventListener("click", (event) => {
  const b = event.target.closest("[data-action]");
  if (!b) return;
  const c = b.closest("article");
  if (b.dataset.action === "review") reviewDialog(c);
  else decisionDialog(c, b.dataset.action === "save" ? "saved" : "dismissed");
});
if (config.include_inactive)
  fields.status.options[0].textContent = "All statuses";

const forkRows = [...document.querySelectorAll("#fork-rows tr")];
let forkLimit = 50;
function setView(view) {
  currentView = view;
  document.querySelector("#fork-view").hidden = view !== "forks";
  document.querySelector("#patch-view").hidden = view !== "patches";
  document
    .querySelector("#show-forks")
    .setAttribute("aria-pressed", String(view === "forks"));
  document
    .querySelector("#show-patches")
    .setAttribute("aria-pressed", String(view === "patches"));
}
function applyFork(repository, reset = false) {
  selectedFork = repository ? forkMap.get(repository) || null : null;
  buildFamilies();
  document.querySelector("#selected-fork").hidden = !selectedFork;
  document.querySelector("#selected-fork-name").textContent = selectedFork
    ? selectedFork.repository
    : "";
  const link = document.querySelector("#selected-fork-github");
  const valid =
    selectedFork &&
    /^[a-z0-9_.-]+\/[a-z0-9_.-]+$/i.test(selectedFork.repository);
  link.hidden = !valid;
  if (valid) link.href = "https://github.com/" + selectedFork.repository;
  else link.removeAttribute("href");
  if (reset)
    for (const id of ["search", "application", "observation", "size", "status"])
      fields[id].value = "";
}
function filterForks() {
  const words = document
      .querySelector("#fork-search")
      .value.toLowerCase()
      .split(/\s+/)
      .filter(Boolean),
    sort = document.querySelector("#fork-sort").value;
  const rows = forkRows.filter((r) =>
    words.every((w) => r.dataset.repository.includes(w)),
  );
  rows.sort((a, b) => {
    if (sort === "name")
      return a.dataset.repository.localeCompare(b.dataset.repository);
    const av = a.dataset[sort],
      bv = b.dataset[sort];
    if (av === "" && bv !== "") return 1;
    if (bv === "" && av !== "") return -1;
    return (
      Number(bv) - Number(av) ||
      a.dataset.repository.localeCompare(b.dataset.repository)
    );
  });
  for (const r of forkRows) r.hidden = true;
  const fragment = document.createDocumentFragment();
  rows.forEach((r, i) => {
    r.hidden = i >= forkLimit;
    fragment.append(r);
  });
  document.querySelector("#fork-rows").append(fragment);
  document.querySelector("#fork-count").textContent =
    `${Math.min(forkLimit, rows.length).toLocaleString()} of ${rows.length.toLocaleString()} matching forks`;
  document.querySelector("#more-forks").hidden = rows.length <= forkLimit;
  rememberNavigation();
}

function navigationSnapshot() {
  return {
    forkpicker: 1,
    repository: config.repository,
    view: currentView,
    fork: selectedFork ? selectedFork.repository : null,
    filters: Object.fromEntries(
      Object.entries(fields).map(([id, el]) => [
        id,
        el.type === "checkbox" ? el.checked : el.value,
      ]),
    ),
    forkSearch: document.querySelector("#fork-search").value,
    forkSort: document.querySelector("#fork-sort").value,
    forkLimit,
    expanded: [...families.values()].filter((f) => f.expanded).map((f) => f.id),
    scrollY: window.scrollY,
  };
}
function rememberNavigation() {
  if (!navigationReady || restoringNavigation) return;
  history.replaceState(navigationSnapshot(), "");
}
function navigate(
  view,
  repository = selectedFork ? selectedFork.repository : null,
) {
  rememberNavigation();
  restoringNavigation = true;
  if (repository !== (selectedFork ? selectedFork.repository : null))
    applyFork(repository, true);
  setView(view);
  filter();
  filterForks();
  restoringNavigation = false;
  const hash =
    view === "forks"
      ? "#forks"
      : selectedFork
        ? "#fork=" + encodeURIComponent(selectedFork.repository)
        : "#patches";
  if (location.hash !== hash)
    history.pushState({ ...navigationSnapshot(), scrollY: 0 }, "", hash);
  lastHandledURL = location.href;
  navigationVersion++;
  window.scrollTo(0, 0);
  rememberNavigation();
}
function restoreNavigation() {
  restoringNavigation = true;
  const version = ++navigationVersion,
    state = history.state;
  const saved =
    state && state.forkpicker === 1 && state.repository === config.repository;
  let anchor = null,
    scroll = 0;
  if (saved) {
    applyFork(state.fork);
    setView(state.view === "patches" ? "patches" : "forks");
    for (const [id, el] of Object.entries(fields))
      if (state.filters && Object.hasOwn(state.filters, id)) {
        if (el.type === "checkbox") el.checked = Boolean(state.filters[id]);
        else el.value = state.filters[id];
      }
    document.querySelector("#fork-search").value = state.forkSearch || "";
    document.querySelector("#fork-sort").value = state.forkSort || "clean";
    forkLimit = Number.isSafeInteger(state.forkLimit)
      ? Math.max(50, Math.min(state.forkLimit, forkRows.length))
      : 50;
    for (const id of state.expanded || [])
      if (families.has(id)) families.get(id).expanded = true;
    scroll = Number.isFinite(state.scrollY) ? Math.max(0, state.scrollY) : 0;
  } else {
    let hash = "";
    try {
      hash = decodeURIComponent(location.hash.slice(1));
    } catch {}
    const repo = hash.startsWith("fork=") ? hash.slice(5) : null;
    applyFork(repo, true);
    setView(selectedFork || hash === "patches" ? "patches" : "forks");
    const id = hash.startsWith("evidence-") ? hash.slice(9) : hash,
      c = byId.get(id);
    if (c) {
      setView("patches");
      fields.status.value = ["dismissed", "adopted"].includes(c.dataset.status)
        ? c.dataset.status
        : "";
      fields.empty.checked = true;
      for (const family of families.values())
        if (family.cards.includes(c)) family.expanded = true;
      anchor = hash.startsWith("evidence-") ? document.getElementById(hash) : c;
      if (anchor && anchor.tagName === "DETAILS") anchor.open = true;
    } else if (!repo && !["forks", "patches", ""].includes(hash))
      anchor = document.getElementById(hash);
  }
  filter();
  filterForks();
  lastHandledURL = location.href;
  restoringNavigation = false;
  navigationReady = true;
  requestAnimationFrame(() => {
    if (version !== navigationVersion) return;
    if (anchor) anchor.scrollIntoView({ block: "start" });
    else window.scrollTo(0, scroll);
    rememberNavigation();
  });
}
document.querySelector("#fork-search").oninput = () => {
  forkLimit = 50;
  filterForks();
};
document.querySelector("#fork-sort").onchange = () => {
  forkLimit = 50;
  filterForks();
};
document.querySelector("#more-forks").onclick = () => {
  forkLimit += 50;
  filterForks();
};
document.querySelector("#show-forks").onclick = () => navigate("forks");
document.querySelector("#show-patches").onclick = () => navigate("patches");
document.querySelector("#clear-fork").onclick = () => navigate("patches", null);
document.querySelector("#fork-rows").onclick = (event) => {
  const b = event.target.closest("[data-fork]");
  if (
    !b ||
    event.button !== 0 ||
    event.metaKey ||
    event.ctrlKey ||
    event.shiftKey ||
    event.altKey
  )
    return;
  event.preventDefault();
  navigate("patches", b.dataset.fork);
};
window.addEventListener("popstate", restoreNavigation);
window.addEventListener("hashchange", () => {
  if (lastHandledURL !== location.href) restoreNavigation();
});
let scrollSaveTimer;
window.addEventListener(
  "scroll",
  () => {
    clearTimeout(scrollSaveTimer);
    scrollSaveTimer = setTimeout(rememberNavigation, 150);
  },
  { passive: true },
);
// Save the current entry before native anchor or external navigation.
document.addEventListener("click", (event) => {
  if (event.target.closest("a")) rememberNavigation();
});
for (const img of document.querySelectorAll(".avatar img")) {
  img.addEventListener("error", () => {
    img.hidden = true;
  });
  if (img.complete && !img.naturalWidth) img.hidden = true;
}
history.scrollRestoration = "manual";
restoreNavigation();
