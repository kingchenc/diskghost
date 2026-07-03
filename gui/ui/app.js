"use strict";

const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);

let lastGroups = []; // duplicate groups from the last search
let dupShown = []; // groups currently shown (filtered/sorted)
const keepSel = new Map(); // group hash -> chosen keep path

// ---------------------------------- i18n ----------------------------------
const I18N = {
  en: {
    pathLabel: "Folder path", browse: "Browse…", top: "Top", minmb: "Min MB",
    depth: "Max depth", exclude: "Exclude", follow: "Follow symlinks",
    scan: "Scan size", dupes: "Find duplicates", cancel: "Cancel",
    placeholder: "Paste a folder path, drop a folder here, or use Browse…",
    total: "total", files: "files", folders: "folders", skipped: "skipped",
    map: "Map (click a tile to drill in)", biggestFolders: "Biggest sub-folders",
    biggestFiles: "Biggest files", rootFiles: "file(s) directly in this folder",
    noDupes: "No duplicates found 🎉", groups: "groups", reclaimable: "reclaimable",
    filter: "Filter", sort: "Sort", dryRun: "Dry-run", reclaimShown: "Reclaim shown",
    enterPath: "Enter a folder path.", working: "Working…", cancelling: "Cancelling…",
    showing: "showing", of: "of", keep: "keep",
    trashMode: "Delete → Recycle Bin", del: "Delete", deleting: "Removing…",
    removedMsg: "removed", errors: "error(s)",
    confirmTrash: "Move to the Recycle Bin?", confirmDelete: "PERMANENTLY delete?",
  },
  de: {
    pathLabel: "Ordnerpfad", browse: "Durchsuchen…", top: "Top", minmb: "Min MB",
    depth: "Max Tiefe", exclude: "Ausschließen", follow: "Symlinks folgen",
    scan: "Größe scannen", dupes: "Duplikate finden", cancel: "Abbrechen",
    placeholder: "Ordnerpfad einfügen, Ordner hierher ziehen oder Durchsuchen…",
    total: "gesamt", files: "Dateien", folders: "Ordner", skipped: "übersprungen",
    map: "Karte (Kachel klicken zum Reinzoomen)", biggestFolders: "Größte Unterordner",
    biggestFiles: "Größte Dateien", rootFiles: "Datei(en) direkt in diesem Ordner",
    noDupes: "Keine Duplikate gefunden 🎉", groups: "Gruppen", reclaimable: "freigebbar",
    filter: "Filter", sort: "Sortierung", dryRun: "Testlauf", reclaimShown: "Ausgewählte freigeben",
    enterPath: "Ordnerpfad eingeben.", working: "Arbeite…", cancelling: "Breche ab…",
    showing: "zeige", of: "von", keep: "behalten",
    trashMode: "Löschen → Papierkorb", del: "Löschen", deleting: "Entferne…",
    removedMsg: "entfernt", errors: "Fehler",
    confirmTrash: "In den Papierkorb verschieben?", confirmDelete: "ENDGÜLTIG löschen?",
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

function human(bytes) {
  const u = ["B", "KB", "MB", "GB", "TB", "PB"];
  let s = bytes, i = 0;
  while (s >= 1024 && i < u.length - 1) { s /= 1024; i++; }
  return i === 0 ? `${bytes} B` : `${s.toFixed(1)} ${u[i]}`;
}

function baseName(p) {
  const parts = String(p).split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] || p;
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

function renderCrumbs(path) {
  const nav = $("crumbs");
  nav.replaceChildren();
  if (!path) return;
  const parts = String(path).split(/[\\/]/);
  let acc = "";
  const sep = path.includes("\\") ? "\\" : "/";
  parts.forEach((part, i) => {
    if (part === "" && i > 0) return;
    acc = i === 0 ? part || sep : acc + sep + part;
    const target = acc;
    nav.append(el("button", { class: "crumb", type: "button", onclick: () => doScan(target) }, part || sep));
    if (i < parts.length - 1) nav.append(el("span", { class: "crumb sep" }, "›"));
  });
}

// ---------------------------------- scan ----------------------------------
async function doScan(pathOverride) {
  const path = (pathOverride ?? $("path").value).trim();
  if (!path) return showError(t("enterPath"));
  $("path").value = path;
  const top = parseInt($("top").value, 10) || 20;
  setBusy(true, t("working"));
  try {
    const r = await invoke("scan_dir", { path, top, opts: readOpts() });
    renderScan(r);
  } catch (e) {
    showError(e);
  } finally {
    setBusy(false, "");
  }
}

// -------------------------------- delete ----------------------------------
// Remove a file/folder. The trash-mode checkbox (default on) routes it to the
// OS Recycle Bin (reversible); unchecked deletes permanently. Always confirms,
// then rescans the current view so the freed space shows immediately.
async function deletePath(path) {
  const trash = $("trashmode") ? $("trashmode").checked : true;
  const question = trash ? t("confirmTrash") : t("confirmDelete");
  if (!window.confirm(`${question}\n\n${path}`)) return;

  let msg = "";
  setBusy(true, t("deleting"));
  try {
    const r = await invoke("remove_path_cmd", { path, trash, apply: true });
    msg = `${t("del")}: ${r.files} ${t("files")}, ${human(r.bytes)} ${t("removedMsg")}`;
    if (r.errors && r.errors.length) msg += ` — ${r.errors.length} ${t("errors")}`;
  } catch (e) {
    setBusy(false, "");
    return showError(e);
  }
  // Refresh the current scan (doScan manages its own busy state + resets status).
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

function stat(label, value) {
  return el("div", { class: "stat" }, el("div", { class: "v", text: value }), el("div", { class: "l", text: label }));
}

function renderScan(r) {
  renderCrumbs(r.root);
  const results = $("results");
  results.replaceChildren();

  const stats = el("div", { class: "stats" },
    stat(t("total"), human(r.total_size)),
    stat(t("files"), r.total_files.toLocaleString()),
    stat(t("folders"), r.total_dirs.toLocaleString()));
  if (r.skipped > 0) stats.append(stat(t("skipped"), r.skipped.toLocaleString()));
  results.append(stats);

  // Treemap (squarified). If anything goes wrong, we just skip it — the bars below
  // always render, so the view never breaks.
  if (r.children.length) {
    results.append(el("h2", { text: t("map") }));
    const map = el("div", { class: "treemap" });
    results.append(map);
    try {
      renderTreemap(map, r.children, r.total_size);
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
    bars.append(el("div", { class: "row" }, bar, el("div", { class: "sz", text: human(d.size) }), delButton(d.path)));
  }
  if (r.root_files_count > 0) {
    bars.append(el("div", { class: "row muted" },
      el("div", { class: "path", text: `(${r.root_files_count} ${t("rootFiles")})` }),
      el("div", { class: "sz", text: human(r.root_files_size) })));
  }
  results.append(bars);

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
// its pixel size). Falls back to a simple proportional strip on any oddity.
function renderTreemap(container, children, totalSize) {
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
    container.append(tile);
  }
}

// Classic squarified treemap layout. Returns [{item,x,y,w,h}].
function squarify(items, x, y, w, h) {
  const total = items.reduce((a, i) => a + i.size, 0) || 1;
  const scale = (w * h) / total;
  const boxes = items.map((it) => ({ item: it, area: it.size * scale }));
  const out = [];
  let cx = x, cy = y, cw = w, ch = h;

  const worst = (row, len) => {
    const s = row.reduce((a, r) => a + r.area, 0);
    const mx = Math.max(...row.map((r) => r.area));
    const mn = Math.min(...row.map((r) => r.area));
    return Math.max((len * len * mx) / (s * s), (s * s) / (len * len * mn));
  };
  const layout = (row, horizontal) => {
    const s = row.reduce((a, r) => a + r.area, 0);
    if (horizontal) {
      const rh = s / cw;
      let px = cx;
      for (const r of row) { const rw = r.area / rh; out.push({ item: r.item, x: px, y: cy, w: rw, h: rh }); px += rw; }
      cy += rh; ch -= rh;
    } else {
      const rw = s / ch;
      let py = cy;
      for (const r of row) { const rh = r.area / rw; out.push({ item: r.item, x: cx, y: py, w: rw, h: rh }); py += rh; }
      cx += rw; cw -= rw;
    }
  };

  let i = 0;
  while (i < boxes.length && cw > 0.5 && ch > 0.5) {
    const horizontal = cw >= ch;
    const len = horizontal ? cw : ch;
    const row = [boxes[i]];
    let j = i + 1;
    while (j < boxes.length && worst(row.concat(boxes[j]), len) <= worst(row, len)) {
      row.push(boxes[j]); j++;
    }
    layout(row, horizontal);
    i = j;
  }
  return out;
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
    labelled(t("sort"), selectEl("dupsort", "wasted", [["wasted", "wasted"], ["size", "size"], ["count", "count"]], updateDupeList)),
    selectEl("dupaction", "trash", [["trash", "→ Trash"], ["delete", "Delete"], ["hardlink", "Hard-link"]]),
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
