// Build the `diskghost` CLI and put it where Tauri looks for the sidecar
// (`bundle.externalBin`): binaries/diskghost-<target triple>[.exe]. Runs as
// beforeBuildCommand / beforeDevCommand; run it by hand before a plain
// `cargo build` here (tauri-build refuses to build without the file).
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const tauriDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const root = join(tauriDir, "..", "..");

// Tauri passes the target it builds for; otherwise build for this machine.
const triple =
  process.env.TAURI_ENV_TARGET_TRIPLE ||
  /^host: (\S+)$/m.exec(execFileSync("rustc", ["-vV"], { encoding: "utf8" }))[1];
const debug = process.env.TAURI_ENV_DEBUG === "true";
const profile = debug ? "debug" : "release";

const args = ["build", "-p", "diskghost-cli", "--target", triple];
if (!debug) args.push("--release");
execFileSync("cargo", args, { cwd: root, stdio: "inherit" });

const ext = triple.includes("windows") ? ".exe" : "";
const targetDir = process.env.CARGO_TARGET_DIR || join(root, "target");
const outDir = join(tauriDir, "binaries");
mkdirSync(outDir, { recursive: true });
copyFileSync(
  join(targetDir, triple, profile, `diskghost${ext}`),
  join(outDir, `diskghost-${triple}${ext}`),
);
console.log(`CLI sidecar: binaries/diskghost-${triple}${ext} (${profile})`);
