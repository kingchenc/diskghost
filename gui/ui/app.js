"use strict";

// Pure helpers (human, humanDelta, fmtMs, baseName, fmtDate, crumbs, squarify,
// deltaTint, changesByPath, pct, EXCLUDE_PRESETS, mergeSettings,
// activeExcludes, categorize, bucketLabel, jobsToStart, mapItems, parentPath,
// isUnder) come from lib.js as globals.

const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);

let settings = mergeSettings(null);
let drives = [];

// Every scan / duplicate search the user started, oldest first. A job is
// queued until a slot is free (settings.parallel), then running, then done,
// cancelled or error. A finished scan keeps its current view, so the user can
// switch between several scans without losing their place.
const jobs = [];
let activeId = null;
let nextId = 1;

// ---------------------------------- i18n ----------------------------------
const I18N = {
  en: {
    pathLabel: "Folder path", placeholder: "Paste a folder path, drop folders anywhere, or use Browse…",
    browse: "Browse…", scan: "Scan", dupes: "Find duplicates", settings: "Settings", refresh: "Refresh",
    drives: "Drives", scanAll: "Scan all drives", scans: "Scans", clearDone: "Clear finished",
    dropHint: "Drop folders anywhere — each one is queued as its own scan.",
    dropHere: "Drop folders to scan them",
    excludesNone: "No excludes", excludesOn: "excludes active",
    free: "free", of: "of", used: "used",
    kindFixed: "Local disk", kindRemovable: "Removable", kindNetwork: "Network drive", kindOther: "Volume",
    stQueued: "Queued", stRunning: "Scanning", stSearching: "Searching duplicates", stDone: "Done",
    stCancelled: "Cancelled — partial", stError: "Error", stLoading: "Loading",
    queuedPos: "Waiting in queue — position", queuedWhy: "Scans running at the same time:",
    removeFromQueue: "Remove from queue", cancel: "Cancel", close: "Close", retry: "Try again",
    files: "files", bytes: "found", elapsed: "elapsed", rate: "files / s",
    estimated: "of the drive's used space", noEstimate: "Size unknown until the walk ends",
    queueInfo: (r, q, p) => `${r} running · ${q} queued · up to ${p} at once`,
    welcomeTitle: "Where did my disk space go?",
    welcomeText: "Drop one or more folders onto the window, pick a drive below, or use Browse. Every scan gets its own tab on the left — they run side by side or wait in the queue.",
    up: "Up", back: "Back", rescan: "Rescan", dupesHere: "Find duplicates here",
    backHint: "Previous folder (mouse back button, Alt+←)", upHint: "Parent folder (right-click the map, Backspace, Alt+↑)",
    cancelling: "Cancelling…",
    cancelledNotice: "This scan was cancelled — every number covers only the part that was walked.",
    kSize: "Size", kSizeOfUsed: "of the used space on", kSizeOfDisk: "of the disk",
    kFiles: "Files", kAvg: "avg", kPerFile: "per file",
    kFolders: "Folders", kDirect: "files directly in this folder",
    kBiggestFolder: "Biggest folder", kBiggestFile: "Biggest file",
    kFree: "Free on this drive", kUsedPct: "used",
    kScan: "Scan", kSkipped: "unreadable, skipped", kNoSkip: "nothing skipped",
    kSince: "Since",
    map: "Map", mapSub: "Area = size. Click to open a folder, right-click to go up, hover for details.",
    filesHere: "files directly in this folder", otherFolders: "smaller folders", filesSmaller: "files & smaller folders",
    shareOfView: "of this folder", folderFiles: "Files",
    types: "What it is made of", typesSub: "by file type, size on disk",
    disk: "Drive", diskThis: "This folder", diskOther: "Everything else", diskFree: "Free",
    tabFolders: "Folders", tabFiles: "Biggest files", tabTypes: "Extensions", tabSizes: "Size ranges", tabChanges: "Changes",
    colName: "Name", colShare: "Share", colSize: "Size", colPct: "%", colFiles: "Files", colDelta: "Change",
    colType: "Type", colExt: "Extension", colCategory: "Category", colCount: "Files",
    filterFiles: "Filter by name or path…", allCategories: "All categories",
    sizesCount: "Number of files per size range", sizesBytes: "Space taken per size range",
    sizesHint: "Many tiny files waste space on block slack and slow down copies; a few huge ones are the quickest wins.",
    since: "Compare with", showChanges: "Show changes", comparing: "Comparing…",
    firstScan: "First scan of this folder — come back after the next scan to see what grew.",
    changes: "Changes since", unchanged: "Nothing changed.", newBadge: "new",
    grew: "Grew", shrank: "Shrank", newFolders: "New folders", removedFolders: "Removed folders",
    newFiles: "New files", removedFiles: "Removed files", changedFiles: "Changed files", biggest: "biggest",
    sinceLast: "Since last scan", firstScanShort: "first scan — compared from the next one on", clickDetails: "click for details",
    newShort: "new", goneShort: "gone", changedShort: "changed",
    oldSnapshotNoFiles: "That snapshot comes from an older version and only knows folders — file changes show up from the next comparison on.",
    snapshots: "Saved snapshots", onDisk: "on disk", thisScan: "this scan", filesAndFolders: "files & folders",
    foldersOnly: "folders only", compare: "Compare", deleteSnapshot: "Delete this snapshot?",
    snapshotsHint: (n) => `Every complete scan is saved automatically (all folders and files, compressed); the newest ${n} per folder are kept — see Settings.`,
    noDupes: "No duplicates found 🎉", noOwnDupes: "No duplicates among your own files 🎉 — program and game files are hidden.",
    ownReclaim: "Your files — reclaimable", protHidden: "Program & game files (hidden)", protHiddenSub: "click to show",
    dupHint: "Every group is byte-for-byte identical content (BLAKE3 over the whole file), whatever the names. Installed programs and games keep such copies on purpose — deleting one breaks them, so they are hidden and never removed while hidden.",
    showProt: "Show program & game files", identical: "identical", protBadge: "program / game",
    protWhy: "Part of an installed program or game (by folder or file type) — deleting it can break that program.",
    protWarn: "This includes files of installed programs or games. They may stop working.",
    tabGroups: "Groups", tabFolderSets: "Folder pairs", colFolders: "Folders sharing identical files",
    folderFilter: "Only", sameFolder: "copies within this folder", nothingToReclaim: "Nothing to reclaim in the shown groups.", groups: "groups", reclaimable: "reclaimable",
    filter: "Filter", sort: "Sort", dryRun: "Dry-run", reclaimShown: "Reclaim shown",
    sortWasted: "wasted", sortSize: "size", sortCount: "count",
    actTrash: "→ Recycle Bin", actDelete: "Delete", actHardlink: "Hard-link",
    showing: "showing", keep: "keep", dupesIn: "Duplicates in",
    del: "Delete", deleting: "Removing…", removedMsg: "removed", errors: "error(s)",
    confirmTrash: "Move to the Recycle Bin?", confirmDelete: "Delete permanently?",
    confirmDeleteWarn: "This cannot be undone.", ok: "OK",
    reclaimConfirm: (a, n) => `${a}: ${n} file(s)?`, irreversible: "This cannot be undone.",
    enterPath: "Enter a folder path first.",
    // categories
    cat_video: "Video", cat_image: "Images", cat_audio: "Audio", cat_archive: "Archives",
    cat_diskimage: "Disk images & VMs", cat_document: "Documents", cat_code: "Code & text data",
    cat_program: "Programs & libraries", cat_gamedata: "Game data", cat_database: "Databases", cat_other: "Other",
    noExt: "(no extension)",
    // settings
    setTitle: "Settings", setDone: "Done", setReset: "Reset to defaults",
    setExcl: "Exclusions", setExclDesc: "Templates for well-known build output, caches and system files. Excluded folders are skipped entirely — no time spent walking them. Click a pattern to switch just that one off. Changes apply to new scans; use Rescan for an open one.",
    setCustom: "Your own patterns", setCustomDesc: "A folder or file name (node_modules), a wildcard (*.iso) or a whole path pattern.",
    setAdd: "Add", setBadGlob: "Not a valid pattern:", setDup: "Already in the list.",
    setScan: "Scanning", setTop: "Items per list", setTopD: "Folders and files listed per view.",
    setDepth: "Max depth", setDepthD: "Empty = unlimited.", setFollow: "Follow symlinks / junctions", setFollowD: "Off avoids loops and double counting.",
    setParallel: "Parallel scans", setParallelD: "Scans running at the same time; more wait in the queue. 1 is best for a single HDD.",
    setDupes: "Duplicates", setMinMb: "Ignore files smaller than (MB)",
    setDelete: "Deleting", setTrash: "Send deleted items to the Recycle Bin", setTrashD: "Off deletes permanently.",
    setEngine: "Scan engine", engineFast: "Fast — SSD / NVMe (default)", engineGentle: "Gentle — HDD / in the background",
    engineFastD: "Lists many folders at once (2 threads per CPU core). Fastest on SSDs; on a spinning disk the head jumps around and it can get slower.",
    engineGentleD: "3 threads. Best for spinning disks and network drives, and keeps the PC responsive while it scans — slower on SSDs.",
    setKeep: "Snapshots kept per folder", setKeepD: "Older ones are deleted automatically. About 8 bytes per file each.",
    setLook: "Window & language", setWin: "Window size at start", setLang: "Language",
    winAuto: "Auto (1440p if it fits, else maximized)", winMax: "Maximized", win1080: "1920 × 1080", win1440: "2560 × 1440", win4k: "3840 × 2160 (4K)",
    langAuto: "System", langEn: "English", langDe: "Deutsch",
    // presets
    pre_vcs: "Version control", pre_node: "JavaScript / Node.js", pre_python: "Python", pre_rust: "Rust / Maven (target)",
    pre_jvm: "Java / Kotlin / Gradle", pre_dotnet: ".NET / C#", pre_vendor: "Go / PHP vendor", pre_cpp: "C / C++ build output",
    pre_dart: "Dart / Flutter", pre_ide: "IDE & editor folders", pre_caches: "Caches", pre_temp: "Temporary files",
    pre_logs: "Log files", pre_winsys: "Windows system files", pre_macos: "macOS system files",
  },
  de: {
    pathLabel: "Ordnerpfad", placeholder: "Ordnerpfad einfügen, Ordner irgendwo hinziehen oder Durchsuchen…",
    browse: "Durchsuchen…", scan: "Scannen", dupes: "Duplikate finden", settings: "Einstellungen", refresh: "Aktualisieren",
    drives: "Laufwerke", scanAll: "Alle Laufwerke scannen", scans: "Scans", clearDone: "Fertige entfernen",
    dropHint: "Ordner irgendwo hinziehen — jeder wird als eigener Scan eingereiht.",
    dropHere: "Ordner ablegen zum Scannen",
    excludesNone: "Keine Ausschlüsse", excludesOn: "Ausschlüsse aktiv",
    free: "frei", of: "von", used: "belegt",
    kindFixed: "Lokale Festplatte", kindRemovable: "Wechseldatenträger", kindNetwork: "Netzlaufwerk", kindOther: "Volume",
    stQueued: "Wartet", stRunning: "Scannt", stSearching: "Sucht Duplikate", stDone: "Fertig",
    stCancelled: "Abgebrochen — unvollständig", stError: "Fehler", stLoading: "Lädt",
    queuedPos: "Wartet in der Warteschlange — Position", queuedWhy: "Gleichzeitig laufende Scans:",
    removeFromQueue: "Aus Warteschlange entfernen", cancel: "Abbrechen", close: "Schließen", retry: "Erneut versuchen",
    files: "Dateien", bytes: "gefunden", elapsed: "vergangen", rate: "Dateien / s",
    estimated: "des belegten Platzes auf dem Laufwerk", noEstimate: "Größe erst nach dem Scan bekannt",
    queueInfo: (r, q, p) => `${r} laufen · ${q} warten · bis zu ${p} gleichzeitig`,
    welcomeTitle: "Wo ist mein Speicherplatz hin?",
    welcomeText: "Zieh einen oder mehrere Ordner aufs Fenster, wähle unten ein Laufwerk oder nutze Durchsuchen. Jeder Scan bekommt links einen eigenen Eintrag — sie laufen parallel oder warten in der Warteschlange.",
    up: "Hoch", back: "Zurück", rescan: "Neu scannen", dupesHere: "Duplikate hier finden",
    backHint: "Vorheriger Ordner (Maus-Zurück-Taste, Alt+←)", upHint: "Übergeordneter Ordner (Rechtsklick auf die Karte, Rücktaste, Alt+↑)",
    cancelling: "Breche ab…",
    cancelledNotice: "Dieser Scan wurde abgebrochen — alle Zahlen decken nur den gescannten Teil ab.",
    kSize: "Größe", kSizeOfUsed: "des belegten Platzes auf", kSizeOfDisk: "der Platte",
    kFiles: "Dateien", kAvg: "Ø", kPerFile: "pro Datei",
    kFolders: "Ordner", kDirect: "Dateien direkt in diesem Ordner",
    kBiggestFolder: "Größter Ordner", kBiggestFile: "Größte Datei",
    kFree: "Frei auf diesem Laufwerk", kUsedPct: "belegt",
    kScan: "Scan", kSkipped: "nicht lesbar, übersprungen", kNoSkip: "nichts übersprungen",
    kSince: "Seit",
    map: "Karte", mapSub: "Fläche = Größe. Klick öffnet einen Ordner, Rechtsklick geht hoch, drüberfahren zeigt Details.",
    filesHere: "Dateien direkt in diesem Ordner", otherFolders: "kleinere Ordner", filesSmaller: "Dateien & kleinere Ordner",
    shareOfView: "dieses Ordners", folderFiles: "Dateien",
    types: "Woraus es besteht", typesSub: "nach Dateityp, Größe auf der Platte",
    disk: "Laufwerk", diskThis: "Dieser Ordner", diskOther: "Alles andere", diskFree: "Frei",
    tabFolders: "Ordner", tabFiles: "Größte Dateien", tabTypes: "Endungen", tabSizes: "Größenbereiche", tabChanges: "Änderungen",
    colName: "Name", colShare: "Anteil", colSize: "Größe", colPct: "%", colFiles: "Dateien", colDelta: "Änderung",
    colType: "Typ", colExt: "Endung", colCategory: "Kategorie", colCount: "Dateien",
    filterFiles: "Nach Name oder Pfad filtern…", allCategories: "Alle Kategorien",
    sizesCount: "Anzahl Dateien pro Größenbereich", sizesBytes: "Belegter Platz pro Größenbereich",
    sizesHint: "Viele winzige Dateien verschwenden Platz durch Block-Verschnitt und bremsen Kopien; wenige riesige sind der schnellste Gewinn.",
    since: "Vergleichen mit", showChanges: "Änderungen zeigen", comparing: "Vergleiche…",
    firstScan: "Erster Scan dieses Ordners — nach dem nächsten Scan siehst du hier, was gewachsen ist.",
    changes: "Änderungen seit", unchanged: "Nichts geändert.", newBadge: "neu",
    grew: "Gewachsen", shrank: "Geschrumpft", newFolders: "Neue Ordner", removedFolders: "Entfernte Ordner",
    newFiles: "Neue Dateien", removedFiles: "Entfernte Dateien", changedFiles: "Geänderte Dateien", biggest: "die größten",
    sinceLast: "Seit letztem Scan", firstScanShort: "erster Scan — Vergleich ab dem nächsten", clickDetails: "klicken für Details",
    newShort: "neu", goneShort: "weg", changedShort: "geändert",
    oldSnapshotNoFiles: "Dieser Snapshot stammt aus einer älteren Version und kennt nur Ordner — Dateiänderungen gibt es ab dem nächsten Vergleich.",
    snapshots: "Gespeicherte Snapshots", onDisk: "auf der Platte", thisScan: "dieser Scan", filesAndFolders: "Dateien & Ordner",
    foldersOnly: "nur Ordner", compare: "Vergleichen", deleteSnapshot: "Diesen Snapshot löschen?",
    snapshotsHint: (n) => `Jeder vollständige Scan wird automatisch gespeichert (alle Ordner und Dateien, komprimiert); die neuesten ${n} pro Ordner bleiben — siehe Einstellungen.`,
    noDupes: "Keine Duplikate gefunden 🎉", noOwnDupes: "Keine Duplikate unter deinen eigenen Dateien 🎉 — Programm- und Spieldateien sind ausgeblendet.",
    ownReclaim: "Deine Dateien — freigebbar", protHidden: "Programm- & Spieldateien (ausgeblendet)", protHiddenSub: "klicken zum Anzeigen",
    dupHint: "Jede Gruppe ist Byte für Byte identischer Inhalt (BLAKE3 über die ganze Datei), egal wie die Dateien heißen. Installierte Programme und Spiele haben solche Kopien absichtlich — Löschen macht sie kaputt, deshalb sind sie ausgeblendet und werden dann nie entfernt.",
    showProt: "Programm- & Spieldateien zeigen", identical: "identisch", protBadge: "Programm / Spiel",
    protWhy: "Gehört zu einem installierten Programm oder Spiel (nach Ordner oder Dateityp) — Löschen kann es beschädigen.",
    protWarn: "Darunter sind Dateien installierter Programme oder Spiele. Sie könnten danach nicht mehr funktionieren.",
    tabGroups: "Gruppen", tabFolderSets: "Ordner-Paare", colFolders: "Ordner mit identischen Dateien",
    folderFilter: "Nur", sameFolder: "Kopien in diesem Ordner", nothingToReclaim: "In den angezeigten Gruppen gibt es nichts freizugeben.", groups: "Gruppen", reclaimable: "freigebbar",
    filter: "Filter", sort: "Sortierung", dryRun: "Testlauf", reclaimShown: "Angezeigte freigeben",
    sortWasted: "verschwendet", sortSize: "Größe", sortCount: "Anzahl",
    actTrash: "→ Papierkorb", actDelete: "Löschen", actHardlink: "Hardlink",
    showing: "zeige", keep: "behalten", dupesIn: "Duplikate in",
    del: "Löschen", deleting: "Entferne…", removedMsg: "entfernt", errors: "Fehler",
    confirmTrash: "In den Papierkorb verschieben?", confirmDelete: "Endgültig löschen?",
    confirmDeleteWarn: "Das lässt sich nicht rückgängig machen.", ok: "OK",
    reclaimConfirm: (a, n) => `${a}: ${n} Datei(en)?`, irreversible: "Das lässt sich nicht rückgängig machen.",
    enterPath: "Bitte zuerst einen Ordnerpfad eingeben.",
    cat_video: "Video", cat_image: "Bilder", cat_audio: "Audio", cat_archive: "Archive",
    cat_diskimage: "Disk-Images & VMs", cat_document: "Dokumente", cat_code: "Code & Textdaten",
    cat_program: "Programme & Bibliotheken", cat_gamedata: "Spieldaten", cat_database: "Datenbanken", cat_other: "Sonstiges",
    noExt: "(keine Endung)",
    setTitle: "Einstellungen", setDone: "Fertig", setReset: "Auf Standard zurücksetzen",
    setExcl: "Ausschlüsse", setExclDesc: "Vorlagen für bekannte Build-Ausgaben, Caches und Systemdateien. Ausgeschlossene Ordner werden komplett übersprungen — kein Zeitaufwand. Klick auf ein Muster schaltet nur dieses ab. Gilt für neue Scans; für einen offenen Scan „Neu scannen“.",
    setCustom: "Eigene Muster", setCustomDesc: "Ein Ordner- oder Dateiname (node_modules), ein Platzhalter (*.iso) oder ein Pfadmuster.",
    setAdd: "Hinzufügen", setBadGlob: "Kein gültiges Muster:", setDup: "Steht schon in der Liste.",
    setScan: "Scannen", setTop: "Einträge pro Liste", setTopD: "Ordner und Dateien pro Ansicht.",
    setDepth: "Max. Tiefe", setDepthD: "Leer = unbegrenzt.", setFollow: "Symlinks / Junctions folgen", setFollowD: "Aus vermeidet Schleifen und Doppelzählung.",
    setParallel: "Parallele Scans", setParallelD: "Gleichzeitig laufende Scans; weitere warten. 1 ist am besten für eine einzelne HDD.",
    setDupes: "Duplikate", setMinMb: "Dateien kleiner als (MB) ignorieren",
    setDelete: "Löschen", setTrash: "Gelöschtes in den Papierkorb", setTrashD: "Aus löscht endgültig.",
    setEngine: "Scan-Engine", engineFast: "Schnell — SSD / NVMe (Standard)", engineGentle: "Schonend — HDD / im Hintergrund",
    engineFastD: "Liest viele Ordner gleichzeitig (2 Threads pro CPU-Kern). Am schnellsten auf SSDs; auf einer HDD springt der Lesekopf hin und her und es kann langsamer werden.",
    engineGentleD: "3 Threads. Am besten für HDDs und Netzlaufwerke, und der PC bleibt beim Scannen flüssig — auf SSDs langsamer.",
    setKeep: "Snapshots pro Ordner behalten", setKeepD: "Ältere werden automatisch gelöscht. Etwa 8 Byte pro Datei je Snapshot.",
    setLook: "Fenster & Sprache", setWin: "Fenstergröße beim Start", setLang: "Sprache",
    winAuto: "Auto (1440p wenn es passt, sonst maximiert)", winMax: "Maximiert", win1080: "1920 × 1080", win1440: "2560 × 1440", win4k: "3840 × 2160 (4K)",
    langAuto: "System", langEn: "English", langDe: "Deutsch",
    pre_vcs: "Versionskontrolle", pre_node: "JavaScript / Node.js", pre_python: "Python", pre_rust: "Rust / Maven (target)",
    pre_jvm: "Java / Kotlin / Gradle", pre_dotnet: ".NET / C#", pre_vendor: "Go / PHP vendor", pre_cpp: "C / C++ Build-Ausgabe",
    pre_dart: "Dart / Flutter", pre_ide: "IDE- & Editor-Ordner", pre_caches: "Caches", pre_temp: "Temporäre Dateien",
    pre_logs: "Log-Dateien", pre_winsys: "Windows-Systemdateien", pre_macos: "macOS-Systemdateien",
  },
};
const lang = () => {
  if (settings.language !== "auto") return settings.language;
  return (navigator.language || "en").toLowerCase().startsWith("de") ? "de" : "en";
};
const t = (k) => I18N[lang()][k] ?? I18N.en[k] ?? k;
const num = (n) => Number(n || 0).toLocaleString(lang() === "de" ? "de-DE" : "en-US");

function applyI18n() {
  document.documentElement.lang = lang();
  document.querySelectorAll("[data-i18n]").forEach((e) => (e.textContent = t(e.dataset.i18n)));
  document.querySelectorAll("[data-i18n-ph]").forEach((e) => (e.placeholder = t(e.dataset.i18nPh)));
  document.querySelectorAll("[data-i18n-title]").forEach((e) => (e.title = t(e.dataset.i18nTitle)));
}

// ---------------------------- safe DOM builder ----------------------------
function el(tag, attrs, ...kids) {
  const n = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v == null || v === false) continue;
      if (k === "class") n.className = v;
      else if (k === "text") n.textContent = v;
      else if (k === "style") Object.assign(n.style, v);
      else if (k.startsWith("on") && typeof v === "function") n.addEventListener(k.slice(2), v);
      else n.setAttribute(k, v === true ? "" : v);
    }
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    n.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return n;
}

function status(msg, isError = false) {
  const s = $("status");
  s.textContent = msg || "";
  s.classList.toggle("error-msg", isError);
}

const errText = (e) => (typeof e === "string" ? e : e?.message ?? JSON.stringify(e));

// ------------------------------- tooltip -------------------------------
// One shared hover card. `content` is built lazily on enter.
function tip(node, content) {
  const box = $("tooltip");
  // Measured once per hover; moving only sets a transform (no layout work).
  let w = 0, h = 0;
  const move = (ev) => {
    const pad = 16;
    let x = ev.clientX + pad, y = ev.clientY + pad;
    if (x + w > innerWidth - 8) x = ev.clientX - w - pad;
    if (y + h > innerHeight - 8) y = ev.clientY - h - pad;
    box.style.transform = `translate(${Math.max(8, x)}px, ${Math.max(8, y)}px)`;
  };
  node.addEventListener("mouseenter", (ev) => {
    box.replaceChildren(...[content()].flat());
    box.classList.add("show");
    ({ width: w, height: h } = box.getBoundingClientRect());
    move(ev);
  });
  node.addEventListener("mousemove", move);
  node.addEventListener("mouseleave", () => box.classList.remove("show"));
}
const hideTip = () => $("tooltip").classList.remove("show");
const tipRow = (label, value) => el("div", { class: "tr" }, el("span", { text: label }), el("span", { text: value }));

// ------------------------------- dialogs -------------------------------
function ask(question, detail, okLabel, danger) {
  const dlg = $("confirm-dlg");
  return new Promise((resolve) => {
    const done = (v) => { dlg.close(); resolve(v); };
    dlg.replaceChildren(el("div", { class: "dlg-in" },
      el("div", { class: "dlg-head" }, el("h2", { text: question })),
      el("div", { class: "dlg-body" }, detail.map((d) => el("p", { class: d.cls || "", text: d.text }))),
      el("div", { class: "dlg-foot" },
        el("button", { type: "button", onclick: () => done(false) }, t("cancel")),
        el("button", { type: "button", class: danger ? "danger" : "primary", onclick: () => done(true) }, okLabel))));
    dlg.onclose = () => resolve(false);
    dlg.showModal();
  });
}

// ------------------------------- settings -------------------------------
let saveTimer = null;
function saveSettings() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => invoke("save_settings", { settings }).catch((e) => status(errText(e), true)), 250);
  updateExcludeChip();
}

function walkOpts() {
  return {
    exclude: activeExcludes(settings),
    max_depth: settings.max_depth,
    follow_symlinks: settings.follow_symlinks,
    // "gentle" keeps a spinning disk from seeking itself to death and leaves
    // the machine responsive; "fast" lets the walker pick (2 x CPU threads).
    threads: settings.engine === "gentle" ? 3 : null,
  };
}

function updateExcludeChip() {
  const n = activeExcludes(settings).length;
  const b = $("btn-excludes");
  b.textContent = n ? `⛔ ${n} ${t("excludesOn")}` : `⛔ ${t("excludesNone")}`;
  b.classList.toggle("on", n > 0);
  b.title = activeExcludes(settings).join(", ");
}

function field(label, desc, input) {
  return el("label", { class: "field" }, el("span", { class: "fl", text: label }), input, desc ? el("span", { class: "fd", text: desc }) : null);
}
function checkField(label, desc, checked, onchange) {
  const box = el("input", { type: "checkbox", onchange: (e) => onchange(e.target.checked) });
  box.checked = checked;
  return el("label", { class: "field chk" }, box, el("span", {}, el("span", { class: "fl", text: label }), desc ? el("div", { class: "fd", text: desc }) : null));
}
function numberInput(value, min, max, onchange, placeholder) {
  return el("input", {
    type: "number", min, max, value: value ?? "", placeholder,
    onchange: (e) => {
      const v = parseInt(e.target.value, 10);
      onchange(Number.isFinite(v) ? Math.min(max, Math.max(min, v)) : null);
    },
  });
}
function selectInput(value, options, onchange) {
  const s = el("select", { onchange: (e) => onchange(e.target.value) });
  for (const [v, label] of options) {
    const o = el("option", { value: v, text: label });
    if (v === value) o.selected = true;
    s.append(o);
  }
  return s;
}

function openSettings(focusExcludes = false) {
  const dlg = $("settings-dlg");
  renderSettings();
  dlg.showModal();
  if (focusExcludes) dlg.querySelector(".set-sec")?.scrollIntoView();
}

function renderSettings() {
  const dlg = $("settings-dlg");
  const scrollTop = dlg.querySelector(".dlg-body")?.scrollTop ?? 0;
  const set = (patch, rerender = false) => {
    Object.assign(settings, patch);
    saveSettings();
    if (rerender) renderSettings();
  };

  // Exclusion templates: the whole template, or single patterns inside it.
  const presets = el("div", { class: "presets" });
  for (const p of EXCLUDE_PRESETS) {
    const on = settings.presets.includes(p.id);
    const box = el("input", {
      type: "checkbox",
      onchange: (e) => set({ presets: e.target.checked ? [...settings.presets, p.id] : settings.presets.filter((x) => x !== p.id) }, true),
    });
    box.checked = on;
    const chips = el("div", { class: "chips" }, p.patterns.map((pat) => {
      const key = `${p.id}:${pat}`;
      const off = settings.preset_off.includes(key);
      return el("button", {
        type: "button", class: `chip${!on || off ? " off" : ""}`, disabled: !on,
        onclick: () => set({ preset_off: off ? settings.preset_off.filter((k) => k !== key) : [...settings.preset_off, key] }, true),
      }, pat);
    }));
    presets.append(el("div", { class: `preset${on ? " on" : ""}` }, el("label", { class: "ph" }, box, t(`pre_${p.id}`)), chips));
  }

  // Custom patterns: toggle, remove, add (validated by the core's glob parser).
  const customChips = el("div", { class: "chips" }, settings.custom.map((c, i) =>
    el("span", { class: `chip${c.on ? "" : " off"}` },
      el("button", {
        type: "button", class: "x", style: { color: "inherit", textDecoration: "inherit", fontFamily: "inherit", fontSize: "12px" },
        onclick: () => { settings.custom[i].on = !c.on; set({}, true); },
      }, c.pattern),
      el("button", { type: "button", class: "x", title: t("del"), onclick: () => { settings.custom.splice(i, 1); set({}, true); } }, "✕"))));
  const addInput = el("input", { type: "text", placeholder: "*.iso, Downloads, node_modules", spellcheck: "false" });
  const addMsg = el("span", { class: "fd" });
  const add = async () => {
    const pats = addInput.value.split(",").map((s) => s.trim()).filter(Boolean);
    if (!pats.length) return;
    const bad = await invoke("check_globs", { patterns: pats }).catch(() => []);
    if (bad.length) { addMsg.textContent = `${t("setBadGlob")} ${bad.join(", ")}`; addMsg.style.color = "var(--danger)"; return; }
    const fresh = pats.filter((p) => !settings.custom.some((c) => c.pattern === p));
    if (!fresh.length) { addMsg.textContent = t("setDup"); return; }
    settings.custom.push(...fresh.map((pattern) => ({ pattern, on: true })));
    set({}, true);
    $("settings-dlg").querySelector(".addrow input")?.focus();
  };
  addInput.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); add(); } });

  const body = el("div", { class: "dlg-body" },
    el("section", { class: "set-sec" },
      el("h2", { text: `${t("setExcl")} — ${activeExcludes(settings).length} ${t("excludesOn")}` }),
      el("div", { class: "desc", text: t("setExclDesc") }),
      presets,
      el("h3", { text: t("setCustom"), style: { marginTop: "6px" } }),
      el("div", { class: "desc", text: t("setCustomDesc") }),
      customChips,
      el("div", { class: "addrow" }, addInput, el("button", { type: "button", onclick: add }, t("setAdd"))),
      addMsg),
    el("section", { class: "set-sec" },
      el("h2", { text: t("setScan") }),
      el("div", { class: "set-grid" },
        field(t("setTop"), t("setTopD"), numberInput(settings.top, 1, 1000, (v) => set({ top: v ?? 50 }))),
        field(t("setDepth"), t("setDepthD"), numberInput(settings.max_depth, 1, 100, (v) => set({ max_depth: v }), "∞")),
        field(t("setParallel"), t("setParallelD"), numberInput(settings.parallel, 1, 8, (v) => { set({ parallel: v ?? 2 }); pump(); })),
        checkField(t("setFollow"), t("setFollowD"), settings.follow_symlinks, (v) => set({ follow_symlinks: v }))),
      el("div", { class: "set-grid" },
        field(t("setEngine"), settings.engine === "gentle" ? t("engineGentleD") : t("engineFastD"),
          selectInput(settings.engine, [["fast", t("engineFast")], ["gentle", t("engineGentle")]], (v) => set({ engine: v }, true))),
        field(t("setKeep"), t("setKeepD"), numberInput(settings.keep_snapshots, 1, 100, (v) => set({ keep_snapshots: v ?? 10 }))))),
    el("section", { class: "set-sec" },
      el("h2", { text: `${t("setDupes")} & ${t("setDelete")}` }),
      el("div", { class: "set-grid" },
        field(t("setMinMb"), null, numberInput(settings.min_mb, 0, 1_000_000, (v) => set({ min_mb: v ?? 0 }))),
        checkField(t("setTrash"), t("setTrashD"), settings.trash, (v) => set({ trash: v })))),
    el("section", { class: "set-sec" },
      el("h2", { text: t("setLook") }),
      el("div", { class: "set-grid" },
        field(t("setWin"), null, selectInput(settings.window_size,
          [["auto", t("winAuto")], ["maximized", t("winMax")], ["1080p", t("win1080")], ["1440p", t("win1440")], ["4k", t("win4k")]],
          (v) => set({ window_size: v }))),
        field(t("setLang"), null, selectInput(settings.language,
          [["auto", t("langAuto")], ["en", t("langEn")], ["de", t("langDe")]],
          (v) => { set({ language: v }, true); applyI18n(); renderAll(); })))));

  dlg.replaceChildren(el("div", { class: "dlg-in" },
    el("div", { class: "dlg-head" }, el("h2", { text: t("setTitle") }),
      el("button", { type: "button", class: "icon-btn", "aria-label": t("close"), onclick: () => dlg.close() }, "✕")),
    body,
    el("div", { class: "dlg-foot" },
      el("button", { type: "button", onclick: () => { settings = mergeSettings(null); saveSettings(); renderSettings(); applyI18n(); renderAll(); } }, t("setReset")),
      el("button", { type: "button", class: "primary", onclick: () => dlg.close() }, t("setDone")))));
  body.scrollTop = scrollTop;
}

// --------------------------------- drives ---------------------------------
async function loadDrives() {
  try { drives = await invoke("list_drives"); } catch (_) { drives = []; }
  renderDrives();
  if (!active()) renderMain();
}

const kindLabel = (k) => t({ fixed: "kindFixed", removable: "kindRemovable", network: "kindNetwork" }[k] || "kindOther");
const driveIcon = (k) => ({ removable: "🔌", network: "🌐" }[k] || "💽");

function driveButton(d) {
  const used = d.total - d.free;
  const name = d.label ? `${d.path.replace(/\\$/, "")} ${d.label}` : d.path;
  const btn = el("button", { type: "button", class: "drive", onclick: () => enqueue("scan", d.path) },
    el("span", { class: "dicon", text: driveIcon(d.kind) }),
    el("span", { class: "dname", text: name }),
    el("span", { class: "dsub", text: `${human(d.free)} ${t("free")} ${t("of")} ${human(d.total)}` }),
    el("span", { class: "meter" }, el("span", { class: used / d.total > 0.9 ? "hot" : "used", style: { width: `${(100 * used) / d.total}%` } })));
  tip(btn, () => [el("b", { text: name }),
    tipRow(kindLabel(d.kind), d.fs || ""),
    tipRow(t("used"), `${human(used)} (${pct(used, d.total)})`),
    tipRow(t("free"), human(d.free)),
    tipRow(t("colSize"), human(d.total))]);
  return btn;
}

function renderDrives() {
  $("drives").replaceChildren(...drives.map(driveButton));
  $("btn-scan-all").disabled = drives.length === 0;
}

// ---------------------------------- jobs ----------------------------------
const active = () => jobs.find((j) => j.id === activeId) || null;
const byId = (id) => jobs.find((j) => j.id === id);

function enqueue(kind, path, { select = true } = {}) {
  path = String(path || "").trim();
  if (!path) return status(t("enterPath"), true);
  const opts = walkOpts();
  // The same scan again: just show it (Rescan walks it afresh).
  const same = jobs.find((j) => j.kind === kind && j.root === path && j.state !== "error" &&
    JSON.stringify(j.opts) === JSON.stringify(opts));
  if (same) {
    if (select) selectJob(same.id);
    return same;
  }
  const j = {
    id: nextId++, kind, root: path, path, opts, top: settings.top, mb: settings.min_mb,
    state: "queued", files: 0, bytes: 0, startedAt: 0, elapsedMs: null,
    view: null, groups: null, error: null, tab: "folders",
    cache: new Map(), // folder -> view, so going back is instant
    back: [], // folders visited before, for Back
  };
  jobs.push(j);
  if (select) activeId = j.id;
  pump();
  renderAll();
  return j;
}

function pump() {
  for (const j of jobsToStart(jobs, settings.parallel)) runJob(j);
  renderQueueInfo();
}

// The sidebar always; the main area only if `j` is the job on screen — a scan
// finishing in the background never rebuilds the view you are looking at.
function renderFor(j) {
  renderJobs();
  renderQueueInfo();
  if (!j || j.id === activeId) renderMain();
}

async function runJob(j) {
  j.state = "running";
  j.files = 0;
  j.bytes = 0;
  j.cancelRequested = false;
  j.startedAt = performance.now();
  j.scanUnix = Math.floor(Date.now() / 1000);
  j.error = null;
  renderFor(j);
  try {
    if (j.kind === "scan") {
      const view = await invoke("scan_dir", {
        job: j.id, path: j.root, top: j.top, opts: j.opts, keepSnapshots: settings.keep_snapshots,
      });
      j.elapsedMs = performance.now() - j.startedAt;
      j.root = String(view.root);
      j.cache = new Map([[j.root, view]]);
      j.back = [];
      j.view = view;
      j.path = j.root;
      j.diff = null;
      j.history = null;
      j.state = view.cancelled ? "cancelled" : "done";
      const back = j.returnTo;
      j.returnTo = null;
      if (back && back !== j.root) navigate(j, back, { keepTab: true, record: false });
      if (!view.cancelled) autoCompare(j);
    } else {
      j.groups = await invoke("find_dupes", { job: j.id, path: j.root, mb: j.mb, opts: j.opts });
      j.elapsedMs = performance.now() - j.startedAt;
      j.keep = new Map();
      j.tagged = false;
      j.dupSet = null;
      j.state = j.cancelRequested ? "cancelled" : "done";
    }
  } catch (e) {
    j.state = "error";
    j.error = errText(e);
  }
  if (byId(j.id)) renderFor(j);
  pump();
  if (j.kind === "scan" && j.state === "done") loadDrives();
}

async function cancelJob(j) {
  if (j.state === "queued") return closeJob(j);
  if (j.state === "running") {
    j.cancelRequested = true;
    renderFor(j);
    try { await invoke("cancel", { job: j.id }); } catch (_) { /* finished meanwhile */ }
  }
}

function closeJob(j) {
  if (j.state === "running") invoke("cancel", { job: j.id }).catch(() => {});
  const i = jobs.indexOf(j);
  if (i < 0) return;
  jobs.splice(i, 1);
  // Free the tree's memory.
  if (j.kind === "scan") invoke("forget_tree", { job: j.id }).catch(() => {});
  if (activeId === j.id) activeId = (jobs[i] || jobs[i - 1])?.id ?? null;
  pump();
  renderAll();
}

function selectJob(id) {
  if (activeId === id) return;
  activeId = id;
  const j = active();
  if (j && j.stale && j.kind === "scan" && j.view) {
    j.stale = false;
    navigate(j, j.path, { keepTab: true, record: false, force: true });
  }
  renderAll();
}

function rescan(j) {
  j.opts = walkOpts();
  j.top = settings.top;
  j.mb = settings.min_mb;
  j.returnTo = j.kind === "scan" ? j.path : null;
  j.auto = null;
  j.state = "queued";
  pump();
  renderAll();
}

// Open `path` inside job `j` — from the job's view cache (instant) or from
// its tree in memory (a fraction of a second). Never walks the disk.
async function navigate(j, path, { keepTab = false, record = true, force = false } = {}) {
  if (!force && j.view && path === j.path) return;
  if (record && j.path !== path) j.back.push(j.path);
  if (!keepTab && j.tab === "changes") j.tab = "folders";
  const show = (v) => {
    j.view = v;
    j.path = String(v.root);
    j.diff = null;
    j.fileFilter = "";
    j.catFilter = "";
    j.loading = false;
    if (byId(j.id)) renderFor(j);
  };
  const cached = !force && j.cache.get(path);
  if (cached) return show(cached);
  j.loading = true;
  $("main").classList.add("busy");
  try {
    const v = await invoke("view", { job: j.id, path, top: j.top });
    j.cache.set(String(v.root), v);
    show(v);
  } catch (e) {
    j.loading = false;
    if (record) j.back.pop();
    status(errText(e), true);
    if (byId(j.id)) renderFor(j);
  }
}

function goBack(j) {
  if (j?.view && j.back.length) navigate(j, j.back.pop(), { record: false });
}

function goUp(j) {
  const up = j?.view && parentPath(j.path);
  if (up && isUnder(up, j.root)) navigate(j, up);
}

// ------------------------------ rendering ------------------------------
function renderAll() {
  hideTip();
  renderJobs();
  renderMain();
  renderQueueInfo();
}

function renderQueueInfo() {
  const r = jobs.filter((j) => j.state === "running").length;
  const q = jobs.filter((j) => j.state === "queued").length;
  $("queue-info").textContent = r || q ? t("queueInfo")(r, q, settings.parallel) : "";
}

function jobIcon(j) {
  if (j.state === "error") return "⚠";
  if (j.state === "queued") return "⏳";
  if (j.kind === "dupes") return "👯";
  if (j.state === "cancelled") return "⏹";
  return j.state === "running" ? "🔍" : "📁";
}

function jobName(j) {
  return /^[A-Za-z]:\\?$/.test(j.root) || j.root === "/" ? j.root : baseName(j.root);
}

function jobSub(j) {
  if (j.state === "queued") return `${t("stQueued")} · ${j.root}`;
  if (j.state === "running") {
    const what = j.cancelRequested ? t("cancelling") : j.kind === "dupes" ? t("stSearching") : t("stRunning");
    return `${what} · ${num(j.files)} ${t("files")} · ${human(j.bytes)}`;
  }
  if (j.state === "error") return `${t("stError")}: ${j.error}`;
  if (j.kind === "dupes" && j.groups) {
    // Counted like the view: only the user's own files are reclaimable.
    tagGroups(j);
    const own = j.groups.filter((g) => !g.allProt);
    return `${own.length} ${t("groups")} · ${human(own.reduce((a, g) => a + g.wasted, 0))} ${t("reclaimable")}`;
  }
  if (j.view) {
    const tag = j.state === "cancelled" ? `${t("stCancelled")} · ` : "";
    return `${tag}${human(j.view.total_size)} · ${num(j.view.total_files)} ${t("files")} · ${fmtMs(j.elapsedMs || 0)}`;
  }
  return "";
}

// Estimated progress of a drive-root scan: bytes found vs the drive's used space.
function estimate(j) {
  const d = drives.find((x) => x.path.toLowerCase() === j.root.toLowerCase() || x.path === `${j.root}\\`);
  if (!d || j.kind !== "scan") return null;
  const used = d.total - d.free;
  return used > 0 ? Math.min(0.99, j.bytes / used) : null;
}

// Sidebar rows are created once per job and then only updated in place:
// progress repaints them several times a second, and replacing the element
// under the pointer between mouse-down and mouse-up swallows the click.
const jobRows = new Map(); // job id -> { row, icon, name, close, sub, meter }

function jobRow(j) {
  let r = jobRows.get(j.id);
  if (r) return r;
  const id = j.id;
  r = {
    icon: el("span", { class: "jicon" }),
    name: el("span", { class: "jname" }),
    close: el("button", {
      type: "button", class: "jclose",
      onclick: (e) => {
        e.stopPropagation();
        const job = byId(id);
        if (job) job.state === "running" ? cancelJob(job) : closeJob(job);
      },
    }),
    sub: el("span", { class: "jsub" }),
    meter: el("span", { class: "meter" }, el("span", { class: "used" })),
  };
  r.row = el("div", {
    role: "button", tabindex: "0",
    onclick: () => selectJob(id),
    onkeydown: (e) => { if (e.key === "Enter") selectJob(id); },
  }, r.icon, r.name, r.close, r.sub, r.meter);
  jobRows.set(id, r);
  return r;
}

// Set a property only when it changes, so unchanged rows cost nothing.
function put(node, key, value) {
  if (node[key] !== value) node[key] = value;
}

function renderJobs() {
  const box = $("jobs");
  for (const id of jobRows.keys()) if (!byId(id)) jobRows.delete(id);
  jobs.forEach((j, i) => {
    const r = jobRow(j);
    const busy = j.state === "running";
    put(r.row, "className", `job ${j.state}${j.id === activeId ? " active" : ""}`);
    put(r.row, "title", j.root);
    put(r.icon, "textContent", jobIcon(j));
    put(r.name, "textContent", `${j.kind === "dupes" ? `${t("dupesIn")} ` : ""}${jobName(j)}`);
    put(r.close, "textContent", busy ? "⏹" : "✕");
    put(r.close, "title", busy ? t("cancel") : t("close"));
    put(r.sub, "textContent", jobSub(j));
    const est = busy ? estimate(j) : null;
    put(r.meter.style, "display", busy ? "" : "none");
    put(r.meter, "className", est == null ? "meter indeterminate" : "meter");
    put(r.meter.firstChild.style, "width", est == null ? "" : `${est * 100}%`);
    if (box.children[i] !== r.row) box.insertBefore(r.row, box.children[i] || null);
  });
  while (box.children.length > jobs.length) box.lastChild.remove();
  $("btn-clear-done").disabled = !jobs.some((j) => ["done", "cancelled", "error"].includes(j.state));
}

function renderMain() {
  const main = $("main");
  hideTip();
  if (mapObserver) { mapObserver.disconnect(); mapObserver = null; }
  const j = active();
  main.classList.toggle("busy", Boolean(j?.loading));
  if (j?.loading && j.view) return; // keep the current view until the next one is ready
  if (!j) return main.replaceChildren(welcomeView());
  if (j.state === "queued") return main.replaceChildren(queuedView(j));
  if (j.state === "running") return main.replaceChildren(progressView(j));
  if (j.state === "error") return main.replaceChildren(errorView(j));
  if (j.kind === "dupes") return main.replaceChildren(dupesView(j));
  main.replaceChildren(scanView(j));
  // The map needs its laid-out size, so it is drawn once in the DOM.
  const map = main.querySelector(".treemap");
  if (map) {
    drawTreemap(map, j);
    let last = `${map.clientWidth}x${map.clientHeight}`;
    mapObserver = new ResizeObserver(() => {
      const now = `${map.clientWidth}x${map.clientHeight}`;
      if (now !== last) { last = now; drawTreemap(map, j); }
    });
    mapObserver.observe(map);
  }
}
let mapObserver = null;

function welcomeView() {
  return el("div", { class: "center" }, el("div", { class: "welcome" },
    el("div", { class: "hero" },
      el("div", { class: "big", text: "👻" }),
      el("h1", { text: t("welcomeTitle") }),
      el("p", { text: t("welcomeText") })),
    drives.length ? el("div", { class: "drive-grid" }, drives.map(driveButton)) : null));
}

function queuedView(j) {
  const pos = jobs.filter((x) => x.state === "queued").indexOf(j) + 1;
  return el("div", { class: "center" }, el("div", { class: "card progress-card" },
    el("h2", { text: t("stQueued") }),
    el("div", { class: "path", text: j.root }),
    el("div", { class: "big", text: `${t("queuedPos")} ${pos}` }),
    el("div", { class: "muted", text: `${t("queuedWhy")} ${settings.parallel}` }),
    el("div", {}, el("button", { type: "button", onclick: () => closeJob(j) }, t("removeFromQueue")))));
}

function progressView(j) {
  const secs = (performance.now() - j.startedAt) / 1000;
  const est = estimate(j);
  return el("div", { class: "center" }, el("div", { class: "card progress-card" },
    el("h2", { text: j.kind === "dupes" ? t("stSearching") : t("stRunning") }),
    el("div", { class: "path", text: j.root }),
    el("div", { class: "big", id: "pv-bytes", text: human(j.bytes) }),
    el("div", { class: "meter big" + (est == null ? " indeterminate" : ""), id: "pv-meter" },
      el("span", { class: "used", style: est == null ? {} : { width: `${est * 100}%` } })),
    el("div", { class: "muted", id: "pv-est", text: est == null ? t("noEstimate") : `≈ ${pct(est, 1)} ${t("estimated")}` }),
    el("div", { class: "progress-grid" },
      kpi(t("kFiles"), num(j.files), null, { id: "pv-files" }),
      kpi(t("elapsed"), fmtMs(secs * 1000), null, { id: "pv-time" }),
      kpi(t("rate"), secs > 0.5 ? num(Math.round(j.files / secs)) : "–", null, { id: "pv-rate" })),
    el("div", {}, el("button", { type: "button", class: "danger", disabled: j.cancelRequested, onclick: () => cancelJob(j) },
      j.cancelRequested ? t("cancelling") : t("cancel")))));
}

// Cheap in-place update of the progress card (no rebuild per event).
function updateProgressView(j) {
  const secs = (performance.now() - j.startedAt) / 1000;
  const set = (id, v) => { const n = $(id); if (n) n.querySelector?.(".v") ? (n.querySelector(".v").textContent = v) : (n.textContent = v); };
  set("pv-bytes", human(j.bytes));
  set("pv-files", num(j.files));
  set("pv-time", fmtMs(secs * 1000));
  set("pv-rate", secs > 0.5 ? num(Math.round(j.files / secs)) : "–");
  const est = estimate(j);
  if (est != null) {
    const bar = $("pv-meter")?.firstChild;
    if (bar) bar.style.width = `${est * 100}%`;
    set("pv-est", `≈ ${pct(est, 1)} ${t("estimated")}`);
  }
}

function errorView(j) {
  return el("div", { class: "center" }, el("div", { class: "card progress-card" },
    el("h2", { text: t("stError") }),
    el("div", { class: "path", text: j.root }),
    el("div", { class: "error", text: `⚠ ${j.error}` }),
    el("div", { style: { display: "flex", gap: "10px" } },
      el("button", { type: "button", class: "primary", onclick: () => rescan(j) }, t("retry")),
      el("button", { type: "button", onclick: () => closeJob(j) }, t("close")))));
}

function kpi(label, value, sub, attrs = {}, cls = "", extra = null) {
  return el("div", { class: `kpi ${cls}`, ...attrs },
    el("div", { class: "l", text: label }),
    el("div", { class: `v${cls.includes("name") ? " name" : ""}`, text: value, title: value }),
    sub ? el("div", { class: "s", text: sub, title: sub }) : null,
    extra);
}

// ------------------------------ scan view ------------------------------
function driveOf(path) {
  return drives
    .filter((d) => isUnder(path, d.path))
    .sort((a, b) => b.path.length - a.path.length)[0] || null;
}

function scanView(j) {
  const v = j.view;
  const diffMap = j.diff ? changesByPath(j.diff) : null;
  const view = el("div", { class: "view" });

  // Header: up, breadcrumbs (clickable inside this scan's tree), actions.
  const parent = parentPath(j.path);
  const head = el("div", { class: "viewhead" },
    el("button", { type: "button", class: "small", title: t("backHint"), disabled: !j.back.length, onclick: () => goBack(j) }, `← ${t("back")}`),
    el("button", { type: "button", class: "small", title: t("upHint"), disabled: !parent || !isUnder(parent, j.root), onclick: () => goUp(j) }, `↑ ${t("up")}`),
    crumbBar(j),
    el("span", { class: "grow" }),
    el("button", { type: "button", class: "small", onclick: () => openCompare(j) }, `⇄ ${t("compare")}`),
    el("button", { type: "button", class: "small", onclick: () => enqueue("dupes", j.path) }, `👯 ${t("dupesHere")}`),
    el("button", { type: "button", class: "small", onclick: () => rescan(j) }, `↻ ${t("rescan")}`));
  view.append(el("div", { class: "full", style: { display: "flex", flexDirection: "column", gap: "12px" } },
    head, v.cancelled ? el("div", { class: "notice", text: `⚠ ${t("cancelledNotice")}` }) : null));

  view.append(kpiRow(j, v));

  // Map (left) and drive + file types (right).
  view.append(el("div", { class: "card", style: { gridColumn: "1" } },
    el("div", { class: "card-head" }, el("h2", { text: t("map") }), el("span", { class: "sub", text: t("mapSub") })),
    el("div", { class: "treemap", oncontextmenu: (e) => { e.preventDefault(); goUp(j); } })));
  view.append(el("div", { style: { display: "flex", flexDirection: "column", gap: "16px", minHeight: "0", gridColumn: "2" } },
    diskCard(v), typesCard(j, v)));

  view.append(detailsCard(j, v, diffMap));
  return view;
}

function crumbBar(j) {
  const nav = el("nav", { class: "crumbs", "aria-label": "Path" });
  const parts = crumbs(j.path);
  parts.forEach((c, i) => {
    const last = i === parts.length - 1;
    const inTree = isUnder(c.target, j.root);
    nav.append(el("button", {
      type: "button", class: `crumb${last ? " cur" : ""}`, disabled: last || !inTree,
      onclick: () => navigate(j, c.target),
    }, c.label));
    if (!last) nav.append(el("span", { class: "crumb sep", text: "›" }));
  });
  return nav;
}

function kpiRow(j, v) {
  const row = el("div", { class: "kpis full" });
  const d = driveOf(v.root);
  const used = v.disk_total - v.disk_free;
  row.append(kpi(t("kSize"), human(v.total_size),
    used > 0 ? `${pct(v.total_size, used)} ${t("kSizeOfUsed")} ${d ? d.path : t("kSizeOfDisk")}` : null));
  row.append(kpi(t("kFiles"), num(v.total_files),
    v.total_files ? `${t("kAvg")} ${human(Math.round(v.total_size / v.total_files))} ${t("kPerFile")}` : null));
  row.append(kpi(t("kFolders"), num(v.total_dirs), `${num(v.root_files_count)} ${t("kDirect")}`));
  const bigDir = v.children[0];
  if (bigDir) {
    row.append(kpi(t("kBiggestFolder"), baseName(bigDir.path), `${human(bigDir.size)} · ${pct(bigDir.size, v.total_size)} · ${num(bigDir.file_count)} ${t("files")}`,
      { role: "button", tabindex: "0", style: { cursor: "pointer" }, onclick: () => navigate(j, bigDir.path) }, "name"));
  }
  const bigFile = v.top_files[0];
  if (bigFile) row.append(kpi(t("kBiggestFile"), baseName(bigFile.path), `${human(bigFile.size)} · ${pct(bigFile.size, v.total_size)}`, { title: bigFile.path }, "name"));
  if (v.disk_total > 0) {
    row.append(kpi(t("kFree"), human(v.disk_free), `${t("of")} ${human(v.disk_total)} · ${pct(used, v.disk_total)} ${t("kUsedPct")}`, {}, "",
      el("span", { class: "meter" }, el("span", { class: used / v.disk_total > 0.9 ? "hot" : "used", style: { width: `${(100 * used) / v.disk_total}%` } }))));
  }
  const secs = (j.elapsedMs || 0) / 1000;
  row.append(kpi(t("kScan"), fmtMs(j.elapsedMs || 0),
    `${secs > 0 ? `${num(Math.round(v.total_files / secs))} ${t("rate")} · ` : ""}${v.skipped ? `${num(v.skipped)} ${t("kSkipped")}` : t("kNoSkip")}`,
    {}, v.skipped ? "" : ""));
  if (j.diff) {
    const tint = deltaTint(j.diff.delta, Math.abs(j.diff.delta));
    row.append(kpi(`${t("kSince")} ${fmtDate(j.diff.old_taken_at)}`, humanDelta(j.diff.delta),
      `${human(j.diff.old_size)} → ${human(j.diff.new_size)}`, {}, tint.cls));
  } else if (j.path === j.root && j.auto) {
    row.append(sinceLastCard(j));
  }
  return row;
}

// "Since last scan": filled in by autoCompare() right after a scan; a click
// opens the full comparison.
function sinceLastCard(j) {
  const open = { role: "button", tabindex: "0", style: { cursor: "pointer" }, onclick: () => openCompare(j) };
  const a = j.auto;
  if (a.state === "loading") return kpi(t("sinceLast"), "…", t("comparing"), open);
  if (a.state === "none") return kpi(t("sinceLast"), "—", t("firstScanShort"), open);
  if (a.state !== "done") return kpi(t("sinceLast"), "?", a.error || "", open);
  const d = a.diff;
  const files = d.files_compared
    ? ` · ${num(d.new_files_count)} ${t("newShort")}, ${num(d.removed_files_count)} ${t("goneShort")}, ${num(d.changed_files_count)} ${t("changedShort")}`
    : "";
  return kpi(`${t("sinceLast")} (${fmtDate(d.old_taken_at)})`, humanDelta(d.delta), `${t("clickDetails")}${files}`, open,
    deltaTint(d.delta, Math.abs(d.delta)).cls);
}

// Compare a finished scan with the newest earlier snapshot of its root.
async function autoCompare(j) {
  j.auto = { state: "loading" };
  renderFor(j);
  try {
    j.history = (await invoke("list_snapshots", { job: j.id })).snapshots || [];
    const older = j.history.filter((s) => s.taken_at < j.scanUnix);
    if (!older.length) {
      j.auto = { state: "none" };
    } else {
      const diff = await invoke("diff_since", { job: j.id, path: j.root, takenAt: older[0].taken_at, top: j.top });
      j.auto = { state: "done", diff };
    }
  } catch (e) {
    j.auto = { state: "error", error: errText(e) };
  }
  if (byId(j.id)) renderFor(j);
}

// The Compare button / card: show the changes tab with a comparison of the
// folder on screen against the newest earlier snapshot.
async function openCompare(j) {
  j.tab = "changes";
  if (j.path === j.root && j.auto?.state === "done") {
    j.diff = j.auto.diff;
    j.sinceAt = j.diff.old_taken_at;
    return renderMain();
  }
  renderMain();
  if (!j.history) {
    try { j.history = (await invoke("list_snapshots", { job: j.id })).snapshots || []; } catch (_) { j.history = []; }
  }
  const older = j.history.filter((s) => s.taken_at < j.scanUnix);
  if (older.length) await showChanges(j, j.sinceAt ?? older[0].taken_at);
  else renderMain();
}

function diskCard(v) {
  if (!(v.disk_total > 0)) return null;
  const used = v.disk_total - v.disk_free;
  const other = Math.max(0, used - v.total_size);
  const d = driveOf(v.root);
  const seg = (cls, size, label) => {
    const s = el("span", { class: cls, style: { width: `${(100 * size) / v.disk_total}%` } });
    tip(s, () => [el("b", { text: label }), tipRow(t("colSize"), human(size)), tipRow(t("colPct"), pct(size, v.disk_total))]);
    return s;
  };
  return el("div", { class: "card" },
    el("div", { class: "card-head" },
      el("h2", { text: t("disk") }),
      el("span", { class: "sub", text: d ? `${d.path}${d.label ? ` · ${d.label}` : ""}${d.fs ? ` · ${d.fs}` : ""}` : human(v.disk_total) })),
    el("div", { class: "meter big" }, seg("used", v.total_size, t("diskThis")), seg("other", other, t("diskOther"))),
    el("div", { class: "legend" },
      el("span", {}, el("i", { class: "k-used" }), `${t("diskThis")} ${human(v.total_size)} (${pct(v.total_size, v.disk_total)})`),
      el("span", {}, el("i", { class: "k-other" }), `${t("diskOther")} ${human(other)}`),
      el("span", {}, el("i", { class: "k-free" }), `${t("diskFree")} ${human(v.disk_free)}`)));
}

const catIcon = { video: "🎬", image: "🖼", audio: "🎵", archive: "🗜", diskimage: "💿", document: "📄", code: "🧩", program: "⚙", gamedata: "🎮", database: "🗄", other: "📦" };

function typesCard(j, v) {
  const cats = categorize(v.stats.extensions);
  const max = cats[0]?.size || 1;
  const list = el("div", { class: "hbars" }, cats.map((c) => {
    const row = el("button", {
      type: "button", class: "hbar",
      onclick: () => { j.tab = "types"; j.catFilter = c.id; renderMain(); },
    },
    el("span", { class: "hl", text: `${catIcon[c.id]} ${t(`cat_${c.id}`)}` }),
    el("span", { class: "htrack" }, el("span", { class: "hfill", style: { display: "block", width: `${(100 * c.size) / max}%` } })),
    el("span", { class: "hv" }, el("b", { text: human(c.size) }), ` · ${pct(c.size, v.total_size)}`));
    tip(row, () => [
      el("b", { text: `${catIcon[c.id]} ${t(`cat_${c.id}`)}` }),
      tipRow(t("colSize"), `${human(c.size)} (${pct(c.size, v.total_size)})`),
      tipRow(t("colFiles"), num(c.count)),
      el("div", { class: "tp", style: { marginTop: "6px" }, text: c.exts.slice(0, 6).map((e) => `${e.ext ? `.${e.ext}` : t("noExt")} ${human(e.size)}`).join(" · ") }),
    ]);
    return row;
  }));
  return el("div", { class: "card", style: { flex: "1" } },
    el("div", { class: "card-head" }, el("h2", { text: t("types") }), el("span", { class: "sub", text: t("typesSub") })),
    list);
}

// Two-level squarified map: each big folder frames its own biggest
// sub-folders; small ones are a single tile. Pseudo tiles ("files directly
// here", "smaller folders") make the areas add up to the folder's total.
function drawTreemap(container, j) {
  const v = j.view;
  container.replaceChildren();
  const W = container.clientWidth, H = container.clientHeight;
  if (!W || !H || !v.total_size) return;
  const diffMap = j.diff ? changesByPath(j.diff) : null;
  const kids = v.children.map((c, i) => ({ ...c, nested: v.nested?.[i] || [] }));
  const items = mapItems(v.total_size, kids, v.root_files_size);
  const rects = squarify(items, 0, 0, W, H);
  const maxL1 = Math.max(1, ...items.filter((i) => i.path).map((i) => i.size));
  const maxL2 = Math.max(1, ...kids.flatMap((k) => k.nested.map((n) => n.size)));
  const GAP = 2, HEAD = 24;

  const place = (node, x, y, w, h) => {
    node.style.left = `${x}px`;
    node.style.top = `${y}px`;
    node.style.width = `${Math.max(0, w)}px`;
    node.style.height = `${Math.max(0, h)}px`;
    container.append(node);
  };
  const label = (name, size, share, w, h, stacked) => {
    if (w < 46 || h < 18) return [];
    const out = [el("span", { class: "tn", text: name })];
    if (stacked ? h > 34 : w > 150) out.push(el("span", { class: "ts", text: `${human(size)}${share ? ` · ${share}` : ""}` }));
    return out;
  };
  const decorate = (node, path) => {
    const c = diffMap?.map.get(String(path));
    if (!c) return;
    const { cls, strength } = c.added ? { cls: "added", strength: 1 } : deltaTint(c.delta, diffMap.maxAbs);
    node.classList.add(cls);
    node.style.setProperty("--tint", String(0.25 + 0.75 * strength));
  };
  const folderTip = (item, parentSize) => () => {
    const out = [el("b", { text: item.path ? baseName(item.path) : item.label }), item.path ? el("div", { class: "tp", text: item.path }) : null,
      tipRow(t("colSize"), human(item.size)),
      tipRow(`${t("colPct")} ${t("shareOfView")}`, pct(item.size, v.total_size))];
    if (item.path && parentSize && parentSize !== v.total_size) out.push(tipRow(`${t("colPct")} ${baseName(parentPath(item.path) || "")}`, pct(item.size, parentSize)));
    if (item.file_count != null) out.push(tipRow(t("colFiles"), num(item.file_count)));
    const c = diffMap?.map.get(String(item.path));
    if (c) out.push(tipRow(t("colDelta"), c.added ? t("newBadge") : humanDelta(c.delta)));
    return out;
  };

  for (const rc of rects) {
    const it = rc.item;
    const x = rc.x + GAP / 2, y = rc.y + GAP / 2, w = rc.w - GAP, h = rc.h - GAP;
    if (!it.path) {
      it.label = it.rest === "files" ? `${num(v.root_files_count)} ${t("filesHere")}` : t("otherFolders");
      const node = el("div", { class: "tile l1 leaf rest" });
      node.append(el("div", { class: "thead" }, label(it.label, it.size, pct(it.size, v.total_size), w, h, true)));
      tip(node, folderTip(it));
      place(node, x, y, w, h);
      continue;
    }
    const framed = w > 150 && h > 90 && it.nested.length > 0;
    const node = el("button", { type: "button", class: `tile l1${framed ? "" : " leaf"}`, onclick: () => navigate(j, it.path) });
    node.style.setProperty("--s", String(Math.sqrt(it.size / maxL1)));
    node.append(el("div", { class: "thead" }, label(baseName(it.path), it.size, pct(it.size, v.total_size), w, h, !framed)));
    decorate(node, it.path);
    tip(node, folderTip(it));
    place(node, x, y, w, h);
    if (!framed) continue;

    const inner = mapItems(it.size, it.nested, 0);
    const ix = x + 3, iy = y + HEAD, iw = w - 6, ih = h - HEAD - 3;
    for (const r2 of squarify(inner, ix, iy, iw, ih)) {
      const n2 = r2.item;
      const x2 = r2.x + 1, y2 = r2.y + 1, w2 = r2.w - 2, h2 = r2.h - 2;
      if (!n2.path) {
        n2.label = t("filesSmaller");
        const rest = el("div", { class: "tile l2 rest" }, label(n2.label, n2.size, null, w2, h2, true));
        tip(rest, folderTip(n2, it.size));
        place(rest, x2, y2, w2, h2);
        continue;
      }
      const t2 = el("button", { type: "button", class: "tile l2", onclick: (e) => { e.stopPropagation(); navigate(j, n2.path); } },
        label(baseName(n2.path), n2.size, null, w2, h2, true));
      t2.style.setProperty("--s", String(Math.sqrt(n2.size / maxL2)));
      tip(t2, folderTip(n2, it.size));
      place(t2, x2, y2, w2, h2);
    }
  }
}

// Details: folders / files / extensions / size ranges / changes.
function detailsCard(j, v, diffMap) {
  const tabs = [
    ["folders", t("tabFolders"), v.children.length],
    ["files", t("tabFiles"), v.top_files.length],
    ["types", t("tabTypes"), v.stats.extensions.length],
    ["sizes", t("tabSizes"), null],
    ["changes", t("tabChanges"), null],
  ];
  const body = el("div", { class: "tabbody" });
  const tools = el("div", { class: "tabtools" });
  const card = el("div", { class: "card full" },
    el("div", { class: "tabs", role: "tablist" }, tabs.map(([id, label, count]) =>
      el("button", { type: "button", role: "tab", class: `tab${j.tab === id ? " on" : ""}`, onclick: () => { j.tab = id; renderMain(); } },
        label, count != null ? el("span", { class: "count", text: num(count) }) : null))),
    tools, body);
  if (j.tab === "folders") foldersTab(j, v, body, diffMap);
  else if (j.tab === "files") filesTab(j, v, body, tools);
  else if (j.tab === "types") typesTab(j, v, body, tools);
  else if (j.tab === "sizes") sizesTab(v, body);
  else changesTab(j, v, body, tools);
  if (!tools.childNodes.length) tools.remove();
  return card;
}

function shareBar(part, whole) {
  return el("div", { class: "share" }, el("span", { style: { width: `${whole ? (100 * part) / whole : 0}%` } }));
}

function delButton(path) {
  return el("button", { type: "button", class: "del", title: t("del"), onclick: (e) => { e.stopPropagation(); deletePath(path); } }, "🗑");
}

function foldersTab(j, v, body, diffMap) {
  const table = el("table", { class: "table" },
    el("thead", {}, el("tr", {},
      el("th", { text: t("colName") }), el("th", { text: t("colShare") }), el("th", { class: "num", text: t("colSize") }),
      el("th", { class: "num", text: t("colPct") }), el("th", { class: "num", text: t("colFiles") }),
      diffMap ? el("th", { class: "num", text: t("colDelta") }) : null, el("th", {}))));
  const tb = el("tbody");
  for (const d of v.children) {
    const c = diffMap?.map.get(String(d.path));
    let cls = "click", delta = "";
    if (c?.added) { cls += " added"; delta = t("newBadge"); } else if (c) { cls += ` ${deltaTint(c.delta, diffMap.maxAbs).cls}`; delta = humanDelta(c.delta); }
    tb.append(el("tr", { class: cls, onclick: () => navigate(j, d.path) },
      el("td", { class: "namecell" }, el("div", { class: "nm", text: `📁 ${baseName(d.path)}` }), el("div", { class: "pth", text: d.path })),
      el("td", { class: "barcell" }, shareBar(d.size, v.total_size)),
      el("td", { class: "num", text: human(d.size) }),
      el("td", { class: "num", text: pct(d.size, v.total_size) }),
      el("td", { class: "num", text: num(d.file_count) }),
      diffMap ? el("td", { class: "num" }, el("span", { class: "delta", text: delta })) : null,
      el("td", { class: "acts" }, delButton(d.path))));
  }
  if (v.root_files_count > 0) {
    tb.append(el("tr", {},
      el("td", { class: "namecell" }, el("div", { class: "nm muted", text: `📄 ${num(v.root_files_count)} ${t("filesHere")}` })),
      el("td", { class: "barcell" }, shareBar(v.root_files_size, v.total_size)),
      el("td", { class: "num", text: human(v.root_files_size) }),
      el("td", { class: "num", text: pct(v.root_files_size, v.total_size) }),
      el("td", { class: "num", text: num(v.root_files_count) }),
      diffMap ? el("td") : null, el("td")));
  }
  const listed = v.children.reduce((a, d) => a + d.size, 0) + v.root_files_size;
  if (v.total_size > listed) {
    tb.append(el("tr", {},
      el("td", { class: "namecell" }, el("div", { class: "nm muted", text: `… ${t("otherFolders")}` })),
      el("td", { class: "barcell" }, shareBar(v.total_size - listed, v.total_size)),
      el("td", { class: "num", text: human(v.total_size - listed) }),
      el("td", { class: "num", text: pct(v.total_size - listed, v.total_size) }),
      el("td"), diffMap ? el("td") : null, el("td")));
  }
  table.append(tb);
  body.append(table);
}

function filesTab(j, v, body, tools) {
  const input = el("input", { type: "text", placeholder: t("filterFiles"), value: j.fileFilter || "", style: { width: "360px" } });
  tools.append(input);
  const box = el("div", { class: "vlist" });
  body.append(box);
  const draw = () => {
    const f = (j.fileFilter || "").toLowerCase();
    const items = f ? v.top_files.filter((x) => String(x.path).toLowerCase().includes(f)) : v.top_files;
    const head = el("div", { class: "vrow vhead" },
      el("span", { text: t("colName") }), el("span", { text: t("colType") }),
      el("span", { class: "num", text: t("colSize") }), el("span", { class: "num", text: t("colPct") }), el("span"));
    virtualList(box, head, items, 44, (x) => {
      const ext = (String(x.path).match(/\.([^.\\/]+)$/) || [])[1] || "";
      return el("div", { class: "vrow" },
        el("div", { style: { minWidth: "0" } },
          el("div", { class: "nm", style: { fontWeight: "600", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }, text: baseName(x.path) }),
          el("div", { class: "pth", style: { fontSize: "12px", color: "var(--muted)", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }, text: parentPath(x.path) || "" })),
        el("span", {}, ext ? el("span", { class: "ext", text: `.${ext.toLowerCase()}` }) : null),
        el("span", { class: "num", text: human(x.size) }),
        el("span", { class: "num", text: pct(x.size, v.total_size) }),
        delButton(x.path));
    });
  };
  input.addEventListener("input", () => { j.fileFilter = input.value; draw(); });
  draw();
}

// Windowed list: only rows in view are built.
function virtualList(container, head, items, rowH, renderRow) {
  const inner = el("div", { class: "vlist-inner" });
  inner.style.height = `${items.length * rowH}px`;
  container.replaceChildren(head, inner);
  const draw = () => {
    const top = Math.max(0, container.scrollTop - head.offsetHeight);
    const vh = container.clientHeight || 400;
    const start = Math.max(0, Math.floor(top / rowH) - 6);
    const end = Math.min(items.length, Math.ceil((top + vh) / rowH) + 6);
    const frag = document.createDocumentFragment();
    for (let k = start; k < end; k++) {
      const row = renderRow(items[k], k);
      Object.assign(row.style, { position: "absolute", top: `${k * rowH}px`, left: "0", right: "0" });
      frag.append(row);
    }
    inner.replaceChildren(frag);
  };
  container.onscroll = draw;
  draw();
}

function typesTab(j, v, body, tools) {
  const cats = categorize(v.stats.extensions);
  tools.append(selectInput(j.catFilter || "", [["", t("allCategories")], ...cats.map((c) => [c.id, `${catIcon[c.id]} ${t(`cat_${c.id}`)}`])],
    (val) => { j.catFilter = val; renderMain(); }));
  const rows = v.stats.extensions.filter((e) => !j.catFilter || categoryOf(e.ext) === j.catFilter);
  const max = rows[0]?.size || 1;
  const shown = rows.slice(0, 500);
  const table = el("table", { class: "table" },
    el("thead", {}, el("tr", {},
      el("th", { text: t("colExt") }), el("th", { text: t("colCategory") }), el("th", { text: t("colShare") }),
      el("th", { class: "num", text: t("colSize") }), el("th", { class: "num", text: t("colPct") }),
      el("th", { class: "num", text: t("colCount") }), el("th", { class: "num", text: `Ø ${t("colSize")}` }))),
    el("tbody", {}, shown.map((e) => el("tr", {},
      el("td", {}, el("span", { class: "ext", text: e.ext ? `.${e.ext}` : t("noExt") })),
      el("td", { text: `${catIcon[categoryOf(e.ext)]} ${t(`cat_${categoryOf(e.ext)}`)}` }),
      el("td", { class: "barcell" }, shareBar(e.size, max)),
      el("td", { class: "num", text: human(e.size) }),
      el("td", { class: "num", text: pct(e.size, v.total_size) }),
      el("td", { class: "num", text: num(e.count) }),
      el("td", { class: "num", text: human(Math.round(e.size / Math.max(1, e.count))) })))));
  body.append(table);
  if (rows.length > shown.length) body.append(el("div", { class: "muted", style: { padding: "8px 10px" }, text: `… ${t("showing")} ${shown.length} / ${num(rows.length)}` }));
}

// Two single-measure column charts side by side (never one chart, two axes).
function sizesTab(v, body) {
  const b = v.stats.size_buckets;
  const chart = (title, values, fmt) => {
    const max = Math.max(1, ...values);
    return el("div", { class: "cols" },
      el("h4", { text: title }),
      el("div", { class: "plot" }, values.map((val, i) => {
        const col = el("div", { class: "col" },
          el("div", { class: "val", text: val ? fmt(val) : "" }),
          el("div", { class: "bar", style: { height: `${(100 * val) / max}%` } }));
        tip(col, () => [el("b", { text: bucketLabel(b, i) }),
          tipRow(t("colFiles"), `${num(b[i].count)} (${pct(b[i].count, v.total_files)})`),
          tipRow(t("colSize"), `${human(b[i].size)} (${pct(b[i].size, v.total_size)})`)]);
        return col;
      })),
      el("div", { class: "axis" }, b.map((_, i) => el("span", { text: bucketLabel(b, i) }))));
  };
  body.append(el("div", { style: { display: "flex", flexDirection: "column", gap: "12px", height: "100%" } },
    el("div", { class: "muted", text: t("sizesHint") }),
    el("div", { class: "cols-wrap", style: { flex: "1" } },
      chart(t("sizesCount"), b.map((x) => x.count), num),
      chart(t("sizesBytes"), b.map((x) => x.size), human))));
}

// ------------------------------ what grew ---------------------------------
// Every complete scan is stored as a snapshot of its root (every folder and
// file, compressed). Compare the folder on screen against an earlier one.
async function changesTab(j, v, body, tools) {
  if (v.cancelled) return body.append(el("div", { class: "muted", text: t("cancelledNotice") }));
  if (!j.history) {
    try {
      j.history = (await invoke("list_snapshots", { job: j.id })).snapshots || [];
    } catch (_) {
      j.history = [];
    }
    if (active() === j && j.tab === "changes") renderMain();
    return;
  }
  // Snapshots older than this scan; the newest one is usually the scan itself.
  const older = j.history.filter((s) => s.taken_at < j.scanUnix);
  if (older.length) {
    const chosen = older.some((s) => s.taken_at === j.sinceAt) ? j.sinceAt : older[0].taken_at;
    const sel = selectInput(String(chosen), older.map((s) => [String(s.taken_at), `${fmtDate(s.taken_at)}${s.files ? "" : ` (${t("foldersOnly")})`}`]),
      (val) => { j.sinceAt = parseInt(val, 10); });
    tools.append(el("label", { class: "inlabel" }, `${t("since")} `, sel),
      el("button", { type: "button", class: "small primary", onclick: () => showChanges(j, parseInt(sel.value, 10)) }, t("showChanges")));
  }
  if (j.diff) renderChanges(j.diff, body, j);
  else if (!older.length) body.append(el("div", { class: "muted", text: t("firstScan") }));
  snapshotList(j, body);
}

async function showChanges(j, takenAt) {
  j.sinceAt = takenAt;
  status(t("comparing"));
  try {
    j.diff = await invoke("diff_since", { job: j.id, path: j.path, takenAt, top: j.top });
    status("");
  } catch (e) {
    status(errText(e), true);
  }
  if (active() === j) renderMain();
}

// The stored snapshots of this scan's root: when, how big on disk, compare
// or delete.
function snapshotList(j, box) {
  if (!j.history.length) return;
  const total = j.history.reduce((a, s) => a + s.bytes, 0);
  box.append(el("h3", { text: `${t("snapshots")} — ${j.history.length} · ${human(total)} ${t("onDisk")}`, style: { margin: "18px 0 6px" } }));
  box.append(el("div", { class: "muted", text: t("snapshotsHint")(settings.keep_snapshots) }));
  box.append(el("table", { class: "table" }, el("tbody", {}, j.history.map((s) => {
    const current = s.taken_at >= j.scanUnix;
    return el("tr", {},
      el("td", { text: fmtDate(s.taken_at) }),
      el("td", { class: "muted", text: current ? t("thisScan") : s.files ? t("filesAndFolders") : t("foldersOnly") }),
      el("td", { class: "num", text: human(s.bytes) }),
      el("td", { class: "acts" },
        current ? null : el("button", { type: "button", class: "small", onclick: () => showChanges(j, s.taken_at) }, t("compare")),
        el("button", {
          type: "button", class: "del", title: t("del"),
          onclick: async () => {
            if (!(await ask(t("deleteSnapshot"), [{ text: fmtDate(s.taken_at) }], t("del"), true))) return;
            try { await invoke("delete_snapshot", { job: j.id, takenAt: s.taken_at }); } catch (e) { status(errText(e), true); }
            j.history = null;
            if (j.diff?.old_taken_at === s.taken_at) j.diff = null;
            if (active() === j) renderMain();
          },
        }, "🗑")));
  }))));
}

function renderChanges(d, box, j) {
  const tint = deltaTint(d.delta, Math.abs(d.delta));
  box.append(el("div", { class: "chead" },
    el("span", { class: `delta-big ${tint.cls}`, text: humanDelta(d.delta) }),
    el("span", { class: "muted", text: `${t("changes")} ${fmtDate(d.old_taken_at)}: ${human(d.old_size)} → ${human(d.new_size)}, ${d.files_delta >= 0 ? "+" : ""}${num(d.files_delta)} ${t("files")}` })));
  if (d.files_compared) {
    box.append(el("div", { class: "kpis", style: { margin: "10px 0" } },
      kpi(t("newFiles"), num(d.new_files_count), `+${human(d.new_files_bytes)}`, {}, d.new_files_count ? "grow" : ""),
      kpi(t("removedFiles"), num(d.removed_files_count), `-${human(d.removed_files_bytes)}`, {}, d.removed_files_count ? "shrink" : ""),
      kpi(t("changedFiles"), num(d.changed_files_count), humanDelta(d.changed_files_delta), {}, deltaTint(d.changed_files_delta, 1).cls)));
  } else {
    box.append(el("div", { class: "muted", text: t("oldSnapshotNoFiles") }));
  }
  const unchanged = !d.grown.length && !d.shrunk.length && !d.added.length && !d.removed.length &&
    !d.new_files_count && !d.removed_files_count && !d.changed_files_count;
  if (unchanged) {
    box.append(el("div", { class: "ok", text: t("unchanged") }));
    return;
  }
  const section = (title, items, cols, total) => {
    if (!items.length) return;
    const more = total > items.length ? ` — ${t("biggest")} ${num(items.length)} / ${num(total)}` : "";
    box.append(el("h3", { text: `${title}${more}`, style: { margin: "14px 0 6px" } }));
    box.append(el("table", { class: "table" }, el("tbody", {}, items.map(cols))));
  };
  const nameCells = (x) => el("td", { class: "namecell" }, el("div", { class: "nm", text: baseName(x.path) }), el("div", { class: "pth", text: x.path }));
  const deltaRow = (clickable) => (x) => el("tr", { class: clickable ? "click" : "", onclick: clickable ? () => navigate(j, x.path) : null },
    el("td", { class: "num", style: { width: "120px" } }, el("span", { class: `delta ${x.delta > 0 ? "grow" : "shrink"}`, text: humanDelta(x.delta) })),
    nameCells(x),
    el("td", { class: "num", text: `${human(x.old_size)} → ${human(x.new_size)}` }));
  const sizeRow = (clickable) => (x) => el("tr", { class: clickable ? "click" : "", onclick: clickable ? () => navigate(j, x.path) : null },
    el("td", { class: "num", style: { width: "120px" }, text: human(x.size) }),
    nameCells(x),
    el("td"));
  section(t("grew"), d.grown, deltaRow(true));
  section(t("shrank"), d.shrunk, deltaRow(true));
  section(t("newFolders"), d.added, sizeRow(true));
  section(t("removedFolders"), d.removed, sizeRow(false));
  section(t("newFiles"), d.new_files_list, deltaRow(false), d.new_files_count);
  section(t("changedFiles"), d.changed_files_list, deltaRow(false), d.changed_files_count);
  section(t("removedFiles"), d.removed_files_list, deltaRow(false), d.removed_files_count);
}

// -------------------------------- delete ----------------------------------
// Remove a file/folder — to the Recycle Bin by default (settings). Every open
// scan holding the path has it taken out of its tree by the backend, so the
// views just re-read their folder (instant) and show the freed space.
async function deletePath(path) {
  const trash = settings.trash;
  const ok = await ask(trash ? t("confirmTrash") : t("confirmDelete"),
    [{ text: path, cls: "path" }, trash ? null : { text: t("confirmDeleteWarn"), cls: "error" }].filter(Boolean),
    t("del"), !trash);
  if (!ok) return;
  status(t("deleting"));
  const id = nextId++;
  let msg;
  try {
    const t0 = performance.now();
    const r = await invoke("remove_path_cmd", { job: id, path, trash, apply: true });
    msg = `${t("del")}: ${num(r.files)} ${t("files")}, ${human(r.bytes)} ${t("removedMsg")} (${fmtMs(performance.now() - t0)})`;
    if (r.errors?.length) msg += ` — ${r.errors.length} ${t("errors")}`;
    status(msg, Boolean(r.errors?.length));
  } catch (e) {
    return status(errText(e), true);
  }
  for (const j of jobs) {
    if (j.kind !== "scan" || !j.view || !isUnder(path, j.root)) continue;
    // The backend took the path out of the tree; cached views are outdated.
    j.cache.clear();
    j.back = j.back.filter((p) => !isUnder(p, path));
    // Stay in the deleted folder's parent if the view was inside it.
    if (isUnder(j.path, path)) j.path = parentPath(path) || j.root;
    if (j === active()) navigate(j, j.path, { keepTab: true, record: false, force: true });
    else j.stale = true;
  }
  loadDrives();
}

// -------------------------------- duplicates --------------------------------
// A group is files with byte-identical content (BLAKE3 over the whole file,
// hard links collapsed) — whatever they are called. Copies inside installed
// programs and games are on purpose (every game ships its own DLLs, some ship
// sound banks twice) and deleting one breaks the program, so groups made only
// of such files are hidden unless asked for, and they are never removed while
// hidden.
function tagGroups(j) {
  if (j.tagged) return;
  for (const g of j.groups) {
    g.prot = g.files.map(isProtectedPath);
    g.allProt = g.prot.every(Boolean);
    g.anyProt = g.prot.some(Boolean);
    g.cat = categoryOf((String(g.files[0]).match(/\.([^.\\/]+)$/) || [])[1] || "");
  }
  j.tagged = true;
}

// Groups that pass the program/game switch, the category and the folder filter.
function baseGroups(j) {
  tagGroups(j);
  return j.groups.filter((g) => (j.dupShowProt || !g.allProt) && (!j.dupCat || g.cat === j.dupCat) &&
    (!j.dupSet || folderSets([g])[0].key === j.dupSet));
}

function dupesView(j) {
  const groups = j.groups || [];
  const wrap = el("div", { class: "dupview" });
  wrap.append(el("div", { class: "viewhead" },
    el("div", { class: "crumbs" }, el("span", { class: "crumb cur", text: `👯 ${t("dupesIn")} ${j.root}` })),
    el("span", { class: "grow" }),
    el("button", { type: "button", class: "small", onclick: () => rescan(j) }, `↻ ${t("rescan")}`)));
  if (j.state === "cancelled") wrap.append(el("div", { class: "notice", text: `⚠ ${t("stCancelled")}` }));
  if (!groups.length) {
    wrap.append(el("div", { class: "card" }, el("div", { class: "ok", style: { fontSize: "18px" }, text: t("noDupes") })));
    return wrap;
  }
  tagGroups(j);
  const own = groups.filter((g) => !g.allProt);
  const prot = groups.filter((g) => g.allProt);
  const sum = (list) => list.reduce((a, g) => a + g.wasted, 0);
  wrap.append(el("div", { class: "kpis" },
    kpi(t("ownReclaim"), human(sum(own)), `${num(own.length)} ${t("groups")} · ${num(own.reduce((a, g) => a + g.files.length, 0))} ${t("files")}`),
    kpi(t("protHidden"), human(sum(prot)), `${num(prot.length)} ${t("groups")} — ${t("protHiddenSub")}`,
      { role: "button", tabindex: "0", style: { cursor: "pointer" }, onclick: () => { j.dupShowProt = !j.dupShowProt; renderMain(); } }),
    kpi(t("kScan"), fmtMs(j.elapsedMs || 0), null)));
  wrap.append(el("div", { class: "muted", text: t("dupHint") }));

  const filter = el("input", { type: "text", value: j.dupFilter || "", placeholder: t("filterFiles"), oninput: (e) => { j.dupFilter = e.target.value; fill(); } });
  const showProt = el("input", { type: "checkbox", onchange: (e) => { j.dupShowProt = e.target.checked; renderMain(); } });
  showProt.checked = Boolean(j.dupShowProt);
  const cats = [...new Set(groups.map((g) => g.cat))];
  const sort = selectInput(j.dupSort || "wasted", [["wasted", t("sortWasted")], ["size", t("sortSize")], ["count", t("sortCount")]],
    (val) => { j.dupSort = val; fill(); });
  const action = selectInput(j.dupAction || "trash", [["trash", t("actTrash")], ["delete", t("actDelete")], ["hardlink", t("actHardlink")]],
    (val) => { j.dupAction = val; });
  wrap.append(el("div", { class: "dupbar" },
    el("label", { class: "inlabel" }, showProt, ` ${t("showProt")}`),
    selectInput(j.dupCat || "", [["", t("allCategories")], ...cats.map((c) => [c, `${catIcon[c]} ${t(`cat_${c}`)}`])],
      (val) => { j.dupCat = val; renderMain(); }),
    filter,
    el("label", { class: "inlabel" }, `${t("sort")} `, sort),
    el("span", { class: "grow", style: { flex: "1" } }),
    action,
    el("button", { type: "button", onclick: () => reclaimShown(j, true) }, t("dryRun")),
    el("button", { type: "button", class: "danger", onclick: () => reclaimShown(j, false) }, t("reclaimShown"))));

  if (j.dupSet) {
    wrap.append(el("div", { class: "chips" },
      el("span", { class: "chip" }, `📁 ${t("folderFilter")}: ${j.dupSetDirs.map((d) => baseName(d)).join(" ⇄ ")}`,
        el("button", { type: "button", class: "x", title: t("close"), onclick: () => { j.dupSet = null; renderMain(); } }, "✕"))));
  }

  const tab = j.dupTab || "groups";
  wrap.append(el("div", { class: "tabs" },
    [["groups", t("tabGroups")], ["sets", t("tabFolderSets")]].map(([id, label]) =>
      el("button", { type: "button", class: `tab${tab === id ? " on" : ""}`, onclick: () => { j.dupTab = id; renderMain(); } }, label))));
  const list = el("div", { class: "dupes" });
  wrap.append(list);

  function fill() {
    const shown = computeShown(j);
    const frag = document.createDocumentFragment();
    if (tab === "sets") {
      const sets = folderSets(shown);
      const table = el("table", { class: "table" },
        el("thead", {}, el("tr", {}, el("th", { text: t("colFolders") }), el("th", { class: "num", text: t("groups") }),
          el("th", { class: "num", text: t("colFiles") }), el("th", { class: "num", text: t("reclaimable") }))),
        el("tbody", {}, sets.slice(0, 500).map((s) => el("tr", {
          class: "click",
          onclick: () => { j.dupSet = s.key; j.dupSetDirs = s.dirs; j.dupTab = "groups"; renderMain(); },
        },
        el("td", { class: "namecell" }, s.dirs.length === 1
          ? [el("div", { class: "nm", text: `${baseName(s.dirs[0])} — ${t("sameFolder")}` }), el("div", { class: "pth", text: s.dirs[0] })]
          : s.dirs.map((d) => el("div", { class: "pth", text: `📁 ${d}` }))),
        el("td", { class: "num", text: num(s.groups) }),
        el("td", { class: "num", text: num(s.files) }),
        el("td", { class: "num", text: human(s.wasted) })))));
      frag.append(table);
    } else {
      for (const g of shown.slice(0, 1000)) {
        const box = el("div", { class: "dup" });
        box.append(el("div", { class: "dhead" },
          el("span", { class: "badge", text: `${g.files.length}×` }),
          ` ${human(g.size)} `,
          el("span", { class: "chip", title: `BLAKE3 ${g.hash}`, text: `✓ ${t("identical")}` }),
          el("span", { class: "muted", text: `${catIcon[g.cat]} ${t(`cat_${g.cat}`)}` }),
          g.anyProt ? el("span", { class: "chip bad", title: t("protWhy"), text: `🔒 ${t("protBadge")}` }) : null,
          el("span", { class: "waste", text: `${human(g.wasted)} ${t("reclaimable")}` })));
        const kept = keptPath(j, g);
        g.files.forEach((f, i) => {
          const radio = el("input", { type: "radio", name: `keep-${g.hash}`, title: t("keep"), onchange: () => { j.keep.set(g.hash, f); fill(); } });
          if (f === kept) radio.checked = true;
          box.append(el("label", { class: `dupfile${f === kept ? " kept" : ""}` }, radio,
            g.prot[i] ? el("span", { title: t("protWhy"), text: "🔒" }) : null,
            el("span", { class: "path", text: f })));
        });
        frag.append(box);
      }
      if (shown.length > 1000) frag.append(el("div", { class: "muted", text: `… ${t("showing")} 1000 / ${num(shown.length)} ${t("groups")}` }));
      if (!shown.length) frag.append(el("div", { class: "ok", text: j.dupShowProt ? t("noDupes") : t("noOwnDupes") }));
    }
    list.replaceChildren(frag);
  }
  fill();
  return wrap;
}

function computeShown(j) {
  const filter = (j.dupFilter || "").toLowerCase();
  const sort = j.dupSort || "wasted";
  const groups = baseGroups(j).filter((g) => !filter || g.files.some((f) => f.toLowerCase().includes(filter)));
  groups.sort((a, b) => (sort === "size" ? b.size - a.size : sort === "count" ? b.files.length - a.files.length : b.wasted - a.wasted));
  return groups;
}

// The copy kept by default: a program's own file if the group has one (so
// the user's stray copy is what goes), else the first.
function keptPath(j, g) {
  const chosen = j.keep.get(g.hash);
  if (chosen) return chosen;
  const i = g.prot ? g.prot.indexOf(true) : -1;
  return g.files[i >= 0 ? i : 0];
}

async function reclaimShown(j, dryRun) {
  const action = j.dupAction || "trash";
  // While program and game files are hidden, none of them is ever removed.
  const reclaimJobs = computeShown(j)
    .map((g) => {
      const keep = keptPath(j, g);
      const remove = g.files.filter((f, i) => f !== keep && (j.dupShowProt || !g.prot[i]));
      return { keep, remove, size: g.size, prot: remove.some(isProtectedPath) };
    })
    .filter((x) => x.remove.length > 0);
  if (!reclaimJobs.length) return status(t("nothingToReclaim"));
  const count = reclaimJobs.reduce((a, x) => a + x.remove.length, 0);
  const touchesProt = reclaimJobs.some((x) => x.prot);
  const label = { trash: t("actTrash"), delete: t("actDelete"), hardlink: t("actHardlink") }[action];
  const warn = [
    action === "trash" ? null : { text: t("irreversible"), cls: "error" },
    touchesProt ? { text: t("protWarn"), cls: "error" } : null,
  ].filter(Boolean);
  if (!dryRun && !(await ask(t("reclaimConfirm")(label, count), warn, t("ok"), action !== "trash" || touchesProt))) return;
  status(dryRun ? `${t("dryRun")}…` : t("deleting"));
  try {
    const rep = await invoke("reclaim_dupes", { jobs: reclaimJobs.map(({ keep, remove, size }) => ({ keep, remove, size })), action, dryRun });
    status(`${rep.dry_run ? "DRY-RUN" : "OK"}: ${num(rep.removed)} ${t("files")}, ${human(rep.reclaimed)} ${t("reclaimable")}` +
      (rep.errors.length ? `, ${rep.errors.length} ${t("errors")}` : ""), rep.errors.length > 0);
    if (!rep.dry_run) {
      for (const o of jobs) {
        if (o.kind !== "scan" || !(isUnder(j.root, o.root) || isUnder(o.root, j.root))) continue;
        o.cache.clear();
        o.stale = true;
      }
      j.tagged = false;
      rescan(j);
      loadDrives();
    }
  } catch (e) {
    status(errText(e), true);
  }
}

// -------------------------------- wiring --------------------------------
async function browse() {
  try {
    const picked = await invoke("pick_folders");
    picked.forEach((p, i) => enqueue("scan", p, { select: i === 0 }));
  } catch (e) {
    status(errText(e), true);
  }
}

$("pathform").addEventListener("submit", (e) => { e.preventDefault(); enqueue("scan", $("path").value); });
$("btn-dupes").addEventListener("click", () => enqueue("dupes", $("path").value.trim() || active()?.path || ""));
$("btn-browse").addEventListener("click", browse);
$("btn-settings").addEventListener("click", () => openSettings());
$("btn-excludes").addEventListener("click", () => openSettings(true));
$("btn-scan-all").addEventListener("click", () => drives.forEach((d, i) => enqueue("scan", d.path, { select: i === 0 })));
$("btn-drives-refresh").addEventListener("click", loadDrives);
$("btn-clear-done").addEventListener("click", () => {
  for (const j of jobs.filter((x) => ["done", "cancelled", "error"].includes(x.state) && !x.loading)) closeJob(j);
});
$("settings-dlg").addEventListener("close", () => renderAll());

// Live progress for every running job; repaint at most once per frame.
let paintQueued = false;
try {
  window.__TAURI__?.event?.listen?.("progress", (e) => {
    const j = e.payload && byId(e.payload.job);
    if (!j || j.state !== "running") return;
    j.files = e.payload.files;
    j.bytes = e.payload.bytes;
    if (paintQueued) return;
    paintQueued = true;
    requestAnimationFrame(() => {
      paintQueued = false;
      renderJobs();
      const a = active();
      if (a && a.state === "running") updateProgressView(a);
    });
  });
} catch (_) { /* events unavailable */ }

// Drag & drop: every dropped folder becomes its own queued scan.
try {
  const wv = window.__TAURI__?.webview?.getCurrentWebview?.();
  if (wv && wv.onDragDropEvent) {
    wv.onDragDropEvent((e) => {
      const p = e.payload || {};
      if (p.type === "enter" || p.type === "over") $("dropzone").classList.add("show");
      else $("dropzone").classList.remove("show");
      if (p.type === "drop" && p.paths?.length) p.paths.forEach((path, i) => enqueue("scan", path, { select: i === 0 }));
    });
  }
} catch (_) { /* drag-drop unavailable */ }

// Back / up without reaching for the toolbar: the mouse's back button,
// Alt+Left, Alt+Up and Backspace (outside text fields).
window.addEventListener("mouseup", (e) => {
  if (e.button === 3) { e.preventDefault(); goBack(active()); }
});
document.addEventListener("keydown", (e) => {
  if (e.target.closest?.("input, textarea, select, dialog")) return;
  const j = active();
  if (!j?.view) return;
  if (e.altKey && e.key === "ArrowLeft") { e.preventDefault(); goBack(j); }
  else if ((e.altKey && e.key === "ArrowUp") || e.key === "Backspace") { e.preventDefault(); goUp(j); }
});

// A finished scan's snapshot is written in the background; refresh the list.
try {
  window.__TAURI__?.event?.listen?.("snapshot-saved", (e) => {
    const [id, err] = e.payload || [];
    const j = byId(id);
    if (err) status(err, true);
    if (!j) return;
    j.history = null;
    if (j === active() && j.tab === "changes") renderMain();
  });
} catch (_) { /* events unavailable */ }

(async function start() {
  invoke("app_version").then((v) => { $("version").textContent = `v${v}`; }).catch(() => {});
  try { settings = mergeSettings(await invoke("load_settings")); } catch (_) { /* defaults */ }
  applyI18n();
  updateExcludeChip();
  renderAll();
  loadDrives();
})();
