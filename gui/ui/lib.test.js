"use strict";

// Unit tests for the pure UI helpers. Run with: node --test gui/ui/lib.test.js
const test = require("node:test");
const assert = require("node:assert/strict");
const lib = require("./lib.js");

test("human formats bytes like the Rust side", () => {
  assert.equal(lib.human(0), "0 B");
  assert.equal(lib.human(1023), "1023 B");
  assert.equal(lib.human(1024), "1.0 KB");
  assert.equal(lib.human(1536), "1.5 KB");
  assert.equal(lib.human(1024 ** 3 * 2.25), "2.3 GB");
  assert.equal(lib.human(1024 ** 5), "1.0 PB");
  assert.equal(lib.human(1024 ** 6), "1024.0 PB"); // capped at PB
});

test("humanDelta carries the sign", () => {
  assert.equal(lib.humanDelta(0), "0 B");
  assert.equal(lib.humanDelta(1536), "+1.5 KB");
  assert.equal(lib.humanDelta(-1536), "-1.5 KB");
  assert.equal(lib.humanDelta(-1), "-1 B");
});

test("fmtMs picks a unit", () => {
  assert.equal(lib.fmtMs(12.4), "12 ms");
  assert.equal(lib.fmtMs(1500), "1.5 s");
  assert.equal(lib.fmtMs(61_000), "1m 1s");
});

test("baseName takes the last segment on either separator", () => {
  assert.equal(lib.baseName("C:\\Users\\me\\Downloads"), "Downloads");
  assert.equal(lib.baseName("/home/me/x/"), "x");
  assert.equal(lib.baseName("/"), "/");
});

test("fmtDate renders local YYYY-MM-DD HH:MM", () => {
  const s = lib.fmtDate(1_600_000_000);
  assert.match(s, /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);
  assert.equal(s.slice(0, 4), String(new Date(1_600_000_000 * 1000).getFullYear()));
});

test("crumbs keeps a drive root scannable", () => {
  assert.deepEqual(lib.crumbs("C:\\Users\\me"), [
    { label: "C:", target: "C:\\" },
    { label: "Users", target: "C:\\Users" },
    { label: "me", target: "C:\\Users\\me" },
  ]);
  assert.deepEqual(lib.crumbs("C:\\"), [{ label: "C:", target: "C:\\" }]);
  assert.deepEqual(lib.crumbs("D:\\Media\\"), [
    { label: "D:", target: "D:\\" },
    { label: "Media", target: "D:\\Media" },
  ]);
});

test("crumbs handles POSIX, UNC and relative paths", () => {
  assert.deepEqual(lib.crumbs("/home/me"), [
    { label: "/", target: "/" },
    { label: "home", target: "/home" },
    { label: "me", target: "/home/me" },
  ]);
  assert.deepEqual(lib.crumbs("/"), [{ label: "/", target: "/" }]);
  assert.deepEqual(lib.crumbs("\\\\server\\share\\x"), [
    { label: "\\\\server\\share", target: "\\\\server\\share" },
    { label: "x", target: "\\\\server\\share\\x" },
  ]);
  assert.deepEqual(lib.crumbs("foo/bar"), [
    { label: "foo", target: "foo" },
    { label: "bar", target: "foo/bar" },
  ]);
});

test("squarify tiles the whole area proportionally", () => {
  const items = [{ size: 6 }, { size: 3 }, { size: 1 }];
  const rects = lib.squarify(items, 0, 0, 100, 50);
  assert.equal(rects.length, 3);
  const area = rects.reduce((a, r) => a + r.w * r.h, 0);
  assert.ok(Math.abs(area - 5000) < 1e-6, `area ${area}`);
  assert.ok(Math.abs(rects[0].w * rects[0].h - 3000) < 1e-6);
  for (const r of rects) {
    assert.ok(r.x >= 0 && r.y >= 0 && r.x + r.w <= 100 + 1e-9 && r.y + r.h <= 50 + 1e-9);
  }
  assert.deepEqual(lib.squarify([], 0, 0, 10, 10), []);
});

test("deltaTint scales against the biggest change", () => {
  assert.deepEqual(lib.deltaTint(0, 100), { cls: "", strength: 0 });
  assert.deepEqual(lib.deltaTint(50, 100), { cls: "grow", strength: 0.5 });
  assert.deepEqual(lib.deltaTint(-100, 100), { cls: "shrink", strength: 1 });
  assert.deepEqual(lib.deltaTint(500, 100), { cls: "grow", strength: 1 });
  assert.deepEqual(lib.deltaTint(7, 0), { cls: "grow", strength: 1 });
});

test("changesByPath indexes grown, shrunk and added folders", () => {
  const { map, maxAbs } = lib.changesByPath({
    grown: [{ path: "/a", delta: 300 }],
    shrunk: [{ path: "/b", delta: -50 }],
    added: [{ path: "/c", size: 900 }],
    removed: [{ path: "/d", size: 10 }],
  });
  assert.equal(maxAbs, 900);
  assert.deepEqual(map.get("/a"), { delta: 300 });
  assert.deepEqual(map.get("/b"), { delta: -50 });
  assert.deepEqual(map.get("/c"), { added: true, delta: 900 });
  assert.equal(map.has("/d"), false);
  assert.equal(lib.changesByPath({}).maxAbs, 0);
});
