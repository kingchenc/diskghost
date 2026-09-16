"use strict";

// Pure helpers (human, humanDelta, fmtMs, baseName, fmtDate, crumbs,
// squarify, deltaTint, changesByPath) come from lib.js as globals.

const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);

let lastGroups = []; // duplicate groups from the last search
let dupShown = []; // groups currently shown (filtered/sorted)
const keepSel = new Map(); // group hash -> chosen keep path

// The scan on screen: its report, and the folder rows/tiles by path so a
// diff can decorate them after the fact.
let currentReport = null;
const rowsByPath = new Map();
const tilesByPath = new Map();

// ---------------------------------- i18n ----------------------------------
const I18N = {
  en: {
    pathLabel: "Folder path", browse: "Browse…", top: "Top", minmb: "Min MB",
    depth: "Max depth", exclude: "Exclude", follow: "Follow symlinks",
    scan: "Scan size", rescan: "Rescan", dupes: "Find duplicates", cancel: "Cancel",
    placeholder: "Paste a folder path, drop a folder here, or use Browse…",
    total: "total", files: "files", folders: "folders", skipped: "skipped",
    diskFree: "disk free", scanTime: "scan time", partial: "partial",
    cancelledNotice: "Scan cancelled — showing a partial result",
    map: "Map (click a tile to drill in)", biggestFolders: "Biggest sub-folders",
    biggestFiles: "Biggest files", rootFiles: "file(s) directly in this folder",
    noDupes: "No duplicates found 🎉", groups: "groups", reclaimable: "reclaimable",
    filter: "Filter", sort: "Sort", dryRun: "Dry-run", reclaimShown: "Reclaim shown",
    sortWasted: "wasted", sortSize: "size", sortCount: "count",
    actTrash: "→ Trash", actDelete: "Delete", actHardlink: "Hard-link",
    enterPath: "Enter a folder path.", working: "Working…", cancelling: "Cancelling…",
    showing: "showing", of: "of", keep: "keep",
    trashMode: "Delete → Recycle Bin", del: "Delete", deleting: "Removing…",
    removedMsg: "removed", errors: "error(s)",
    confirmTrash: "Move to the Recycle Bin?", confirmDelete: "PERMANENTLY delete?",
    since: "Since", showChanges: "Show changes", comparing: "Comparing…",
    firstScan: "First scan of this folder — changes will show up next time.",
    changes: "Changes since", unchanged: "Nothing changed.", newBadge: "new",
    grew: "Grew", shrank: "Shrank", newFolders: "New folders", removedFolders: "Removed folders",
    sinceStat: "since",
  },
  de: {
    pathLabel: "Ordnerpfad", browse: "Durchsuchen…", top: "Top", minmb: "Min MB",
    depth: "Max Tiefe", exclude: "Ausschließen", follow: "Symlinks folgen",
    scan: "Größe scannen", rescan: "Neu scannen", dupes: "Duplikate finden", cancel: "Abbrechen",
    placeholder: "Ordnerpfad einfügen, Ordner hierher ziehen oder Durchsuchen…",
    total: "gesamt", files: "Dateien", folders: "Ordner", skipped: "übersprungen",
    diskFree: "Platte frei", scanTime: "Scan-Zeit", partial: "unvollständig",
    cancelledNotice: "Scan abgebrochen — Teilergebnis",
    map: "Karte (Kachel klicken zum Reinzoomen)", biggestFolders: "Größte Unterordner",
    biggestFiles: "Größte Dateien", rootFiles: "Datei(en) direkt in diesem Ordner",
    noDupes: "Keine Duplikate gefunden 🎉", groups: "Gruppen", reclaimable: "freigebbar",
    filter: "Filter", sort: "Sortierung", dryRun: "Testlauf", reclaimShown: "Ausgewählte freigeben",
    sortWasted: "verschwendet", sortSize: "Größe", sortCount: "Anzahl",
    actTrash: "→ Papierkorb", actDelete: "Löschen", actHardlink: "Hardlink",
    enterPath: "Ordnerpfad eingeben.", working: "Arbeite…", cancelling: "Breche ab…",
    showing: "zeige", of: "von", keep: "behalten",
    trashMode: "Löschen → Papierkorb", del: "Löschen", deleting: "Entferne…",
    removedMsg: "entfernt", errors: "Fehler",
    confirmTrash: "In den Papierkorb verschieben?", confirmDelete: "ENDGÜLTIG löschen?",
    since: "Seit", showChanges: "Änderungen zeigen", comparing: "Vergleiche…",
    firstScan: "Erster Scan dieses Ordners — Änderungen erscheinen beim nächsten Mal.",
    changes: "Änderungen seit", unchanged: "Nichts geändert.", newBadge: "neu",
    grew: "Gewachsen", shrank: "Geschrumpft", newFolders: "Neue Ordner", removedFolders: "Entfernte Ordner",
    sinceStat: "seit",
  },
};
const LANG = (navigator.language || "en").toLowerCase().startsWith("de") ? "de" : "en";
const t = (k) => I18N[LANG][k] ?? I18N.en[k] ?? k;

function applyI18n() {
  document.querySelectorAll("[data-i18n]").forEach((e) => (e.textContent = t(e.dataset.i18n)));
  document.querySelectorAll("[data-i18n-ph]").forEach((e) => (e.placeholder = t(e.dataset.i18nPh)));
}

// ---------------------------- safe DOM builder ----------------------------
function el(tag, attrs, ...kids) {
  const n = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v == null) continue;
      if (k === "class") n.className = v;
      else if (k === "text") n.textContent = v;
      else if (k === "style") Object.assign(n.style, v);
      else if (k.startsWith("on") && typeof v === "function") n.addEventListener(k.slice(2), v);
      else n.setAttribute(k, v);
    }
  }
  for (const kid of kids) {
    if (kid == null) continue;
    n.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return n;
}

function status(msg) { $("status").textContent = msg || ""; }

function setBusy(busy, msg) {
  status(msg);
  $("results").classList.toggle("loading", busy);
  for (const b of document.querySelectorAll("button, input, select")) {
    if (b.id === "btn-cancel") continue;
    b.disabled = busy;
  }
  const c = $("btn-cancel");
  if (c) c.style.display = busy ? "" : "none";
}

function showError(e) {
  const msg = typeof e === "string" ? e : e?.message ?? JSON.stringify(e);
  $("results").replaceChildren(el("div", { class: "error" }, `⚠ ${msg}`));
}

function readOpts() {
  const depth = parseInt($("depth").value, 10);
  const exclude = $("exclude").value.split(",").map((s) => s.trim()).filter(Boolean);
  return {
    exclude,
    max_depth: Number.isFinite(depth) && depth > 0 ? depth : null,
    follow_symlinks: $("follow").checked,
  };
}

function readTop() {
  return parseInt($("top").value, 10) || 20;
}

function renderCrumbs(path) {
  const nav = $("crumbs");
  nav.replaceChildren();
  if (!path) return;
  const parts = crumbs(path);
  parts.forEach((c, i) => {
    nav.append(el("button", { class: "crumb", type: "button", onclick: () => doScan(c.target) }, c.label));
    if (i < parts.length - 1) nav.append(el("span", { class: "crumb sep" }, "›"));
  });
}

// ---------------------------------- scan ----------------------------------
// Drill-ins below the last full walk are answered from the backend's cached
// tree (instant); `refresh` forces a fresh walk of `path`.
async function doScan(pathOverride, refresh = false) {
  const path = (pathOverride ?? $("path").value).trim();
  if (!path) return showError(t("enterPath"));
  $("path").value = path;
  const top = readTop();
  setBusy(true, t("working"));
  try {
    const t0 = performance.now();
    const r = await invoke("scan_dir", { path, top, opts: readOpts(), refresh });
    r._elapsedMs = performance.now() - t0;
    renderScan(r);
  } catch (e) {
    showError(e);
  } finally {
    setBusy(false, currentReport?.cancelled ? t("cancelledNotice") : "");
  }
}

// -------------------------------- delete ----------------------------------
// Remove a file/folder. The trash-mode checkbox (default on) routes it to the
// OS Recycle Bin (reversible); unchecked deletes permanently. Always confirms,
// then re-renders the current view — the backend has already taken the path
// out of its cached tree, so this is instant and shows the freed space.
async function deletePath(path) {
  const trash = $("trashmode") ? $("trashmode").checked : true;
  const question = trash ? t("confirmTrash") : t("confirmDelete");
  if (!window.confirm(`${question}\n\n${path}`)) return;

  let msg = "";
  setBusy(true, t("deleting"));
  try {
    const t0 = performance.now();
    const r = await invoke("remove_path_cmd", { path, trash, apply: true });
    const took = fmtMs(performance.now() - t0);
    msg = `${t("del")}: ${r.files} ${t("files")}, ${human(r.bytes)} ${t("removedMsg")} (${took})`;
    if (r.errors && r.errors.length) msg += ` — ${r.errors.length} ${t("errors")}`;
  } catch (e) {
    setBusy(false, "");
    return showError(e);
  }
  const cur = ($("path").value || "").trim();
  if (cur) {
    try { await doScan(cur); } catch (_) { /* keep the delete result message */ }
  }
  setBusy(false, "");
  status(msg);
}

// Small trash button shared by folder + file rows.
function delButton(path) {
  return el("button", {
    class: "del", type: "button", title: t("del"),
    style: { flex: "0 0 auto", marginLeft: "6px", padding: "2px 6px", cursor: "pointer" },
    onclick: (ev) => { ev.stopPropagation(); deletePath(path); },
  }, "🗑");
}

function stat(label, value, cls) {
  return el("div", { class: "stat" + (cls ? ` ${cls}` : "") }, el("div", { class: "v", text: value }), el("div", { class: "l", text: label }));
}

function renderScan(r) {
  currentReport = r;
  rowsByPath.clear();
  tilesByPath.clear();
  renderCrumbs(r.root);
  const results = $("results");
  results.replaceChildren();

  if (r.cancelled) {
    results.append(el("div", { class: "notice", text: `⚠ ${t("cancelledNotice")}` }));
  }

  const stats = el("div", { class: "stats" },
    stat(t("total"), human(r.total_size)),
    stat(t("files"), r.total_files.toLocaleString()),
    stat(t("folders"), r.total_dirs.toLocaleString()));
  if (r.skipped > 0) stats.append(stat(t("skipped"), r.skipped.toLocaleString()));
  if (r.disk_total > 0) stats.append(stat(t("diskFree"), `${human(r.disk_free)} / ${human(r.disk_total)}`));
  if (r._elapsedMs != null) stats.append(stat(t("scanTime"), fmtMs(r._elapsedMs)));
  if (r.cancelled) stats.append(stat(t("partial"), "⚠", "warn"));
  results.append(stats);

  // "Since…" toolbar: filled once the snapshot history has been fetched.
  const since = el("div", { class: "since", id: "since" });
  results.append(since);
  if (!r.cancelled) loadHistory(r.root, since);

  // Treemap (squarified). If anything goes wrong, we just skip it — the bars below
  // always render, so the view never breaks.
  if (r.children.length) {
    results.append(el("h2", { text: t("map") }));
    const map = el("div", { class: "treemap" });
    results.append(map);
    try {
      renderTreemap(map, r.children);
    } catch (_) {
      map.remove();
    }
  }

  // Biggest sub-folders (bars) — keyboard-accessible drill-in.
  results.append(el("h2", { text: t("biggestFolders") }));
  const bars = el("div", { class: "bars" });
  const max = r.children.length ? r.children[0].size : 1;
  for (const d of r.children) {
    const fill = el("div", { class: "fill" });
    fill.style.width = `${Math.max(2, (100 * d.size) / max)}%`;
    const bar = el("button", { class: "bar", type: "button", title: baseName(d.path), onclick: () => doScan(d.path) },
      fill, el("span", { class: "path", text: d.path }));
    const badge = el("span", { class: "delta" });
    const row = el("div", { class: "row" }, bar, badge, el("div", { class: "sz", text: human(d.size) }), delButton(d.path));
    rowsByPath.set(String(d.path), { row, badge });
    bars.append(row);
  }
  if (r.root_files_count > 0) {
    bars.append(el("div", { class: "row muted" },
      el("div", { class: "path", text: `(${r.root_files_count} ${t("rootFiles")})` }),
      el("div", { class: "sz", text: human(r.root_files_size) })));
  }
  results.append(bars);

  // Changes section: filled by showChanges().
  results.append(el("div", { id: "changes" }));

  // Biggest files — virtualized (only visible rows are in the DOM).
  results.append(el("h2", { text: t("biggestFiles") }));
  const vbox = el("div", { class: "vlist" });
  results.append(vbox);
  virtualList(vbox, r.top_files, 34, (f) =>
    el("div", { class: "frow" },
      el("span", { class: "sz", text: human(f.size) }),
      el("span", { class: "path", text: f.path }),
      delButton(f.path)));
}

// Squarified treemap into `container` (must already be in the DOM so we can read
// its pixel size).
function renderTreemap(container, children) {
  const W = container.clientWidth || 800;
  const H = container.clientHeight || 300;
  const items = children.filter((c) => c.size > 0).map((c) => ({ ...c }));
  if (!items.length) return;
  const rects = squarify(items, 0, 0, W, H);
  for (const rc of rects) {
    const tile = el("button", {
      class: "tile", type: "button", title: `${rc.item.path} — ${human(rc.item.size)}`,
      onclick: () => doScan(rc.item.path),
    }, el("span", { class: "tname", text: baseName(rc.item.path) }), el("span", { class: "tsize", text: human(rc.item.size) }));
    tile.style.left = `${rc.x}px`;
    tile.style.top = `${rc.y}px`;
    tile.style.width = `${Math.max(0, rc.w - 2)}px`;
    tile.style.height = `${Math.max(0, rc.h - 2)}px`;
    tilesByPath.set(String(rc.item.path), tile);
    container.append(tile);
  }
}

// Windowed list: only rows in view are built. Guarded — on any failure it falls
// back to rendering everything.
function virtualList(container, items, rowH, renderRow) {
  try {
    const inner = el("div", { class: "vlist-inner" });
    inner.style.height = `${items.length * rowH}px`;
    container.replaceChildren(inner);
    const draw = () => {
      const top = container.scrollTop;
      const vh = container.clientHeight || 400;
      const start = Math.max(0, Math.floor(top / rowH) - 6);
      const end = Math.min(items.length, Math.ceil((top + vh) / rowH) + 6);
      const frag = document.createDocumentFragment();
      for (let k = start; k < end; k++) {
        const row = renderRow(items[k], k);
        row.style.position = "absolute";
        row.style.top = `${k * rowH}px`;
        row.style.left = "0";
        row.style.right = "0";
        frag.append(row);
      }
      inner.replaceChildren(frag);
    };
    container.onscroll = draw;
    draw();
  } catch (_) {
    const frag = document.createDocumentFragment();
    items.forEach((it, k) => frag.append(renderRow(it, k)));
    container.replaceChildren(frag);
  }
}

// ------------------------------ what grew ---------------------------------
// Every complete walk is recorded as a snapshot of its root. Offer the earlier
// ones of the root this view belongs to; the newest is usually the scan on
// screen, so the one before it is preselected.
async function loadHistory(path, box) {
  let list;
  try {
    list = await invoke("list_snapshots", { path });
  } catch (_) {
    return; // history is a convenience; the scan view stands on its own
  }
  if (currentReport == null || String(currentReport.root) !== String(path)) return;
  const times = list.taken_at || [];
  box.replaceChildren();
  if (times.length < 2) {
    box.append(el("span", { class: "small", text: t("firstScan") }));
    return;
  }
  const sel = el("select", { id: "sincesel" });
  times.forEach((ts, i) => {
    const o = el("option", { value: String(ts), text: fmtDate(ts) });
    if (i === 1) o.selected = true;
    sel.append(o);
  });
  box.append(
    el("label", { class: "inlabel" }, `${t("since")} `, sel),
    el("button", { type: "button", onclick: () => showChanges(path) }, t("showChanges")));
}

async function showChanges(path) {
  const sel = $("sincesel");
  if (!sel) return;
  const takenAt = parseInt(sel.value, 10);
  setBusy(true, t("comparing"));
  try {
    const d = await invoke("diff_since", { path, takenAt, top: readTop() });
    renderChanges(d);
  } catch (e) {
    showError(e);
  } finally {
    setBusy(false, "");
  }
}

// Decorate the current view with a diff: a stat tile with the total change,
// a ± badge on each folder row, tinted treemap tiles, and a changes section.
function renderChanges(d) {
  const { map, maxAbs } = changesByPath(d);

  // Stat tile (replace an earlier one).
  document.querySelector(".stat.since")?.remove();
  const tint = deltaTint(d.delta, Math.abs(d.delta));
  document.querySelector(".stats")?.append(
    stat(`${t("sinceStat")} ${fmtDate(d.old_taken_at)}`, humanDelta(d.delta), `since ${tint.cls}`));

  // Row badges + tile tints.
  for (const [path, { row, badge }] of rowsByPath) {
    const c = map.get(path);
    row.classList.remove("grow", "shrink", "added");
    badge.textContent = "";
    if (!c) continue;
    if (c.added) {
      row.classList.add("added");
      badge.textContent = t("newBadge");
    } else {
      row.classList.add(deltaTint(c.delta, maxAbs).cls);
      badge.textContent = humanDelta(c.delta);
    }
  }
  for (const [path, tile] of tilesByPath) {
    const c = map.get(path);
    tile.classList.remove("grow", "shrink", "added");
    tile.style.removeProperty("--tint");
    if (!c) continue;
    const { cls, strength } = c.added ? { cls: "added", strength: 1 } : deltaTint(c.delta, maxAbs);
    tile.classList.add(cls);
    tile.style.setProperty("--tint", String(0.25 + 0.75 * strength));
  }

  // Changes section.
  const box = $("changes");
  if (!box) return;
  box.replaceChildren(el("h2", { text: `${t("changes")} ${fmtDate(d.old_taken_at)}` }));
  const head = el("div", { class: "chead" },
    el("span", { class: `delta-big ${tint.cls}`, text: humanDelta(d.delta) }),
    el("span", { class: "small", text: `${human(d.old_size)} → ${human(d.new_size)}, ${d.files_delta >= 0 ? "+" : ""}${d.files_delta.toLocaleString()} ${t("files")}` }));
  box.append(head);
  if (!d.grown.length && !d.shrunk.length && !d.added.length && !d.removed.length) {
    box.append(el("div", { class: "ok", text: t("unchanged") }));
    return;
  }
  const list = (title, items, render) => {
    if (!items.length) return;
    const ul = el("div", { class: "clist" }, el("h3", { text: title }));
    for (const it of items) ul.append(render(it));
    box.append(ul);
  };
  const deltaRow = (x) => el("button", { class: "crow", type: "button", onclick: () => doScan(x.path) },
    el("span", { class: `delta ${x.delta > 0 ? "grow" : "shrink"}`, text: humanDelta(x.delta) }),
    el("span", { class: "path", text: x.path }),
    el("span", { class: "small", text: `${human(x.old_size)} → ${human(x.new_size)}` }));
  const sizeRow = (x, clickable) => el(clickable ? "button" : "div", { class: "crow", type: clickable ? "button" : null, onclick: clickable ? () => doScan(x.path) : null },
    el("span", { class: "delta", text: human(x.size) }),
    el("span", { class: "path", text: x.path }));
  list(t("grew"), d.grown, deltaRow);
  list(t("shrank"), d.shrunk, deltaRow);
  list(t("newFolders"), d.added, (x) => sizeRow(x, true));
  list(t("removedFolders"), d.removed, (x) => sizeRow(x, false));
}

// -------------------------------- duplicates --------------------------------
async function doDupes() {
  const path = $("path").value.trim();
  if (!path) return showError(t("enterPath"));
  const mb = parseInt($("mb").value, 10) || 0;
  setBusy(true, t("working"));
  try {
    lastGroups = await invoke("find_dupes", { path, mb, opts: readOpts() });
    keepSel.clear();
    renderDupes();
  } catch (e) {
    showError(e);
  } finally {
    setBusy(false, "");
  }
}

function labelled(text, input) {
  return el("label", { class: "inlabel" }, `${text} `, input);
}

function selectEl(id, value, options, onchange) {
  const s = el("select", { id, onchange: onchange || null });
  for (const [val, label] of options) {
    const o = el("option", { value: val, text: label });
    if (val === value) o.selected = true;
    s.append(o);
  }
  return s;
}

function renderDupes() {
  const results = $("results");
  currentReport = null;
  renderCrumbs($("path").value.trim());
  if (!lastGroups.length) {
    results.replaceChildren(el("div", { class: "ok" }, t("noDupes")));
    return;
  }
  const totalWasted = lastGroups.reduce((a, g) => a + g.wasted, 0);

  // Toolbar built once; typing in the filter only updates the list (keeps focus).
  const bar = el("div", { class: "dupbar" },
    el("div", { class: "ok", text: `${lastGroups.length} ${t("groups")} — ${human(totalWasted)} ${t("reclaimable")}` }),
    labelled(t("filter"), el("input", { id: "dupfilter", type: "text", oninput: updateDupeList })),
    labelled(t("sort"), selectEl("dupsort", "wasted", [["wasted", t("sortWasted")], ["size", t("sortSize")], ["count", t("sortCount")]], updateDupeList)),
    selectEl("dupaction", "trash", [["trash", t("actTrash")], ["delete", t("actDelete")], ["hardlink", t("actHardlink")]]),
    el("button", { type: "button", onclick: () => reclaimShown(true) }, t("dryRun")),
    el("button", { class: "danger", type: "button", onclick: () => reclaimShown(false) }, t("reclaimShown")));

  const list = el("div", { class: "dupes" });
  results.replaceChildren(bar, list);
  updateDupeList();
}

function computeShown() {
  const filter = $("dupfilter")?.value?.toLowerCase() || "";
  const sort = $("dupsort")?.value || "wasted";
  const groups = lastGroups.filter((g) => !filter || g.files.some((f) => f.toLowerCase().includes(filter)));
  groups.sort((a, b) =>
    sort === "size" ? b.size - a.size : sort === "count" ? b.files.length - a.files.length : b.wasted - a.wasted);
  dupShown = groups;
  return groups;
}

function keptPath(g) {
  return keepSel.get(g.hash) ?? g.files[0];
}

function updateDupeList() {
  const list = document.querySelector(".dupes");
  if (!list) return;
  const groups = computeShown();
  const frag = document.createDocumentFragment();
  for (const g of groups.slice(0, 1000)) {
    const box = el("div", { class: "dup" });
    box.append(el("div", { class: "dhead" },
      el("span", { class: "badge", text: `${g.files.length}×` }),
      document.createTextNode(` ${human(g.size)} `),
      el("span", { class: "waste", text: `${human(g.wasted)} ${t("reclaimable")}` })));
    const kept = keptPath(g);
    g.files.forEach((f) => {
      const isKeep = f === kept;
      const radio = el("input", {
        type: "radio", name: `keep-${g.hash}`, title: t("keep"),
        onchange: () => { keepSel.set(g.hash, f); updateDupeList(); },
      });
      if (isKeep) radio.checked = true;
      box.append(el("label", { class: "dupfile" + (isKeep ? " kept" : "") }, radio, el("span", { class: "path", text: f })));
    });
    frag.append(box);
  }
  if (groups.length > 1000) {
    frag.append(el("div", { class: "muted", text: `… ${t("showing")} 1000 ${t("of")} ${groups.length} ${t("groups")}` }));
  }
  list.replaceChildren(frag);
}

async function reclaimShown(dryRun) {
  const action = $("dupaction")?.value || "trash";
  const jobs = dupShown
    .filter((g) => g.files.length > 1)
    .map((g) => {
      const keep = keptPath(g);
      return { keep, remove: g.files.filter((f) => f !== keep), size: g.size };
    })
    .filter((j) => j.remove.length > 0);
  if (!jobs.length) return;
  const count = jobs.reduce((a, j) => a + j.remove.length, 0);
  if (!dryRun) {
    const warn = action === "trash" ? "" : "  (irreversible!)";
    if (!confirm(`${action} ${count} file(s)?${warn}`)) return;
  }

  setBusy(true, dryRun ? t("dryRun") + "…" : t("working"));
  try {
    const rep = await invoke("reclaim_dupes", { jobs, action, dryRun });
    const tag = rep.dry_run ? "DRY-RUN" : "OK";
    status(`${tag}: ${rep.removed} file(s), ${human(rep.reclaimed)} ${t("reclaimable")}` +
      (rep.errors.length ? `, ${rep.errors.length} error(s)` : ""));
    if (!rep.dry_run) await doDupes();
  } catch (e) {
    showError(e);
  } finally {
    setBusy(false, $("status").textContent);
  }
}

// -------------------------------- wiring --------------------------------
async function browse() {
  try {
    const picked = await invoke("pick_folder");
    if (picked) { $("path").value = picked; doScan(); }
  } catch (e) {
    showError(e);
  }
}

applyI18n();
$("btn-scan").addEventListener("click", () => doScan());
$("btn-rescan").addEventListener("click", () => doScan(undefined, true));
$("btn-dupes").addEventListener("click", () => doDupes());
$("btn-browse").addEventListener("click", browse);
$("path").addEventListener("keydown", (e) => { if (e.key === "Enter") doScan(); });
$("btn-cancel").addEventListener("click", async () => {
  try { await invoke("cancel"); status(t("cancelling")); } catch (_) { /* ignore */ }
});

try {
  window.__TAURI__?.event?.listen?.("progress", (e) => {
    if ($("results").classList.contains("loading") && e.payload) {
      status(`${t("working")} ${e.payload.files.toLocaleString()} ${t("files")}, ${human(e.payload.bytes)}`);
    }
  });
} catch (_) { /* events unavailable */ }

try {
  const wv = window.__TAURI__?.webview?.getCurrentWebview?.();
  if (wv && wv.onDragDropEvent) {
    wv.onDragDropEvent((e) => {
      if (e.payload && e.payload.type === "drop" && e.payload.paths && e.payload.paths.length) {
        $("path").value = e.payload.paths[0];
        doScan();
      }
    });
  }
} catch (_) { /* drag-drop unavailable */ }
