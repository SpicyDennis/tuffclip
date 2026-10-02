"use strict";
const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (s) => document.querySelector(s);
const $$ = (s) => [...document.querySelectorAll(s)];
const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

const S = {
  cfg: null,
  view: "library",
  kind: "raw",
  clips: [],
  game: "all",
  sort: "new",
  sel: null,         // selected clip object
  dur: 0,
  start: 0,
  end: 0,
  keep: null,        // trim to restore after the video reloads (rename)
  previewing: false,
  mode: "original",  // export: original | size | bitrate
  mb: 0,             // export: target size
  kbps: 0,           // export: target video bitrate
  nameTouched: false,
  exporting: false,
  monitors: [],
  ff: null,
  warn: "",
};

// ------------------------------------------------------------------ utils
function fmtTime(t) {
  if (!isFinite(t)) t = 0;
  const m = Math.floor(t / 60);
  const s = t - m * 60;
  return `${m}:${s.toFixed(1).padStart(4, "0")}`;
}
function fmtSize(b) {
  if (b >= 1e9) return (b / 1e9).toFixed(2) + " GB";
  if (b >= 1e6) return (b / 1e6).toFixed(1) + " MB";
  return Math.max(1, Math.round(b / 1e3)) + " KB";
}
const fmtMb = (mb) => fmtSize(mb * 1e6);
function fmtDate(sec) {
  const d = new Date(sec * 1000);
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" }) + ", " +
         d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}
function toast(msg, err = false) {
  const el = document.createElement("div");
  el.className = "toast" + (err ? " err" : "");
  el.textContent = msg;
  $("#toasts").appendChild(el);
  setTimeout(() => el.remove(), err ? 7000 : 3500);
}
const inInput = () => ["INPUT", "SELECT", "TEXTAREA"].includes(document.activeElement?.tagName);
const baseName = (p) => String(p).split(/[\\/]/).pop();
const stemOf = (p) => baseName(p).replace(/\.[^.]+$/, "");

// ---- bitrates: one setting decides kbps vs Mbps everywhere
const unit = () => (S.cfg?.bitrate_unit === "kbps" ? "kbps" : "mbps");
const trimNum = (n) => String(Math.round(n * 10) / 10).replace(/\.0$/, "");
function fmtRate(kbps) {
  return unit() === "mbps" ? `${trimNum(kbps / 1000)} Mbps` : `${Math.round(kbps).toLocaleString()} kbps`;
}
const rateToInput = (kbps) => (unit() === "mbps" ? Math.round(kbps / 100) / 10 : Math.round(kbps));
const inputToRate = (v) => Math.round(unit() === "mbps" ? v * 1000 : v);
const unitLabel = () => (unit() === "mbps" ? "Mbps" : "kbps");

// ------------------------------------------------------------------ views
function showView(v) {
  S.view = v;
  $("#view-library").hidden = v !== "library";
  $("#view-settings").hidden = v !== "settings";
  $("#tabLibrary").classList.toggle("active", v === "library");
  $("#tabSettings").classList.toggle("active", v === "settings");
  if (v === "settings") openSettings();
}
$("#tabLibrary").addEventListener("click", () => showView("library"));
$("#tabSettings").addEventListener("click", () => showView("settings"));
$("#settingsBack").addEventListener("click", () => showView("library"));

// ---- draggable top-bar tabs (File / Settings / Library); order is remembered
(() => {
  const bar = $("#tabs");
  const KEY = "clipr.tabOrder";
  const items = () => [...bar.children];
  try {
    const saved = JSON.parse(localStorage.getItem(KEY) || "[]");
    saved.forEach((id) => { const el = bar.querySelector(`[data-tab="${id}"]`); if (el) bar.appendChild(el); });
  } catch {}
  const save = () => {
    try { localStorage.setItem(KEY, JSON.stringify(items().map((el) => el.dataset.tab))); } catch {}
  };

  let drag = null, moved = false;
  bar.addEventListener("pointerdown", (e) => {
    const el = e.target.closest("[data-tab]");
    if (!el || e.button !== 0) return;
    drag = { el, x: e.clientX, active: false };
  });
  window.addEventListener("pointermove", (e) => {
    if (!drag) return;
    if (!drag.active) {
      if (Math.abs(e.clientX - drag.x) < 6) return;
      drag.active = true;
      drag.el.classList.add("dragging");
      document.body.style.cursor = "grabbing";
    }
    const others = items().filter((el) => el !== drag.el);
    const next = others.find((el) => {
      const r = el.getBoundingClientRect();
      return e.clientX < r.left + r.width / 2;
    });
    if (next) { if (drag.el.nextElementSibling !== next) bar.insertBefore(drag.el, next); }
    else if (bar.lastElementChild !== drag.el) bar.appendChild(drag.el);
  });
  const end = () => {
    if (!drag) return;
    if (drag.active) {
      moved = true;
      drag.el.classList.remove("dragging");
      document.body.style.cursor = "";
      save();
      setTimeout(() => { moved = false; }, 0);
    }
    drag = null;
  };
  window.addEventListener("pointerup", end);
  window.addEventListener("pointercancel", end);
  // a drag must not also count as a click (would switch tab / open File menu)
  window.addEventListener("click", (e) => {
    if (moved && e.target.closest("#tabs")) { e.stopPropagation(); e.preventDefault(); }
  }, true);
})();

document.addEventListener("menu-action", (e) => {
  switch (e.detail) {
    case "save-clip": invoke("save_clip_now").catch(() => {}); break;
    case "open-folder": invoke("open_clips_folder", { kind: S.kind }).catch((x) => toast(x, true)); break;
    case "data-folder": invoke("open_data_folder").catch((x) => toast(x, true)); break;
    case "hide": invoke("hide_window"); break;
    case "quit": invoke("quit_app"); break;
  }
});
document.addEventListener("keydown", (e) => {
  if (e.ctrlKey && e.key === ",") { e.preventDefault(); showView("settings"); }
  else if (e.ctrlKey && e.key.toLowerCase() === "q") { e.preventDefault(); invoke("quit_app"); }
  else if (e.key === "Escape" && !e.defaultPrevented) {
    if (S.view === "settings") showView("library");
    else if (!inInput() && !document.fullscreenElement) invoke("hide_window");
  }
});

// ------------------------------------------------------------------ status
function renderStatus(st) {
  const el = $("#status");
  el.classList.toggle("live", !!st.recording);
  el.classList.toggle("err", !!st.error);
  let text;
  if (st.error) text = st.error;
  else if (st.recording) {
    const where = st.region && st.region !== "whole display" ? `, ${st.region}` : "";
    text = `Recording ${st.game || "desktop"} (${(st.monitor || "").replace(/ \(.*/, "")}${where}, ${st.fps} fps)`;
  }
  else if (S.cfg?.mode === "games") text = S.cfg.games.some((g) => g.enabled) ? "Waiting for a game" : "Add a game in Settings to start";
  else text = "Idle";
  $("#statusText").textContent = text;
  el.title = text;

  const buf = $("#buf");
  if (st.recording || st.buffer_bytes > 0) {
    buf.hidden = false;
    buf.innerHTML = `${st.buffer_ram ? "RAM" : "Disk"} buffer <b>${fmtSize(st.buffer_bytes)}</b>`;
    buf.title = st.buffer_ram ? "The rolling buffer is held in memory" : "The rolling buffer is stored on disk";
  } else {
    buf.hidden = true;
  }

  const warn = st.warn || "";
  if (warn && warn !== S.warn) toast(warn, true);
  S.warn = warn;
  $("#hotkeyErr").hidden = !warn || S.view !== "settings";
  if (warn) $("#hotkeyErr").textContent = warn;
}

// ------------------------------------------------------------------ library
async function loadClips() {
  S.clips = await invoke("list_clips", { kind: S.kind });
  renderGameFilter();
  renderList();
}

function renderGameFilter() {
  const games = [...new Set(S.clips.map((c) => c.game))].sort((a, b) => a.localeCompare(b));
  if (S.game !== "all" && !games.includes(S.game)) S.game = "all";
  $("#gameFilter").innerHTML =
    `<option value="all">All games</option>` +
    games.map((g) => `<option ${g === S.game ? "selected" : ""}>${esc(g)}</option>`).join("");
}

function visibleClips() {
  let v = S.clips.filter((c) => S.game === "all" || c.game === S.game);
  const by = {
    new: (a, b) => b.modified - a.modified,
    old: (a, b) => a.modified - b.modified,
    size: (a, b) => b.size - a.size,
    game: (a, b) => a.game.localeCompare(b.game) || b.modified - a.modified,
  }[S.sort];
  return v.sort(by);
}

function renderList() {
  const list = visibleClips();
  const box = $("#clipList");
  $("#clipCount").textContent = `${list.length} clip${list.length === 1 ? "" : "s"}`;
  if (!list.length) {
    box.innerHTML = `<div class="list-empty">${
      S.kind === "raw"
        ? `No clips yet. Press <kbd>${esc(S.cfg?.hotkey || "")}</kbd> while recording to save one.`
        : "Nothing exported yet. Open a raw clip, trim it and press Export."
    }</div>`;
    return;
  }
  let html = "";
  let lastGame = null;
  for (const c of list) {
    if (S.sort === "game" && c.game !== lastGame) {
      html += `<div class="group-head">${esc(c.game)}</div>`;
      lastGame = c.game;
    }
    const active = S.sel && S.sel.path === c.path ? " active" : "";
    html += `<div class="item${active}" data-path="${esc(c.path)}">
      <span class="n" title="${esc(c.name)}">${esc(c.name)}</span><span class="s">${fmtSize(c.size)}</span>
      <span class="g">${S.sort === "game" ? "" : esc(c.game)}</span><span class="s">${fmtDate(c.modified)}</span>
    </div>`;
  }
  box.innerHTML = html;
}

$("#clipList").addEventListener("click", (e) => {
  const row = e.target.closest(".item");
  if (!row) return;
  const clip = S.clips.find((c) => c.path === row.dataset.path);
  if (clip) selectClip(clip);
});

$$("#kindSeg button").forEach((b) =>
  b.addEventListener("click", () => {
    $$("#kindSeg button").forEach((x) => x.classList.toggle("active", x === b));
    S.kind = b.dataset.kind;
    loadClips();
  })
);
$("#gameFilter").addEventListener("change", (e) => { S.game = e.target.value; renderList(); });
$("#sortSel").addEventListener("change", (e) => { S.sort = e.target.value; renderList(); });
$("#openFolder").addEventListener("click", () => invoke("open_clips_folder", { kind: S.kind }).catch((e) => toast(e, true)));

// ------------------------------------------------------------------ player
const video = $("#video");

function selectClip(clip) {
  S.sel = clip;
  S.keep = null;
  S.nameTouched = false;
  $("#emptyStage").hidden = true;
  $("#player").hidden = false;
  $("#clipTitle").value = clip.name;
  $("#clipMeta").innerHTML = `<span>${esc(clip.game)}</span><span>${fmtSize(clip.size)}</span><span>${fmtDate(clip.modified)}</span>`;
  video.src = convertFileSrc(clip.path);
  S.dur = 0; S.start = 0; S.end = 0;
  disarmDelete();
  renderList();
  renderTrim();
}

video.addEventListener("loadedmetadata", () => {
  S.dur = video.duration || 0;
  if (S.keep) {
    S.start = Math.min(S.keep.start, S.dur);
    S.end = Math.min(S.keep.end, S.dur);
    video.currentTime = Math.min(S.keep.t, S.dur);
    S.keep = null;
  } else {
    S.start = 0;
    S.end = S.dur;
  }
  renderTrim();
});
video.addEventListener("timeupdate", () => {
  if (S.previewing && video.currentTime >= S.end) {
    video.pause();
    S.previewing = false;
  }
  renderHead();
});
video.addEventListener("pause", () => (S.previewing = false));
video.addEventListener("error", () => {
  if (S.keep) return; // we removed the source on purpose (rename)
  if (video.getAttribute("src")) toast("This clip can't be played here. HEVC needs Microsoft's HEVC Video Extensions.", true);
});

// rename the raw clip right in its title
const title = $("#clipTitle");
title.addEventListener("keydown", (e) => {
  e.stopPropagation();
  if (e.key === "Enter") title.blur();
  else if (e.key === "Escape") { title.value = S.sel?.name || ""; title.blur(); }
});
title.addEventListener("change", async () => {
  if (!S.sel) return;
  const name = title.value.trim();
  if (!name || name === S.sel.name) { title.value = S.sel.name; return; }
  S.keep = { start: S.start, end: S.end, t: video.currentTime };
  video.removeAttribute("src");
  video.load(); // release the file handle before renaming
  try {
    const np = await invoke("rename_clip", { path: S.sel.path, name });
    S.sel = { ...S.sel, path: np, name: stemOf(np) };
    title.value = S.sel.name;
    toast("Renamed");
  } catch (e) {
    toast(String(e), true);
    title.value = S.sel.name;
  }
  video.src = convertFileSrc(S.sel.path);
  loadClips();
});

// ------------------------------------------------------------------ trim
const tl = $("#timeline");
const pct = (t) => (S.dur ? (t / S.dur) * 100 : 0);

function renderTrim() {
  const a = pct(S.start), b = pct(S.end);
  $("#shadeL").style.width = a + "%";
  $("#shadeR").style.width = 100 - b + "%";
  $("#tlSel").style.left = a + "%";
  $("#tlSel").style.width = b - a + "%";
  $("#hStart").style.left = a + "%";
  $("#hEnd").style.left = b + "%";
  $("#tcStart").textContent = fmtTime(S.start);
  $("#tcEnd").textContent = fmtTime(S.end);
  $("#tcLen").textContent = S.dur ? `${(S.end - S.start).toFixed(1)} s selected` : "";
  renderHead();
  renderExport();
}
function renderHead() {
  $("#tlHead").style.left = pct(video.currentTime || 0) + "%";
}
function timeAt(clientX) {
  const r = tl.getBoundingClientRect();
  return Math.min(1, Math.max(0, (clientX - r.left) / r.width)) * S.dur;
}
function setStart(t) { S.start = Math.max(0, Math.min(t, S.end - 0.2)); renderTrim(); }
function setEnd(t) { S.end = Math.min(S.dur, Math.max(t, S.start + 0.2)); renderTrim(); }

let drag = null;
tl.addEventListener("pointerdown", (e) => {
  if (!S.dur) return;
  tl.setPointerCapture(e.pointerId);
  drag = e.target.dataset.h || "seek";
  move(e);
});
tl.addEventListener("pointermove", (e) => drag && move(e));
tl.addEventListener("pointerup", () => (drag = null));
function move(e) {
  const t = timeAt(e.clientX);
  if (drag === "start") { setStart(t); video.currentTime = S.start; }
  else if (drag === "end") { setEnd(t); video.currentTime = S.end; }
  else { video.currentTime = t; renderHead(); }
}
[$("#hStart"), $("#hEnd")].forEach((h) =>
  h.addEventListener("keydown", (e) => {
    const step = e.shiftKey ? 1 : 0.1;
    const d = e.key === "ArrowLeft" ? -step : e.key === "ArrowRight" ? step : 0;
    if (!d) return;
    e.preventDefault();
    if (h.dataset.h === "start") setStart(S.start + d); else setEnd(S.end + d);
  })
);

$("#setStart").addEventListener("click", () => setStart(video.currentTime));
$("#setEnd").addEventListener("click", () => setEnd(video.currentTime));
$("#resetTrim").addEventListener("click", () => { S.start = 0; S.end = S.dur; renderTrim(); });
$("#previewSel").addEventListener("click", () => {
  video.currentTime = S.start;
  S.previewing = true;
  video.play();
});

document.addEventListener("keydown", (e) => {
  if (S.view !== "library" || !S.sel || inInput() || e.ctrlKey || e.altKey || e.metaKey) return;
  const k = e.key.toLowerCase();
  if (k === "i") setStart(video.currentTime);
  else if (k === "o") setEnd(video.currentTime);
  else if (k === " " && document.activeElement !== video) { e.preventDefault(); video.paused ? video.play() : video.pause(); }
});

// ------------------------------------------------------------------ export
const SIZE_PRESETS = [5, 10, 25, 50, 100, 200, 500];
const RATE_PRESETS = [1000, 2500, 5000, 8000, 15000, 25000, 40000, 60000];

const audioTier = (total) => (total < 600 ? 48 : total < 2000 ? 96 : 128);
const isWebm = () => S.cfg?.export_format === "webm";

// What the clip itself weighs, so nothing can be exported bigger than "native".
function native() {
  const len = S.end - S.start;
  if (!S.sel || !S.dur || len <= 0) return null;
  const total = (S.sel.size * 8) / 1000 / S.dur;        // kbps, video + audio
  return { len, total, mb: (total * 1000 / 8 * len) / 1e6, cap: total - 128 };
}
const sizeOk = (mb, n) => mb > 0 && mb < n.mb * 0.98;
const rateOk = (k, n) => k >= 300 && k <= n.cap;

function exportTag() {
  if (S.mode === "size" && S.mb > 0) return `${Number.isInteger(S.mb) ? S.mb : S.mb.toFixed(1)}MB`;
  if (S.mode === "bitrate" && S.kbps > 0) return `${trimNum(S.kbps / 1000)}Mbps`;
  return "trim";
}
function renderName() {
  if (S.nameTouched || !S.sel) return;
  $("#expName").value = `${S.sel.name}_${exportTag()}`;
}
$("#expName").addEventListener("input", (e) => {
  S.nameTouched = e.target.value.trim() !== "";
  if (!S.nameTouched) renderName();
});
$("#expName").addEventListener("keydown", (e) => e.stopPropagation());

$$("#modeSeg button").forEach((b) =>
  b.addEventListener("click", () => {
    S.mode = b.dataset.mode;
    renderExport();
  })
);

function pickDefaults(n) {
  if (S.mode === "size") {
    const ok = SIZE_PRESETS.filter((m) => sizeOk(m, n));
    if (!sizeOk(S.mb, n)) S.mb = [...ok].reverse().find((m) => m <= 25) ?? ok[0] ?? 0;
  }
  if (S.mode === "bitrate") {
    const ok = RATE_PRESETS.filter((k) => rateOk(k, n));
    if (!rateOk(S.kbps, n)) S.kbps = [...ok].reverse().find((k) => k <= 8000) ?? ok[0] ?? 0;
  }
}

function renderExport() {
  const n = native();
  const reencode = S.mode !== "original" || isWebm();
  $$("#modeSeg button").forEach((b) => b.classList.toggle("active", b.dataset.mode === S.mode));
  $("#sizeRow").hidden = S.mode !== "size";
  $("#rateRow").hidden = S.mode !== "bitrate";
  $("#expFormat").value = S.cfg?.export_format || "mp4";
  $("#expRes").disabled = !reencode;
  $("#expPrecise").disabled = !reencode;
  $("#customRateUnit").textContent = unitLabel();

  const cap = $("#capHint");
  cap.hidden = true;
  let valid = !!n;
  let est = "";
  let hint = "";

  if (n) {
    pickDefaults(n);
    const customMb = $("#customMb").value !== "";
    const customRate = $("#customRate").value !== "";

    if (S.mode === "size") {
      const presets = SIZE_PRESETS.filter((m) => sizeOk(m, n));
      $("#sizeChips").innerHTML = presets
        .map((m) => `<button data-mb="${m}" class="${!customMb && m === S.mb ? "active" : ""}">${m} MB</button>`)
        .join("");
      $("#customMb").classList.toggle("active", customMb);
      const bad = customMb && !sizeOk(S.mb, n);
      $("#customMb").classList.toggle("bad", bad);
      if (!presets.length) { cap.hidden = false; cap.textContent = `This selection is only about ${fmtMb(n.mb)} already. Export the original instead.`; }
      else if (bad) { cap.hidden = false; cap.textContent = `Can't be as big as the clip already is (about ${fmtMb(n.mb)} for this selection).`; }
      valid = sizeOk(S.mb, n);
      if (valid) {
        est = `About <b>${fmtMb(S.mb)}</b>`;
        const v = (S.mb * 8000 * 0.96) / n.len - audioTier((S.mb * 8000 * 0.96) / n.len);
        if (v < 800) hint = "That will look rough. Trim shorter or raise the size.";
        else hint = "Picks a bitrate so the file lands near this size.";
      }
    } else if (S.mode === "bitrate") {
      const presets = RATE_PRESETS.filter((k) => rateOk(k, n));
      $("#rateChips").innerHTML = presets
        .map((k) => `<button data-k="${k}" class="${!customRate && k === S.kbps ? "active" : ""}">${fmtRate(k)}</button>`)
        .join("");
      $("#customRate").classList.toggle("active", customRate);
      const bad = customRate && !rateOk(S.kbps, n);
      $("#customRate").classList.toggle("bad", bad);
      if (!presets.length) { cap.hidden = false; cap.textContent = `This clip's own bitrate is only about ${fmtRate(n.total)}. Export the original instead.`; }
      else if (bad) {
        cap.hidden = false;
        cap.textContent = S.kbps < 300 ? `Too low to look like anything. Try at least ${fmtRate(300)}.` : `Can't be higher than the clip's own bitrate (about ${fmtRate(n.cap)} of video).`;
      }
      valid = rateOk(S.kbps, n);
      if (valid) {
        const total = S.kbps + audioTier(S.kbps + 128);
        est = `About <b>${fmtMb((total * 1000 / 8 * n.len) / 1e6)}</b>`;
        hint = S.kbps < 800 ? "That will look rough." : "Video bitrate; audio is added on top.";
      }
    } else {
      est = `About <b>${fmtMb(n.mb)}</b>`;
      hint = isWebm()
        ? "WebM can't hold this clip's video as it is, so it's re-encoded. That takes a while and keeps roughly the same quality."
        : "Cuts land on the nearest keyframe. Instant and lossless.";
    }
    if (isWebm() && S.mode !== "original") hint += " WebM is re-encoded on the CPU, so it takes longer.";
  }
  $("#estimate").innerHTML = est;
  $("#modeHint").textContent = hint;
  $("#exportBtn").disabled = !valid || S.exporting;
  renderName();
}

$("#sizeChips").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (!b) return;
  S.mb = Number(b.dataset.mb);
  $("#customMb").value = "";
  renderExport();
});
$("#rateChips").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (!b) return;
  S.kbps = Number(b.dataset.k);
  $("#customRate").value = "";
  renderExport();
});
$("#customMb").addEventListener("input", (e) => {
  const v = Number(e.target.value);
  S.mb = v > 0 ? v : 0;
  renderExport();
});
$("#customRate").addEventListener("input", (e) => {
  const v = Number(e.target.value);
  S.kbps = v > 0 ? inputToRate(v) : 0;
  renderExport();
});
$("#expFormat").addEventListener("change", (e) => setCfg({ export_format: e.target.value }));
$("#expRes").addEventListener("change", renderExport);

$("#exportBtn").addEventListener("click", async () => {
  const n = native();
  if (!S.sel || !n || S.exporting) return;
  S.exporting = true;
  $("#exportBtn").disabled = true;
  $("#exportBtn").textContent = "Exporting";
  $("#progress").hidden = false;
  $("#progressBar").style.width = "0%";
  try {
    const out = await invoke("export_clip", {
      req: {
        path: S.sel.path,
        start: S.start,
        end: S.end,
        mode: S.mode,
        target_mb: S.mode === "size" ? S.mb : 0,
        target_kbps: S.mode === "bitrate" ? S.kbps : 0,
        height: Number($("#expRes").value),
        precise: $("#expPrecise").checked,
        name: S.nameTouched ? $("#expName").value.trim() : "",
        src_kbps: n.total,
      },
    });
    toast(`Exported ${baseName(out)}`);
    S.nameTouched = false;
    if (S.kind === "exports") loadClips();
  } catch (e) {
    toast(String(e), true);
  } finally {
    S.exporting = false;
    $("#exportBtn").textContent = "Export";
    renderExport();
    setTimeout(() => ($("#progress").hidden = true), 600);
  }
});

// ------------------------------------------------------------------ reveal / delete
$("#revealBtn").addEventListener("click", () => S.sel && invoke("reveal_clip", { path: S.sel.path }));

let deleteTimer = null;
function disarmDelete() {
  clearTimeout(deleteTimer);
  $("#deleteBtn").classList.remove("armed");
  $("#deleteBtn").textContent = "Delete";
}
$("#deleteBtn").addEventListener("click", async () => {
  const btn = $("#deleteBtn");
  if (!btn.classList.contains("armed")) {
    btn.classList.add("armed");
    btn.textContent = "Click again to delete";
    deleteTimer = setTimeout(disarmDelete, 3000);
    return;
  }
  disarmDelete();
  const path = S.sel.path;
  video.removeAttribute("src");
  video.load(); // release the file handle before deleting
  try {
    await invoke("delete_clip", { path });
    S.sel = null;
    $("#player").hidden = true;
    $("#emptyStage").hidden = false;
    loadClips();
  } catch (e) {
    toast(String(e), true);
  }
});

// ================================================================== settings
// Settings save themselves a moment after every change; there is no Save button.
let saveTimer = null;
let savedTimer = null;

function flash(msg, err = false) {
  const el = $("#savedMsg");
  el.textContent = msg;
  el.classList.toggle("err", err);
  el.classList.add("show");
  clearTimeout(savedTimer);
  savedTimer = setTimeout(() => el.classList.remove("show"), err ? 6000 : 1800);
}

function schedule(delay = 250) {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(flush, delay);
}

async function flush() {
  clearTimeout(saveTimer);
  try {
    await invoke("save_config", { cfg: { ...S.cfg } });
    S.cfg = await invoke("get_config");
    flash("Saved");
    fillControls();
    renderEstimates();
    renderExport();
    renderStatus(await invoke("get_status"));
  } catch (e) {
    flash(String(e), true);
    toast(String(e), true);
    S.cfg = await invoke("get_config");
    fillControls(true);
    renderEstimates();
  }
}

function setCfg(patch) {
  Object.assign(S.cfg, patch);
  fillControls();
  renderEstimates();
  renderExport();
  schedule(150);
}

function setSeg(sel, attr, val) {
  $$(`${sel} button`).forEach((b) => b.classList.toggle("active", b.dataset[attr] === val));
}

const FORMAT_HINTS = {
  mp4: "Plays almost everywhere. Exporting the original quality is instant.",
  mkv: "Holds anything, but some editors and websites won't take it.",
  mov: "Handy for editing on a Mac or in some video editors.",
  webm: "Small and web-friendly. Every WebM export is re-encoded, so it takes longer and uses more CPU.",
};

function monitorHz() {
  const id = S.cfg?.mode === "desktop" ? S.cfg.monitor : null;
  const m = S.monitors.find((x) => x.id === id) || S.monitors.find((x) => x.primary) || S.monitors[0];
  return m?.refresh_hz || 0;
}

function renderFps() {
  const hz = monitorHz();
  const opts = [[0, hz ? `Native (${hz} Hz)` : "Native (match my display)"]];
  const list = [30, 60, 120, 144, 165, 240];
  if (S.cfg.fps && !list.includes(S.cfg.fps)) list.push(S.cfg.fps);
  list.sort((a, b) => a - b).forEach((f) => opts.push([f, `${f} fps`]));
  const sel = $("#fps");
  sel.innerHTML = opts.map(([v, l]) => `<option value="${v}">${l}</option>`).join("");
  sel.value = String(S.cfg.fps);
  $("#fpsHint").textContent = !S.cfg.fps
    ? "Follows your display's refresh rate."
    : hz && S.cfg.fps > hz
      ? `Your display only runs at ${hz} Hz, so the extra frames would be copies.`
      : "";
}

function renderEstimates() {
  const c = S.cfg;
  if (!c) return;
  const total = c.bitrate_kbps + (c.audio ? c.audio_kbps : 0);
  const clipMb = (total * c.clip_seconds) / 8 / 1000;
  const bufSecs = Math.ceil(c.clip_seconds / 2) * 2 + 8;
  const bufMb = (total * bufSecs) / 8 / 1000;
  $("#clipEstimate").innerHTML =
    `A <b>${c.clip_seconds} s</b> clip is about <b>${fmtMb(clipMb)}</b>. ` +
    `While recording, the buffer holds up to about <b>${fmtMb(bufMb)}</b> ${c.buffer_in_ram ? "in RAM" : "on disk"}.`;
  $("#ramHint").textContent = c.buffer_in_ram
    ? `Uses about ${fmtMb(bufMb)} of memory while recording and writes nothing to your drive. It's emptied when recording stops.`
    : `Off: the buffer (about ${fmtMb(bufMb)}) is written to your drive. Turn on to keep it in memory and spare your SSD.`;
  $("#bufferDirField").hidden = !!c.buffer_in_ram;
  $("#formatHint").textContent = FORMAT_HINTS[c.export_format] +
    (c.export_format === "webm" && S.ff?.ok && !S.ff.vp9 ? " This FFmpeg build has no VP9 encoder, so WebM won't work." : "");
  $("#formatHint").classList.toggle("err", c.export_format === "webm" && !!S.ff?.ok && !S.ff.vp9);
}

// Put config values into the controls. `skipActive` keeps whatever the user is typing in.
function fillControls(force = false) {
  const c = S.cfg;
  if (!c) return;
  const act = document.activeElement;
  const set = (sel, fn) => { const el = $(sel); if (force || el !== act) fn(el); };

  setSeg("#captureSeg", "mode", c.mode);
  $("#monitorField").hidden = c.mode !== "desktop";
  $("#modeHint2").textContent =
    c.mode === "games"
      ? "Records the monitor your game is on, only while one of your games is running. Nothing runs otherwise."
      : "Records the chosen monitor all the time. Clips made while a listed game is focused are still filed under that game.";
  set("#monitorSel", (el) => { if (c.monitor) el.value = c.monitor; });
  set("#drawMouse", (el) => (el.checked = c.draw_mouse));
  set("#startHidden", (el) => (el.checked = c.start_hidden));
  set("#beep", (el) => (el.checked = c.beep));
  set("#audio", (el) => (el.checked = c.audio));
  set("#exportGentle", (el) => (el.checked = c.export_gentle));
  set("#bufferRam", (el) => (el.checked = c.buffer_in_ram));
  set("#height", (el) => (el.value = String(c.height)));
  set("#encoder", (el) => (el.value = c.encoder));
  set("#codec", (el) => (el.value = c.codec));
  set("#clipSeconds", (el) => (el.value = c.clip_seconds));
  set("#audioKbps", (el) => (el.value = c.audio_kbps));
  set("#audioOffset", (el) => (el.value = c.audio_offset_ms));
  set("#clipsDir", (el) => (el.value = c.clips_dir));
  set("#bufferDir", (el) => (el.value = c.buffer_dir || ""));
  set("#ffmpeg", (el) => (el.value = c.ffmpeg));
  set("#hotkey", (el) => { if (!el.classList.contains("listening")) el.value = c.hotkey; });

  $$("#unitChips button").forEach((b) => b.classList.toggle("active", b.dataset.unit === unit()));
  $$("#formatChips button").forEach((b) => b.classList.toggle("active", b.dataset.fmt === c.export_format));
  $("#bitrateLabel").textContent = `Bitrate (${unitLabel()})`;
  $("#bitrateUnit").textContent = unitLabel();
  const br = $("#bitrate");
  br.step = unit() === "mbps" ? 1 : 500;
  br.min = unit() === "mbps" ? 2 : 2000;
  br.max = unit() === "mbps" ? 150 : 150000;
  set("#bitrate", (el) => (el.value = rateToInput(c.bitrate_kbps)));
  $("#bitrateHint").textContent = `1080p60: ${fmtRate(25000)} to ${fmtRate(40000)}. 1440p60: ${fmtRate(40000)} to ${fmtRate(60000)}.`;
  renderFps();
  renderGames();
  $("#menuHotkey").textContent = c.hotkey;
  $("#customRateUnit").textContent = unitLabel();
}

async function openSettings() {
  try { S.monitors = await invoke("list_monitors"); } catch { S.monitors = []; }
  $("#monitorSel").innerHTML = S.monitors
    .map((m) => `<option value="${esc(m.id)}">${esc(m.label)}</option>`)
    .join("");
  if (!S.cfg.monitor || !S.monitors.some((m) => m.id === S.cfg.monitor)) {
    const m = S.monitors.find((x) => x.primary) || S.monitors[0];
    if (m) $("#monitorSel").value = m.id;
  }
  fillControls(true);
  renderEstimates();
  refreshRunning();
  renderFfHints();
  try {
    const info = await invoke("app_info");
    $("#autostart").checked = info.autostart;
    $("#aboutName").textContent = `Clipr ${info.version}`;
    $("#aboutBuild").textContent = `Built ${info.build_date}`;
    $("#dataPath").textContent = info.data_dir;
  } catch {}
  spy();
}

// ---- generic bindings (change = after you finish typing; no half-typed values get applied)
function onChange(sel, fn, ev = "change") {
  $(sel).addEventListener(ev, (e) => {
    const patch = fn(e.target);
    if (patch === undefined) return;
    setCfg(patch);
  });
}
function numField(el, min, max) {
  const v = parseFloat(el.value);
  const field = el.closest(".field");
  if (!isFinite(v)) { field?.classList.add("invalid"); return undefined; }
  field?.classList.remove("invalid");
  return Math.min(max, Math.max(min, v));
}

onChange("#drawMouse", (el) => ({ draw_mouse: el.checked }));
onChange("#startHidden", (el) => ({ start_hidden: el.checked }));
onChange("#beep", (el) => ({ beep: el.checked }));
onChange("#audio", (el) => ({ audio: el.checked }));
onChange("#exportGentle", (el) => ({ export_gentle: el.checked }));
onChange("#bufferRam", (el) => ({ buffer_in_ram: el.checked }));
onChange("#height", (el) => ({ height: Number(el.value) }));
onChange("#fps", (el) => ({ fps: Number(el.value) }));
onChange("#encoder", (el) => ({ encoder: el.value }));
onChange("#codec", (el) => ({ codec: el.value }));
onChange("#monitorSel", (el) => ({ monitor: el.value || null }));
onChange("#bitrate", (el) => {
  const v = numField(el, 0, 1e9);
  if (v === undefined) return;
  return { bitrate_kbps: Math.min(150000, Math.max(2000, inputToRate(v))) };
});
onChange("#clipSeconds", (el) => {
  const v = numField(el, 5, 600);
  return v === undefined ? undefined : { clip_seconds: Math.round(v) };
});
onChange("#audioKbps", (el) => {
  const v = numField(el, 64, 320);
  return v === undefined ? undefined : { audio_kbps: Math.round(v) };
});
onChange("#audioOffset", (el) => {
  const v = numField(el, -2000, 2000);
  return v === undefined ? undefined : { audio_offset_ms: Math.round(v) };
});
onChange("#clipsDir", (el) => (el.value.trim() ? { clips_dir: el.value.trim() } : undefined));
onChange("#bufferDir", (el) => ({ buffer_dir: el.value.trim() || null }));
onChange("#ffmpeg", (el) => ({ ffmpeg: el.value.trim() }));
$("#ffmpeg").addEventListener("change", () => setTimeout(renderFfHints, 600));

$$("#captureSeg button").forEach((b) => b.addEventListener("click", () => setCfg({ mode: b.dataset.mode })));
$$("#unitChips button").forEach((b) => b.addEventListener("click", () => {
  setCfg({ bitrate_unit: b.dataset.unit });
  $("#customRate").value = "";
}));
$$("#formatChips button").forEach((b) => b.addEventListener("click", () => setCfg({ export_format: b.dataset.fmt })));
$("#autostart").addEventListener("change", async (e) => {
  try { await invoke("set_autostart_cmd", { on: e.target.checked }); flash("Saved"); }
  catch (x) { e.target.checked = !e.target.checked; toast(String(x), true); }
});

// ---- games
function renderGames() {
  const box = $("#gameList");
  if (box.contains(document.activeElement)) return; // don't yank the field being edited
  const games = S.cfg.games;
  if (!games.length) {
    box.innerHTML = `<div class="games-empty">No games yet.</div>`;
    return;
  }
  box.innerHTML = games
    .map(
      (g, i) => `<div class="game${g.enabled ? "" : " off"}" data-i="${i}">
        <input type="checkbox" data-k="enabled" ${g.enabled ? "checked" : ""} title="Record this game">
        <input class="name" data-k="name" value="${esc(g.name)}" aria-label="Name shown on clips">
        <span class="exe" title="${esc(g.exe)}">${esc(g.exe)}</span>
        <button class="btn sm ghost" data-k="remove">Remove</button>
      </div>`
    )
    .join("");
}
$("#gameList").addEventListener("input", (e) => {
  const row = e.target.closest(".game");
  if (!row) return;
  const g = S.cfg.games[Number(row.dataset.i)];
  if (e.target.dataset.k === "name") { g.name = e.target.value; schedule(700); }
  if (e.target.dataset.k === "enabled") {
    g.enabled = e.target.checked;
    row.classList.toggle("off", !g.enabled);
    schedule(150);
  }
});
$("#gameList").addEventListener("click", (e) => {
  if (e.target.dataset.k !== "remove") return;
  S.cfg.games.splice(Number(e.target.closest(".game").dataset.i), 1);
  $("#gameList").querySelectorAll(":focus").forEach((x) => x.blur());
  renderGames();
  schedule(150);
});

function addGame(exe, name) {
  exe = exe.trim();
  if (!exe) return;
  if (!/\.exe$/i.test(exe)) exe += ".exe";
  if (S.cfg.games.some((g) => g.exe.toLowerCase() === exe.toLowerCase())) {
    toast(`${exe} is already in the list`);
    return;
  }
  S.cfg.games.push({ exe, name: name || exe.replace(/\.exe$/i, ""), enabled: true });
  renderGames();
  schedule(150);
}

async function refreshRunning() {
  const apps = await invoke("list_windows");
  $("#runningSel").innerHTML =
    `<option value="">Add a running appâ€¦</option>` +
    apps.map((a) => `<option value="${esc(a.exe)}" data-title="${esc(a.title)}">${esc(a.exe)}, ${esc(a.title.slice(0, 50))}</option>`).join("");
}
$("#refreshRunning").addEventListener("click", refreshRunning);
$("#addRunning").addEventListener("click", () => {
  const sel = $("#runningSel");
  if (!sel.value) return;
  const t = sel.selectedOptions[0].dataset.title || "";
  addGame(sel.value, t.length && t.length < 40 ? t : "");
  sel.value = "";
});
$("#addManual").addEventListener("click", () => { addGame($("#manualExe").value); $("#manualExe").value = ""; });
$("#manualExe").addEventListener("keydown", (e) => { if (e.key === "Enter") $("#addManual").click(); });

// ---- hotkey capture
const hk = $("#hotkey");
let hkPrev = "";
hk.addEventListener("focus", () => { hkPrev = S.cfg.hotkey; hk.value = "Press keysâ€¦"; hk.classList.add("listening"); });
hk.addEventListener("blur", () => {
  hk.classList.remove("listening");
  if (hk.value === "Press keysâ€¦") hk.value = S.cfg.hotkey;
});
hk.addEventListener("keydown", (e) => {
  e.preventDefault();
  e.stopPropagation();
  if (e.key === "Escape") { hk.value = hkPrev; hk.blur(); return; }
  if (e.key === "Backspace") {
    hk.value = hkPrev;
    flash("A hotkey is needed to save clips", true);
    hk.blur();
    return;
  }
  if (["Control", "Alt", "Shift", "Meta"].includes(e.key)) return;
  let key = e.code;
  if (key.startsWith("Key")) key = key.slice(3);
  else if (key.startsWith("Digit")) key = key.slice(5);
  const combo = [e.ctrlKey && "Ctrl", e.altKey && "Alt", e.shiftKey && "Shift", e.metaKey && "Super", key].filter(Boolean).join("+");
  hk.classList.remove("listening");
  hk.value = combo;
  hk.blur();
  if (combo !== S.cfg.hotkey) {
    S.cfg.hotkey = combo;
    flush(); // a taken hotkey is reported (and reverted) straight away
  }
});

// ---- ffmpeg
let ffBusy = false;
$("#getFfmpeg").addEventListener("click", async () => {
  ffBusy = true;
  $("#getFfmpeg").hidden = true;
  $("#ffHint").textContent = "Downloading FFmpeg...";
  const un = await listen("ffmpeg-download", (e) => {
    $("#ffHint").textContent = `Downloading FFmpeg... ${(e.payload / 1048576).toFixed(0)} MB`;
  });
  try {
    S.cfg.ffmpeg = await invoke("download_ffmpeg");
    $("#ffmpeg").value = S.cfg.ffmpeg;
  } catch (x) { toast(String(x), true); }
  un();
  ffBusy = false;
  renderFfHints();
});
async function renderFfHints() {
  $("#ffHint").textContent = "Checking FFmpegâ€¦";
  try {
    S.ff = await invoke("ffmpeg_info");
  } catch { S.ff = { ok: false }; }
  const f = S.ff;
  $("#getFfmpeg").hidden = !!f.ok || ffBusy;
  if (!f.ok) {
    $("#ffHint").textContent = "FFmpeg wasn't found. Use the button above to download it, or enter the full path to ffmpeg.exe.";
    $("#encHint").textContent = "";
    return;
  }
  $("#ffHint").textContent = f.ddagrab ? f.version : `${f.version}. This build has no ddagrab filter, so screen capture won't work. Use a full build from gyan.dev or BtbN.`;
  $("#encHint").textContent = f.encoders.length
    ? `Your FFmpeg supports: ${f.encoders.map((e) => ({ nvenc: "NVIDIA", amf: "AMD", qsv: "Intel" }[e])).join(", ")}`
    : "This FFmpeg build has no hardware encoders.";
  $("#scaleHint").textContent = f.scale_d3d11 ? "" : "Native only: this FFmpeg can't resize on the GPU (needs FFmpeg 8).";
  $$("#height option").forEach((o) => (o.disabled = !f.scale_d3d11 && o.value !== "0"));
  renderEstimates();
}

// ---- advanced / about
$("#openData").addEventListener("click", () => invoke("open_data_folder").catch((e) => toast(e, true)));
let resetTimer = null;
$("#resetAll").addEventListener("click", async (e) => {
  const btn = e.currentTarget;
  if (!btn.classList.contains("armed")) {
    btn.classList.add("armed");
    btn.textContent = "Click again to reset";
    resetTimer = setTimeout(() => { btn.classList.remove("armed"); btn.textContent = "Reset all settings"; }, 3000);
    return;
  }
  clearTimeout(resetTimer);
  btn.classList.remove("armed");
  btn.textContent = "Reset all settings";
  try {
    S.cfg = await invoke("reset_config");
    $("#autostart").checked = false;
    fillControls(true);
    renderEstimates();
    renderExport();
    flash("Settings reset");
  } catch (x) { toast(String(x), true); }
});
$("#copyPath").addEventListener("click", async () => {
  const text = $("#dataPath").textContent;
  try { await navigator.clipboard.writeText(text); }
  catch {
    const t = document.createElement("textarea");
    t.value = text; document.body.appendChild(t); t.select();
    document.execCommand("copy"); t.remove();
  }
  toast("Copied");
});

// ---- category list: click scrolls, scrolling highlights
$$("#settingsNav .item").forEach((b) =>
  b.addEventListener("click", () => {
    document.getElementById("g-" + b.dataset.group)?.scrollIntoView({ behavior: "smooth", block: "start" });
  })
);
function spy() {
  const box = $("#settingsScroll");
  const top = box.getBoundingClientRect().top + 40;
  let cur = "general";
  for (const b of $$("#settingsNav .item")) {
    const g = document.getElementById("g-" + b.dataset.group);
    if (g && g.getBoundingClientRect().top <= top) cur = b.dataset.group;
  }
  if (box.scrollTop + box.clientHeight >= box.scrollHeight - 4) cur = "about";
  $$("#settingsNav .item").forEach((b) => b.classList.toggle("active", b.dataset.group === cur));
}
$("#settingsScroll").addEventListener("scroll", spy);

// ------------------------------------------------------------------ boot
(async function boot() {
  S.cfg = await invoke("get_config");
  try { S.monitors = await invoke("list_monitors"); } catch {}
  $("#menuHotkey").textContent = S.cfg.hotkey;
  $("#emptySub").innerHTML = `Clips are saved with <kbd>${esc(S.cfg.hotkey)}</kbd>, or File â†’ Save clip now.`;
  renderStatus(await invoke("get_status"));
  renderExport();
  await loadClips();
  try {
    const info = await invoke("app_info");
    if (info.config_broken) toast("Your settings file was damaged, so Clipr started with defaults. The old file is saved as config.broken.json.", true);
  } catch {}

  listen("status", (e) => renderStatus(e.payload));
  listen("clip-saved", (e) => {
    toast(`Saved ${baseName(e.payload)}`);
    if (S.kind === "raw") loadClips();
  });
  listen("clip-error", (e) => toast(String(e.payload), true));
  listen("export-progress", (e) => {
    if (S.sel && e.payload.path === S.sel.path) $("#progressBar").style.width = Math.round(e.payload.pct * 100) + "%";
  });
})();
