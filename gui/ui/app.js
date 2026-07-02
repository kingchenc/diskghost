"use strict";

const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);

// Cap how many rows we build at once so a huge scan can't freeze the webview.
const RENDER_CAP = 500;

let lastGroups = []; // duplicate groups from the last search (for sort/filter/reclaim)

// ---------- tiny safe DOM builder (textContent only, no innerHTML) ----------
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

function status(msg) {
  $("status").textContent = msg || "";
}

function setBusy(busy, msg) {
  status(msg);
  const r = $("results");
  r.classList.toggle("loading", busy);
  for (const b of document.querySelectorAll("button, input")) b.disabled = busy;
}

function showError(msg) {
  const r = $("results");
  r.replaceChildren(el("div", { class: "error" }, `⚠ ${msg}`));
}

function readOpts() {
  const depth = parseInt($("depth").value, 10);
  const exclude = $("exclude").value
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  return {
    exclude,
    max_depth: Number.isFinite(depth) && depth > 0 ? depth : null,
    follow_symlinks: $("follow").checked,
  };
}

// ---------------------------------- scan ----------------------------------
async function doScan(pathOverride) {
  const path = (pathOverride ?? $("path").value).trim();
  if (!path) return showError("Enter a folder path.");
  $("path").value = path;
  const top = parseInt($("top").value, 10) || 20;
  setBusy(true, "Scanning…");
  try {
    const r = await invoke("scan_dir", { path, top, opts: readOpts() });
    renderScan(r);
  } catch (e) {
    showError(e);
  } finally {
    setBusy(false, "");
  }
}

function stat(label, value) {
  return el("div", { class: "stat" }, el("div", { class: "v", text: value }), el("div", { class: "l", text: label }));
}

function renderScan(r) {
  const results = $("results");
  const frag = document.createDocumentFragment();

  const stats = el("div", { class: "stats" },
    stat("total", human(r.total_size)),
    stat("files", r.total_files.toLocaleString()),
    stat("folders", r.total_dirs.toLocaleString()));
  if (r.skipped > 0) stats.append(stat("skipped", r.skipped.toLocaleString()));
  frag.append(stats);

  // Treemap: proportional, clickable tiles (drill down into a folder).
  if (r.children.length) {
    frag.append(el("h2", { text: "Map (click a folder to drill in)" }));
    const map = el("div", { class: "treemap" });
    const maxByTotal = r.total_size || 1;
    for (const d of r.children) {
      const tile = el("button", {
        class: "tile",
        type: "button",
        title: `${d.path} — ${human(d.size)}`,
        style: { flexGrow: String(Math.max(1, Math.round((1000 * d.size) / maxByTotal))) },
        onclick: () => doScan(d.path),
      },
        el("span", { class: "tname", text: baseName(d.path) }),
        el("span", { class: "tsize", text: human(d.size) }));
      map.append(tile);
    }
    frag.append(map);
  }

  // Biggest sub-folders (bars).
  frag.append(el("h2", { text: "Biggest sub-folders" }));
  const bars = el("div", { class: "bars" });
  const max = r.children.length ? r.children[0].size : 1;
  for (const d of r.children) {
    const fill = el("div", { class: "fill" });
    fill.style.width = `${Math.max(2, (100 * d.size) / max)}%`;
    const bar = el("div", { class: "bar", onclick: () => doScan(d.path), title: "drill in" },
      fill, el("span", { class: "path", text: d.path }));
    bars.append(el("div", { class: "row" }, bar, el("div", { class: "sz", text: human(d.size) })));
  }
  if (r.root_files_count > 0) {
    bars.append(el("div", { class: "row muted" },
      el("div", { class: "path", text: `(${r.root_files_count} file(s) directly in this folder)` }),
      el("div", { class: "sz", text: human(r.root_files_size) })));
  }
  frag.append(bars);

  // Biggest files (capped).
  frag.append(el("h2", { text: "Biggest files" }));
  const files = el("div", { class: "files" });
  for (const f of r.top_files.slice(0, RENDER_CAP)) {
    files.append(el("div", { class: "frow" },
      el("span", { class: "sz", text: human(f.size) }),
      el("span", { class: "path", text: f.path })));
  }
  frag.append(files);

  results.replaceChildren(frag);
}

function baseName(p) {
  const parts = String(p).split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] || p;
}

// -------------------------------- duplicates --------------------------------
async function doDupes() {
  const path = $("path").value.trim();
  if (!path) return showError("Enter a folder path.");
  const mb = parseInt($("mb").value, 10) || 0;
  setBusy(true, "Hunting duplicates…");
  try {
    lastGroups = await invoke("find_dupes", { path, mb, opts: readOpts() });
    renderDupes();
  } catch (e) {
    showError(e);
  } finally {
    setBusy(false, "");
  }
}

function renderDupes() {
  const results = $("results");
  if (!lastGroups.length) {
    results.replaceChildren(el("div", { class: "ok" }, "No duplicates found 🎉"));
    return;
  }

  const filter = $("dupfilter")?.value?.toLowerCase() || "";
  const sort = $("dupsort")?.value || "wasted";
  let groups = lastGroups.filter((g) => !filter || g.files.some((f) => f.toLowerCase().includes(filter)));
  groups.sort((a, b) =>
    sort === "size" ? b.size - a.size : sort === "count" ? b.files.length - a.files.length : b.wasted - a.wasted);

  const totalWasted = lastGroups.reduce((a, g) => a + g.wasted, 0);
  const frag = document.createDocumentFragment();

  // toolbar
  const bar = el("div", { class: "dupbar" },
    el("div", { class: "ok", text: `${lastGroups.length} groups — ${human(totalWasted)} reclaimable` }),
    labelled("Filter", el("input", { id: "dupfilter", type: "text", value: filter, oninput: renderDupes })),
    labelled("Sort", selectEl("dupsort", sort, [["wasted", "wasted"], ["size", "size"], ["count", "count"]], renderDupes)),
    selectEl("dupaction", "trash", [["trash", "→ Trash"], ["delete", "Delete"], ["hardlink", "Hard-link"]]),
    el("button", { type: "button", onclick: () => reclaimShown(groups, true) }, "Dry-run"),
    el("button", { class: "danger", type: "button", onclick: () => reclaimShown(groups, false) }, "Reclaim shown"));
  frag.append(bar);

  const list = el("div", { class: "dupes" });
  for (const g of groups.slice(0, RENDER_CAP)) {
    const box = el("div", { class: "dup" });
    box.append(el("div", { class: "dhead" },
      el("span", { class: "badge", text: `${g.files.length}×` }),
      document.createTextNode(` ${human(g.size)} `),
      el("span", { class: "waste", text: `${human(g.wasted)} reclaimable` })));
    g.files.forEach((f, i) => {
      box.append(el("div", { class: "path small" + (i === 0 ? " keep" : "") },
        (i === 0 ? "keep  " : "dup   ") + f));
    });
    list.append(box);
  }
  if (groups.length > RENDER_CAP) {
    list.append(el("div", { class: "muted", text: `… showing ${RENDER_CAP} of ${groups.length} groups` }));
  }
  frag.append(list);
  results.replaceChildren(frag);
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

async function reclaimShown(groups, dryRun) {
  const action = $("dupaction")?.value || "trash";
  const jobs = groups
    .filter((g) => g.files.length > 1)
    .map((g) => ({ keep: g.files[0], remove: g.files.slice(1), size: g.size }));
  if (!jobs.length) return;
  if (!dryRun && !confirm(`Really ${action} ${jobs.reduce((a, j) => a + j.remove.length, 0)} file(s)?`)) return;

  setBusy(true, dryRun ? "Dry-run…" : "Reclaiming…");
  try {
    const rep = await invoke("reclaim_dupes", { jobs, action, dryRun });
    const tag = rep.dry_run ? "DRY-RUN (nothing changed)" : "done";
    status(`${tag}: ${rep.removed} file(s), ${human(rep.reclaimed)} reclaimable` +
      (rep.errors.length ? `, ${rep.errors.length} error(s)` : ""));
    if (!rep.dry_run) {
      await doDupes(); // refresh after real changes
    }
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
    if (picked) {
      $("path").value = picked;
      doScan();
    }
  } catch (e) {
    showError(e);
  }
}

$("btn-scan").addEventListener("click", () => doScan());
$("btn-dupes").addEventListener("click", () => doDupes());
$("btn-browse").addEventListener("click", browse);
$("path").addEventListener("keydown", (e) => {
  if (e.key === "Enter") doScan();
});

// Drag & drop a folder onto the window (Tauri webview event; guarded).
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
} catch (_) {
  /* drag-drop not available; ignore */
}
