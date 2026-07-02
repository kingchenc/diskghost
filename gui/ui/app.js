const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);

function human(bytes) {
  const u = ["B", "KB", "MB", "GB", "TB", "PB"];
  let s = bytes;
  let i = 0;
  while (s >= 1024 && i < u.length - 1) {
    s /= 1024;
    i++;
  }
  return i === 0 ? `${bytes} B` : `${s.toFixed(1)} ${u[i]}`;
}

function esc(s) {
  return String(s).replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));
}

function busy(msg) {
  $("status").textContent = msg || "";
  $("results").classList.toggle("loading", !!msg);
}

function err(msg) {
  $("results").innerHTML = `<div class="error">⚠ ${esc(msg)}</div>`;
}

function card(label, val) {
  return `<div class="stat"><div class="v">${val}</div><div class="l">${label}</div></div>`;
}

async function doScan() {
  const path = $("path").value.trim();
  if (!path) return err("Enter a folder path.");
  const top = parseInt($("top").value, 10) || 20;
  busy("Scanning…");
  try {
    renderScan(await invoke("scan_dir", { path, top }));
  } catch (e) {
    err(e);
  }
  busy("");
}

async function doDupes() {
  const path = $("path").value.trim();
  if (!path) return err("Enter a folder path.");
  const mb = parseInt($("mb").value, 10) || 0;
  busy("Hunting duplicates…");
  try {
    renderDupes(await invoke("find_dupes", { path, mb }));
  } catch (e) {
    err(e);
  }
  busy("");
}

function renderScan(r) {
  const max = r.children.length ? r.children[0].size : 1;
  const bars = r.children
    .map(
      (d) => `
      <div class="row">
        <div class="bar"><div class="fill" style="width:${Math.max(2, (100 * d.size) / max)}%"></div>
          <span class="path">${esc(d.path)}</span></div>
        <div class="sz">${human(d.size)}</div>
      </div>`
    )
    .join("");
  const files = r.top_files
    .map((f) => `<div class="frow"><span class="sz">${human(f.size)}</span><span class="path">${esc(f.path)}</span></div>`)
    .join("");
  $("results").innerHTML = `
    <div class="stats">
      ${card("total", human(r.total_size))}
      ${card("files", r.total_files.toLocaleString())}
      ${card("folders", r.total_dirs.toLocaleString())}
    </div>
    <h2>Biggest sub-folders</h2>
    <div class="bars">${bars || "<em>none</em>"}</div>
    <h2>Biggest files</h2>
    <div class="files">${files || "<em>none</em>"}</div>`;
}

function renderDupes(groups) {
  if (!groups.length) {
    $("results").innerHTML = `<div class="ok">No duplicates found 🎉</div>`;
    return;
  }
  const total = groups.reduce((a, g) => a + g.wasted, 0);
  const items = groups
    .map(
      (g) => `
      <div class="dup">
        <div class="dhead"><span class="badge">${g.files.length}×</span> ${human(g.size)}
          <span class="waste">${human(g.wasted)} reclaimable</span></div>
        ${g.files.map((f) => `<div class="path small">${esc(f)}</div>`).join("")}
      </div>`
    )
    .join("");
  $("results").innerHTML = `<div class="ok">${groups.length} duplicate groups — <b>${human(total)}</b> reclaimable</div>${items}`;
}

$("btn-scan").addEventListener("click", doScan);
$("btn-dupes").addEventListener("click", doDupes);
$("path").addEventListener("keydown", (e) => {
  if (e.key === "Enter") doScan();
});
