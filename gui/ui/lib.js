"use strict";

// Pure helpers shared by the UI (as globals) and its unit tests (as a Node
// module). Nothing in here touches the DOM or Tauri.
(function (root, factory) {
  const lib = factory();
  if (typeof module !== "undefined" && module.exports) module.exports = lib;
  else Object.assign(root, lib);
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  // Bytes as a short human string: 1536 -> "1.5 KB".
  function human(bytes) {
    const u = ["B", "KB", "MB", "GB", "TB", "PB"];
    let s = bytes, i = 0;
    while (s >= 1024 && i < u.length - 1) { s /= 1024; i++; }
    return i === 0 ? `${bytes} B` : `${s.toFixed(1)} ${u[i]}`;
  }

  // A signed byte difference: +1.5 KB / -1.5 KB / 0 B.
  function humanDelta(delta) {
    if (!delta) return "0 B";
    return (delta > 0 ? "+" : "-") + human(Math.abs(delta));
  }

  function fmtMs(ms) {
    if (ms < 1000) return `${Math.round(ms)} ms`;
    const s = ms / 1000;
    if (s < 60) return `${s.toFixed(1)} s`;
    const m = Math.floor(s / 60);
    return `${m}m ${Math.round(s - m * 60)}s`;
  }

  function baseName(p) {
    const parts = String(p).split(/[\\/]/).filter(Boolean);
    return parts[parts.length - 1] || p;
  }

  // Unix seconds -> "YYYY-MM-DD HH:MM" in the viewer's local time.
  function fmtDate(secs) {
    const d = new Date(secs * 1000);
    const p = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
  }

  // Breadcrumb segments for a path: [{label, target}], where `target` is the
  // full path to scan when that segment is clicked. A drive root becomes
  // "C:\" (not "C:", which Windows resolves to the drive's working directory),
  // a POSIX root "/", and a UNC share "\\server\share" stays one segment.
  function crumbs(path) {
    const p = String(path);
    const win = p.includes("\\") || /^[A-Za-z]:/.test(p);
    const sep = win ? "\\" : "/";
    let parts = p.split(/[\\/]+/).filter((s, i) => s !== "" || i === 0);
    const out = [];
    let acc = "";
    if (win && p.startsWith("\\\\") && parts.length >= 3) {
      acc = `\\\\${parts[1]}\\${parts[2]}`;
      out.push({ label: acc, target: acc });
      parts = parts.slice(3);
    } else if (parts.length && parts[0] === "") {
      acc = "/";
      out.push({ label: "/", target: "/" });
      parts = parts.slice(1);
    } else if (parts.length && win && /^[A-Za-z]:$/.test(parts[0])) {
      acc = `${parts[0]}\\`;
      out.push({ label: parts[0], target: acc });
      parts = parts.slice(1);
    }
    for (const part of parts) {
      acc = acc === "" || acc.endsWith(sep) ? acc + part : acc + sep + part;
      out.push({ label: part, target: acc });
    }
    return out;
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

  // How to colour a folder by its change: a CSS class and a 0..1 strength
  // relative to the biggest change on screen.
  function deltaTint(delta, maxAbs) {
    if (!delta) return { cls: "", strength: 0 };
    const strength = maxAbs > 0 ? Math.min(1, Math.abs(delta) / maxAbs) : 1;
    return { cls: delta > 0 ? "grow" : "shrink", strength };
  }

  // Index a diff report's folder changes by path so rows/tiles can look
  // theirs up: {path -> {delta}} for grown/shrunk, {path -> {added:true}}
  // for new folders. Also returns the biggest |delta| for tint scaling.
  function changesByPath(diff) {
    const map = new Map();
    let maxAbs = 0;
    for (const g of diff.grown || []) { map.set(String(g.path), { delta: g.delta }); maxAbs = Math.max(maxAbs, Math.abs(g.delta)); }
    for (const s of diff.shrunk || []) { map.set(String(s.path), { delta: s.delta }); maxAbs = Math.max(maxAbs, Math.abs(s.delta)); }
    for (const a of diff.added || []) { map.set(String(a.path), { added: true, delta: a.size }); maxAbs = Math.max(maxAbs, a.size); }
    return { map, maxAbs };
  }

  return { human, humanDelta, fmtMs, baseName, fmtDate, crumbs, squarify, deltaTint, changesByPath };
});
