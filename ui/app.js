"use strict";
const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (s) => document.querySelector(s);
const $$ = (s) => [...document.querySelectorAll(s)];
const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

// small per-viewer preferences (remembered between runs; never required)
const store = {
  get(k, d) { try { const v = localStorage.getItem(k); return v === null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch {} },
};

const HEVC_RATIO = 0.65; // HEVC needs about this share of H.264's bitrate for the same picture quality

const S = {
  cfg: null,
  view: "library",
  kind: "raw",
  clips: [],
  game: "all",
  sort: store.get("clipr.sort", "new"),
  group: store.get("clipr.group", false),
  sel: null,         // selected clip object
  selKind: "raw",
  dur: 0,
  start: 0,
  end: 0,
  keep: null,        // trim to restore after the video reloads (rename)
  previewing: false,
  freeRoam: false,   // the playhead was put outside the trim on purpose, so playback may leave it
  mode: "original",  // export: original | size | bitrate
  format: store.get("clipr.format", "mp4"),
  codec: store.get("clipr.codec", "keep"), // export: keep | h264 | hevc
  srcCodec: "",      // the selected clip's own video codec
  mb: 0,             // export: target size
  kbps: 0,           // export: target video bitrate
  nameTouched: false,
  exporting: false,
  expStart: 0,
  monitors: [],
  ff: null,
  warn: "",
  status: null,
  targetSig: "",
  renamedFrom: [],   // game names that were just changed (their clips move on save)
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
function fmtEta(s) {
  s = Math.max(1, Math.round(s));
  return s < 60 ? `${s} s` : `${Math.floor(s / 60)} min ${s % 60} s`;
}
const plural = (n, w) => `${n} ${w}${n === 1 ? "" : "s"}`;
function toast(msg, err = false) {
  const el = document.createElement("div");
  el.className = "toast" + (err ? " err" : "");
  el.textContent = msg;
  $("#toasts").appendChild(el);
  setTimeout(() => el.remove(), err ? 7000 : 3500);
}
const inInput = () => ["INPUT", "SELECT", "TEXTAREA"].includes(document.activeElement?.tagName);
// same as the Rust side: what a game name becomes as a folder name
const folderName = (n) => (String(n).replace(/[<>:"\/\\|?*\x00-\x1f]/g, "_").trim().replace(/\.+$/, "").trim() || "Unknown");
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

// A yes/no question that needs a real answer (used for anything that deletes or resets).
function confirmDialog(title, body, okLabel) {
  return new Promise((resolve) => {
    const d = $("#confirm");
    $("#confirmTitle").textContent = title;
    $("#confirmBody").textContent = body;
    $("#confirmOk").textContent = okLabel;
    d.returnValue = "cancel";
    d.addEventListener("close", () => resolve(d.returnValue === "ok"), { once: true });
    d.showModal();
  });
}

// ------------------------------------------------------------------ views
function showView(v) {
  S.view = v;
  $("#view-library").hidden = v !== "library";
  $("#view-settings").hidden = v !== "settings";
  $("#tabLibrary").classList.toggle("active", v === "library");
  $("#tabSettings").classList.toggle("active", v === "settings");
  if (v === "settings") openSettings(); else stopMemPoll();
}
$("#tabLibrary").addEventListener("click", () => showView("library"));
$("#tabSettings").addEventListener("click", () => showView("settings"));
$("#expLow").checked = store.get("expLow", false);
$("#expLow").addEventListener("change", (e) => store.set("expLow", e.target.checked));

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

document.addEventListener("keydown", (e) => {
  if ($("#confirm").open) return; // the dialog handles its own Esc / Enter
  if (e.ctrlKey && e.key === ",") { e.preventDefault(); showView("settings"); }
  else if (e.ctrlKey && e.key.toLowerCase() === "q") { e.preventDefault(); invoke("quit_app"); }
  else if (e.key === "Escape" && !e.defaultPrevented) {
    if (S.view === "settings") showView("library");
    else if (!inInput() && !document.fullscreenElement) invoke("hide_window");
  }
});

// ------------------------------------------------------------------ status
const hotkeyText = () => S.cfg?.hotkey || "the hotkey";

function updateStatusText() {
  const st = S.status;
  if (!st) return;
  const held = !st.recording && !!st.held_until_ms;
  let text;
  if (st.error) text = st.error;
  else if (st.recording) {
    const mon = (st.monitor || "").replace(/ \(.*/, "");
    const region = st.region && st.region !== "whole display" ? st.region : "";
    text = [`Recording ${st.game || "desktop"}`, mon, region, `${st.fps} fps`].filter(Boolean).join(" · ");
  } else if (held) {
    const left = Math.max(0, Math.round((st.held_until_ms - Date.now()) / 1000));
    text = `${st.game} closed · buffer kept ${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")} · ${hotkeyText()} still saves`;
  } else if (st.game && st.region) text = `Waiting for ${st.game}'s window`;
  else if (S.cfg?.mode === "games") text = S.cfg.games.some((g) => g.enabled) ? "Waiting for a game" : "Add a game in Settings to start";
  else text = "Idle";
  $("#statusText").textContent = text;
  $("#status").title = st.summary ? `${text}\n${st.summary}` : text;
}
setInterval(() => { if (S.status?.held_until_ms && !S.status.recording) updateStatusText(); }, 1000);

function renderTarget(st) {
  const choices = st.choices || [];
  $("#targetWrap").hidden = choices.length < 2;
  if (choices.length < 2) { S.targetSig = ""; return; }
  const sel = $("#targetSel");
  const sig = choices.map((c) => `${c.exe}|${c.name}|${c.clips}`).join(",");
  if (sig !== S.targetSig) {
    sel.innerHTML = choices.map((c) => `<option value="${esc(c.exe)}">${esc(c.name)} · ${plural(c.clips, "clip")}</option>`).join("");
    S.targetSig = sig;
  }
  if (document.activeElement !== sel && st.target) sel.value = st.target;
}
$("#targetWrap").title = "Several of your games are running. Clipr records the one you've clipped most; pick another to switch (this restarts its buffer).";
$("#targetSel").addEventListener("change", (e) => invoke("set_target", { exe: e.target.value }).catch((x) => toast(String(x), true)));

function renderStatus(st) {
  S.status = st;
  const el = $("#status");
  el.classList.toggle("live", !!st.recording);
  el.classList.toggle("ok", !st.recording && !!st.held_until_ms);
  el.classList.toggle("err", !!st.error);
  updateStatusText();
  renderTarget(st);

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
  const v = S.clips.filter((c) => S.game === "all" || c.game === S.game);
  const by = {
    new: (a, b) => b.modified - a.modified,
    old: (a, b) => a.modified - b.modified,
    size: (a, b) => b.size - a.size,
  }[S.sort] || ((a, b) => b.modified - a.modified);
  return v.sort(S.group ? (a, b) => a.game.localeCompare(b.game) || by(a, b) : by);
}

function renderList() {
  const list = visibleClips();
  const box = $("#clipList");
  $("#clipCount").textContent = plural(list.length, "clip");
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
    if (S.group && c.game !== lastGame) {
      html += `<div class="group-head">${esc(c.game)}</div>`;
      lastGame = c.game;
    }
    const active = S.sel && S.sel.path === c.path ? " active" : "";
    const sub = [S.group ? "" : esc(c.game), c.exported ? "exported" : ""].filter(Boolean).join(" · ");
    html += `<div class="item${active}" data-path="${esc(c.path)}">
      <span class="n" title="${esc(c.name)}">${esc(c.name)}</span><span class="s">${fmtSize(c.size)}</span>
      <span class="g">${sub}</span><span class="s">${fmtDate(c.modified)}</span>
    </div>`;
  }
  box.innerHTML = html;
}

// Clicking the clip that is already open closes it and brings back the blank screen.
$("#clipList").addEventListener("click", (e) => {
  const row = e.target.closest(".item");
  if (!row) return;
  const clip = S.clips.find((c) => c.path === row.dataset.path);
  if (!clip) return;
  if (S.sel && S.sel.path === clip.path) closeClip(); else selectClip(clip);
});

$$("#kindSeg button").forEach((b) =>
  b.addEventListener("click", () => {
    $$("#kindSeg button").forEach((x) => x.classList.toggle("active", x === b));
    S.kind = b.dataset.kind;
    loadClips();
  })
);
$("#gameFilter").addEventListener("change", (e) => { S.game = e.target.value; renderList(); });
$("#sortSel").addEventListener("change", (e) => { S.sort = e.target.value; store.set("clipr.sort", S.sort); renderList(); });
$("#groupBy").addEventListener("change", (e) => { S.group = e.target.checked; store.set("clipr.group", S.group); renderList(); });
$("#openFolder").addEventListener("click", () => invoke("open_clips_folder", { kind: S.kind }).catch((e) => toast(e, true)));

// ------------------------------------------------------------------ player
const video = $("#video");

function selectClip(clip) {
  S.sel = clip;
  S.selKind = S.kind;
  S.keep = null;
  S.nameTouched = false;
  S.freeRoam = false;
  S.srcCodec = "";
  $("#emptyStage").hidden = true;
  $("#player").hidden = false;
  $("#clipTitle").value = clip.name;
  $("#clipMeta").innerHTML = `<span>${esc(clip.game)}</span><span>${fmtSize(clip.size)}</span><span>${fmtDate(clip.modified)}</span>`;
  video.src = convertFileSrc(clip.path);
  S.dur = 0; S.start = 0; S.end = 0;
  disarmDelete();
  renderList();
  renderTrim();
  const path = clip.path;
  invoke("probe_clip", { path }).then((c) => {
    if (S.sel && S.sel.path === path) { S.srcCodec = c; renderExport(); }
  }).catch(() => {});
}

function closeClip() {
  S.sel = null;
  S.keep = null;
  S.previewing = false;
  video.pause();
  video.removeAttribute("src");
  video.load(); // lets go of the file
  $("#player").hidden = true;
  $("#emptyStage").hidden = false;
  disarmDelete();
  renderList();
}

video.addEventListener("loadedmetadata", () => {
  S.dur = video.duration || 0;
  if (S.keep) {
    S.start = Math.min(S.keep.start, S.dur);
    S.end = Math.min(S.keep.end, S.dur);
    seekTo(Math.min(S.keep.t, S.dur));
    S.keep = null;
  } else {
    S.start = 0;
    S.end = S.dur;
  }
  renderTrim();
});
video.addEventListener("timeupdate", renderHead);
video.addEventListener("error", () => {
  if (S.keep) return; // we removed the source on purpose (rename)
  if (video.getAttribute("src")) toast("This clip can't be played here. HEVC needs Microsoft's HEVC Video Extensions.", true);
});

// ---- playback stays inside the trim unless you deliberately put the playhead outside it
const bounds = { raf: 0, prog: { t: -1, at: 0 } };
function seekTo(t) {
  bounds.prog = { t, at: performance.now() };
  video.currentTime = t;
}
// A seek we didn't make is the user's: if it lands outside the trim, let playback roam from there.
video.addEventListener("seeking", () => {
  const p = bounds.prog;
  if (performance.now() - p.at < 1500 && Math.abs(video.currentTime - p.t) < 0.1) return;
  S.freeRoam = S.dur > 0 && (video.currentTime < S.start - 0.01 || video.currentTime > S.end + 0.01);
});
function guard() {
  bounds.raf = 0;
  if (video.paused || !S.dur) return;
  const t = video.currentTime;
  if (S.freeRoam) {
    if (t >= S.start && t < S.end) S.freeRoam = false; // back inside: the trim applies again
  } else if (t >= S.end - 0.03) {
    if (S.previewing) { video.pause(); S.previewing = false; }
    else seekTo(S.start); // loop the selection
  } else if (t < S.start - 0.05) {
    seekTo(S.start);
  }
  bounds.raf = requestAnimationFrame(guard);
}
video.addEventListener("play", () => {
  if (S.dur && !S.freeRoam) {
    const t = video.currentTime;
    if (t < S.start - 0.05 || t >= S.end - 0.05) seekTo(S.start);
  }
  if (!bounds.raf) bounds.raf = requestAnimationFrame(guard);
});
video.addEventListener("pause", () => {
  S.previewing = false;
  cancelAnimationFrame(bounds.raf);
  bounds.raf = 0;
});
video.addEventListener("ended", () => {
  if (S.dur && !S.freeRoam) { seekTo(S.start); video.play().catch(() => {}); }
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
  if (drag === "start") { setStart(t); S.freeRoam = false; seekTo(S.start); }
  else if (drag === "end") { setEnd(t); S.freeRoam = false; seekTo(S.end); }
  else { video.currentTime = t; renderHead(); } // a click or drag on the bar is on purpose, even outside the trim
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
$("#resetTrim").addEventListener("click", () => { S.start = 0; S.end = S.dur; S.freeRoam = false; renderTrim(); });
$("#previewSel").addEventListener("click", () => {
  S.freeRoam = false;
  seekTo(S.start);
  video.play().catch(() => {});
  S.previewing = true; // set after play(): the pause event of a restart clears it
});

document.addEventListener("keydown", (e) => {
  if (S.view !== "library" || !S.sel || inInput() || e.ctrlKey || e.altKey || e.metaKey || $("#confirm").open) return;
  const k = e.key.toLowerCase();
  if (k === "i") setStart(video.currentTime);
  else if (k === "o") setEnd(video.currentTime);
  else if (k === " " && document.activeElement !== video) { e.preventDefault(); video.paused ? video.play() : video.pause(); }
});

// ------------------------------------------------------------------ export
// rough size of a lossless .png of game footage at the clip's resolution
function frameEst() {
  const px = (video.videoWidth || 1920) * (video.videoHeight || 1080);
  return `About <b>${fmtMb((px * 1.8) / 1e6)}</b>`;
}
const SIZE_PRESETS = [5, 10, 25, 50, 100, 200, 500];
const RATE_PRESETS = [1000, 2500, 5000, 8000, 15000, 25000, 40000, 60000];

const audioTier = (total) => (total < 600 ? 48 : total < 2000 ? 96 : 128);
const isWebm = () => S.format === "webm";
const isGif = () => S.format === "gif";
const isPng = () => S.format === "png"; // a screenshot: the frame under the playhead, untouched
const GIF_BYTES_PER_PX = 0.1; // rough: palette GIFs of game footage land near this
const GIF_MAX_SECS = 60;
const srcCodec = () => S.srcCodec || S.cfg?.codec || "h264";
// "Same as clip" can only mean something when nothing but the cut changes; otherwise it's H.264.
const effCodec = () => (S.mode === "original" ? S.codec : S.codec === "keep" ? "h264" : S.codec);
// Original quality in a different codec: a quality-matched re-encode instead of a straight copy.
const recoding = () => S.mode === "original" && !isWebm() && !isGif() && !isPng() && S.codec !== "keep" && S.codec !== srcCodec();
const codecName = (c) => ({ h264: "H.264", hevc: "HEVC" }[c] || c);

// What the clip itself weighs, so nothing can be exported bigger than "native".
function native() {
  const len = S.end - S.start;
  if (!S.sel || !S.dur || len <= 0) return null;
  const total = (S.sel.size * 8) / 1000 / S.dur;        // kbps, video + audio
  return { len, total, mb: (total * 1000 / 8 * len) / 1e6, cap: total - 128 };
}
const sizeOk = (mb, n) => mb > 0 && mb < n.mb * 0.98;
const rateOk = (k, n) => k >= 300 && k <= n.cap;
const mbAt = (kbps, len) => (kbps * 1000 / 8 * len) / 1e6;

function exportTag() {
  if (isGif()) return "gif";
  if (isPng()) return "frame";
  if (S.mode === "size" && S.mb > 0) return `${Number.isInteger(S.mb) ? S.mb : S.mb.toFixed(1)}MB`;
  if (S.mode === "bitrate" && S.kbps > 0) return `${trimNum(S.kbps / 1000)}Mbps`;
  if (recoding()) return effCodec();
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
  const webm = isWebm();
  const gif = isGif();
  const png = isPng();
  const clipOnly = gif || png; // these ignore the size / bitrate / codec choices
  const recode = recoding();
  const reencode = S.mode !== "original" || webm || recode;
  $$("#modeSeg button").forEach((b) => b.classList.toggle("active", b.dataset.mode === S.mode));
  $("#modeSeg").parentElement.hidden = clipOnly;
  $("#sizeRow").hidden = clipOnly || S.mode !== "size";
  $("#rateRow").hidden = clipOnly || S.mode !== "bitrate";
  $("#gifFpsWrap").hidden = !gif;
  $("#gifResWrap").hidden = !gif;
  $("#expResWrap").hidden = clipOnly;
  $("#expPreciseWrap").hidden = clipOnly;
  $("#expCodecWrap").hidden = png;
  $("#expLowWrap").hidden = png;
  if (!S.exporting) $("#exportBtn").textContent = png ? "Save frame" : "Export";
  $("#expFormat").value = S.format;
  const cs = $("#expCodec");
  cs.querySelector('[value="keep"]').disabled = S.mode !== "original";
  cs.disabled = webm || gif;
  cs.value = gif ? "gif" : webm ? "vp9" : effCodec();
  $("#expRes").disabled = !reencode;
  $("#expPrecise").disabled = !reencode;
  $("#customRateUnit").textContent = unitLabel();

  const cap = $("#capHint");
  cap.hidden = true;
  let valid = !!n;
  let est = "";
  let hint = "";

  if (png) {
    valid = S.dur > 0;
    est = frameEst();
    hint = "Saves the exact frame under the playhead as a lossless .png, with no scaling or recompression. Pause on the frame you want first.";
  } else if (n) {
    pickDefaults(n);
    const customMb = $("#customMb").value !== "";
    const customRate = $("#customRate").value !== "";

    if (gif) {
      const h = Number($("#gifRes").value) || 480;
      const ar = video.videoWidth && video.videoHeight ? video.videoWidth / video.videoHeight : 16 / 9;
      const px = Math.round(h * ar) * Math.min(h, video.videoHeight || h);
      const mb = (px * Number($("#gifFps").value) * n.len * GIF_BYTES_PER_PX) / 1e6;
      est = `About <b>${fmtMb(mb)}</b>`;
      if (n.len > GIF_MAX_SECS) {
        valid = false;
        cap.hidden = false;
        cap.textContent = `A .gif can be at most ${GIF_MAX_SECS} s here. Trim the selection shorter.`;
      } else {
        hint = n.len > 15 ? "Long for a .gif: the file will be big. No sound." : "Looping, no sound. Size is a rough guess.";
      }
    } else if (S.mode === "size") {
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
        est = `About <b>${fmtMb(mbAt(total, n.len))}</b>`;
        hint = S.kbps < 800 ? "That will look rough." : "Video bitrate; audio is added on top.";
      }
    } else if (webm) {
      est = `About <b>${fmtMb(mbAt(n.total * 0.85, n.len))}</b>`;
      hint = ".webm can't hold this clip's video as it is, so it's re-encoded as VP9. That takes a while and keeps roughly the same quality in about 15% less space.";
    } else if (recode) {
      const r = hevc ? HEVC_RATIO : 1 / HEVC_RATIO;
      const v = Math.max(500, n.total - 128) * r;
      est = `About <b>${fmtMb(mbAt(v + 128, n.len))}</b>`;
      hint = hevc
        ? "Re-encodes as HEVC at the same picture quality. It takes longer, and not every player or site takes HEVC. Audio is untouched."
        : "Re-encodes as H.264 at the same picture quality. It takes longer, but plays everywhere. Audio is untouched.";
    } else {
      est = `About <b>${fmtMb(n.mb * (S.format === "mkv" ? 0.99 : 1))}</b>`;
      hint = "Cuts land on the nearest keyframe. Instant and lossless." +
        (S.format === "mkv" ? " .mkv comes out about 1% smaller than .mp4." : "");
    }
    if (webm && S.mode !== "original") hint += " .webm is re-encoded on the CPU, so it takes longer.";
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
$("#expFormat").addEventListener("change", (e) => {
  S.format = e.target.value;
  store.set("clipr.format", S.format);
  renderExport();
});
$("#expCodec").addEventListener("change", (e) => {
  if (e.target.value === "vp9") return;
  S.codec = e.target.value;
  store.set("clipr.codec", S.codec);
  renderExport();
});
$("#expRes").addEventListener("change", renderExport);
$("#gifFps").value = store.get("clipr.gifFps", "15");
$("#gifRes").value = store.get("clipr.gifRes", "480");
$("#gifFps").addEventListener("change", (e) => { store.set("clipr.gifFps", e.target.value); renderExport(); });
$("#gifRes").addEventListener("change", (e) => { store.set("clipr.gifRes", e.target.value); renderExport(); });

$("#exportBtn").addEventListener("click", async () => {
  const n = native() || (isPng() && S.sel && S.dur > 0 ? { total: 0 } : null);
  if (!S.sel || !n || S.exporting) return;
  S.exporting = true;
  S.expStart = Date.now();
  $("#exportBtn").disabled = true;
  $("#exportBtn").textContent = "Exporting";
  $("#progress").hidden = false;
  $("#progressBar").style.width = "0%";
  $("#progressInfo").textContent = "Starting…";
  try {
    const out = await invoke("export_clip", {
      req: {
        path: S.sel.path,
        start: isPng() ? video.currentTime : S.start,
        end: isPng() ? video.currentTime + 0.1 : S.end,
        mode: S.mode,
        format: S.format,
        codec: S.codec,
        target_mb: S.mode === "size" ? S.mb : 0,
        target_kbps: S.mode === "bitrate" ? S.kbps : 0,
        height: isGif() ? Number($("#gifRes").value) : Number($("#expRes").value),
        fps: Number($("#gifFps").value),
        precise: $("#expPrecise").checked,
        low_impact: $("#expLow").checked,
        name: S.nameTouched ? $("#expName").value.trim() : "",
        src_kbps: n.total,
      },
    });
    toast(`${isPng() ? "Saved" : "Exported"} ${baseName(out)}`);
    S.nameTouched = false;
    loadClips(); // the raw clip now shows as exported
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
    closeClip();
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
  const renamed = S.renamedFrom.splice(0);
  if (renamed.length && S.sel && renamed.some((n) => folderName(n) === S.sel.game)) {
    closeClip(); // its folder is about to move, so let go of the file first
    await new Promise((r) => setTimeout(r, 200));
  }
  try {
    await invoke("save_config", { cfg: { ...S.cfg } });
    if (renamed.length) loadClips();
    S.cfg = await invoke("get_config");
    flash("Saved");
    $("#hotkeyErr").hidden = true;
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

function curMonitor() {
  const id = S.cfg?.mode === "desktop" ? S.cfg.monitor : null;
  return S.monitors.find((x) => x.id === id) || S.monitors.find((x) => x.primary) || S.monitors[0];
}
const monitorHz = () => curMonitor()?.refresh_hz || 0;

// Rebuild a select's options only when they changed (a rebuild would close an open dropdown).
function fillSelect(el, opts, value) {
  const sig = JSON.stringify(opts);
  if (el.dataset.sig !== sig) {
    el.innerHTML = opts.map(([v, l, off]) => `<option value="${v}"${off ? " disabled" : ""}>${esc(l)}</option>`).join("");
    el.dataset.sig = sig;
  }
  el.value = String(value);
}

const even = (x) => Math.max(2, Math.floor(x / 2) * 2);

function renderHeight() {
  const m = curMonitor();
  const canScale = !S.ff || S.ff.scale_d3d11 !== false;
  const opts = [];
  let cur = S.cfg.height;
  if (m) {
    opts.push([0, `${m.width}×${m.height}`]);
    const hs = [1440, 1080, 720, 480];
    if (cur && cur < m.height && !hs.includes(cur)) hs.push(cur);
    hs.filter((h) => h < m.height).sort((a, b) => b - a)
      .forEach((h) => opts.push([h, `${even((m.width * h) / m.height)}×${h}`, !canScale]));
    if (cur >= m.height) cur = 0; // as big as the display is the same as no scaling
  } else {
    opts.push([0, "Same as the display"], [1440, "1440p"], [1080, "1080p"], [720, "720p"]);
  }
  $$("#height, #qHeight").forEach((el) => fillSelect(el, opts, cur));
}

function renderFps() {
  const hz = monitorHz();
  const opts = [[0, hz ? `${hz} fps (display)` : "Match my display"]];
  const list = [30, 60, 120, 144, 165, 240];
  if (S.cfg.fps && !list.includes(S.cfg.fps)) list.push(S.cfg.fps);
  list.sort((a, b) => a - b).forEach((f) => opts.push([f, `${f} fps`]));
  $$("#fps, #qFps").forEach((el) => fillSelect(el, opts, S.cfg.fps));
  $("#fpsHint").textContent = !S.cfg.fps
    ? "Follows your display's refresh rate."
    : hz && S.cfg.fps > hz
      ? `Your display only runs at ${hz} Hz, so the extra frames would be copies.`
      : "";
}

const lenLabel = (s) => (s < 60 ? `${s} s` : s % 60 ? `${Math.floor(s / 60)} min ${s % 60} s` : `${s / 60} min`);
function renderClipLen() {
  const list = [10, 15, 20, 30, 45, 60, 90, 120, 180, 300, 600];
  if (!list.includes(S.cfg.clip_seconds)) list.push(S.cfg.clip_seconds);
  list.sort((a, b) => a - b);
  fillSelect($("#qClip"), list.map((n) => [n, lenLabel(n)]), S.cfg.clip_seconds);
}

// Bitrate picked for the current settings. Mirrors `auto_bitrate` in config.rs; keep them equal.
function autoRate(c = S.cfg) {
  const m = curMonitor();
  let w = m?.width || 1920, h = m?.height || 1080;
  if (c.height && c.height < h) { w = even((w * c.height) / h); h = even(c.height); }
  const fps = c.fps || monitorHz() || 60;
  const h264 = (w * h * 60 * Math.pow(fps / 60, 0.75) * 0.2) / 1000;
  const k = c.codec === "hevc" ? h264 * HEVC_RATIO : h264;
  return Math.min(150000, Math.max(4000, Math.round(k / 500) * 500));
}
const effRate = (c = S.cfg) => (c.bitrate_auto ? autoRate(c) : c.bitrate_kbps);

function renderEstimates() {
  const c = S.cfg;
  if (!c) return;
  const rate = effRate(c);
  const total = rate + (c.audio ? c.audio_kbps : 0);
  const clipMb = (total * c.clip_seconds) / 8 / 1000;
  const bufSecs = Math.ceil(c.clip_seconds / 2) * 2 + 8;
  const bufMb = (total * bufSecs) / 8 / 1000;
  $("#clipEstimate").innerHTML =
    `<b>${lenLabel(c.clip_seconds)}</b> clip: about <b>${fmtMb(clipMb)}</b> · video <b>${fmtRate(rate)}</b><br>` +
    `Buffer: <b>${fmtMb(bufMb)}</b> ${c.buffer_in_ram ? "in RAM" : "on disk"}`;
  $("#ramHint").textContent = c.buffer_in_ram
    ? `Uses about ${fmtMb(bufMb)} of memory while recording and writes nothing to your drive. It's emptied when recording stops, or a few minutes after the game closes.`
    : `Off: the buffer (about ${fmtMb(bufMb)}) is written to your drive. Turn on to keep it in memory and spare your SSD.`;
  $("#bufferDirField").hidden = !!c.buffer_in_ram;
}

// Put config values into the controls. `force` also overwrites the field being edited.
function fillControls(force = false) {
  const c = S.cfg;
  if (!c) return;
  const act = document.activeElement;
  const set = (sel, fn) => { const el = $(sel); if (force || el !== act) fn(el); };

  setSeg("#captureSeg", "mode", c.mode);
  $("#monitorField").hidden = c.mode !== "desktop";
  $("#methodField").hidden = c.mode !== "games";
  $$("#methodChips button").forEach((b) => b.classList.toggle("active", b.dataset.method === c.capture_method));
  $("#methodHint").textContent = c.capture_method === "window"
    ? "Records only the game's own window, so anything in front of it (Discord, a browser, a notification) never shows up. If a game's clips come out black, switch to Whole display."
    : "Records everything on the monitor, cropped to the game's window if it isn't fullscreen. Anything in front of the game is recorded too.";
  $("#modeHint2").textContent =
    c.mode === "games"
      ? "Records the game's window, only while one of your games is running. Nothing runs otherwise."
      : "Records the chosen monitor all the time. Clips made while a listed game is focused are still filed under that game.";
  $("#closeHint").textContent = c.close_to_tray
    ? "The X closes this window; Clipr keeps recording from the tray."
    : "The X quits Clipr completely, which also stops recording.";
  set("#monitorSel", (el) => { if (c.monitor) el.value = c.monitor; });
  set("#drawMouse", (el) => (el.checked = c.draw_mouse));
  set("#startHidden", (el) => (el.checked = c.start_hidden));
  set("#closeToTray", (el) => (el.checked = c.close_to_tray));
  set("#minToTray", (el) => (el.checked = c.minimize_to_tray));
  set("#beep", (el) => (el.checked = c.beep));
  set("#audio", (el) => (el.checked = c.audio));
  set("#gentleSave", (el) => (el.checked = c.gentle_save));
  set("#bufferRam", (el) => (el.checked = c.buffer_in_ram));
  set("#height", (el) => (el.value = String(c.height)));
  set("#encoder", (el) => (el.value = c.encoder));
  set("#codec", (el) => (el.value = c.codec));
  set("#clipSeconds", (el) => (el.value = c.clip_seconds));
  renderClipLen();
  set("#holdMinutes", (el) => (el.value = c.hold_minutes));
  set("#audioKbps", (el) => (el.value = c.audio_kbps));
  set("#audioOffset", (el) => (el.value = c.audio_offset_ms));
  set("#clipsDir", (el) => (el.value = c.clips_dir));
  set("#bufferDir", (el) => (el.value = c.buffer_dir || ""));
  set("#ffmpeg", (el) => (el.value = c.ffmpeg));
  set("#hotkey", (el) => { if (!el.classList.contains("listening")) el.value = c.hotkey; });
  set("#hotkey2", (el) => { if (!el.classList.contains("listening")) el.value = c.hotkey2 || ""; });

  $$("#unitChips button").forEach((b) => b.classList.toggle("active", b.dataset.unit === unit()));
  $("#bitrateLabel").textContent = `Bitrate (${unitLabel()})`;
  $("#bitrateUnit").textContent = unitLabel();
  const br = $("#bitrate");
  br.step = unit() === "mbps" ? 1 : 500;
  br.min = unit() === "mbps" ? 2 : 2000;
  br.max = unit() === "mbps" ? 150 : 150000;
  br.disabled = c.bitrate_auto;
  $("#bitrateManual").checked = !c.bitrate_auto;
  set("#bitrate", (el) => (el.value = rateToInput(effRate(c))));
  $("#bitrateHint").textContent = c.bitrate_auto
    ? "Picked for you from the resolution, frame rate and codec."
    : `For these settings Clipr would pick ${fmtRate(autoRate(c))}.`;
  renderFps();
  renderHeight();
  renderGames();
  $("#hkKey").textContent = c.hotkey;
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
  renderStorage();
  startMemPoll();
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
onChange("#closeToTray", (el) => ({ close_to_tray: el.checked }));
onChange("#minToTray", (el) => ({ minimize_to_tray: el.checked }));
onChange("#beep", (el) => ({ beep: el.checked }));
onChange("#audio", (el) => ({ audio: el.checked }));
onChange("#gentleSave", (el) => ({ gentle_save: el.checked }));
onChange("#bufferRam", (el) => ({ buffer_in_ram: el.checked }));
onChange("#height", (el) => ({ height: Number(el.value) }));
onChange("#qHeight", (el) => ({ height: Number(el.value) }));
onChange("#fps", (el) => ({ fps: Number(el.value) }));
onChange("#qFps", (el) => ({ fps: Number(el.value) }));
onChange("#qClip", (el) => ({ clip_seconds: Number(el.value) }));
onChange("#bitrateManual", (el) => (el.checked ? { bitrate_auto: false, bitrate_kbps: autoRate() } : { bitrate_auto: true }));
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
onChange("#holdMinutes", (el) => {
  const v = numField(el, 0, 60);
  return v === undefined ? undefined : { hold_minutes: Math.round(v) };
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
$$("#methodChips button").forEach((b) => b.addEventListener("click", () => setCfg({ capture_method: b.dataset.method })));
$$("#unitChips button").forEach((b) => b.addEventListener("click", () => {
  setCfg({ bitrate_unit: b.dataset.unit });
  $("#customRate").value = "";
}));
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
  if (e.target.dataset.k === "enabled") {
    g.enabled = e.target.checked;
    row.classList.toggle("off", !g.enabled);
    schedule(150);
  }
});
// a new name is applied when you leave the field; the game's clips move to it
$("#gameList").addEventListener("change", (e) => {
  const row = e.target.closest(".game");
  if (!row || e.target.dataset.k !== "name") return;
  const g = S.cfg.games[Number(row.dataset.i)];
  const name = e.target.value.trim();
  if (!name) { e.target.value = g.name; return; }
  if (name === g.name) return;
  if (folderName(name) !== folderName(g.name)) S.renamedFrom.push(g.name);
  g.name = name;
  schedule(150);
});
$("#gameList").addEventListener("click", (e) => {
  if (e.target.dataset.k !== "remove") return;
  S.cfg.games.splice(Number(e.target.closest(".game").dataset.i), 1);
  $("#gameList").querySelectorAll(":focus").forEach((x) => x.blur());
  renderGames();
  schedule(150);
});

function addGame(exe) {
  exe = exe.trim();
  if (!exe) return;
  if (!/\.exe$/i.test(exe)) exe += ".exe";
  if (S.cfg.games.some((g) => g.exe.toLowerCase() === exe.toLowerCase())) {
    toast(`${exe} is already in the list`);
    return;
  }
  S.cfg.games.push({ exe, name: exe.replace(/\.exe$/i, ""), enabled: true });
  renderGames();
  schedule(150);
}

async function refreshRunning() {
  const apps = await invoke("list_windows");
  $("#runningSel").innerHTML =
    `<option value="">Add a running app…</option>` +
    apps.map((a) => `<option value="${esc(a.exe)}" data-title="${esc(a.title)}">${esc(a.exe)}, ${esc(a.title.slice(0, 50))}</option>`).join("");
}
$("#refreshRunning").addEventListener("click", refreshRunning);
$("#addRunning").addEventListener("click", () => {
  const sel = $("#runningSel");
  if (!sel.value) return;
  addGame(sel.value);
  sel.value = "";
});
$("#addManual").addEventListener("click", () => {
  addGame($("#manualExe").value);
  $("#manualExe").value = "";
});
$("#manualExe").addEventListener("keydown", (e) => { if (e.key === "Enter") $("#addManual").click(); });

// ---- shortcuts: click, then press the keys. The second one is optional (Backspace clears it).
$("#hotkey2").placeholder = "Not set";
function bindHotkey(el, key, optional) {
  const other = () => (key === "hotkey" ? S.cfg.hotkey2 : S.cfg.hotkey) || "";
  let prev = "";
  el.addEventListener("focus", () => { prev = S.cfg[key] || ""; el.value = "Press keys…"; el.classList.add("listening"); });
  el.addEventListener("blur", () => {
    el.classList.remove("listening");
    if (el.value === "Press keys…") el.value = S.cfg[key] || "";
  });
  el.addEventListener("keydown", (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") { el.value = prev; el.blur(); return; }
    if (e.key === "Backspace") {
      el.value = prev;
      el.blur();
      if (!optional) { flash("The main shortcut can't be empty", true); return; }
      if (prev) { S.cfg[key] = ""; flush(); }
      return;
    }
    if (["Control", "Alt", "Shift", "Meta"].includes(e.key)) return;
    let k = e.code;
    if (k.startsWith("Key")) k = k.slice(3);
    else if (k.startsWith("Digit")) k = k.slice(5);
    const combo = [e.ctrlKey && "Ctrl", e.altKey && "Alt", e.shiftKey && "Shift", e.metaKey && "Super", k].filter(Boolean).join("+");
    el.classList.remove("listening");
    el.value = combo;
    el.blur();
    if (other() && combo.toLowerCase() === other().toLowerCase()) {
      el.value = prev;
      $("#hotkeyErr").textContent = "That's already your other shortcut. Pick a different one.";
      $("#hotkeyErr").hidden = false;
      return;
    }
    $("#hotkeyErr").hidden = true;
    if (combo !== S.cfg[key]) {
      S.cfg[key] = combo;
      flush(); // a taken shortcut is reported (and reverted) straight away
    }
  });
}
bindHotkey($("#hotkey"), "hotkey", false);
bindHotkey($("#hotkey2"), "hotkey2", true);

// ---- ffmpeg
let ffBusy = false;
async function downloadFfmpeg() {
  if (ffBusy) return;
  ffBusy = true;
  $("#getFfmpeg").hidden = true;
  $("#ffBannerBtn").hidden = true;
  const say = (t) => { $("#ffHint").textContent = t; $("#ffBannerText").textContent = t; };
  say("Downloading FFmpeg...");
  const un = await listen("ffmpeg-download", (e) => say(`Downloading FFmpeg... ${(e.payload / 1048576).toFixed(0)} MB`));
  try {
    S.cfg.ffmpeg = await invoke("download_ffmpeg");
    $("#ffmpeg").value = S.cfg.ffmpeg;
    toast("FFmpeg is ready");
  } catch (x) { toast(String(x), true); }
  un();
  ffBusy = false;
  renderFfHints();
}
$("#getFfmpeg").addEventListener("click", downloadFfmpeg);
$("#ffBannerBtn").addEventListener("click", downloadFfmpeg);

async function renderFfHints() {
  if (ffBusy) return;
  try {
    S.ff = await invoke("ffmpeg_info");
  } catch { S.ff = { ok: false }; }
  const f = S.ff;
  $("#getFfmpeg").hidden = !!f.ok;
  $("#ffBanner").hidden = !!f.ok;
  $("#ffBannerBtn").hidden = !!f.ok ? true : false;
  if (!f.ok) {
    $("#ffBannerText").textContent = "FFmpeg isn't set up yet. Clipr needs it to record and export.";
    $("#ffHint").textContent = "FFmpeg wasn't found. Use the button above to download it, or enter the full path to ffmpeg.exe.";
    $("#encHint").textContent = "";
    return;
  }
  $("#ffHint").textContent = f.ddagrab ? f.version : `${f.version}. This build has no ddagrab filter, so screen capture won't work. Use a full build from gyan.dev or BtbN.`;
  $("#encHint").textContent = f.encoders.length
    ? `Your FFmpeg supports: ${f.encoders.map((e) => ({ nvenc: "NVIDIA", amf: "AMD", qsv: "Intel" }[e])).join(", ")}`
    : "This FFmpeg build has no hardware encoders.";
  $("#scaleHint").textContent = f.scale_d3d11 ? "" : "Native only: this FFmpeg can't resize on the GPU (needs FFmpeg 8).";
  renderHeight();
  renderEstimates();
}

// ---- storage: what the clips use, cleanup, live memory
async function renderStorage() {
  let s;
  try { s = await invoke("storage_info"); } catch { return; }
  $("#stCard").innerHTML =
    `Raw clips <b>${fmtSize(s.raw_bytes)}</b> in ${plural(s.raw_count, "clip")}<br>` +
    `Exported clips <b>${fmtSize(s.exports_bytes)}</b> in ${plural(s.exports_count, "clip")}`;
  $("#cleanupBtn").disabled = !s.done_count;
  $("#cleanupHint").textContent = s.done_count
    ? `${plural(s.done_count, "raw clip")} (${fmtSize(s.done_bytes)}) already ${s.done_count === 1 ? "has" : "have"} a trimmed version. Your exported clips aren't touched.`
    : "No raw clips have a trimmed version yet. Clipr remembers which ones you've exported.";
}
$("#openClips").addEventListener("click", () => invoke("open_clips_folder", { kind: "all" }).catch((e) => toast(e, true)));
$("#cleanupBtn").addEventListener("click", async () => {
  const s = await invoke("storage_info");
  if (!s.done_count) return renderStorage();
  const ok = await confirmDialog(
    `Delete ${plural(s.done_count, "raw clip")}?`,
    `These raw clips already have a trimmed version, and together they use ${fmtSize(s.done_bytes)}. Your exported clips stay. This can't be undone.`,
    "Delete them"
  );
  if (!ok) return;
  if (S.sel && S.selKind === "raw") closeClip(); // lets go of the open file
  try {
    const r = await invoke("delete_exported_raws");
    toast(`Deleted ${plural(r.count, "raw clip")} and freed ${fmtSize(r.bytes)}`);
  } catch (e) { toast(String(e), true); }
  renderStorage();
  loadClips();
});

let memTimer = null, memTick = 0;
async function pollMem() {
  try {
    const m = await invoke("memory_info");
    const st = S.status || {};
    const usedPct = m.system_total ? Math.round((m.system_used / m.system_total) * 100) : 0;
    $("#memCard").innerHTML =
      `Clipr and ffmpeg use <b>${fmtSize(m.clipr_bytes)}</b>` +
      (st.buffer_ram && st.buffer_bytes ? `, including a RAM buffer of <b>${fmtSize(st.buffer_bytes)}</b>` : "") + `.<br>` +
      `Your PC: <b>${fmtSize(m.system_used)}</b> of ${fmtSize(m.system_total)} in use (${usedPct}%)`;
    $("#memBar").style.width = usedPct + "%";
  } catch {}
  if (++memTick % 5 === 0) renderStorage(); // clips come and go while this screen is open
}
function startMemPoll() {
  stopMemPoll();
  memTick = 0;
  pollMem();
  memTimer = setInterval(() => { if (!document.hidden) pollMem(); }, 2000);
}
function stopMemPoll() { clearInterval(memTimer); memTimer = null; }

// ---- advanced / about
$("#openData").addEventListener("click", () => invoke("open_data_folder").catch((e) => toast(e, true)));
$("#resetAll").addEventListener("click", async () => {
  const ok = await confirmDialog(
    "Reset all settings?",
    "Every setting goes back to its default, including your games list and shortcuts. Your clips and the FFmpeg and clips-folder paths are not touched.",
    "Reset settings"
  );
  if (!ok) return;
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
  $("#emptySub").innerHTML = `Clips are saved with <kbd>${esc(S.cfg.hotkey)}</kbd>.`;
  fillControls(true);
  renderEstimates();
  $("#sortSel").value = S.sort;
  $("#groupBy").checked = S.group;
  renderStatus(await invoke("get_status"));
  renderExport();
  await loadClips();
  renderFfHints(); // shows the download strip if FFmpeg is missing
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
    if (!S.sel || e.payload.path !== S.sel.path) return;
    const p = e.payload.pct;
    $("#progressBar").style.width = Math.round(p * 100) + "%";
    const parts = [`${Math.round(p * 100)}%`];
    const elapsed = (Date.now() - S.expStart) / 1000;
    if (p > 0.03 && p < 1) parts.push(`about ${fmtEta((elapsed * (1 - p)) / p)} left`);
    $("#progressInfo").textContent = parts.join(" · ");
  });
})();
